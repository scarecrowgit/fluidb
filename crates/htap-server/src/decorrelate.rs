use std::collections::{BTreeMap, BTreeSet, HashMap};

use htap_common::error::Result;
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_sql::expr::{BinOp, EvalContext, Expr};
use htap_sql::query::{BoundQuery, JoinTree, ProjectionItem, QueryBody, SelectBody};

use crate::memory_budget::MemoryReservation;

/// A statement-scoped lookup replacing a key-only correlated EXISTS subquery.
pub(crate) struct ExistsLookup {
    pub(crate) probe_keys: Vec<Expr>,
    pub(crate) key_types: Vec<DataType>,
    pub(crate) rows: HashMap<Vec<Value>, Vec<Row>>,
    pub(crate) residual_exprs: Vec<Expr>,
    pub(crate) _reservations: Vec<MemoryReservation>,
}

impl ExistsLookup {
    pub(crate) fn run(&self, outer_row: &[Value]) -> Result<Vec<Row>> {
        let key = self
            .probe_keys
            .iter()
            .zip(&self.key_types)
            .map(|(expr, data_type)| {
                expr.eval(&EvalContext::new(
                    &[],
                    Some(outer_row),
                    &[],
                    None,
                    &[],
                    htap_sql::expr::EvalServices::none(),
                ))
                .map(|value| (value, *data_type))
            })
            .collect::<Result<Vec<_>>>()?;
        let key = normalized_lookup_key(
            &key.into_iter().map(|(value, _)| value).collect::<Vec<_>>(),
            &self.key_types,
            crate::query_exec::normalize_key,
        )?;
        let Some(bucket) = key.and_then(|key| self.rows.get(&key)) else {
            return Ok(Vec::new());
        };

        for row in bucket {
            let mut matched = true;
            for residual in &self.residual_exprs {
                let context = EvalContext::new(
                    row.values(),
                    Some(outer_row),
                    &[],
                    None,
                    &[],
                    htap_sql::expr::EvalServices::none(),
                );
                if !residual.eval_predicate(&context)? {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Ok(vec![Row::new(Vec::new())]);
            }
        }
        Ok(Vec::new())
    }
}

/// An eligible correlated EXISTS build query.
pub(crate) struct ExistsLookupPlan {
    pub(crate) build_query: BoundQuery,
    pub(crate) probe_keys: Vec<Expr>,
    pub(crate) key_types: Vec<DataType>,
    pub(crate) residual_exprs: Vec<Expr>,
}

/// A statement-scoped lookup replacing a keyed correlated scalar aggregate subquery.
pub(crate) struct ScalarAggregateLookup {
    pub(crate) probe_keys: Vec<Expr>,
    pub(crate) key_types: Vec<DataType>,
    pub(crate) values: HashMap<Vec<Value>, Value>,
    pub(crate) default: Value,
    pub(crate) _reservations: Vec<MemoryReservation>,
}

impl ScalarAggregateLookup {
    pub(crate) fn run(&self, outer_row: &[Value]) -> Result<Vec<Row>> {
        let key = self
            .probe_keys
            .iter()
            .zip(&self.key_types)
            .map(|(expr, data_type)| {
                expr.eval(&EvalContext::new(
                    &[],
                    Some(outer_row),
                    &[],
                    None,
                    &[],
                    htap_sql::expr::EvalServices::none(),
                ))
                .map(|value| (value, *data_type))
            })
            .collect::<Result<Vec<_>>>()?;
        let key = normalized_lookup_key(
            &key.into_iter().map(|(value, _)| value).collect::<Vec<_>>(),
            &self.key_types,
            crate::query_exec::normalize_key,
        )?;
        let value = key
            .and_then(|key| self.values.get(&key))
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        Ok(vec![Row::new(vec![value])])
    }
}

/// An eligible correlated scalar-aggregate build query.
pub(crate) struct ScalarAggregateLookupPlan {
    pub(crate) build_query: BoundQuery,
    pub(crate) default_query: BoundQuery,
    pub(crate) probe_keys: Vec<Expr>,
    pub(crate) key_types: Vec<DataType>,
}

/// Returns a correlated EXISTS lookup plan, or `None` when ordinary per-row execution is
/// required. Keyed residual correlated predicates are supported by task 2b.
pub(crate) fn plan_exists_lookup(query: &BoundQuery) -> Option<ExistsLookupPlan> {
    let QueryBody::Select(select) = &query.body else {
        return None;
    };
    if query.limit.is_some()
        || query.offset.is_some()
        || !query.order_by.is_empty()
        || !select.group_by.is_empty()
        || !select.aggregates.is_empty()
        || select.having.is_some()
    {
        return None;
    }
    if query.subqueries.iter().any(|subquery| subquery.correlated) {
        return None;
    }
    if correlated_outside_filter(select) {
        return None;
    }

    let conjuncts = select
        .filter
        .as_ref()
        .map(split_conjuncts)
        .unwrap_or_default();
    let mut local_filters = Vec::new();
    let mut build_keys = Vec::new();
    let mut probe_keys = Vec::new();
    let mut residuals = Vec::new();

    for conjunct in conjuncts {
        match classify_conjunct(&conjunct) {
            Conjunct::Key { local, outer } => {
                build_keys.push(local);
                probe_keys.push(outer);
            }
            Conjunct::Local => local_filters.push(conjunct),
            Conjunct::Residual => residuals.push(conjunct),
        }
    }
    if build_keys.is_empty() {
        return None;
    }

    if local_filters.iter().any(residual_contains_unsupported_expr) {
        return None;
    }

    let key_types = build_keys
        .iter()
        .zip(&probe_keys)
        .map(|(local, outer)| {
            canonical_numeric_key_type(local.expr_type().data_type, outer.expr_type().data_type)
        })
        .collect::<Option<Vec<_>>>()?;

    let mut accounted = Vec::new();
    for key in &probe_keys {
        accounted.extend(key.correlated_outer_refs());
    }
    for residual in &residuals {
        accounted.extend(residual.correlated_outer_refs());
    }
    if query
        .correlated_outer_refs
        .iter()
        .any(|outer| !accounted.contains(outer))
    {
        return None;
    }

    let mut residual_columns = BTreeMap::new();
    let mut residual_column_offsets = Vec::with_capacity(residuals.len());
    for residual in &residuals {
        if residual_contains_unsupported_expr(residual) {
            return None;
        }

        let mut offsets = BTreeSet::new();
        residual.walk(&mut |expr| {
            if let Expr::ColumnRef { offset, .. } = expr {
                offsets.insert(*offset);
                residual_columns
                    .entry(*offset)
                    .or_insert_with(|| expr.clone());
            }
        });
        residual_column_offsets.push(offsets);
    }

    let key_count = build_keys.len();
    let residual_offset_map = residual_columns
        .keys()
        .enumerate()
        .map(|(index, offset)| (*offset, key_count + index))
        .collect::<BTreeMap<_, _>>();
    let mut residual_exprs = Vec::with_capacity(residuals.len());
    for residual in residuals {
        let mut rebased = residual.clone();
        if !rebase_column_refs(&mut rebased, &residual_offset_map) {
            return None;
        }
        residual_exprs.push(rebased);
    }

    let mut build_select = select.clone();
    build_select.filter = combine_conjuncts(local_filters);
    build_select.group_by.clear();
    build_select.aggregates.clear();
    build_select.windows.clear();
    build_select.having = None;
    build_select.distinct = false;
    build_select.projection = build_keys
        .iter()
        .enumerate()
        .map(|(index, expr)| ProjectionItem {
            expr: expr.clone(),
            name: format!("decorrelation_key_{index}"),
        })
        .chain(
            residual_columns
                .into_values()
                .enumerate()
                .map(|(index, expr)| ProjectionItem {
                    expr,
                    name: format!("decorrelation_residual_{index}"),
                }),
        )
        .collect();

    let output_columns = build_select
        .projection
        .iter()
        .map(|projection| {
            let ty = projection.expr.expr_type();
            ColumnDef {
                name: projection.name.clone(),
                data_type: ty.data_type,
                nullable: ty.nullable,
                primary_key: false,
            }
        })
        .collect();

    Some(ExistsLookupPlan {
        build_query: BoundQuery {
            body: QueryBody::Select(build_select),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            subqueries: query.subqueries.clone(),
            correlated: false,
            correlated_outer_refs: Vec::new(),
            output_columns,
        },
        probe_keys,
        key_types,
        residual_exprs,
    })
}

pub(crate) fn plan_scalar_aggregate_lookup(
    query: &BoundQuery,
) -> Option<ScalarAggregateLookupPlan> {
    let QueryBody::Select(select) = &query.body else {
        return None;
    };
    if query.limit.is_some()
        || query.offset.is_some()
        || !query.order_by.is_empty()
        || !select.group_by.is_empty()
        || select.aggregates.len() != 1
        || select.having.is_some()
        || select.distinct
        || !select.windows.is_empty()
        || select.projection.len() != 1
    {
        return None;
    }
    if query.subqueries.iter().any(|subquery| subquery.correlated)
        || correlated_outside_filter(select)
    {
        return None;
    }

    let aggregate = &select.aggregates[0];
    if aggregate.arg.as_ref().is_some_and(|arg| {
        !arg.correlated_outer_refs().is_empty() || residual_contains_unsupported_expr(arg)
    }) {
        return None;
    }

    let mut projection_aggregate_refs = Vec::new();
    select.projection[0].expr.walk(&mut |expr| {
        if let Expr::AggregateRef { index, .. } = expr {
            projection_aggregate_refs.push(*index);
        }
    });
    if projection_aggregate_refs != [0] {
        return None;
    }

    let conjuncts = select
        .filter
        .as_ref()
        .map(split_conjuncts)
        .unwrap_or_default();
    let mut local_filters = Vec::new();
    let mut build_keys = Vec::new();
    let mut probe_keys = Vec::new();

    for conjunct in conjuncts {
        match classify_conjunct(&conjunct) {
            Conjunct::Key { local, outer } => {
                build_keys.push(local);
                probe_keys.push(outer);
            }
            Conjunct::Local => local_filters.push(conjunct),
            // Scalar aggregate lookup cannot evaluate a remaining predicate per outer row.
            Conjunct::Residual => return None,
        }
    }
    if build_keys.is_empty() {
        return None;
    }

    if local_filters.iter().any(residual_contains_unsupported_expr) {
        return None;
    }

    let key_types = build_keys
        .iter()
        .zip(&probe_keys)
        .map(|(local, outer)| {
            canonical_numeric_key_type(local.expr_type().data_type, outer.expr_type().data_type)
        })
        .collect::<Option<Vec<_>>>()?;

    let mut accounted = Vec::new();
    for key in &probe_keys {
        accounted.extend(key.correlated_outer_refs());
    }
    if query
        .correlated_outer_refs
        .iter()
        .any(|outer| !accounted.contains(outer))
    {
        return None;
    }

    let mut build_select = select.clone();
    build_select.filter = combine_conjuncts(local_filters);
    build_select.group_by = build_keys.clone();
    build_select.windows.clear();
    build_select.having = None;
    build_select.distinct = false;
    build_select.projection = build_keys
        .iter()
        .enumerate()
        .map(|(index, expr)| ProjectionItem {
            expr: expr.clone(),
            name: format!("decorrelation_key_{index}"),
        })
        .chain(std::iter::once(ProjectionItem {
            expr: select.projection[0].expr.clone(),
            name: "decorrelation_value".to_string(),
        }))
        .collect();

    let output_columns = build_select
        .projection
        .iter()
        .map(|projection| {
            let ty = projection.expr.expr_type();
            ColumnDef {
                name: projection.name.clone(),
                data_type: ty.data_type,
                nullable: ty.nullable,
                primary_key: false,
            }
        })
        .collect::<Vec<_>>();

    let mut default_select = select.clone();
    let mut default_filters = select
        .filter
        .as_ref()
        .map(split_conjuncts)
        .unwrap_or_default()
        .into_iter()
        .filter(|conjunct| matches!(classify_conjunct(conjunct), Conjunct::Local))
        .collect::<Vec<_>>();
    // Force standard empty-input aggregate semantics for misses and NULL probe keys.
    default_filters.push(Expr::Literal(Value::Bool(false)));
    default_select.filter = combine_conjuncts(default_filters);
    default_select.group_by.clear();
    default_select.windows.clear();
    default_select.having = None;
    default_select.distinct = false;

    let default_output_columns = default_select
        .projection
        .iter()
        .map(|projection| {
            let ty = projection.expr.expr_type();
            ColumnDef {
                name: projection.name.clone(),
                data_type: ty.data_type,
                nullable: ty.nullable,
                primary_key: false,
            }
        })
        .collect::<Vec<_>>();

    if output_columns[key_types.len()].data_type != default_output_columns[0].data_type {
        return None;
    }

    Some(ScalarAggregateLookupPlan {
        build_query: BoundQuery {
            body: QueryBody::Select(build_select),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            subqueries: query.subqueries.clone(),
            correlated: false,
            correlated_outer_refs: Vec::new(),
            output_columns,
        },
        default_query: BoundQuery {
            body: QueryBody::Select(default_select),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            subqueries: query.subqueries.clone(),
            correlated: false,
            correlated_outer_refs: Vec::new(),
            output_columns: default_output_columns,
        },
        probe_keys,
        key_types,
    })
}

fn residual_contains_unsupported_expr(expr: &Expr) -> bool {
    let mut unsupported = false;
    expr.walk(&mut |expr| {
        unsupported |= matches!(
            expr,
            Expr::ScalarSubquery { .. }
                | Expr::InSubquery { .. }
                | Expr::Exists { .. }
                | Expr::AggregateRef { .. }
                | Expr::WindowRef { .. }
                | Expr::Variable { .. }
                | Expr::OutputColumn { .. }
        );
    });
    unsupported
}

fn rebase_column_refs(expr: &mut Expr, offsets: &BTreeMap<usize, usize>) -> bool {
    fn visit(expr: &mut Expr, offsets: &BTreeMap<usize, usize>) -> bool {
        match expr {
            Expr::ColumnRef { offset, .. } => {
                let Some(rebased) = offsets.get(offset) else {
                    return false;
                };
                *offset = *rebased;
            }
            Expr::BinaryOp { left, right, .. } => {
                if !visit(left, offsets) || !visit(right, offsets) {
                    return false;
                }
            }
            Expr::Not(inner)
            | Expr::Negate(inner)
            | Expr::IsNull(inner)
            | Expr::IsNotNull(inner)
            | Expr::Cast { expr: inner, .. } => {
                if !visit(inner, offsets) {
                    return false;
                }
            }
            Expr::Like { expr, pattern, .. } => {
                if !visit(expr, offsets) || !visit(pattern, offsets) {
                    return false;
                }
            }
            Expr::In { expr, list, .. } => {
                if !visit(expr, offsets) || !list.iter_mut().all(|item| visit(item, offsets)) {
                    return false;
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                if !visit(expr, offsets) || !visit(low, offsets) || !visit(high, offsets) {
                    return false;
                }
            }
            Expr::Case {
                operand,
                branches,
                else_result,
                ..
            } => {
                if operand
                    .as_deref_mut()
                    .is_some_and(|operand| !visit(operand, offsets))
                    || !branches.iter_mut().all(|(condition, result)| {
                        visit(condition, offsets) && visit(result, offsets)
                    })
                    || else_result
                        .as_deref_mut()
                        .is_some_and(|result| !visit(result, offsets))
                {
                    return false;
                }
            }
            Expr::CalendarInterval { expr, quantity, .. } => {
                if !visit(expr, offsets) || !visit(quantity, offsets) {
                    return false;
                }
            }
            Expr::ScalarFunction { args, .. } => {
                if !args.iter_mut().all(|arg| visit(arg, offsets)) {
                    return false;
                }
            }
            Expr::InSubquery { expr, .. } => {
                if !visit(expr, offsets) {
                    return false;
                }
            }
            Expr::CorrelatedColumnRef { .. }
            | Expr::OutputColumn { .. }
            | Expr::Literal(_)
            | Expr::AggregateRef { .. }
            | Expr::WindowRef { .. }
            | Expr::ScalarSubquery { .. }
            | Expr::Exists { .. }
            | Expr::Variable { .. } => {}
        }
        true
    }

    visit(expr, offsets)
}

/// Normalizes a key vector for build or probe use. A NULL component never participates in an
/// EXISTS equality match.
pub(crate) fn normalized_lookup_key(
    values: &[Value],
    key_types: &[DataType],
    normalize: impl Fn(Value, DataType) -> Result<Value>,
) -> Result<Option<Vec<Value>>> {
    let mut key = Vec::with_capacity(values.len());
    for (value, data_type) in values.iter().cloned().zip(key_types.iter().copied()) {
        if value.is_null() {
            return Ok(None);
        }
        let normalized = normalize(value, data_type)?;
        if normalized.is_null() {
            return Ok(None);
        }
        key.push(normalized);
    }
    Ok(Some(key))
}

enum Conjunct {
    Key { local: Expr, outer: Expr },
    Local,
    Residual,
}

fn classify_conjunct(expr: &Expr) -> Conjunct {
    let Expr::BinaryOp {
        op: BinOp::Eq,
        left,
        right,
    } = expr
    else {
        return if expr.correlated_outer_refs().is_empty() {
            Conjunct::Local
        } else {
            Conjunct::Residual
        };
    };

    let left_refs = left.correlated_outer_refs();
    let right_refs = right.correlated_outer_refs();
    match (left_refs.as_slice(), right_refs.as_slice()) {
        ([..], [])
            if left_refs.len() == 1
                && matches!(left.as_ref(), Expr::CorrelatedColumnRef { .. }) =>
        {
            Conjunct::Key {
                local: (**right).clone(),
                outer: (**left).clone(),
            }
        }
        ([], [..])
            if right_refs.len() == 1
                && matches!(right.as_ref(), Expr::CorrelatedColumnRef { .. }) =>
        {
            Conjunct::Key {
                local: (**left).clone(),
                outer: (**right).clone(),
            }
        }
        ([], []) => Conjunct::Local,
        _ => Conjunct::Residual,
    }
}

fn split_conjuncts(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            op: BinOp::And,
            left,
            right,
        } => {
            let mut conjuncts = split_conjuncts(left);
            conjuncts.extend(split_conjuncts(right));
            conjuncts
        }
        _ => vec![expr.clone()],
    }
}

fn combine_conjuncts(mut conjuncts: Vec<Expr>) -> Option<Expr> {
    let mut result = conjuncts.pop()?;
    while let Some(left) = conjuncts.pop() {
        result = Expr::BinaryOp {
            op: BinOp::And,
            left: Box::new(left),
            right: Box::new(result),
        };
    }
    Some(result)
}

fn correlated_outside_filter(select: &SelectBody) -> bool {
    let has_correlated = |expr: &Expr| !expr.correlated_outer_refs().is_empty();
    if select.group_by.iter().any(has_correlated)
        || select
            .projection
            .iter()
            .any(|projection| has_correlated(&projection.expr))
        || select.having.as_ref().is_some_and(has_correlated)
        || select
            .aggregates
            .iter()
            .any(|aggregate| aggregate.arg.as_ref().is_some_and(has_correlated))
        || select.windows.iter().any(|window| {
            window.partition_by.iter().any(has_correlated)
                || window
                    .order_by
                    .iter()
                    .any(|item| has_correlated(&item.expr))
                || window.args.iter().any(has_correlated)
        })
    {
        return true;
    }

    fn tree_has_correlated(tree: &JoinTree) -> bool {
        match tree {
            JoinTree::Leaf(_) => false,
            JoinTree::Join {
                left, right, on, ..
            } => {
                tree_has_correlated(left)
                    || tree_has_correlated(right)
                    || on
                        .as_ref()
                        .is_some_and(|expr| !expr.correlated_outer_refs().is_empty())
            }
        }
    }
    tree_has_correlated(&select.join_tree)
}

fn canonical_numeric_key_type(left: DataType, right: DataType) -> Option<DataType> {
    let numeric = |data_type| {
        matches!(
            data_type,
            DataType::Int32
                | DataType::Int64
                | DataType::Float64
                | DataType::Timestamp
                | DataType::Decimal { .. }
        )
    };
    if !numeric(left) || !numeric(right) {
        return None;
    }

    Some(match (left, right) {
        (
            DataType::Decimal {
                precision: left_precision,
                scale: left_scale,
            },
            DataType::Decimal {
                precision: right_precision,
                scale: right_scale,
            },
        ) => DataType::Decimal {
            precision: left_precision.max(right_precision),
            scale: left_scale.max(right_scale),
        },
        (
            DataType::Decimal { precision, scale },
            DataType::Int32 | DataType::Int64 | DataType::Timestamp,
        )
        | (
            DataType::Int32 | DataType::Int64 | DataType::Timestamp,
            DataType::Decimal { precision, scale },
        ) => DataType::Decimal { precision, scale },
        (DataType::Float64, _) | (_, DataType::Float64) => DataType::Float64,
        _ => DataType::Int64,
    })
}
