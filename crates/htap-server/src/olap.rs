//! Typed analytical query executor for LocalServer.
//!
//! Evaluates [`AnalyticSelect`] statements over a set of logical partition [`Row`]s.
//! Implements SQL filter evaluation (`WHERE`), scalar and grouped aggregation
//! (`COUNT(*)`, `COUNT(col)`, `SUM(col)`, `MIN(col)`, `MAX(col)`), deterministic
//! `GROUP BY` ordering, and NULL group semantics.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_convert::Predicate;
use htap_sql::ast::{
    AggregateFunction, AnalyticExpr, AnalyticFilter, AnalyticSelect, ComparisonOp,
};
use htap_sql::result::QueryResult;

/// Executes an [`AnalyticSelect`] against a collection of logical input rows.
///
/// Returns a [`QueryResult`] matching `select.output_schema`.
///
/// # Errors
///
/// Returns [`HtapError`] on:
/// - Evaluation type mismatches or missing columns
/// - Arithmetic overflow during SUM aggregation
/// - Unsupported aggregate types
pub fn execute_analytic_select(select: &AnalyticSelect, rows: Vec<Row>) -> Result<QueryResult> {
    let output_columns: Vec<ColumnDef> = select.output_schema.columns().to_vec();

    // 1. Evaluate WHERE filter if present
    let filtered_rows = match &select.filter {
        Some(filter) => {
            let mut passed = Vec::new();
            for row in rows {
                if evaluate_filter(filter, &row)? {
                    passed.push(row);
                }
            }
            passed
        }
        None => rows,
    };

    let has_aggregates = select.projection.iter().any(|e| e.is_aggregate());
    let has_group_by = !select.group_by.is_empty();

    // 2. Dispatch execution based on aggregation and grouping
    if !has_group_by {
        if !has_aggregates {
            // Case A: Plain scan / projection (no aggregates, no GROUP BY)
            let mut projected_rows = Vec::with_capacity(filtered_rows.len());
            for row in filtered_rows {
                let mut values = Vec::with_capacity(select.projection.len());
                for expr in &select.projection {
                    match expr {
                        AnalyticExpr::Column { index, .. } => {
                            let val = row.get(*index).cloned().ok_or_else(|| {
                                HtapError::Internal(format!("row missing column index {index}"))
                            })?;
                            values.push(val);
                        }
                        AnalyticExpr::Aggregate { .. } => unreachable!(),
                    }
                }
                projected_rows.push(Row::new(values));
            }
            Ok(QueryResult::new(output_columns, projected_rows))
        } else {
            // Case B: Global aggregation (aggregates present, no GROUP BY)
            let mut accumulators = Vec::with_capacity(select.projection.len());
            for expr in &select.projection {
                accumulators.push(Accumulator::from_expr(expr)?);
            }

            if filtered_rows.is_empty() {
                // Empty global aggregate returns exactly one row: COUNT 0, other aggregates NULL.
                let values: Vec<Value> = accumulators.iter().map(|acc| acc.finish()).collect();
                Ok(QueryResult::new(output_columns, vec![Row::new(values)]))
            } else {
                for row in &filtered_rows {
                    for acc in &mut accumulators {
                        acc.update(row)?;
                    }
                }
                let values: Vec<Value> = accumulators.iter().map(|acc| acc.finish()).collect();
                Ok(QueryResult::new(output_columns, vec![Row::new(values)]))
            }
        }
    } else {
        // Case C: Grouped aggregation / plain GROUP BY
        if filtered_rows.is_empty() {
            // Empty grouped aggregate returns zero rows.
            return Ok(QueryResult::new(output_columns, Vec::new()));
        }

        let acc_template: Vec<Option<Accumulator>> = select
            .projection
            .iter()
            .map(|expr| match expr {
                AnalyticExpr::Aggregate { .. } => Accumulator::from_expr(expr).map(Some),
                AnalyticExpr::Column { .. } => Ok(None),
            })
            .collect::<Result<Vec<_>>>()?;

        let mut groups: BTreeMap<Vec<Value>, Vec<Option<Accumulator>>> = BTreeMap::new();

        for row in &filtered_rows {
            let mut group_key = Vec::with_capacity(select.group_by.len());
            for &col_idx in &select.group_by {
                let val = row.get(col_idx).cloned().ok_or_else(|| {
                    HtapError::Internal(format!("row missing GROUP BY column index {col_idx}"))
                })?;
                group_key.push(val);
            }

            let group_accs = groups
                .entry(group_key)
                .or_insert_with(|| acc_template.clone());
            for acc in group_accs.iter_mut().flatten() {
                acc.update(row)?;
            }
        }

        let mut output_rows = Vec::with_capacity(groups.len());
        for (group_key, group_accs) in groups {
            let mut row_values = Vec::with_capacity(select.projection.len());
            for (i, expr) in select.projection.iter().enumerate() {
                match expr {
                    AnalyticExpr::Column { index, .. } => {
                        let pos = select
                            .group_by
                            .iter()
                            .position(|&g_idx| g_idx == *index)
                            .ok_or_else(|| {
                                HtapError::Internal(format!(
                                    "projected column index {index} not found in GROUP BY"
                                ))
                            })?;
                        row_values.push(group_key[pos].clone());
                    }
                    AnalyticExpr::Aggregate { .. } => {
                        let acc = group_accs[i].as_ref().ok_or_else(|| {
                            HtapError::Internal("missing aggregate accumulator".into())
                        })?;
                        row_values.push(acc.finish());
                    }
                }
            }
            output_rows.push(Row::new(row_values));
        }

        Ok(QueryResult::new(output_columns, output_rows))
    }
}

/// Plans the required source columns for an [`AnalyticSelect`] by collecting column references
/// from projection expressions, `GROUP BY` columns, and filter leaf predicates.
///
/// Returns a sorted and deduplicated vector of source column indices, along with a mapping
/// from original source table column index to compact position in that vector.
pub fn plan_source_columns(select: &AnalyticSelect) -> (Vec<usize>, BTreeMap<usize, usize>) {
    let mut columns = BTreeSet::new();

    for expr in &select.projection {
        match expr {
            AnalyticExpr::Column { index, .. } => {
                columns.insert(*index);
            }
            AnalyticExpr::Aggregate { column_index, .. } => {
                if let Some(idx) = column_index {
                    columns.insert(*idx);
                }
            }
        }
    }

    for &idx in &select.group_by {
        columns.insert(idx);
    }

    if let Some(filter) = &select.filter {
        for leaf in filter.leaves() {
            match leaf {
                AnalyticFilter::Comparison { column, .. } => {
                    columns.insert(*column);
                }
                AnalyticFilter::IsNull { column } | AnalyticFilter::IsNotNull { column } => {
                    columns.insert(*column);
                }
                AnalyticFilter::And(_) => {}
            }
        }
    }

    let source_columns: Vec<usize> = columns.into_iter().collect();
    let mapping: BTreeMap<usize, usize> = source_columns
        .iter()
        .enumerate()
        .map(|(compact_idx, &orig_idx)| (orig_idx, compact_idx))
        .collect();

    (source_columns, mapping)
}

/// Selects at most one deterministic pushdown leaf predicate from the filter tree.
///
/// Evaluates leaf predicates in deterministic AST order. Eligible pushdown leaves are
/// `Eq`, `Lt`, `Lte`, `Gt`, `Gte`, `IsNull`, and `IsNotNull`. `NotEq` is never pushed down,
/// and `AND` trees are never pushed down as a whole.
pub fn select_pushdown_predicate(filter: Option<&AnalyticFilter>) -> Option<Predicate> {
    let filter = filter?;
    for leaf in filter.leaves() {
        match leaf {
            AnalyticFilter::Comparison { column, op, value } => {
                if value.is_null() {
                    continue;
                }
                let pred = match op {
                    ComparisonOp::Eq => Some(Predicate::Eq {
                        column: *column,
                        value: value.clone(),
                    }),
                    ComparisonOp::Lt => Some(Predicate::Lt {
                        column: *column,
                        value: value.clone(),
                    }),
                    ComparisonOp::Lte => Some(Predicate::Lte {
                        column: *column,
                        value: value.clone(),
                    }),
                    ComparisonOp::Gt => Some(Predicate::Gt {
                        column: *column,
                        value: value.clone(),
                    }),
                    ComparisonOp::Gte => Some(Predicate::Gte {
                        column: *column,
                        value: value.clone(),
                    }),
                    ComparisonOp::NotEq => None,
                };
                if pred.is_some() {
                    return pred;
                }
            }
            AnalyticFilter::IsNull { column } => {
                return Some(Predicate::IsNull { column: *column });
            }
            AnalyticFilter::IsNotNull { column } => {
                return Some(Predicate::IsNotNull { column: *column });
            }
            AnalyticFilter::And(_) => {}
        }
    }
    None
}

/// Remaps original source column indices in an [`AnalyticSelect`] statement to compact positions
/// according to `mapping`.
pub fn remap_analytic_select(
    select: &AnalyticSelect,
    mapping: &BTreeMap<usize, usize>,
) -> Result<AnalyticSelect> {
    let remap_idx = |idx: usize| -> Result<usize> {
        mapping.get(&idx).copied().ok_or_else(|| {
            HtapError::Internal(format!("column index {idx} not found in compact mapping"))
        })
    };

    let mut new_projection = Vec::with_capacity(select.projection.len());
    for expr in &select.projection {
        let new_expr = match expr {
            AnalyticExpr::Column {
                index,
                name,
                data_type,
                nullable,
            } => AnalyticExpr::Column {
                index: remap_idx(*index)?,
                name: name.clone(),
                data_type: *data_type,
                nullable: *nullable,
            },
            AnalyticExpr::Aggregate {
                function,
                column_index,
                name,
                data_type,
                nullable,
            } => AnalyticExpr::Aggregate {
                function: *function,
                column_index: match column_index {
                    Some(idx) => Some(remap_idx(*idx)?),
                    None => None,
                },
                name: name.clone(),
                data_type: *data_type,
                nullable: *nullable,
            },
        };
        new_projection.push(new_expr);
    }

    let new_group_by = select
        .group_by
        .iter()
        .map(|&idx| remap_idx(idx))
        .collect::<Result<Vec<_>>>()?;

    let new_filter = match &select.filter {
        Some(filter) => Some(remap_filter(filter, mapping)?),
        None => None,
    };

    Ok(AnalyticSelect {
        table: select.table.clone(),
        projection: new_projection,
        filter: new_filter,
        group_by: new_group_by,
        output_schema: select.output_schema.clone(),
    })
}

fn remap_filter(
    filter: &AnalyticFilter,
    mapping: &BTreeMap<usize, usize>,
) -> Result<AnalyticFilter> {
    let remap_idx = |idx: usize| -> Result<usize> {
        mapping.get(&idx).copied().ok_or_else(|| {
            HtapError::Internal(format!(
                "filter column index {idx} not found in compact mapping"
            ))
        })
    };

    match filter {
        AnalyticFilter::IsNull { column } => Ok(AnalyticFilter::IsNull {
            column: remap_idx(*column)?,
        }),
        AnalyticFilter::IsNotNull { column } => Ok(AnalyticFilter::IsNotNull {
            column: remap_idx(*column)?,
        }),
        AnalyticFilter::Comparison { column, op, value } => Ok(AnalyticFilter::Comparison {
            column: remap_idx(*column)?,
            op: *op,
            value: value.clone(),
        }),
        AnalyticFilter::And(children) => {
            let remapped_children = children
                .iter()
                .map(|c| remap_filter(c, mapping))
                .collect::<Result<Vec<_>>>()?;
            Ok(AnalyticFilter::And(remapped_children))
        }
    }
}

/// Executes an [`AnalyticSelect`] against compact input rows whose columns correspond to `mapping`.
///
/// Remaps original column references in `select` to compact column positions, then evaluates
/// filter, grouping, aggregations, and projections using the existing analytical query semantics.
pub fn execute_analytic_select_compact(
    select: &AnalyticSelect,
    rows: Vec<Row>,
    mapping: &BTreeMap<usize, usize>,
) -> Result<QueryResult> {
    let remapped_select = remap_analytic_select(select, mapping)?;
    execute_analytic_select(&remapped_select, rows)
}

fn evaluate_filter(filter: &AnalyticFilter, row: &Row) -> Result<bool> {
    match filter {
        AnalyticFilter::IsNull { column } => {
            let val = row
                .get(*column)
                .ok_or_else(|| HtapError::Internal(format!("row missing column index {column}")))?;
            Ok(val.is_null())
        }
        AnalyticFilter::IsNotNull { column } => {
            let val = row
                .get(*column)
                .ok_or_else(|| HtapError::Internal(format!("row missing column index {column}")))?;
            Ok(!val.is_null())
        }
        AnalyticFilter::Comparison { column, op, value } => {
            let val = row
                .get(*column)
                .ok_or_else(|| HtapError::Internal(format!("row missing column index {column}")))?;
            if val.is_null() {
                // SQL three-valued logic: comparison with NULL yields UNKNOWN (false in WHERE).
                return Ok(false);
            }
            if let Some(col_type) = val.data_type() {
                if let Some(val_type) = value.data_type() {
                    if col_type != val_type {
                        return Err(HtapError::InvalidArgument(format!(
                            "type mismatch in comparison: column has {col_type:?}, filter has {val_type:?}"
                        )));
                    }
                }
            }
            let matched = match op {
                ComparisonOp::Eq => val == value,
                ComparisonOp::NotEq => val != value,
                ComparisonOp::Lt => val < value,
                ComparisonOp::Lte => val <= value,
                ComparisonOp::Gt => val > value,
                ComparisonOp::Gte => val >= value,
            };
            Ok(matched)
        }
        AnalyticFilter::And(children) => {
            for child in children {
                if !evaluate_filter(child, row)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

#[derive(Debug, Clone)]
enum Accumulator {
    CountStar(i64),
    Count {
        column: usize,
        count: i64,
    },
    SumInt {
        column: usize,
        sum: Option<i64>,
    },
    SumFloat {
        column: usize,
        sum: Option<f64>,
    },
    Min {
        column: usize,
        value: Option<Value>,
        data_type: DataType,
    },
    Max {
        column: usize,
        value: Option<Value>,
        data_type: DataType,
    },
}

impl Accumulator {
    fn from_expr(expr: &AnalyticExpr) -> Result<Self> {
        match expr {
            AnalyticExpr::Aggregate {
                function,
                column_index,
                data_type,
                ..
            } => match function {
                AggregateFunction::CountStar => Ok(Accumulator::CountStar(0)),
                AggregateFunction::Count => {
                    let col = column_index
                        .ok_or_else(|| HtapError::Internal("COUNT requires column index".into()))?;
                    Ok(Accumulator::Count {
                        column: col,
                        count: 0,
                    })
                }
                AggregateFunction::Sum => {
                    let col = column_index
                        .ok_or_else(|| HtapError::Internal("SUM requires column index".into()))?;
                    match data_type {
                        DataType::Int64 => Ok(Accumulator::SumInt {
                            column: col,
                            sum: None,
                        }),
                        DataType::Float64 => Ok(Accumulator::SumFloat {
                            column: col,
                            sum: None,
                        }),
                        other => Err(HtapError::InvalidArgument(format!(
                            "unsupported data type for SUM: {other:?}"
                        ))),
                    }
                }
                AggregateFunction::Min => {
                    let col = column_index
                        .ok_or_else(|| HtapError::Internal("MIN requires column index".into()))?;
                    Ok(Accumulator::Min {
                        column: col,
                        value: None,
                        data_type: *data_type,
                    })
                }
                AggregateFunction::Max => {
                    let col = column_index
                        .ok_or_else(|| HtapError::Internal("MAX requires column index".into()))?;
                    Ok(Accumulator::Max {
                        column: col,
                        value: None,
                        data_type: *data_type,
                    })
                }
            },
            AnalyticExpr::Column { .. } => Err(HtapError::Internal(
                "cannot create accumulator for Column expression".into(),
            )),
        }
    }

    fn update(&mut self, row: &Row) -> Result<()> {
        match self {
            Accumulator::CountStar(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or(HtapError::CounterOverflow { counter: "count" })?;
            }
            Accumulator::Count { column, count } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                if !val.is_null() {
                    *count = count
                        .checked_add(1)
                        .ok_or(HtapError::CounterOverflow { counter: "count" })?;
                }
            }
            Accumulator::SumInt { column, sum } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                match val {
                    Value::Null => {}
                    Value::Int32(v) => {
                        let v64 = *v as i64;
                        let next = match sum {
                            None => v64,
                            Some(s) => s.checked_add(v64).ok_or_else(|| {
                                HtapError::InvalidArgument(
                                    "arithmetic overflow in SUM aggregation".into(),
                                )
                            })?,
                        };
                        *sum = Some(next);
                    }
                    Value::Int64(v) => {
                        let next = match sum {
                            None => *v,
                            Some(s) => s.checked_add(*v).ok_or_else(|| {
                                HtapError::InvalidArgument(
                                    "arithmetic overflow in SUM aggregation".into(),
                                )
                            })?,
                        };
                        *sum = Some(next);
                    }
                    other => {
                        return Err(HtapError::InvalidArgument(format!(
                            "unsupported value type for integer SUM: {other:?}"
                        )));
                    }
                }
            }
            Accumulator::SumFloat { column, sum } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                match val {
                    Value::Null => {}
                    Value::Float64(v) => {
                        let next = match sum {
                            None => *v,
                            Some(s) => *s + *v,
                        };
                        *sum = Some(next);
                    }
                    other => {
                        return Err(HtapError::InvalidArgument(format!(
                            "unsupported value type for float SUM: {other:?}"
                        )));
                    }
                }
            }
            Accumulator::Min {
                column,
                value,
                data_type,
            } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                if !val.is_null() {
                    if let Some(vt) = val.data_type() {
                        if vt != *data_type {
                            return Err(HtapError::InvalidArgument(format!(
                                "type mismatch in MIN aggregation: expected {data_type:?}, got {vt:?}"
                            )));
                        }
                    }
                    match value {
                        None => *value = Some(val.clone()),
                        Some(current) => {
                            if val < current {
                                *value = Some(val.clone());
                            }
                        }
                    }
                }
            }
            Accumulator::Max {
                column,
                value,
                data_type,
            } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                if !val.is_null() {
                    if let Some(vt) = val.data_type() {
                        if vt != *data_type {
                            return Err(HtapError::InvalidArgument(format!(
                                "type mismatch in MAX aggregation: expected {data_type:?}, got {vt:?}"
                            )));
                        }
                    }
                    match value {
                        None => *value = Some(val.clone()),
                        Some(current) => {
                            if val > current {
                                *value = Some(val.clone());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Value {
        match self {
            Accumulator::CountStar(c) => Value::Int64(*c),
            Accumulator::Count { count, .. } => Value::Int64(*count),
            Accumulator::SumInt { sum, .. } => sum.map(Value::Int64).unwrap_or(Value::Null),
            Accumulator::SumFloat { sum, .. } => sum.map(Value::Float64).unwrap_or(Value::Null),
            Accumulator::Min { value, .. } => value.clone().unwrap_or(Value::Null),
            Accumulator::Max { value, .. } => value.clone().unwrap_or(Value::Null),
        }
    }
}
