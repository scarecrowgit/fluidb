//! Typed analytical query executor for LocalServer.
//!
//! Evaluates [`AnalyticSelect`] statements over a set of logical partition [`Row`]s.
//! Implements SQL filter evaluation (`WHERE`), scalar and grouped aggregation
//! (`COUNT(*)`, `COUNT(col)`, `SUM(col)`, `MIN(col)`, `MAX(col)`), deterministic
//! `GROUP BY` ordering, and NULL group semantics.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use htap_catalog::{PartitionDescriptor, PartitioningMethod, TableDescriptor};
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_convert::Predicate;
use htap_sql::ast::{
    AggregateFunction, AnalyticExpr, AnalyticFilter, AnalyticOrderBy, AnalyticSelect, ComparisonOp,
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
            let mut rows = filtered_rows;
            if !select.order_by.is_empty() {
                rows.sort_by(|row_a, row_b| {
                    for order in &select.order_by {
                        let val_a = row_a.get(order.column).unwrap_or(&Value::Null);
                        let val_b = row_b.get(order.column).unwrap_or(&Value::Null);
                        let cmp =
                            compare_values_with_order(val_a, val_b, order.asc, order.nulls_first);
                        if cmp != std::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    // Deterministic tie-break with full output row
                    let proj_a = select.projection.iter().map(|e| match e {
                        AnalyticExpr::Column { index, .. } => {
                            row_a.get(*index).unwrap_or(&Value::Null)
                        }
                        _ => &Value::Null,
                    });
                    let proj_b = select.projection.iter().map(|e| match e {
                        AnalyticExpr::Column { index, .. } => {
                            row_b.get(*index).unwrap_or(&Value::Null)
                        }
                        _ => &Value::Null,
                    });
                    let proj_cmp = proj_a.cmp(proj_b);
                    if proj_cmp != std::cmp::Ordering::Equal {
                        return proj_cmp;
                    }
                    row_a.values().cmp(row_b.values())
                });
            }

            let mut projected_rows = Vec::with_capacity(rows.len());
            for row in rows {
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
                let values: Vec<Value> = accumulators
                    .iter()
                    .map(Accumulator::finish)
                    .collect::<Result<Vec<_>>>()?;
                Ok(QueryResult::new(output_columns, vec![Row::new(values)]))
            } else {
                for row in &filtered_rows {
                    for acc in &mut accumulators {
                        acc.update(row)?;
                    }
                }
                let values: Vec<Value> = accumulators
                    .iter()
                    .map(Accumulator::finish)
                    .collect::<Result<Vec<_>>>()?;
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

        let mut output_pairs = Vec::with_capacity(groups.len());
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
                        row_values.push(acc.finish()?);
                    }
                }
            }
            output_pairs.push((group_key, Row::new(row_values)));
        }

        if !select.order_by.is_empty() {
            output_pairs.sort_by(|(key_a, row_a), (key_b, row_b)| {
                for order in &select.order_by {
                    let pos = select
                        .group_by
                        .iter()
                        .position(|&idx| idx == order.column)
                        .unwrap_or(0);
                    let val_a = key_a.get(pos).unwrap_or(&Value::Null);
                    let val_b = key_b.get(pos).unwrap_or(&Value::Null);
                    let cmp = compare_values_with_order(val_a, val_b, order.asc, order.nulls_first);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                // Deterministic tie-break with full output row
                row_a.values().cmp(row_b.values())
            });
        }

        let output_rows = output_pairs.into_iter().map(|(_, r)| r).collect();
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

    for order in &select.order_by {
        columns.insert(order.column);
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

    let new_order_by = select
        .order_by
        .iter()
        .map(|order| {
            Ok(AnalyticOrderBy {
                column: remap_idx(order.column)?,
                asc: order.asc,
                nulls_first: order.nulls_first,
            })
        })
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
        order_by: new_order_by,
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
    SumDecimal {
        column: usize,
        sum: Option<i128>,
        precision: u8,
        scale: u8,
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

fn decimal_scaled_value(value: &Value, target_scale: u8) -> Result<i128> {
    match value {
        Value::Decimal {
            value,
            scale: value_scale,
            ..
        } => {
            let value = i128::from(*value);
            match target_scale.cmp(value_scale) {
                std::cmp::Ordering::Greater => {
                    let factor = 10_i128
                        .checked_pow(u32::from(target_scale - *value_scale))
                        .ok_or_else(|| {
                            HtapError::InvalidArgument("DECIMAL scale is out of range".into())
                        })?;
                    value
                        .checked_mul(factor)
                        .ok_or_else(|| HtapError::InvalidArgument("DECIMAL SUM overflow".into()))
                }
                std::cmp::Ordering::Less => {
                    let factor = 10_i128
                        .checked_pow(u32::from(*value_scale - target_scale))
                        .ok_or_else(|| {
                            HtapError::InvalidArgument("DECIMAL scale is out of range".into())
                        })?;
                    Ok(value / factor)
                }
                std::cmp::Ordering::Equal => Ok(value),
            }
        }
        other => Err(HtapError::InvalidArgument(format!(
            "unsupported value type for decimal SUM: {other:?}"
        ))),
    }
}

fn decimal_result_value(value: i128, precision: u8, scale: u8) -> Result<Value> {
    let max = 10_i128
        .checked_pow(u32::from(precision))
        .ok_or_else(|| HtapError::InvalidArgument("DECIMAL precision is out of range".into()))?;
    if value <= -max || value >= max {
        return Err(HtapError::InvalidArgument(
            "DECIMAL aggregate result is out of range".into(),
        ));
    }
    let value = i64::try_from(value).map_err(|_| {
        HtapError::InvalidArgument("DECIMAL aggregate result is out of range".into())
    })?;
    Ok(Value::Decimal {
        value,
        precision,
        scale,
    })
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
                        DataType::Decimal { precision, scale } => Ok(Accumulator::SumDecimal {
                            column: col,
                            sum: None,
                            precision: *precision,
                            scale: *scale,
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
                        if !next.is_finite() {
                            return Err(HtapError::InvalidArgument(
                                "DOUBLE value is out of range in 'SUM'".into(),
                            ));
                        }
                        *sum = Some(next);
                    }
                    other => {
                        return Err(HtapError::InvalidArgument(format!(
                            "unsupported value type for float SUM: {other:?}"
                        )));
                    }
                }
            }
            Accumulator::SumDecimal {
                column,
                sum,
                precision: _,
                scale,
            } => {
                let val = row.get(*column).ok_or_else(|| {
                    HtapError::Internal(format!("row missing column index {column}"))
                })?;
                if !val.is_null() {
                    let value = decimal_scaled_value(val, *scale)?;
                    *sum = Some(match *sum {
                        Some(current) => current.checked_add(value).ok_or_else(|| {
                            HtapError::InvalidArgument("DECIMAL SUM overflow".into())
                        })?,
                        None => value,
                    });
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

    fn finish(&self) -> Result<Value> {
        match self {
            Accumulator::CountStar(c) => Ok(Value::Int64(*c)),
            Accumulator::Count { count, .. } => Ok(Value::Int64(*count)),
            Accumulator::SumInt { sum, .. } => Ok(sum.map(Value::Int64).unwrap_or(Value::Null)),
            Accumulator::SumFloat { sum, .. } => Ok(sum.map(Value::Float64).unwrap_or(Value::Null)),
            Accumulator::SumDecimal {
                sum,
                precision,
                scale,
                ..
            } => sum
                .map(|value| decimal_result_value(value, *precision, *scale))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null)),
            Accumulator::Min { value, .. } => Ok(value.clone().unwrap_or(Value::Null)),
            Accumulator::Max { value, .. } => Ok(value.clone().unwrap_or(Value::Null)),
        }
    }
}

/// Compares two [`Value`] references according to sort direction and NULL ordering policy.
///
/// # NULL Ordering Policy
/// - If `nulls_first` is `true`, `Value::Null` sorts before any non-NULL value.
/// - If `nulls_first` is `false`, `Value::Null` sorts after any non-NULL value.
/// - When comparing two `Value::Null`s, they are equal.
/// - Non-NULL values are compared according to `asc`:
///   - If `asc` is `true`, natural ascending order (`a.cmp(b)`).
///   - If `asc` is `false`, descending order (`b.cmp(a)`).
pub fn compare_values_with_order(
    a: &Value,
    b: &Value,
    asc: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (false, false) => {
            let ord = a.cmp(b);
            if asc {
                ord
            } else {
                ord.reverse()
            }
        }
    }
}

/// Conservatively prunes partitions based on table partitioning metadata and filter predicates.
///
/// Only prunes when provably safe for partition key `Eq`, range comparisons (`<`, `<=`, `>`, `>=`),
/// `IsNull`, and `IsNotNull`.
/// Retains all partitions for `!=`, ambiguous type comparisons, missing metadata, or unsupported operators.
/// Preserves the original catalog order of retained partitions.
pub fn prune_partitions<'a>(
    table_desc: &TableDescriptor,
    partitions: &[&'a PartitionDescriptor],
    filter: Option<&AnalyticFilter>,
) -> Vec<&'a PartitionDescriptor> {
    let partitioning = match &table_desc.partitioning {
        Some(p) => p,
        None => return partitions.to_vec(),
    };

    let key_col_idx = partitioning.key_column;
    let key_col = match table_desc.schema.column(key_col_idx) {
        Some(col) => col,
        None => return partitions.to_vec(),
    };

    let filter = match filter {
        Some(f) => f,
        None => return partitions.to_vec(),
    };

    let leaves = filter.leaves();

    partitions
        .iter()
        .copied()
        .filter(|part| {
            for leaf in &leaves {
                match leaf {
                    AnalyticFilter::IsNull { column } if *column == key_col_idx => {
                        // Partition key values in this architecture are strictly non-null.
                        // Therefore, `key IS NULL` can never match any row in any partition.
                        return false;
                    }
                    AnalyticFilter::IsNotNull { column } if *column == key_col_idx => {
                        // All partitions contain non-null keys, so retain.
                        continue;
                    }
                    AnalyticFilter::Comparison { column, op, value } if *column == key_col_idx => {
                        if value.is_null() {
                            // Comparison with NULL yields UNKNOWN/false in SQL.
                            return false;
                        }
                        if value.data_type() != Some(key_col.data_type) {
                            // Type mismatch: conservative retain.
                            continue;
                        }

                        match partitioning.method {
                            PartitioningMethod::Range => {
                                let range = match &part.range {
                                    Some(r) => r,
                                    None => continue,
                                };
                                if let Some(l) = &range.lower {
                                    if l.data_type() != Some(key_col.data_type) {
                                        continue;
                                    }
                                }
                                if let Some(u) = &range.upper {
                                    if u.data_type() != Some(key_col.data_type) {
                                        continue;
                                    }
                                }

                                match op {
                                    ComparisonOp::Eq => {
                                        if !range.contains(value) {
                                            return false;
                                        }
                                    }
                                    ComparisonOp::Lt => {
                                        if let Some(l) = &range.lower {
                                            if l >= value {
                                                return false;
                                            }
                                        }
                                    }
                                    ComparisonOp::Lte => {
                                        if let Some(l) = &range.lower {
                                            if l > value {
                                                return false;
                                            }
                                        }
                                    }
                                    ComparisonOp::Gt => {
                                        if let Some(u) = &range.upper {
                                            if u <= value {
                                                return false;
                                            }
                                        }
                                    }
                                    ComparisonOp::Gte => {
                                        if let Some(u) = &range.upper {
                                            if u <= value {
                                                return false;
                                            }
                                        }
                                    }
                                    ComparisonOp::NotEq => {
                                        continue;
                                    }
                                }
                            }
                            PartitioningMethod::List => match op {
                                ComparisonOp::Eq => {
                                    if !part.list_values.iter().any(|v| v == value) {
                                        return false;
                                    }
                                }
                                ComparisonOp::Lt => {
                                    if !part.list_values.iter().any(|v| v < value) {
                                        return false;
                                    }
                                }
                                ComparisonOp::Lte => {
                                    if !part.list_values.iter().any(|v| v <= value) {
                                        return false;
                                    }
                                }
                                ComparisonOp::Gt => {
                                    if !part.list_values.iter().any(|v| v > value) {
                                        return false;
                                    }
                                }
                                ComparisonOp::Gte => {
                                    if !part.list_values.iter().any(|v| v >= value) {
                                        return false;
                                    }
                                }
                                ComparisonOp::NotEq => {
                                    continue;
                                }
                            },
                        }
                    }
                    _ => continue,
                }
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_catalog::{
        PartitionId, PartitioningDescriptor, RangeBound, StorageDescriptor, TableId,
    };
    use htap_common::types::{ColumnDef, DataType, Schema};

    #[test]
    fn test_compare_values_with_order_semantics() {
        let n = Value::Null;
        let v1 = Value::Int64(10);
        let v2 = Value::Int64(20);

        // ASC, NULLS FIRST
        assert_eq!(
            compare_values_with_order(&n, &v1, true, true),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_values_with_order(&v1, &n, true, true),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_values_with_order(&n, &n, true, true),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_values_with_order(&v1, &v2, true, true),
            std::cmp::Ordering::Less
        );

        // ASC, NULLS LAST
        assert_eq!(
            compare_values_with_order(&n, &v1, true, false),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_values_with_order(&v1, &n, true, false),
            std::cmp::Ordering::Less
        );

        // DESC, NULLS LAST
        assert_eq!(
            compare_values_with_order(&n, &v1, false, false),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_values_with_order(&v1, &n, false, false),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_values_with_order(&v1, &v2, false, false),
            std::cmp::Ordering::Greater
        );

        // DESC, NULLS FIRST
        assert_eq!(
            compare_values_with_order(&n, &v1, false, true),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_values_with_order(&v1, &n, false, true),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_prune_partitions_range_and_conservative() {
        let schema = Schema::new(vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "val".into(),
                data_type: DataType::String,
                nullable: false,
                primary_key: false,
            },
        ])
        .unwrap();

        let mut table = TableDescriptor::new(
            TableId::new(1),
            "users",
            schema,
            vec![0],
            vec![
                PartitionId::new(10),
                PartitionId::new(20),
                PartitionId::new(30),
            ],
            1,
        );
        table.partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));

        let p0 = PartitionDescriptor::new(
            PartitionId::new(10),
            TableId::new(1),
            "p0",
            StorageDescriptor::Row,
            vec![],
            1,
        )
        .with_range(RangeBound::new(Value::Int64(0), Value::Int64(100)));

        let p1 = PartitionDescriptor::new(
            PartitionId::new(20),
            TableId::new(1),
            "p1",
            StorageDescriptor::Row,
            vec![],
            1,
        )
        .with_range(RangeBound::new(Value::Int64(100), Value::Int64(200)));

        let p2 = PartitionDescriptor::new(
            PartitionId::new(30),
            TableId::new(1),
            "p2",
            StorageDescriptor::Row,
            vec![],
            1,
        )
        .with_range(RangeBound::new(Value::Int64(200), Value::Int64(300)));

        let partitions = [&p0, &p1, &p2];

        // 1. Eq 50 -> only p0
        let filter_eq = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Eq,
            value: Value::Int64(50),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_eq));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p0");

        // 2. Lt 100 -> only p0
        let filter_lt = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Lt,
            value: Value::Int64(100),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_lt));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p0");

        // 3. Lte 100 -> p0 and p1 (since 100 is in p1)
        let filter_lte = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Lte,
            value: Value::Int64(100),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_lte));
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].name, "p0");
        assert_eq!(res[1].name, "p1");

        // 4. Gt 150 -> p1 and p2
        let filter_gt = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Gt,
            value: Value::Int64(150),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_gt));
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].name, "p1");
        assert_eq!(res[1].name, "p2");

        // 5. Gte 200 -> only p2
        let filter_gte = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Gte,
            value: Value::Int64(200),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_gte));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p2");

        // 6. AND of Gte 100 and Lt 200 -> only p1
        let filter_and = AnalyticFilter::And(vec![
            AnalyticFilter::Comparison {
                column: 0,
                op: ComparisonOp::Gte,
                value: Value::Int64(100),
            },
            AnalyticFilter::Comparison {
                column: 0,
                op: ComparisonOp::Lt,
                value: Value::Int64(200),
            },
        ]);
        let res = prune_partitions(&table, &partitions, Some(&filter_and));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p1");

        // 7. Empty range: Lt 0 -> 0 partitions
        let filter_empty = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Lt,
            value: Value::Int64(0),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_empty));
        assert_eq!(res.len(), 0);

        // 8. Conservative NotEq 50 -> retains all
        let filter_ne = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::NotEq,
            value: Value::Int64(50),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_ne));
        assert_eq!(res.len(), 3);

        // 9. Non-partition key filter -> retains all
        let filter_non_pk = AnalyticFilter::Comparison {
            column: 1,
            op: ComparisonOp::Eq,
            value: Value::String("alice".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_non_pk));
        assert_eq!(res.len(), 3);

        // 10. Type mismatch -> retains all (conservative)
        let filter_mismatch = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Eq,
            value: Value::String("bad".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_mismatch));
        assert_eq!(res.len(), 3);

        // 11. IsNull on partition key -> 0 partitions (since partition keys are never null)
        let filter_null = AnalyticFilter::IsNull { column: 0 };
        let res = prune_partitions(&table, &partitions, Some(&filter_null));
        assert_eq!(res.len(), 0);

        // 12. IsNotNull on partition key -> retains all
        let filter_not_null = AnalyticFilter::IsNotNull { column: 0 };
        let res = prune_partitions(&table, &partitions, Some(&filter_not_null));
        assert_eq!(res.len(), 3);
    }

    #[test]
    fn test_prune_partitions_list_and_conservative() {
        let schema = Schema::new(vec![
            ColumnDef {
                name: "region".into(),
                data_type: DataType::String,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "sales".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
            },
        ])
        .unwrap();

        let mut table = TableDescriptor::new(
            TableId::new(2),
            "regional",
            schema,
            vec![0],
            vec![PartitionId::new(10), PartitionId::new(20)],
            1,
        );
        table.partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));

        let p_us = PartitionDescriptor::new(
            PartitionId::new(10),
            TableId::new(2),
            "p_us",
            StorageDescriptor::Row,
            vec![],
            1,
        )
        .with_list_values(vec![Value::String("US".into()), Value::String("CA".into())]);

        let p_eu = PartitionDescriptor::new(
            PartitionId::new(20),
            TableId::new(2),
            "p_eu",
            StorageDescriptor::Row,
            vec![],
            1,
        )
        .with_list_values(vec![Value::String("EU".into()), Value::String("UK".into())]);

        let partitions = [&p_us, &p_eu];

        // 1. Eq 'US' -> only p_us
        let filter_us = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Eq,
            value: Value::String("US".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_us));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p_us");

        // 2. Eq 'UK' -> only p_eu
        let filter_uk = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Eq,
            value: Value::String("UK".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_uk));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "p_eu");

        // 3. Eq 'JP' -> 0 partitions
        let filter_jp = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::Eq,
            value: Value::String("JP".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_jp));
        assert_eq!(res.len(), 0);

        // 4. NotEq 'US' -> retains all
        let filter_ne = AnalyticFilter::Comparison {
            column: 0,
            op: ComparisonOp::NotEq,
            value: Value::String("US".into()),
        };
        let res = prune_partitions(&table, &partitions, Some(&filter_ne));
        assert_eq!(res.len(), 2);
    }
}
