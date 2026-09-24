//! Binder for the general query path: joins, expressions, subqueries, set operations,
//! `UPDATE`, `DROP TABLE`, and `SHOW`/`DESCRIBE`.
//!
//! Name resolution works over FROM *slots*. Each slot contributes its columns to a flat
//! joined row; a column reference is resolved to `(slot, column)` and its flat offset. An
//! unqualified name must be unique across slots; a qualified name `t.c` is looked up in the
//! slot named `t` (table name or alias). In `ORDER BY`, projection aliases and ordinals take
//! precedence over source columns; in `HAVING`, source columns take precedence over aliases.
//!
//! Subqueries in expressions are bound with no access to the enclosing slots: a reference to
//! an outer column is reported as an unsupported correlated subquery.

use std::collections::HashSet;

use htap_catalog::{CatalogSnapshot, TableDescriptor};
use htap_common::error::{HtapError, Result};
use htap_common::types::{parse_date_to_timestamp_micros, ColumnDef, DataType, Value};
use sqlparser::ast::{
    self as sql, Expr as SqlExpr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, LimitClause, ObjectName, ObjectNamePart, OrderByKind, Query,
    Select, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement, TableFactor,
    TableWithJoins, WindowType,
};

use crate::ast::{
    BoundStatement, DeleteStatement, DeleteTarget, DropTableStatement, ShowStatement,
    UpdateStatement, UpdateTarget,
};
use crate::expr::{
    arithmetic_result_type, cast_value, AggFn, AggregateSpec, BinOp, CalendarIntervalUnit, Expr,
    ExprType, ScalarFn,
};
use crate::query::{
    BoundQuery, JoinKind, JoinSpec, JoinTree, OrderItem, PeerFrameBound, ProjectionItem, QueryBody,
    RowFrameBound, SelectBody, SetOpKind, TableSlot, ValueFrameBound, VisibleColumn, VisibleSchema,
    WindowFrame, WindowFrameDirection, WindowFunctionKind, WindowSpec,
};
use crate::table_not_found;

fn unsupported(msg: impl Into<String>) -> HtapError {
    HtapError::Unsupported(msg.into())
}

fn invalid(msg: impl Into<String>) -> HtapError {
    HtapError::InvalidArgument(msg.into())
}

/// Extracts a single-part object name.
pub(crate) fn object_name_single(name: &ObjectName) -> Result<String> {
    if name.0.len() != 1 {
        return Err(unsupported(format!(
            "qualified names not supported: '{name}'"
        )));
    }
    match &name.0[0] {
        ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
        ObjectNamePart::Function(_) => Err(unsupported(format!(
            "function names not supported: '{name}'"
        ))),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<String>> {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
            ObjectNamePart::Function(_) => Err(unsupported(format!(
                "function names not supported: '{name}'"
            ))),
        })
        .collect()
}

/// A FROM slot during binding.
#[derive(Debug, Clone)]
struct SlotInfo {
    alias: String,
    columns: Vec<ColumnDef>,
    offset: usize,
}

#[derive(Clone)]
enum CteBinding {
    Query(Box<BoundQuery>),
    Working(Vec<ColumnDef>),
}

#[derive(Default)]
struct CteScope {
    ctes: Vec<(String, CteBinding)>,
}

impl CteScope {
    fn lookup(&self, name: &str) -> Option<&CteBinding> {
        self.ctes
            .iter()
            .rev()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, binding)| binding)
    }
}

/// Returns whether a CTE body contains a reference to its own name.
fn recursive_cte_body_is_self_referencing(cte: &sql::Cte) -> Result<bool> {
    set_expr_references_cte(cte.query.body.as_ref(), &cte.alias.name.value)
}

fn set_expr_references_cte(body: &SetExpr, cte_name: &str) -> Result<bool> {
    match body {
        SetExpr::Select(select) => select
            .from
            .iter()
            .try_fold(false, |found, table_with_joins| {
                Ok(found || table_with_joins_references_cte(table_with_joins, cte_name)?)
            }),
        SetExpr::Query(query) => set_expr_references_cte(query.body.as_ref(), cte_name),
        SetExpr::SetOperation { left, right, .. } => {
            Ok(set_expr_references_cte(left, cte_name)?
                || set_expr_references_cte(right, cte_name)?)
        }
        SetExpr::Values(_) => Ok(false),
        _ => Ok(false),
    }
}

fn table_with_joins_references_cte(
    table_with_joins: &TableWithJoins,
    cte_name: &str,
) -> Result<bool> {
    if table_factor_references_cte(&table_with_joins.relation, cte_name)? {
        return Ok(true);
    }

    table_with_joins
        .joins
        .iter()
        .try_fold(false, |found, join| {
            Ok(found || table_factor_references_cte(&join.relation, cte_name)?)
        })
}

fn table_factor_references_cte(factor: &TableFactor, cte_name: &str) -> Result<bool> {
    match factor {
        TableFactor::Table { name, .. } => {
            Ok(object_name_single(name)?.eq_ignore_ascii_case(cte_name))
        }
        TableFactor::Derived { subquery, .. } => {
            set_expr_references_cte(subquery.body.as_ref(), cte_name)
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_references_cte(table_with_joins, cte_name),
        _ => Ok(false),
    }
}

/// Binds a general `SELECT` query.
pub(crate) fn bind_query(query: &Query, catalog: &CatalogSnapshot) -> Result<BoundQuery> {
    bind_query_scoped(query, catalog, &CteScope::default(), &[], 0)
}

fn bind_query_scoped(
    query: &Query,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    immediate_outer_len: usize,
) -> Result<BoundQuery> {
    if query.fetch.is_some() {
        return Err(unsupported("FETCH clause not supported in SELECT"));
    }
    if !query.locks.is_empty() {
        return Err(unsupported("locking clauses not supported in SELECT"));
    }
    if query.for_clause.is_some() {
        return Err(unsupported("FOR clause not supported in SELECT"));
    }
    if query.settings.is_some() || query.format_clause.is_some() {
        return Err(unsupported("SETTINGS / FORMAT clauses not supported"));
    }
    if !query.pipe_operators.is_empty() {
        return Err(unsupported("pipe operators not supported"));
    }

    // Inline non-recursive CTEs. A recursive CTE is represented by a synthetic query binding
    // whose recursive term reads a WorkingTableSlot instead of catalog storage.
    let mut local_scope = CteScope::default();
    if let Some(with) = &query.with {
        if with.recursive && with.cte_tables.len() != 1 {
            return Err(unsupported(
                "nested or mutually recursive CTEs not supported",
            ));
        }
        if with.recursive && recursive_cte_body_is_self_referencing(&with.cte_tables[0])? {
            let cte = &with.cte_tables[0];
            if cte.from.is_some() {
                return Err(unsupported("CTE FROM clause not supported"));
            }
            let SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier,
                left,
                right,
            } = cte.query.body.as_ref()
            else {
                return Err(invalid("recursive CTE body must be a UNION or UNION ALL"));
            };
            let distinct = match set_quantifier {
                SetQuantifier::All => false,
                SetQuantifier::None | SetQuantifier::Distinct => true,
                other => return Err(unsupported(format!("UNION {other} not supported"))),
            };

            let anchor = bind_branch(left, catalog, ctes, outer, immediate_outer_len)?;
            let mut columns = anchor.output_columns.clone();
            if !cte.alias.columns.is_empty() {
                if cte.alias.columns.len() != columns.len() {
                    return Err(invalid(format!(
                        "recursive CTE '{}' has {} column name(s), but its anchor has {} column(s)",
                        cte.alias.name,
                        cte.alias.columns.len(),
                        columns.len()
                    )));
                }
                for (column, name) in columns.iter_mut().zip(&cte.alias.columns) {
                    column.name = name.name.value.clone();
                }
            }

            let recursive_scope = CteScope {
                ctes: {
                    let mut bindings = ctes.ctes.clone();
                    bindings.push((
                        cte.alias.name.value.clone(),
                        CteBinding::Working(columns.clone()),
                    ));
                    bindings
                },
            };
            let recursive_term =
                bind_branch(right, catalog, &recursive_scope, outer, immediate_outer_len)?;
            validate_recursive_term(&recursive_term)?;
            validate_recursive_reference(&recursive_term, &cte.alias.name.value)?;

            if anchor.output_columns.len() != recursive_term.output_columns.len() {
                return Err(invalid(format!(
                    "UNION branches have different column counts ({} vs {})",
                    anchor.output_columns.len(),
                    recursive_term.output_columns.len()
                )));
            }
            for ((anchor_column, recursive_column), output) in anchor
                .output_columns
                .iter()
                .zip(&recursive_term.output_columns)
                .zip(columns.iter_mut())
            {
                output.data_type = union_type(anchor_column.data_type, recursive_column.data_type)
                    .ok_or_else(|| {
                        invalid(format!(
                            "UNION column '{}' has incompatible types {} and {}",
                            anchor_column.name,
                            anchor_column.data_type.name(),
                            recursive_column.data_type.name()
                        ))
                    })?;
                output.nullable = anchor_column.nullable || recursive_column.nullable;
                output.primary_key = false;
            }

            let recursive_query = BoundQuery {
                body: QueryBody::RecursiveQueryBody {
                    anchor: Box::new(anchor),
                    recursive_term: Box::new(recursive_term),
                    distinct,
                    output_columns: columns.clone(),
                },
                order_by: Vec::new(),
                limit: None,
                offset: None,
                subqueries: Vec::new(),
                correlated: false,
                correlated_outer_refs: Vec::new(),
                output_columns: columns,
            };
            local_scope.ctes = ctes.ctes.clone();
            local_scope.ctes.push((
                cte.alias.name.value.clone(),
                CteBinding::Query(Box::new(recursive_query)),
            ));
        } else {
            local_scope.ctes = ctes.ctes.clone();
            for cte in &with.cte_tables {
                if cte.from.is_some() {
                    return Err(unsupported("CTE FROM clause not supported"));
                }
                if !cte.alias.columns.is_empty() {
                    return Err(unsupported("CTE column lists not supported"));
                }
                let bound = bind_query_scoped(
                    &cte.query,
                    catalog,
                    &local_scope,
                    outer,
                    immediate_outer_len,
                )?;
                ensure_unique_output_names(&bound, &cte.alias.name.value)?;
                local_scope.ctes.push((
                    cte.alias.name.value.clone(),
                    CteBinding::Query(Box::new(bound)),
                ));
            }
        }
    } else {
        local_scope.ctes = ctes.ctes.clone();
    }

    let mut subqueries = Vec::new();
    let (body, order_scope) = bind_set_expr(
        &query.body,
        catalog,
        &local_scope,
        outer,
        immediate_outer_len,
        &mut subqueries,
    )?;
    let output_columns = body_output_columns(&body);

    let order_by = bind_order_by(
        query.order_by.as_ref(),
        &order_scope,
        &output_columns,
        &mut subqueries,
        catalog,
        &local_scope,
        outer,
    )?;
    if let (QueryBody::Select(sel), true) = (&body, !order_by.is_empty()) {
        if sel.distinct
            && order_by
                .iter()
                .any(|o| !matches!(o.expr, Expr::OutputColumn { .. }))
        {
            return Err(invalid(
                "ORDER BY expressions must appear in the select list when SELECT DISTINCT is used",
            ));
        }
    }
    let (limit, offset) = bind_limit(query.limit_clause.as_ref())?;

    let correlated_outer_refs = body_correlated_outer_refs(&body);
    let correlated = !correlated_outer_refs.is_empty();

    Ok(BoundQuery {
        body,
        order_by,
        limit,
        offset,
        subqueries,
        correlated,
        correlated_outer_refs,
        output_columns,
    })
}

fn ensure_unique_output_names(query: &BoundQuery, what: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for c in &query.output_columns {
        if !seen.insert(c.name.to_ascii_lowercase()) {
            return Err(invalid(format!(
                "duplicate column name '{}' in '{what}'",
                c.name
            )));
        }
    }
    Ok(())
}

fn body_correlated_outer_refs(body: &QueryBody) -> Vec<Expr> {
    match body {
        QueryBody::Select(select) => select.correlated_outer_refs.clone(),
        QueryBody::SetOp { left, right, .. }
        | QueryBody::RecursiveQueryBody {
            anchor: left,
            recursive_term: right,
            ..
        } => left
            .correlated_outer_refs
            .iter()
            .chain(right.correlated_outer_refs.iter())
            .cloned()
            .collect(),
    }
}

fn select_correlated_outer_refs(select: &SelectBody) -> Vec<Expr> {
    fn collect_expr_refs(expr: &Expr, refs: &mut Vec<Expr>) {
        refs.extend(expr.correlated_outer_refs());
    }

    fn collect_join_tree_refs(tree: &JoinTree, refs: &mut Vec<Expr>) {
        match tree {
            JoinTree::Leaf(_) => {}
            JoinTree::Join {
                left, right, on, ..
            } => {
                collect_join_tree_refs(left, refs);
                collect_join_tree_refs(right, refs);
                if let Some(on) = on {
                    collect_expr_refs(on, refs);
                }
            }
        }
    }

    let mut refs = Vec::new();

    collect_join_tree_refs(&select.join_tree, &mut refs);
    if let Some(filter) = &select.filter {
        collect_expr_refs(filter, &mut refs);
    }
    for group_by in &select.group_by {
        collect_expr_refs(group_by, &mut refs);
    }
    for aggregate in &select.aggregates {
        if let Some(arg) = &aggregate.arg {
            collect_expr_refs(arg, &mut refs);
        }
    }
    for window in &select.windows {
        for arg in &window.args {
            collect_expr_refs(arg, &mut refs);
        }
        for partition_by in &window.partition_by {
            collect_expr_refs(partition_by, &mut refs);
        }
        for order_by in &window.order_by {
            collect_expr_refs(&order_by.expr, &mut refs);
        }
        if let WindowFrame::ValueRange { start, end } = &window.frame {
            for bound in [start, end] {
                if let ValueFrameBound::Offset { value, .. } = bound {
                    collect_expr_refs(value, &mut refs);
                }
            }
        }
    }
    if let Some(having) = &select.having {
        collect_expr_refs(having, &mut refs);
    }
    for projection in &select.projection {
        collect_expr_refs(&projection.expr, &mut refs);
    }
    refs
}

fn check_correlated_subquery_grouping(
    query: &BoundQuery,
    group_by: &[Expr],
    what: &str,
) -> Result<()> {
    for outer_ref in &query.correlated_outer_refs {
        let Expr::CorrelatedColumnRef { offset, name, .. } = outer_ref else {
            continue;
        };

        // The child stores the parent column as a CorrelatedColumnRef, whereas the
        // parent's GROUP BY stores it as a local ColumnRef. Their shared row offset
        // identifies the same parent input column.
        let grouped = group_by.iter().any(|group_expr| {
            matches!(
                group_expr,
                Expr::ColumnRef {
                    offset: group_offset,
                    ..
                } if group_offset == offset
            )
        });

        if !grouped {
            return Err(invalid(format!(
                "column '{name}' in '{what}' must appear in the GROUP BY clause or be used in an aggregate function"
            )));
        }
    }
    Ok(())
}

fn body_output_columns(body: &QueryBody) -> Vec<ColumnDef> {
    match body {
        QueryBody::Select(sel) => sel
            .projection
            .iter()
            .map(|p| {
                let t = p.expr.expr_type();
                ColumnDef {
                    name: p.name.clone(),
                    data_type: t.data_type,
                    nullable: t.nullable,
                    primary_key: false,
                }
            })
            .collect(),
        QueryBody::SetOp { left, right, .. } => left
            .output_columns
            .iter()
            .zip(right.output_columns.iter())
            .map(|(l, r)| ColumnDef {
                name: l.name.clone(),
                data_type: union_type(l.data_type, r.data_type).unwrap_or(l.data_type),
                nullable: l.nullable || r.nullable,
                primary_key: false,
            })
            .collect(),
        QueryBody::RecursiveQueryBody { output_columns, .. } => output_columns.clone(),
    }
}

/// Common type of two set-operation branch columns.
fn union_type(l: DataType, r: DataType) -> Option<DataType> {
    if l == r {
        return Some(l);
    }
    let numeric = |d: DataType| {
        matches!(
            d,
            DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Decimal { .. }
        )
    };
    if !numeric(l) || !numeric(r) {
        return None;
    }
    if l == DataType::Float64 || r == DataType::Float64 {
        return Some(DataType::Float64);
    }

    match (l, r) {
        (
            DataType::Decimal {
                precision: left_precision,
                scale: left_scale,
            },
            DataType::Decimal {
                precision: right_precision,
                scale: right_scale,
            },
        ) => {
            let scale = left_scale.max(right_scale);
            let integer_digits = (left_precision - left_scale).max(right_precision - right_scale);
            let precision = integer_digits
                .checked_add(scale)?
                .min(htap_common::types::MAX_DECIMAL_PRECISION);
            // Clamping can drop fractional digits from the declared type, as MySQL does.
            let scale = scale.min(precision);
            Some(DataType::Decimal { precision, scale })
        }
        (DataType::Decimal { precision, scale }, DataType::Int32)
        | (DataType::Int32, DataType::Decimal { precision, scale }) => {
            let integer_digits = (precision - scale).max(10);
            let precision = integer_digits
                .checked_add(scale)?
                .min(htap_common::types::MAX_DECIMAL_PRECISION);
            // Clamping can drop fractional digits from the declared type, as MySQL does.
            let scale = scale.min(precision);
            Some(DataType::Decimal { precision, scale })
        }
        (DataType::Decimal { precision, scale }, DataType::Int64)
        | (DataType::Int64, DataType::Decimal { precision, scale }) => {
            let integer_digits = (precision - scale).max(19);
            let precision = integer_digits
                .checked_add(scale)?
                .min(htap_common::types::MAX_DECIMAL_PRECISION);
            // Clamping can drop fractional digits from the declared type, as MySQL does.
            let scale = scale.min(precision);
            Some(DataType::Decimal { precision, scale })
        }
        _ => Some(DataType::Int64),
    }
}

/// What `ORDER BY` may resolve against.
enum OrderScope {
    /// A select block: slots, visible join columns, aggregates, and output aliases.
    Select {
        slots: Vec<SlotInfo>,
        visible_schema: Option<VisibleSchema>,
        is_aggregate: bool,
        aggregates: Vec<AggregateSpec>,
        group_by: Vec<Expr>,
    },
    /// A set operation: output columns only.
    Output,
}

fn bind_set_expr(
    body: &SetExpr,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    immediate_outer_len: usize,
    subqueries: &mut Vec<BoundQuery>,
) -> Result<(QueryBody, OrderScope)> {
    match body {
        SetExpr::Select(select) => {
            let (sel, scope) = bind_select_body(
                select,
                catalog,
                ctes,
                outer,
                immediate_outer_len,
                subqueries,
            )?;
            Ok((QueryBody::Select(sel), scope))
        }
        SetExpr::Query(inner) => {
            // Parenthesized branch with its own ORDER BY / LIMIT.
            let bound = bind_query_scoped(inner, catalog, ctes, outer, immediate_outer_len)?;
            let cols = bound.output_columns.clone();
            let _ = cols;
            Ok((
                QueryBody::SetOp {
                    kind: SetOpKind::UnionAll,
                    left: Box::new(bound.clone()),
                    right: Box::new(empty_query_like(&bound)),
                },
                OrderScope::Output,
            ))
        }
        SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            let kind = match (op, set_quantifier) {
                (SetOperator::Union, SetQuantifier::All) => SetOpKind::UnionAll,
                (SetOperator::Union, SetQuantifier::None | SetQuantifier::Distinct) => {
                    SetOpKind::UnionDistinct
                }
                (SetOperator::Except, SetQuantifier::All) => SetOpKind::ExceptAll,
                (SetOperator::Except, SetQuantifier::None | SetQuantifier::Distinct) => {
                    SetOpKind::ExceptDistinct
                }
                (SetOperator::Intersect, SetQuantifier::All) => SetOpKind::IntersectAll,
                (SetOperator::Intersect, SetQuantifier::None | SetQuantifier::Distinct) => {
                    SetOpKind::IntersectDistinct
                }
                (SetOperator::Union, other) => {
                    return Err(unsupported(format!("UNION {other} not supported")))
                }
                (SetOperator::Except, other) => {
                    return Err(unsupported(format!("EXCEPT {other} not supported")))
                }
                (SetOperator::Intersect, other) => {
                    return Err(unsupported(format!("INTERSECT {other} not supported")))
                }
                (other, _) => {
                    return Err(unsupported(format!("set operator {other} not supported")))
                }
            };
            let left_q = bind_branch(left, catalog, ctes, outer, immediate_outer_len)?;
            let right_q = bind_branch(right, catalog, ctes, outer, immediate_outer_len)?;
            if left_q.output_columns.len() != right_q.output_columns.len() {
                return Err(invalid(format!(
                    "UNION branches have different column counts ({} vs {})",
                    left_q.output_columns.len(),
                    right_q.output_columns.len()
                )));
            }
            for (l, r) in left_q
                .output_columns
                .iter()
                .zip(right_q.output_columns.iter())
            {
                if union_type(l.data_type, r.data_type).is_none() {
                    return Err(invalid(format!(
                        "UNION column '{}' has incompatible types {} and {}",
                        l.name,
                        l.data_type.name(),
                        r.data_type.name()
                    )));
                }
            }
            Ok((
                QueryBody::SetOp {
                    kind,
                    left: Box::new(left_q),
                    right: Box::new(right_q),
                },
                OrderScope::Output,
            ))
        }
        SetExpr::Values(_) => Err(unsupported("VALUES as a query body not supported")),
        _ => Err(unsupported("unsupported query body in SELECT")),
    }
}

/// A branch of a set operation bound as a standalone query.
fn bind_branch(
    branch: &SetExpr,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    immediate_outer_len: usize,
) -> Result<BoundQuery> {
    match branch {
        SetExpr::Query(inner) => {
            bind_query_scoped(inner, catalog, ctes, outer, immediate_outer_len)
        }
        other => {
            let mut subqueries = Vec::new();
            let (body, _) = bind_set_expr(
                other,
                catalog,
                ctes,
                outer,
                immediate_outer_len,
                &mut subqueries,
            )?;
            let output_columns = body_output_columns(&body);
            let correlated_outer_refs = body_correlated_outer_refs(&body);
            let correlated = !correlated_outer_refs.is_empty();
            Ok(BoundQuery {
                body,
                order_by: Vec::new(),
                limit: None,
                offset: None,
                subqueries,
                correlated,
                correlated_outer_refs,
                output_columns,
            })
        }
    }
}

/// An always-empty query with the same output columns (used to wrap a lone parenthesized
/// query as `q UNION ALL <empty>`).
fn empty_query_like(q: &BoundQuery) -> BoundQuery {
    BoundQuery {
        body: q.body.clone(),
        order_by: Vec::new(),
        limit: Some(0),
        offset: None,
        subqueries: q.subqueries.clone(),
        correlated: q.correlated,
        correlated_outer_refs: q.correlated_outer_refs.clone(),
        output_columns: q.output_columns.clone(),
    }
}

fn bind_select_body(
    select: &Select,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    immediate_outer_len: usize,
    subqueries: &mut Vec<BoundQuery>,
) -> Result<(SelectBody, OrderScope)> {
    if select.top.is_some() {
        return Err(unsupported("TOP not supported in SELECT"));
    }
    if select.into.is_some() {
        return Err(unsupported("INTO not supported in SELECT"));
    }
    if !select.lateral_views.is_empty() {
        return Err(unsupported("LATERAL VIEW not supported in SELECT"));
    }
    if select.prewhere.is_some() {
        return Err(unsupported("PREWHERE not supported in SELECT"));
    }
    if !select.named_window.is_empty() {
        return Err(unsupported("WINDOW clauses not supported in SELECT"));
    }
    if select.qualify.is_some() {
        return Err(unsupported("QUALIFY not supported in SELECT"));
    }
    if !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
    {
        return Err(unsupported(
            "CLUSTER BY / DISTRIBUTE BY / SORT BY not supported",
        ));
    }
    if !select.connect_by.is_empty() {
        return Err(unsupported("CONNECT BY not supported"));
    }
    if select.exclude.is_some() || select.value_table_mode.is_some() {
        return Err(unsupported("SELECT modifiers not supported"));
    }
    let distinct = match &select.distinct {
        None | Some(sql::Distinct::All) => false,
        Some(sql::Distinct::Distinct) => true,
        Some(sql::Distinct::On(_)) => return Err(unsupported("DISTINCT ON not supported")),
    };
    if select.projection.is_empty() {
        return Err(invalid("SELECT projection cannot be empty"));
    }

    // FROM
    let mut slots: Vec<TableSlot> = Vec::new();
    let mut infos: Vec<SlotInfo> = Vec::new();
    let mut joins: Vec<JoinSpec> = Vec::new();
    let mut aggregates: Vec<AggregateSpec> = Vec::new();
    let mut windows: Vec<WindowSpec> = Vec::new();
    let mut join_tree: Option<JoinTree> = None;
    let mut visible: VisibleSchema = Vec::new();
    let mut visible_schemas: Vec<VisibleSchema> = Vec::new();

    for twj in &select.from {
        let (tree, tree_visible) = bind_table_with_joins(
            twj,
            catalog,
            ctes,
            outer,
            &mut slots,
            &mut infos,
            &mut joins,
            &mut aggregates,
            subqueries,
            &mut visible_schemas,
        )?;
        if let Some(left) = join_tree {
            let right_slot = tree_slot_range(&tree).1;
            joins.push(JoinSpec {
                kind: JoinKind::Cross,
                right_slot,
                on: None,
                equi_key_types: Vec::new(),
            });
            visible.extend(tree_visible);
            join_tree = Some(JoinTree::Join {
                kind: JoinKind::Cross,
                left: Box::new(left),
                right: Box::new(tree),
                on: None,
            });
            visible_schemas.push(visible.clone());
        } else {
            join_tree = Some(tree);
            visible = tree_visible;
        }
    }

    // WHERE
    let filter = match &select.selection {
        Some(expr) => {
            let mut binder = ExprBinder::new(
                &infos,
                catalog,
                ctes,
                outer,
                immediate_outer_len,
                &mut aggregates,
                subqueries,
            );
            binder.visible = Some(&visible);
            binder.allow_aggregates = false;
            binder.window_clause = Some("WHERE");
            let bound = binder.bind(expr)?;
            require_bool(&bound, "WHERE")?;
            Some(bound)
        }
        None => None,
    };

    // GROUP BY
    let group_by: Vec<Expr> = match &select.group_by {
        GroupByExpr::All(_) => return Err(unsupported("GROUP BY ALL is not supported")),
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported("GROUP BY modifiers not supported"));
            }
            let mut out = Vec::with_capacity(exprs.len());
            for e in exprs {
                let mut binder = ExprBinder::new(
                    &infos,
                    catalog,
                    ctes,
                    outer,
                    immediate_outer_len,
                    &mut aggregates,
                    subqueries,
                );
                binder.visible = Some(&visible);
                binder.allow_aggregates = false;
                binder.window_clause = Some("GROUP BY");
                let bound = match e {
                    SqlExpr::Value(v) if is_integer_literal(&v.value) => {
                        let sql::Value::Number(n, _) = &v.value else {
                            unreachable!();
                        };
                        let ordinal = n.parse::<usize>().ok();
                        let Some(ordinal) =
                            ordinal.filter(|n| *n >= 1 && *n <= select.projection.len())
                        else {
                            return Err(invalid(format!(
                                "GROUP BY position {n} is not in select list"
                            )));
                        };
                        let projection_expr = match &select.projection[ordinal - 1] {
                            SelectItem::UnnamedExpr(expr)
                            | SelectItem::ExprWithAlias { expr, .. } => expr,
                            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                                return Err(invalid(format!(
                                    "GROUP BY position {ordinal} refers to a wildcard"
                                )))
                            }
                            SelectItem::ExprWithAliases { .. } => unreachable!(),
                        };
                        binder.bind(projection_expr)?
                    }
                    other => binder.bind(other)?,
                };
                if out.contains(&bound) {
                    return Err(invalid(format!("duplicate expression '{e}' in GROUP BY")));
                }
                out.push(bound);
            }
            out
        }
    };

    // Projection
    let mut projection: Vec<ProjectionItem> = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(opts) => {
                if opts.opt_exclude.is_some()
                    || opts.opt_except.is_some()
                    || opts.opt_replace.is_some()
                    || opts.opt_rename.is_some()
                    || opts.opt_ilike.is_some()
                {
                    return Err(unsupported("wildcard modifiers not supported"));
                }
                if visible.is_empty() {
                    return Err(invalid("SELECT * requires a FROM clause"));
                }
                for column in &visible {
                    projection.push(ProjectionItem {
                        expr: visible_column_expr(column, &infos),
                        name: visible_column_name(column, &infos),
                    });
                }
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let name = match kind {
                    sql::SelectItemQualifiedWildcardKind::ObjectName(n) => object_name_parts(n)?,
                    sql::SelectItemQualifiedWildcardKind::Expr(_) => {
                        return Err(unsupported("expression-qualified wildcard not supported"))
                    }
                };
                if name.len() != 1 {
                    return Err(unsupported("multi-part qualified wildcard not supported"));
                }
                let (si, info) = find_slot(&infos, &name[0])?;
                let _ = si;
                for (ci, col) in info.columns.iter().enumerate() {
                    projection.push(ProjectionItem {
                        expr: column_ref(info, 0, ci, &infos),
                        name: col.name.clone(),
                    });
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                let mut binder = ExprBinder::new(
                    &infos,
                    catalog,
                    ctes,
                    outer,
                    immediate_outer_len,
                    &mut aggregates,
                    subqueries,
                );
                binder.visible = Some(&visible);
                binder.windows = Some(&mut windows);
                let bound = binder.bind(expr)?;
                let name = match expr {
                    SqlExpr::Identifier(ident) => ident.value.clone(),
                    SqlExpr::CompoundIdentifier(parts) => {
                        parts.last().map(|p| p.value.clone()).unwrap_or_default()
                    }
                    other => other.to_string(),
                };
                projection.push(ProjectionItem { expr: bound, name });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let mut binder = ExprBinder::new(
                    &infos,
                    catalog,
                    ctes,
                    outer,
                    immediate_outer_len,
                    &mut aggregates,
                    subqueries,
                );
                binder.visible = Some(&visible);
                binder.windows = Some(&mut windows);
                let bound = binder.bind(expr)?;
                projection.push(ProjectionItem {
                    expr: bound,
                    name: alias.value.clone(),
                });
            }
            SelectItem::ExprWithAliases { .. } => {
                return Err(unsupported("multiple aliases per expression not supported"))
            }
        }
    }

    let output_defs: Vec<ColumnDef> = projection
        .iter()
        .map(|p| {
            let t = p.expr.expr_type();
            ColumnDef {
                name: p.name.clone(),
                data_type: t.data_type,
                nullable: t.nullable,
                primary_key: false,
            }
        })
        .collect();

    // HAVING (source columns first, then aliases).
    let having = match &select.having {
        Some(expr) => {
            let mut binder = ExprBinder::new(
                &infos,
                catalog,
                ctes,
                outer,
                immediate_outer_len,
                &mut aggregates,
                subqueries,
            );
            binder.visible = Some(&visible);
            binder.output = Some(&output_defs);
            binder.alias_priority = false;
            binder.window_clause = Some("HAVING");
            let bound = binder.bind(expr)?;
            require_bool(&bound, "HAVING")?;
            Some(bound)
        }
        None => None,
    };

    if let Some(having) = &having {
        if contains_window_through_output(having, &projection) {
            return Err(invalid(
                "window function results cannot be referenced in HAVING clause",
            ));
        }
    }

    let is_aggregate = !group_by.is_empty() || !aggregates.is_empty();
    if is_aggregate {
        for window in &windows {
            for arg in &window.args {
                check_grouped(arg, &group_by, "window function", subqueries)?;
            }
            for expr in &window.partition_by {
                check_grouped(expr, &group_by, "window function", subqueries)?;
            }
            for item in &window.order_by {
                check_grouped(&item.expr, &group_by, "window function", subqueries)?;
            }
        }

        // Aggregate inputs may use ungrouped local columns, but correlated subqueries within
        // those inputs still reference this query's grouped context.
        for aggregate in &aggregates {
            if let Some(arg) = &aggregate.arg {
                check_aggregate_argument_subqueries(arg, &group_by, subqueries)?;
            }
        }
        for p in &projection {
            check_grouped(&p.expr, &group_by, &p.name, subqueries)?;
        }
        if let Some(h) = &having {
            check_grouped(h, &group_by, "HAVING", subqueries)?;
        }
    } else if having.is_some() {
        // HAVING without GROUP BY / aggregates behaves like WHERE over the projected row.
    }

    let join_tree = join_tree.unwrap_or_else(|| left_deep_join_tree(&joins));

    let mut body = SelectBody {
        slots,
        join_tree,
        visible_schemas,
        filter,
        group_by: group_by.clone(),
        aggregates: aggregates.clone(),
        windows,
        having,
        projection,
        distinct,
        correlated_outer_refs: Vec::new(),
    };
    body.correlated_outer_refs = select_correlated_outer_refs(&body);
    Ok((
        body,
        OrderScope::Select {
            slots: infos,
            visible_schema: Some(visible),
            is_aggregate,
            aggregates,
            group_by,
        },
    ))
}

/// Checks for window references, resolving output-column aliases through the projection.
fn contains_window_through_output(expr: &Expr, projection: &[ProjectionItem]) -> bool {
    if expr.contains_window() {
        return true;
    }

    match expr {
        Expr::OutputColumn { index, .. } => projection
            .get(*index)
            .is_some_and(|item| contains_window_through_output(&item.expr, projection)),
        Expr::BinaryOp { left, right, .. } => {
            contains_window_through_output(left, projection)
                || contains_window_through_output(right, projection)
        }
        Expr::Not(expr)
        | Expr::Negate(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => contains_window_through_output(expr, projection),
        Expr::CalendarInterval { expr, quantity, .. } => {
            contains_window_through_output(expr, projection)
                || contains_window_through_output(quantity, projection)
        }
        Expr::Like { expr, pattern, .. } => {
            contains_window_through_output(expr, projection)
                || contains_window_through_output(pattern, projection)
        }
        Expr::In { expr, list, .. } => {
            contains_window_through_output(expr, projection)
                || list
                    .iter()
                    .any(|item| contains_window_through_output(item, projection))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            contains_window_through_output(expr, projection)
                || contains_window_through_output(low, projection)
                || contains_window_through_output(high, projection)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            operand
                .as_deref()
                .is_some_and(|expr| contains_window_through_output(expr, projection))
                || branches.iter().any(|(condition, result)| {
                    contains_window_through_output(condition, projection)
                        || contains_window_through_output(result, projection)
                })
                || else_result
                    .as_deref()
                    .is_some_and(|expr| contains_window_through_output(expr, projection))
        }
        Expr::ScalarFunction { args, .. } => args
            .iter()
            .any(|arg| contains_window_through_output(arg, projection)),
        Expr::InSubquery { expr, .. } => contains_window_through_output(expr, projection),
        Expr::ColumnRef { .. }
        | Expr::CorrelatedColumnRef { .. }
        | Expr::Literal(_)
        | Expr::AggregateRef { .. }
        | Expr::WindowRef { .. }
        | Expr::ScalarSubquery { .. }
        | Expr::Exists { .. }
        | Expr::Variable { .. } => false,
    }
}

fn validate_recursive_term(query: &BoundQuery) -> Result<()> {
    if !query.order_by.is_empty() {
        return Err(invalid("recursive CTE recursive term cannot use ORDER BY"));
    }
    if query.limit.is_some() || query.offset.is_some() {
        return Err(invalid("recursive CTE recursive term cannot use LIMIT"));
    }
    let QueryBody::Select(select) = &query.body else {
        return Ok(());
    };
    if !select.aggregates.is_empty() {
        return Err(invalid(
            "recursive CTE recursive term cannot use aggregates",
        ));
    }
    if !select.group_by.is_empty() {
        return Err(invalid("recursive CTE recursive term cannot use GROUP BY"));
    }
    if select.distinct {
        return Err(invalid("recursive CTE recursive term cannot use DISTINCT"));
    }
    Ok(())
}

fn validate_recursive_reference(query: &BoundQuery, cte_name: &str) -> Result<()> {
    fn contains_working(query: &BoundQuery) -> bool {
        match &query.body {
            QueryBody::Select(select) => {
                select
                    .slots
                    .iter()
                    .any(|slot| matches!(slot, TableSlot::WorkingTableSlot { .. }))
                    || select.slots.iter().any(|slot| {
                        matches!(slot, TableSlot::Derived { query, .. } if contains_working(query))
                    })
                    || query.subqueries.iter().any(contains_working)
            }
            QueryBody::SetOp { left, right, .. }
            | QueryBody::RecursiveQueryBody {
                anchor: left,
                recursive_term: right,
                ..
            } => contains_working(left) || contains_working(right),
        }
    }

    fn tree_contains_slot(tree: &JoinTree, slot: usize) -> bool {
        match tree {
            JoinTree::Leaf(tree_slot) => *tree_slot == slot,
            JoinTree::Join { left, right, .. } => {
                tree_contains_slot(left, slot) || tree_contains_slot(right, slot)
            }
        }
    }

    fn tree_slot_is_null_supplying(tree: &JoinTree, slot: usize) -> bool {
        match tree {
            JoinTree::Leaf(_) => false,
            JoinTree::Join {
                kind, left, right, ..
            } => {
                let null_supplying = match kind {
                    JoinKind::Left => tree_contains_slot(right, slot),
                    JoinKind::Right => tree_contains_slot(left, slot),
                    JoinKind::Full => {
                        tree_contains_slot(left, slot) || tree_contains_slot(right, slot)
                    }
                    JoinKind::Inner | JoinKind::Cross => false,
                };
                null_supplying
                    || tree_slot_is_null_supplying(left, slot)
                    || tree_slot_is_null_supplying(right, slot)
            }
        }
    }

    let QueryBody::Select(select) = &query.body else {
        return Err(invalid(
            "recursive CTE must reference itself exactly once at top level of FROM",
        ));
    };
    if query.subqueries.iter().any(contains_working)
        || select
            .slots
            .iter()
            .any(|slot| matches!(slot, TableSlot::Derived { query, .. } if contains_working(query)))
    {
        return Err(invalid(
            "recursive CTE cannot be referenced inside a subquery",
        ));
    }

    let working_slots: Vec<usize> = select
        .slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            matches!(slot, TableSlot::WorkingTableSlot { .. }).then_some(index)
        })
        .collect();
    if working_slots.len() != 1 {
        return Err(invalid(
            "recursive CTE must reference itself exactly once at top level of FROM",
        ));
    }

    let working_slot = working_slots[0];
    if tree_slot_is_null_supplying(&select.join_tree, working_slot) {
        return Err(invalid(
            "recursive CTE cannot be referenced on the null-supplying side of an outer join",
        ));
    }

    let _ = cte_name;
    Ok(())
}

fn is_integer_literal(v: &sql::Value) -> bool {
    matches!(v, sql::Value::Number(n, _) if n.chars().all(|c| c.is_ascii_digit()))
}

fn require_bool(expr: &Expr, clause: &str) -> Result<()> {
    let t = expr.expr_type();
    if t.data_type != DataType::Bool && !t.is_permissive() {
        return Err(invalid(format!(
            "{clause} condition must be boolean, found {}",
            t.data_type.name()
        )));
    }
    Ok(())
}

/// Checks correlated subqueries nested in an aggregate argument against this query's grouping.
/// Local columns in the aggregate argument itself are valid without appearing in GROUP BY.
fn check_aggregate_argument_subqueries(
    expr: &Expr,
    group_by: &[Expr],
    subqueries: &[BoundQuery],
) -> Result<()> {
    match expr {
        Expr::ScalarSubquery { index, .. } | Expr::Exists { index, .. } => {
            check_correlated_subquery_grouping(&subqueries[*index], group_by, "aggregate")
        }
        Expr::InSubquery { expr, index, .. } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)?;
            check_correlated_subquery_grouping(&subqueries[*index], group_by, "aggregate")
        }
        Expr::BinaryOp { left, right, .. } => {
            check_aggregate_argument_subqueries(left, group_by, subqueries)?;
            check_aggregate_argument_subqueries(right, group_by, subqueries)
        }
        Expr::Not(expr)
        | Expr::Negate(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)
        }
        Expr::CalendarInterval { expr, quantity, .. } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)?;
            check_aggregate_argument_subqueries(quantity, group_by, subqueries)
        }
        Expr::Like { expr, pattern, .. } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)?;
            check_aggregate_argument_subqueries(pattern, group_by, subqueries)
        }
        Expr::In { expr, list, .. } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)?;
            list.iter().try_for_each(|expr| {
                check_aggregate_argument_subqueries(expr, group_by, subqueries)
            })
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            check_aggregate_argument_subqueries(expr, group_by, subqueries)?;
            check_aggregate_argument_subqueries(low, group_by, subqueries)?;
            check_aggregate_argument_subqueries(high, group_by, subqueries)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                check_aggregate_argument_subqueries(operand, group_by, subqueries)?;
            }
            for (condition, result) in branches {
                check_aggregate_argument_subqueries(condition, group_by, subqueries)?;
                check_aggregate_argument_subqueries(result, group_by, subqueries)?;
            }
            if let Some(result) = else_result {
                check_aggregate_argument_subqueries(result, group_by, subqueries)?;
            }
            Ok(())
        }
        Expr::ScalarFunction { args, .. } => args
            .iter()
            .try_for_each(|expr| check_aggregate_argument_subqueries(expr, group_by, subqueries)),
        Expr::ColumnRef { .. }
        | Expr::CorrelatedColumnRef { .. }
        | Expr::OutputColumn { .. }
        | Expr::Literal(_)
        | Expr::AggregateRef { .. }
        | Expr::WindowRef { .. }
        | Expr::Variable { .. } => Ok(()),
    }
}

/// Checks that a non-aggregate expression only depends on grouped expressions.
fn check_grouped(
    expr: &Expr,
    group_by: &[Expr],
    what: &str,
    subqueries: &[BoundQuery],
) -> Result<()> {
    if group_by.contains(expr) {
        return Ok(());
    }
    match expr {
        Expr::ColumnRef { name, .. } => Err(invalid(format!(
            "column '{name}' in '{what}' must appear in the GROUP BY clause or be used in an aggregate function"
        ))),
        Expr::CorrelatedColumnRef { .. } => Ok(()),
        Expr::OutputColumn { .. }
        | Expr::Literal(_)
        | Expr::AggregateRef { .. }
        | Expr::WindowRef { .. }
        | Expr::Variable { .. } => Ok(()),
        Expr::ScalarSubquery { index, .. } | Expr::Exists { index, .. } => {
            check_correlated_subquery_grouping(&subqueries[*index], group_by, what)
        }
        Expr::BinaryOp { left, right, .. } => {
            check_grouped(left, group_by, what, subqueries)?;
            check_grouped(right, group_by, what, subqueries)
        }
        Expr::Not(e) | Expr::Negate(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            check_grouped(e, group_by, what, subqueries)
        }
        Expr::Like { expr, pattern, .. } => {
            check_grouped(expr, group_by, what, subqueries)?;
            check_grouped(pattern, group_by, what, subqueries)
        }
        Expr::In { expr, list, .. } => {
            check_grouped(expr, group_by, what, subqueries)?;
            list.iter()
                .try_for_each(|e| check_grouped(e, group_by, what, subqueries))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            check_grouped(expr, group_by, what, subqueries)?;
            check_grouped(low, group_by, what, subqueries)?;
            check_grouped(high, group_by, what, subqueries)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                check_grouped(o, group_by, what, subqueries)?;
            }
            for (c, r) in branches {
                check_grouped(c, group_by, what, subqueries)?;
                check_grouped(r, group_by, what, subqueries)?;
            }
            if let Some(e) = else_result {
                check_grouped(e, group_by, what, subqueries)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => check_grouped(expr, group_by, what, subqueries),
        Expr::CalendarInterval { expr, quantity, .. } => {
            check_grouped(expr, group_by, what, subqueries)?;
            check_grouped(quantity, group_by, what, subqueries)
        }
        Expr::ScalarFunction { args, .. } => args
            .iter()
            .try_for_each(|e| check_grouped(e, group_by, what, subqueries)),
        Expr::InSubquery { expr, index, .. } => {
            check_grouped(expr, group_by, what, subqueries)?;
            check_correlated_subquery_grouping(&subqueries[*index], group_by, what)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn bind_table_with_joins(
    twj: &TableWithJoins,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    slots: &mut Vec<TableSlot>,
    infos: &mut Vec<SlotInfo>,
    joins: &mut Vec<JoinSpec>,
    aggregates: &mut Vec<AggregateSpec>,
    subqueries: &mut Vec<BoundQuery>,
    visible_schemas: &mut Vec<VisibleSchema>,
) -> Result<(JoinTree, VisibleSchema)> {
    let (mut tree, mut visible) = bind_table_factor_tree(
        &twj.relation,
        catalog,
        ctes,
        outer,
        slots,
        infos,
        joins,
        aggregates,
        subqueries,
        visible_schemas,
    )?;

    for join in &twj.joins {
        if join.global {
            return Err(unsupported("GLOBAL joins not supported"));
        }
        let (kind, constraint) = match &join.join_operator {
            JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinKind::Inner, c),
            JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
            JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinKind::Right, c),
            JoinOperator::CrossJoin(c) => (JoinKind::Cross, c),
            JoinOperator::FullOuter(c) => (JoinKind::Full, c),
            other => {
                return Err(unsupported(format!(
                    "join type not supported: {}",
                    join_operator_name(other)
                )))
            }
        };
        let (right, right_visible) = bind_table_factor_tree(
            &join.relation,
            catalog,
            ctes,
            outer,
            slots,
            infos,
            joins,
            aggregates,
            subqueries,
            visible_schemas,
        )?;
        let right_slot = tree_slot_range(&right).1;

        let (kind, on, merged_names) = match constraint {
            JoinConstraint::On(expr) => {
                let left_range = tree_slot_range(&tree);
                let right_range = tree_slot_range(&right);
                let scope_start = left_range.0;
                let scope_end = right_range.1;
                let mut binder = ExprBinder::new(
                    &infos[scope_start..=scope_end],
                    catalog,
                    ctes,
                    outer,
                    0,
                    aggregates,
                    subqueries,
                );
                binder.allow_aggregates = false;
                binder.window_clause = Some("JOIN ON");
                let bound = binder.bind(expr)?;
                require_bool(&bound, "JOIN ON")?;
                let bound = rebase_expr(bound, 0, scope_start)?;
                (kind, Some(bound), None)
            }
            JoinConstraint::None => match kind {
                JoinKind::Inner | JoinKind::Cross => (JoinKind::Cross, None, None),
                JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                    return Err(invalid("outer joins require an ON condition"))
                }
            },
            JoinConstraint::Using(columns) => {
                if kind == JoinKind::Cross {
                    return Err(invalid("CROSS JOIN cannot have a USING clause"));
                }
                let names: Vec<String> = columns
                    .iter()
                    .map(object_name_single)
                    .collect::<Result<_>>()?;
                if names.is_empty() {
                    return Err(invalid("JOIN USING requires at least one column"));
                }
                let on = using_predicate(&visible, &right_visible, &names, infos)?;
                (kind, on, Some(names))
            }
            JoinConstraint::Natural => {
                if kind == JoinKind::Cross {
                    return Err(invalid("NATURAL CROSS JOIN is not supported"));
                }
                let names = natural_columns(&visible, &right_visible, infos);
                if names.is_empty() {
                    let on = match kind {
                        JoinKind::Inner => None,
                        JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                            Some(Expr::Literal(Value::Bool(true)))
                        }
                        JoinKind::Cross => unreachable!(),
                    };
                    let effective_kind = if kind == JoinKind::Inner {
                        JoinKind::Cross
                    } else {
                        kind
                    };
                    (effective_kind, on, None)
                } else {
                    let on = using_predicate(&visible, &right_visible, &names, infos)?;
                    (kind, on, Some(names))
                }
            }
        };
        if kind == JoinKind::Cross && on.is_some() {
            return Err(invalid("CROSS JOIN cannot have an ON condition"));
        }

        let left_range = tree_slot_range(&tree);
        let right_range = tree_slot_range(&right);
        let pre_padding_infos = infos.clone();
        match kind {
            JoinKind::Left => mark_nullable_range(infos, right_range),
            JoinKind::Right => mark_nullable_range(infos, left_range),
            JoinKind::Full => {
                mark_nullable_range(infos, left_range);
                mark_nullable_range(infos, right_range);
            }
            JoinKind::Inner | JoinKind::Cross => {}
        }

        if let Some(names) = merged_names {
            visible = join_visible_schema(
                &visible,
                &right_visible,
                &names,
                infos,
                &pre_padding_infos,
                kind,
            )?;
        } else {
            visible.extend(right_visible);
        }
        visible_schemas.push(visible.clone());

        let equi_key_types = join_equi_key_types(on.as_ref(), right_range);
        joins.push(JoinSpec {
            kind,
            right_slot,
            on: on.clone(),
            equi_key_types,
        });
        let base_offset = infos[left_range.0].offset;
        tree = JoinTree::Join {
            kind,
            left: Box::new(tree),
            right: Box::new(right),
            on: on
                .map(|expr| rebase_expr(expr, base_offset, 0))
                .transpose()?,
        };
    }
    Ok((tree, visible))
}

fn join_operator_name(op: &JoinOperator) -> &'static str {
    match op {
        JoinOperator::Semi(_) | JoinOperator::LeftSemi(_) | JoinOperator::RightSemi(_) => {
            "SEMI JOIN"
        }
        JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_) => {
            "ANTI JOIN"
        }
        JoinOperator::CrossApply | JoinOperator::OuterApply => "APPLY",
        JoinOperator::AsOf { .. } => "ASOF JOIN",
        _ => "unknown join",
    }
}

fn mark_nullable(info: &mut SlotInfo) {
    for c in &mut info.columns {
        c.nullable = true;
    }
}

fn mark_nullable_range(infos: &mut [SlotInfo], range: (usize, usize)) {
    infos[range.0..=range.1].iter_mut().for_each(mark_nullable);
}

fn tree_slot_range(tree: &JoinTree) -> (usize, usize) {
    match tree {
        JoinTree::Leaf(slot) => (*slot, *slot),
        JoinTree::Join { left, right, .. } => {
            let (first, _) = tree_slot_range(left);
            let (_, last) = tree_slot_range(right);
            (first, last)
        }
    }
}

fn left_deep_join_tree(joins: &[JoinSpec]) -> JoinTree {
    let mut tree = JoinTree::Leaf(0);
    for join in joins {
        tree = JoinTree::Join {
            kind: join.kind,
            left: Box::new(tree),
            right: Box::new(JoinTree::Leaf(join.right_slot)),
            on: join.on.clone(),
        };
    }
    tree
}

fn join_equi_key_types(
    on: Option<&Expr>,
    right_slot_range: (usize, usize),
) -> Vec<Option<DataType>> {
    fn conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::BinaryOp {
                op: BinOp::And,
                left,
                right,
            } => {
                conjuncts(left, out);
                conjuncts(right, out);
            }
            other => out.push(other),
        }
    }

    let Some(on) = on else {
        return Vec::new();
    };
    let mut terms = Vec::new();
    conjuncts(on, &mut terms);

    let is_left =
        |slots: &[usize]| !slots.is_empty() && slots.iter().all(|slot| *slot < right_slot_range.0);
    let is_right = |slots: &[usize]| {
        !slots.is_empty()
            && slots
                .iter()
                .all(|slot| *slot >= right_slot_range.0 && *slot <= right_slot_range.1)
    };

    terms
        .into_iter()
        .filter_map(|term| {
            let Expr::BinaryOp {
                op: BinOp::Eq,
                left,
                right,
            } = term
            else {
                return None;
            };
            let left_slots = left.referenced_slots();
            let right_slots = right.referenced_slots();
            let (left, right) = if is_left(&left_slots) && is_right(&right_slots) {
                (left.as_ref(), right.as_ref())
            } else if is_right(&left_slots) && is_left(&right_slots) {
                (right.as_ref(), left.as_ref())
            } else {
                return None;
            };

            let left_type = left.expr_type().data_type;
            let right_type = right.expr_type().data_type;
            let numeric = |data_type| {
                matches!(
                    data_type,
                    DataType::Int32
                        | DataType::Int64
                        | DataType::Float64
                        | DataType::Decimal { .. }
                        | DataType::Timestamp
                )
            };
            Some(if numeric(left_type) && numeric(right_type) {
                Some(
                    if left_type == DataType::Float64 || right_type == DataType::Float64 {
                        DataType::Float64
                    } else {
                        match (left_type, right_type) {
                            (
                                DataType::Decimal {
                                    precision: left_precision,
                                    scale: left_scale,
                                },
                                DataType::Decimal {
                                    precision: right_precision,
                                    scale: right_scale,
                                },
                            ) => {
                                let scale = left_scale.max(right_scale);
                                let integer_digits = (left_precision - left_scale)
                                    .max(right_precision - right_scale);
                                let precision = integer_digits.checked_add(scale)?;
                                if precision > htap_common::types::MAX_DECIMAL_PRECISION {
                                    return None;
                                }
                                DataType::Decimal { precision, scale }
                            }
                            (decimal @ DataType::Decimal { .. }, _)
                            | (_, decimal @ DataType::Decimal { .. }) => decimal,
                            _ => DataType::Int64,
                        }
                    },
                )
            } else {
                None
            })
        })
        .collect()
}

fn push_slot(slot: TableSlot, slots: &mut Vec<TableSlot>, infos: &mut Vec<SlotInfo>) -> Result<()> {
    let alias = slot.alias().to_string();
    if infos.iter().any(|i| i.alias.eq_ignore_ascii_case(&alias)) {
        return Err(invalid(format!("not unique table/alias: '{alias}'")));
    }
    let offset = infos.iter().map(|i| i.columns.len()).sum();
    infos.push(SlotInfo {
        alias,
        columns: slot.columns().to_vec(),
        offset,
    });
    slots.push(slot);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn bind_table_factor_tree(
    factor: &TableFactor,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    slots: &mut Vec<TableSlot>,
    infos: &mut Vec<SlotInfo>,
    joins: &mut Vec<JoinSpec>,
    aggregates: &mut Vec<AggregateSpec>,
    subqueries: &mut Vec<BoundQuery>,
    visible_schemas: &mut Vec<VisibleSchema>,
) -> Result<(JoinTree, VisibleSchema)> {
    match factor {
        TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } => {
            if alias.is_some() {
                return Err(unsupported(
                    "aliases on nested join expressions not supported",
                ));
            }
            bind_table_with_joins(
                table_with_joins,
                catalog,
                ctes,
                outer,
                slots,
                infos,
                joins,
                aggregates,
                subqueries,
                visible_schemas,
            )
        }
        _ => {
            let slot = bind_table_factor_leaf(factor, catalog, ctes, outer)?;
            push_slot(slot, slots, infos)?;
            let slot_index = slots.len() - 1;
            let visible = physical_visible_schema(slot_index, &infos[slot_index]);
            visible_schemas.push(visible.clone());
            Ok((JoinTree::Leaf(slot_index), visible))
        }
    }
}

fn bind_table_factor_leaf(
    factor: &TableFactor,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
) -> Result<TableSlot> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            partitions,
            with_ordinality,
            json_path,
            sample,
            index_hints,
        } => {
            if args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
                || !partitions.is_empty()
                || *with_ordinality
                || json_path.is_some()
                || sample.is_some()
                || !index_hints.is_empty()
            {
                return Err(unsupported("unsupported table factor options in SELECT"));
            }
            if let Some(a) = alias {
                if !a.columns.is_empty() {
                    return Err(unsupported("table alias column lists not supported"));
                }
            }
            let table_name = object_name_single(name)?;
            let alias_name = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| table_name.clone());
            if let Some(cte) = ctes.lookup(&table_name) {
                return Ok(match cte {
                    CteBinding::Query(query) => TableSlot::Derived {
                        columns: query.output_columns.clone(),
                        query: query.clone(),
                        alias: alias_name,
                    },
                    CteBinding::Working(columns) => TableSlot::WorkingTableSlot {
                        alias: alias_name,
                        columns: columns.clone(),
                    },
                });
            }
            let table_desc = catalog
                .table_by_name(&table_name)
                .ok_or_else(|| table_not_found(&table_name))?;
            crate::binder::validate_table_descriptor_primary_key(table_desc)?;
            Ok(TableSlot::Base {
                table: table_name,
                alias: alias_name,
                columns: table_desc.schema.columns().to_vec(),
            })
        }
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample,
        } => {
            if *lateral {
                return Err(unsupported("LATERAL derived tables not supported"));
            }
            if sample.is_some() {
                return Err(unsupported("TABLESAMPLE not supported"));
            }
            let alias = alias
                .as_ref()
                .ok_or_else(|| invalid("every derived table must have its own alias"))?;
            if !alias.columns.is_empty() {
                return Err(unsupported("derived table column lists not supported"));
            }
            let bound = bind_query_scoped(subquery, catalog, ctes, outer, 0)?;
            ensure_unique_output_names(&bound, &alias.name.value)?;
            Ok(TableSlot::Derived {
                columns: bound.output_columns.clone(),
                query: Box::new(bound),
                alias: alias.name.value.clone(),
            })
        }
        TableFactor::NestedJoin { .. } => unreachable!("nested joins are bound as join trees"),
        _ => Err(unsupported("unsupported table relation in SELECT")),
    }
}

fn find_slot<'a>(infos: &'a [SlotInfo], name: &str) -> Result<(usize, &'a SlotInfo)> {
    if let Some(found) = infos.iter().enumerate().find(|(_, i)| i.alias == name) {
        return Ok(found);
    }
    infos
        .iter()
        .enumerate()
        .find(|(_, i)| i.alias.eq_ignore_ascii_case(name))
        .ok_or_else(|| invalid(format!("unknown table or alias '{name}'")))
}

fn column_ref(info: &SlotInfo, _unused: usize, column: usize, infos: &[SlotInfo]) -> Expr {
    let slot = infos
        .iter()
        .position(|i| std::ptr::eq(i, info))
        .unwrap_or(0);
    let col = &info.columns[column];
    Expr::ColumnRef {
        slot,
        column,
        offset: info.offset + column,
        name: col.name.clone(),
        data_type: col.data_type,
        nullable: col.nullable,
    }
}

/// Produces the physical visible schema for a newly-added FROM slot.
fn physical_visible_schema(slot: usize, info: &SlotInfo) -> VisibleSchema {
    info.columns
        .iter()
        .enumerate()
        .map(|(column, _)| VisibleColumn::Physical { slot, column })
        .collect()
}

/// Returns the visible name of a column.
fn visible_column_name(column: &VisibleColumn, infos: &[SlotInfo]) -> String {
    match column {
        VisibleColumn::Physical { slot, column } => infos[*slot].columns[*column].name.clone(),
        VisibleColumn::Merged { name, .. } => name.clone(),
    }
}

/// Returns an expression which evaluates a visible column against the complete joined row.
fn visible_column_expr(column: &VisibleColumn, infos: &[SlotInfo]) -> Expr {
    match column {
        VisibleColumn::Physical { slot, column } => {
            let info = &infos[*slot];
            column_ref(info, 0, *column, infos)
        }
        VisibleColumn::Merged { expr, .. } => expr.clone(),
    }
}

/// Finds a visible column, preserving exact-case preference used by ordinary column binding.
fn find_visible_column<'a>(
    schema: &'a VisibleSchema,
    infos: &[SlotInfo],
    name: &str,
) -> Result<&'a VisibleColumn> {
    let mut exact = schema
        .iter()
        .filter(|column| visible_column_name(column, infos) == name);
    if let Some(column) = exact.next() {
        if exact.next().is_none() {
            return Ok(column);
        }
        return Err(invalid(format!("column '{name}' is ambiguous")));
    }

    let mut folded = schema
        .iter()
        .filter(|column| visible_column_name(column, infos).eq_ignore_ascii_case(name));
    match (folded.next(), folded.next()) {
        (Some(column), None) => Ok(column),
        (Some(_), Some(_)) => Err(invalid(format!("column '{name}' is ambiguous"))),
        (None, _) => Err(invalid(format!("unknown column '{name}'"))),
    }
}

/// Builds a merged `USING`/`NATURAL` output column. SQL join equality guarantees that either
/// value is suitable for inner joins; COALESCE preserves the non-null side for outer joins.
fn merged_visible_column(
    name: String,
    left: &VisibleColumn,
    right: &VisibleColumn,
    infos: &[SlotInfo],
    pre_padding_infos: &[SlotInfo],
    kind: JoinKind,
) -> Result<VisibleColumn> {
    let left_expr = visible_column_expr(left, infos);
    let right_expr = visible_column_expr(right, infos);
    check_comparable(&left_expr, &right_expr, "JOIN USING")?;
    let left_type = left_expr.expr_type();
    let right_type = right_expr.expr_type();
    let left_pre_padding_type = visible_column_expr(left, pre_padding_infos).expr_type();
    let right_pre_padding_type = visible_column_expr(right, pre_padding_infos).expr_type();
    let nullable = match kind {
        JoinKind::Inner => left_pre_padding_type.nullable && right_pre_padding_type.nullable,
        JoinKind::Left => left_pre_padding_type.nullable,
        JoinKind::Right => right_pre_padding_type.nullable,
        JoinKind::Full => left_pre_padding_type.nullable || right_pre_padding_type.nullable,
        JoinKind::Cross => unreachable!("CROSS JOIN cannot merge visible columns"),
    };
    let data_type =
        union_type(left_type.data_type, right_type.data_type).unwrap_or(left_type.data_type);
    Ok(VisibleColumn::Merged {
        expr: Expr::ScalarFunction {
            func: ScalarFn::Coalesce,
            args: vec![left_expr, right_expr],
            data_type,
            nullable,
        },
        name,
        data_type,
        nullable,
    })
}

/// Builds the visible schema for a join. Common columns occur first, followed by the remaining
/// visible left columns and then the remaining visible right columns, as required by MySQL.
fn join_visible_schema(
    left: &VisibleSchema,
    right: &VisibleSchema,
    names: &[String],
    infos: &[SlotInfo],
    pre_padding_infos: &[SlotInfo],
    kind: JoinKind,
) -> Result<VisibleSchema> {
    let mut merged = Vec::with_capacity(names.len() + left.len() + right.len());
    let mut left_used = vec![false; left.len()];
    let mut right_used = vec![false; right.len()];

    for name in names {
        let left_index = left
            .iter()
            .position(|column| visible_column_name(column, infos).eq_ignore_ascii_case(name))
            .ok_or_else(|| invalid(format!("column '{name}' does not exist in left join input")))?;
        let right_index = right
            .iter()
            .position(|column| visible_column_name(column, infos).eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                invalid(format!(
                    "column '{name}' does not exist in right join input"
                ))
            })?;
        if left_used[left_index] || right_used[right_index] {
            return Err(invalid(format!("duplicate column '{name}' in JOIN USING")));
        }
        left_used[left_index] = true;
        right_used[right_index] = true;
        merged.push(merged_visible_column(
            visible_column_name(&left[left_index], infos),
            &left[left_index],
            &right[right_index],
            infos,
            pre_padding_infos,
            kind,
        )?);
    }

    merged.extend(
        left.iter()
            .enumerate()
            .filter(|(index, _)| !left_used[*index])
            .map(|(_, column)| column.clone()),
    );
    merged.extend(
        right
            .iter()
            .enumerate()
            .filter(|(index, _)| !right_used[*index])
            .map(|(_, column)| column.clone()),
    );
    Ok(merged)
}

/// Produces equality predicates for all `USING`/`NATURAL` columns.
fn using_predicate(
    left: &VisibleSchema,
    right: &VisibleSchema,
    names: &[String],
    infos: &[SlotInfo],
) -> Result<Option<Expr>> {
    let mut predicate = None;
    for name in names {
        let left_column = find_visible_column(left, infos, name)?;
        let right_column = find_visible_column(right, infos, name)?;
        let left_expr = visible_column_expr(left_column, infos);
        let right_expr = visible_column_expr(right_column, infos);
        check_comparable(&left_expr, &right_expr, "JOIN USING")?;
        let equality = Expr::BinaryOp {
            op: BinOp::Eq,
            left: Box::new(left_expr),
            right: Box::new(right_expr),
        };
        predicate = Some(match predicate {
            Some(previous) => Expr::BinaryOp {
                op: BinOp::And,
                left: Box::new(previous),
                right: Box::new(equality),
            },
            None => equality,
        });
    }
    Ok(predicate)
}

/// Names shared by both visible schemas, in left-input order.
fn natural_columns(left: &VisibleSchema, right: &VisibleSchema, infos: &[SlotInfo]) -> Vec<String> {
    left.iter()
        .filter_map(|left_column| {
            let name = visible_column_name(left_column, infos);
            right
                .iter()
                .any(|right_column| {
                    visible_column_name(right_column, infos).eq_ignore_ascii_case(&name)
                })
                .then_some(name)
        })
        .collect()
}

/// Rebases column offsets when an expression is evaluated against a joined-row suffix.
fn rebase_expr(expr: Expr, base_offset: usize, slot_base: usize) -> Result<Expr> {
    Ok(match expr {
        Expr::ColumnRef {
            slot,
            column,
            offset,
            name,
            data_type,
            nullable,
        } => Expr::ColumnRef {
            slot: slot
                .checked_add(slot_base)
                .ok_or_else(|| invalid("column slot overflow while rebasing join expression"))?,
            column,
            offset: offset
                .checked_sub(base_offset)
                .ok_or_else(|| invalid("column offset is outside the visible join subtree"))?,
            name,
            data_type,
            nullable,
        },
        Expr::CorrelatedColumnRef {
            offset,
            name,
            data_type,
            nullable,
        } => Expr::CorrelatedColumnRef {
            offset,
            name,
            data_type,
            nullable,
        },
        Expr::OutputColumn {
            index,
            data_type,
            nullable,
        } => Expr::OutputColumn {
            index,
            data_type,
            nullable,
        },
        Expr::Literal(value) => Expr::Literal(value),
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(rebase_expr(*left, base_offset, slot_base)?),
            right: Box::new(rebase_expr(*right, base_offset, slot_base)?),
        },
        Expr::Not(expr) => Expr::Not(Box::new(rebase_expr(*expr, base_offset, slot_base)?)),
        Expr::Negate(expr) => Expr::Negate(Box::new(rebase_expr(*expr, base_offset, slot_base)?)),
        Expr::IsNull(expr) => Expr::IsNull(Box::new(rebase_expr(*expr, base_offset, slot_base)?)),
        Expr::IsNotNull(expr) => {
            Expr::IsNotNull(Box::new(rebase_expr(*expr, base_offset, slot_base)?))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            pattern: Box::new(rebase_expr(*pattern, base_offset, slot_base)?),
            negated,
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            list: list
                .into_iter()
                .map(|expr| rebase_expr(expr, base_offset, slot_base))
                .collect::<Result<_>>()?,
            negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            low: Box::new(rebase_expr(*low, base_offset, slot_base)?),
            high: Box::new(rebase_expr(*high, base_offset, slot_base)?),
            negated,
        },
        Expr::Case {
            operand,
            branches,
            else_result,
            data_type,
            nullable,
        } => Expr::Case {
            operand: operand
                .map(|expr| rebase_expr(*expr, base_offset, slot_base).map(Box::new))
                .transpose()?,
            branches: branches
                .into_iter()
                .map(|(condition, result)| {
                    Ok((
                        rebase_expr(condition, base_offset, slot_base)?,
                        rebase_expr(result, base_offset, slot_base)?,
                    ))
                })
                .collect::<Result<_>>()?,
            else_result: else_result
                .map(|expr| rebase_expr(*expr, base_offset, slot_base).map(Box::new))
                .transpose()?,
            data_type,
            nullable,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            to,
        },
        Expr::CalendarInterval {
            expr,
            quantity,
            unit,
            negated,
        } => Expr::CalendarInterval {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            quantity: Box::new(rebase_expr(*quantity, base_offset, slot_base)?),
            unit,
            negated,
        },
        Expr::ScalarFunction {
            func,
            args,
            data_type,
            nullable,
        } => Expr::ScalarFunction {
            func,
            args: args
                .into_iter()
                .map(|expr| rebase_expr(expr, base_offset, slot_base))
                .collect::<Result<_>>()?,
            data_type,
            nullable,
        },
        Expr::AggregateRef {
            index,
            data_type,
            nullable,
        } => Expr::AggregateRef {
            index,
            data_type,
            nullable,
        },
        Expr::WindowRef {
            index,
            data_type,
            nullable,
        } => Expr::WindowRef {
            index,
            data_type,
            nullable,
        },
        Expr::ScalarSubquery {
            index,
            data_type,
            correlated,
        } => Expr::ScalarSubquery {
            index,
            data_type,
            correlated,
        },
        Expr::InSubquery {
            expr,
            index,
            negated,
            correlated,
        } => Expr::InSubquery {
            expr: Box::new(rebase_expr(*expr, base_offset, slot_base)?),
            index,
            negated,
            correlated,
        },
        Expr::Exists {
            index,
            negated,
            correlated,
        } => Expr::Exists {
            index,
            negated,
            correlated,
        },
        Expr::Variable { name, is_system } => Expr::Variable { name, is_system },
    })
}

#[allow(clippy::too_many_arguments)]
fn bind_order_by(
    order_by: Option<&sql::OrderBy>,
    scope: &OrderScope,
    output: &[ColumnDef],
    subqueries: &mut Vec<BoundQuery>,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
) -> Result<Vec<OrderItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    if order_by.interpolate.is_some() {
        return Err(unsupported("INTERPOLATE not supported in ORDER BY"));
    }
    let exprs = match &order_by.kind {
        OrderByKind::Expressions(e) => e,
        OrderByKind::All(_) => return Err(unsupported("ORDER BY ALL not supported")),
    };
    let mut items = Vec::with_capacity(exprs.len());
    for ob in exprs {
        if ob.with_fill.is_some() {
            return Err(unsupported("WITH FILL not supported in ORDER BY"));
        }
        let asc = ob.options.asc.unwrap_or(true);
        let nulls_first = ob.options.nulls_first.unwrap_or(asc);
        let expr = match &ob.expr {
            SqlExpr::Value(v) if is_integer_literal(&v.value) => {
                let sql::Value::Number(n, _) = &v.value else {
                    unreachable!();
                };
                let ordinal = n.parse::<usize>().ok();
                let Some(ordinal) = ordinal.filter(|n| *n >= 1 && *n <= output.len()) else {
                    return Err(invalid(format!(
                        "ORDER BY position {n} is not in select list"
                    )));
                };
                let column = &output[ordinal - 1];
                Expr::OutputColumn {
                    index: ordinal - 1,
                    data_type: column.data_type,
                    nullable: column.nullable,
                }
            }
            _ => match scope {
                OrderScope::Output => {
                    let mut no_aggs = Vec::new();
                    let mut binder =
                        ExprBinder::new(&[], catalog, ctes, outer, 0, &mut no_aggs, subqueries);
                    binder.output = Some(output);
                    binder.alias_priority = true;
                    binder.output_only = true;
                    binder.bind(&ob.expr)?
                }
                OrderScope::Select {
                    slots,
                    visible_schema,
                    is_aggregate,
                    aggregates,
                    group_by,
                } => {
                    let mut aggs = aggregates.clone();
                    let physical_visible;
                    let visible = match visible_schema.as_ref() {
                        Some(visible) => visible,
                        None => {
                            physical_visible = slots
                                .iter()
                                .enumerate()
                                .flat_map(|(slot, info)| physical_visible_schema(slot, info))
                                .collect::<VisibleSchema>();
                            &physical_visible
                        }
                    };
                    let mut binder =
                        ExprBinder::new(slots, catalog, ctes, outer, 0, &mut aggs, subqueries);
                    binder.visible = Some(visible);
                    binder.output = Some(output);
                    binder.alias_priority = true;
                    let bound = binder.bind(&ob.expr)?;
                    if aggs.len() != aggregates.len() {
                        return Err(invalid(
                            "ORDER BY aggregate expressions must also appear in the select list",
                        ));
                    }
                    if *is_aggregate {
                        check_grouped(&bound, group_by, "ORDER BY", subqueries)?;
                    }
                    bound
                }
            },
        };
        items.push(OrderItem {
            expr,
            asc,
            nulls_first,
        });
    }
    Ok(items)
}

fn bind_limit(limit: Option<&LimitClause>) -> Result<(Option<u64>, Option<u64>)> {
    let Some(limit) = limit else {
        return Ok((None, None));
    };
    match limit {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if !limit_by.is_empty() {
                return Err(unsupported("LIMIT BY not supported"));
            }
            let l = match limit {
                Some(e) => Some(limit_literal(e, "LIMIT")?),
                None => None,
            };
            let o = match offset {
                Some(off) => Some(limit_literal(&off.value, "OFFSET")?),
                None => None,
            };
            Ok((l, o))
        }
        LimitClause::OffsetCommaLimit { offset, limit } => Ok((
            Some(limit_literal(limit, "LIMIT")?),
            Some(limit_literal(offset, "OFFSET")?),
        )),
    }
}

fn limit_literal(expr: &SqlExpr, what: &str) -> Result<u64> {
    match expr {
        SqlExpr::Value(v) => match &v.value {
            sql::Value::Number(n, _) => n
                .parse::<u64>()
                .map_err(|_| invalid(format!("{what} must be a non-negative integer, found {n}"))),
            other => Err(invalid(format!(
                "{what} must be a non-negative integer literal, found {other}"
            ))),
        },
        other => Err(invalid(format!(
            "{what} must be a non-negative integer literal, found {other}"
        ))),
    }
}

/// Parses a MySQL variable identifier with no scope qualifier: `@name` (user variable) or
/// `@@name` (unscoped system variable; MySQL treats this as session-scoped). Returns `Ok(None)`
/// if `raw` does not start with `@` (an ordinary column reference).
///
/// The MySQL dialect's tokenizer treats `@` as an identifier-start character
/// (`vendor/sqlparser/src/dialect/mysql.rs`), so `@x` and `@@x` each arrive as one
/// `Expr::Identifier`, not a unary operator applied to a bare identifier.
fn parse_variable_ident(raw: &str) -> Result<Option<Expr>> {
    let (rest, is_system) = if let Some(r) = raw.strip_prefix("@@") {
        (r, true)
    } else if let Some(r) = raw.strip_prefix('@') {
        (r, false)
    } else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Err(invalid("empty variable name"));
    }
    Ok(Some(Expr::Variable {
        name: rest.to_string(),
        is_system,
    }))
}

/// Parses `@@session.name` / `@@global.name`: a two-part compound identifier whose first part
/// starts with `@@` (the `.` is not an identifier character, so the tokenizer splits `@@scope`
/// and `name` into separate parts). Returns `Ok(None)` if `qualifier` does not start with `@@`
/// (an ordinary qualified column reference, e.g. `t.c`).
fn parse_scoped_system_variable(qualifier: &str, name: &str) -> Result<Option<Expr>> {
    let Some(scope) = qualifier.strip_prefix("@@") else {
        return Ok(None);
    };
    if scope.eq_ignore_ascii_case("global") {
        return Err(unsupported(
            "SET/SELECT @@GLOBAL is not supported; this engine only exposes SESSION-scoped system variables",
        ));
    }
    if !scope.eq_ignore_ascii_case("session") {
        return Err(unsupported(format!(
            "unsupported system variable scope '{scope}'"
        )));
    }
    if name.is_empty() {
        return Err(invalid("empty system variable name"));
    }
    Ok(Some(Expr::Variable {
        name: name.to_string(),
        is_system: true,
    }))
}

/// Expression binder over a set of slots.
struct ExprBinder<'a> {
    slots: &'a [SlotInfo],
    catalog: &'a CatalogSnapshot,
    ctes: &'a CteScope,
    outer: &'a [SlotInfo],
    immediate_outer_len: usize,
    aggregates: &'a mut Vec<AggregateSpec>,
    subqueries: &'a mut Vec<BoundQuery>,
    /// Visible columns after applying NATURAL/USING coalescing.
    visible: Option<&'a VisibleSchema>,
    /// Output columns available for alias / ordinal resolution.
    output: Option<&'a [ColumnDef]>,
    /// Resolve aliases before source columns (ORDER BY) or after (HAVING).
    alias_priority: bool,
    /// Only output columns may be referenced (ORDER BY of a set operation).
    output_only: bool,
    allow_aggregates: bool,
    allow_subqueries: bool,
    in_aggregate: bool,
    /// Window specifications collected while binding the projection.
    windows: Option<&'a mut Vec<WindowSpec>>,
    /// Clause in which window functions are forbidden.
    window_clause: Option<&'static str>,
    /// Whether the current expression is part of another window specification.
    in_window: bool,
}

impl<'a> ExprBinder<'a> {
    fn new(
        slots: &'a [SlotInfo],
        catalog: &'a CatalogSnapshot,
        ctes: &'a CteScope,
        outer: &'a [SlotInfo],
        immediate_outer_len: usize,
        aggregates: &'a mut Vec<AggregateSpec>,
        subqueries: &'a mut Vec<BoundQuery>,
    ) -> Self {
        Self {
            slots,
            catalog,
            ctes,
            outer,
            immediate_outer_len,
            aggregates,
            subqueries,
            visible: None,
            output: None,
            alias_priority: false,
            output_only: false,
            allow_aggregates: true,
            allow_subqueries: true,
            in_aggregate: false,
            windows: None,
            window_clause: None,
            in_window: false,
        }
    }

    fn immediate_outer(&self) -> &[SlotInfo] {
        &self.outer[..self.immediate_outer_len]
    }

    fn ancestor_slots(&self) -> &[SlotInfo] {
        &self.outer[self.immediate_outer_len..]
    }

    fn resolve_alias(&self, name: &str) -> Option<Expr> {
        let output = self.output?;
        let idx = output.iter().position(|c| c.name == name).or_else(|| {
            output
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
        })?;
        let c = &output[idx];
        Some(Expr::OutputColumn {
            index: idx,
            data_type: c.data_type,
            nullable: c.nullable,
        })
    }

    fn resolve_column(&self, qualifier: Option<&str>, name: &str) -> Result<Expr> {
        if self.output_only {
            return self
                .resolve_alias(name)
                .ok_or_else(|| invalid(format!("unknown column '{name}' in ORDER BY")));
        }
        if qualifier.is_none() && self.alias_priority {
            if let Some(e) = self.resolve_alias(name) {
                return Ok(e);
            }
        }
        if qualifier.is_none() {
            if let Some(visible) = self.visible {
                match find_visible_column(visible, self.slots, name) {
                    Ok(column) => return Ok(visible_column_expr(column, self.slots)),
                    Err(HtapError::InvalidArgument(message))
                        if message == format!("unknown column '{name}'") => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let candidates: Vec<(usize, usize)> = match qualifier {
            Some(q) => {
                let (si, info) = match find_slot(self.slots, q) {
                    Ok(found) => found,
                    Err(e) => {
                        // Resolve against the immediate enclosing query when possible. Only an
                        // unresolved qualifier can indicate an unsupported grandparent reference.
                        if self
                            .immediate_outer()
                            .iter()
                            .any(|info| info.alias.eq_ignore_ascii_case(q))
                        {
                            return self.resolve_correlated_column(Some(q), name);
                        }
                        if self
                            .ancestor_slots()
                            .iter()
                            .any(|info| info.alias.eq_ignore_ascii_case(q))
                        {
                            return Err(invalid(
                                "correlated subqueries may only reference the immediately enclosing query",
                            ));
                        }
                        return Err(e);
                    }
                };
                info.columns
                    .iter()
                    .position(|c| c.name == name)
                    .or_else(|| {
                        info.columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(name))
                    })
                    .map(|ci| vec![(si, ci)])
                    .unwrap_or_default()
            }
            None => {
                let mut exact: Vec<(usize, usize)> = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(si, info)| {
                        info.columns
                            .iter()
                            .position(|c| c.name == name)
                            .map(|ci| (si, ci))
                    })
                    .collect();
                if exact.is_empty() {
                    exact = self
                        .slots
                        .iter()
                        .enumerate()
                        .filter_map(|(si, info)| {
                            info.columns
                                .iter()
                                .position(|c| c.name.eq_ignore_ascii_case(name))
                                .map(|ci| (si, ci))
                        })
                        .collect();
                }
                exact
            }
        };
        match candidates.len() {
            1 => {
                let (si, ci) = candidates[0];
                let info = &self.slots[si];
                let col = &info.columns[ci];
                Ok(Expr::ColumnRef {
                    slot: si,
                    column: ci,
                    offset: info.offset + ci,
                    name: col.name.clone(),
                    data_type: col.data_type,
                    nullable: col.nullable,
                })
            }
            0 => {
                if qualifier.is_none() && !self.alias_priority {
                    if let Some(e) = self.resolve_alias(name) {
                        return Ok(e);
                    }
                }
                let display = match qualifier {
                    Some(q) => format!("{q}.{name}"),
                    None => name.to_string(),
                };
                let in_outer = self.immediate_outer().iter().any(|info| {
                    qualifier.is_none_or(|q| info.alias.eq_ignore_ascii_case(q))
                        && info
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(name))
                });
                if in_outer {
                    return self.resolve_correlated_column(qualifier, name);
                }
                let in_ancestor = self.ancestor_slots().iter().any(|info| {
                    qualifier.is_none_or(|q| info.alias.eq_ignore_ascii_case(q))
                        && info
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(name))
                });
                if in_ancestor {
                    return Err(invalid(
                        "correlated subqueries may only reference the immediately enclosing query",
                    ));
                }
                Err(invalid(format!("unknown column '{display}'")))
            }
            _ => Err(invalid(format!("column '{name}' is ambiguous"))),
        }
    }

    fn resolve_correlated_column(&self, qualifier: Option<&str>, name: &str) -> Result<Expr> {
        let candidates: Vec<(usize, usize)> = match qualifier {
            Some(q) => {
                let Some((slot, info)) = self
                    .immediate_outer()
                    .iter()
                    .enumerate()
                    .find(|(_, info)| info.alias.eq_ignore_ascii_case(q))
                else {
                    return Err(invalid(format!("unknown table or alias '{q}'")));
                };
                info.columns
                    .iter()
                    .position(|column| column.name == name)
                    .or_else(|| {
                        info.columns
                            .iter()
                            .position(|column| column.name.eq_ignore_ascii_case(name))
                    })
                    .map(|column| vec![(slot, column)])
                    .unwrap_or_default()
            }
            None => {
                let mut exact: Vec<(usize, usize)> = self
                    .immediate_outer()
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, info)| {
                        info.columns
                            .iter()
                            .position(|column| column.name == name)
                            .map(|column| (slot, column))
                    })
                    .collect();
                if exact.is_empty() {
                    exact = self
                        .immediate_outer()
                        .iter()
                        .enumerate()
                        .filter_map(|(slot, info)| {
                            info.columns
                                .iter()
                                .position(|column| column.name.eq_ignore_ascii_case(name))
                                .map(|column| (slot, column))
                        })
                        .collect();
                }
                exact
            }
        };

        match candidates.as_slice() {
            [(slot, column)] => {
                let info = &self.immediate_outer()[*slot];
                let column = &info.columns[*column];
                Ok(Expr::CorrelatedColumnRef {
                    offset: info.offset + candidates[0].1,
                    name: column.name.clone(),
                    data_type: column.data_type,
                    nullable: column.nullable,
                })
            }
            [] => {
                let display = match qualifier {
                    Some(q) => format!("{q}.{name}"),
                    None => name.to_string(),
                };
                Err(invalid(format!("unknown column '{display}'")))
            }
            _ => Err(invalid(format!("column '{name}' is ambiguous"))),
        }
    }

    fn bind(&mut self, expr: &SqlExpr) -> Result<Expr> {
        match expr {
            SqlExpr::Nested(inner) => self.bind(inner),
            SqlExpr::Identifier(ident) => match parse_variable_ident(&ident.value)? {
                Some(v) => Ok(v),
                None => self.resolve_column(None, &ident.value),
            },
            SqlExpr::CompoundIdentifier(parts) => match parts.len() {
                2 => {
                    if let Some(v) = parse_scoped_system_variable(&parts[0].value, &parts[1].value)?
                    {
                        return Ok(v);
                    }
                    self.resolve_column(Some(&parts[0].value), &parts[1].value)
                }
                _ => Err(unsupported(format!(
                    "multi-part column reference not supported: {expr}"
                ))),
            },
            SqlExpr::Value(v) => bind_literal(&v.value),
            SqlExpr::TypedString(typed_string) => {
                if matches!(typed_string.data_type, sql::DataType::Date) {
                    let value = typed_string
                        .value
                        .clone()
                        .into_string()
                        .ok_or_else(|| invalid("DATE literal requires a value"))?;
                    return parse_date_to_timestamp_micros(&value)
                        .map(|micros| Expr::Literal(Value::Timestamp(micros)));
                }
                Err(unsupported(format!("typed literal not supported: {expr}")))
            }
            SqlExpr::Interval(_) => {
                Err(unsupported(format!("typed literal not supported: {expr}")))
            }
            SqlExpr::UnaryOp { op, expr: inner } => match op {
                sql::UnaryOperator::Not => {
                    let e = self.bind(inner)?;
                    require_bool(&e, "NOT")?;
                    Ok(Expr::Not(Box::new(e)))
                }
                sql::UnaryOperator::Minus => {
                    let e = self.bind(inner)?;
                    match e {
                        Expr::Literal(Value::Int64(i)) => Ok(Expr::Literal(Value::Int64(
                            i.checked_neg()
                                .ok_or_else(|| invalid("integer literal overflow"))?,
                        ))),
                        Expr::Literal(Value::Float64(f)) => Ok(Expr::Literal(Value::Float64(-f))),
                        other => {
                            require_numeric(&other, "unary -")?;
                            Ok(Expr::Negate(Box::new(other)))
                        }
                    }
                }
                sql::UnaryOperator::Plus => {
                    let e = self.bind(inner)?;
                    require_numeric(&e, "unary +")?;
                    Ok(e)
                }
                other => Err(unsupported(format!("unary operator {other} not supported"))),
            },
            SqlExpr::BinaryOp { left, op, right } => {
                if let SqlExpr::Interval(interval) = right.as_ref() {
                    let negated = match op {
                        sql::BinaryOperator::Plus => false,
                        sql::BinaryOperator::Minus => true,
                        _ => {
                            return Err(invalid("calendar intervals may only be used with + or -"))
                        }
                    };
                    let timestamp = self.bind(left)?;
                    require_timestamp(&timestamp, "calendar interval")?;
                    let (quantity, unit) = bind_calendar_interval(interval)?;
                    return Ok(Expr::CalendarInterval {
                        expr: Box::new(timestamp),
                        quantity: Box::new(quantity),
                        unit,
                        negated,
                    });
                }
                if let SqlExpr::Interval(interval) = left.as_ref() {
                    if !matches!(op, sql::BinaryOperator::Plus) {
                        return Err(invalid(
                            "calendar intervals may only be used with timestamp + INTERVAL",
                        ));
                    }
                    let timestamp = self.bind(right)?;
                    require_timestamp(&timestamp, "calendar interval")?;
                    let (quantity, unit) = bind_calendar_interval(interval)?;
                    return Ok(Expr::CalendarInterval {
                        expr: Box::new(timestamp),
                        quantity: Box::new(quantity),
                        unit,
                        negated: false,
                    });
                }

                let op = match op {
                    sql::BinaryOperator::Plus => BinOp::Add,
                    sql::BinaryOperator::Minus => BinOp::Sub,
                    sql::BinaryOperator::Multiply => BinOp::Mul,
                    sql::BinaryOperator::Divide => BinOp::Div,
                    sql::BinaryOperator::MyIntegerDivide => BinOp::IntDiv,
                    sql::BinaryOperator::Modulo => BinOp::Mod,
                    sql::BinaryOperator::Eq => BinOp::Eq,
                    sql::BinaryOperator::NotEq => BinOp::NotEq,
                    sql::BinaryOperator::Lt => BinOp::Lt,
                    sql::BinaryOperator::LtEq => BinOp::Lte,
                    sql::BinaryOperator::Gt => BinOp::Gt,
                    sql::BinaryOperator::GtEq => BinOp::Gte,
                    sql::BinaryOperator::And => BinOp::And,
                    sql::BinaryOperator::Or => BinOp::Or,
                    other => {
                        return Err(unsupported(format!(
                            "binary operator {other} not supported"
                        )))
                    }
                };
                let l = self.bind(left)?;
                let r = self.bind(right)?;
                let (l, r) = if op.is_comparison() {
                    let (l, r) = coerce_bool_literal_pair(l, r);
                    coerce_bytes_literal_pair(l, r)
                } else {
                    (l, r)
                };
                check_binary_types(op, &l, &r)?;
                Ok(Expr::BinaryOp {
                    op,
                    left: Box::new(l),
                    right: Box::new(r),
                })
            }
            SqlExpr::Extract {
                field,
                expr,
                syntax: _,
            } => {
                let unit = match field {
                    sql::DateTimeField::Year => CalendarIntervalUnit::Year,
                    sql::DateTimeField::Month => CalendarIntervalUnit::Month,
                    sql::DateTimeField::Day => CalendarIntervalUnit::Day,
                    other => {
                        return Err(unsupported(format!("EXTRACT field {other} not supported")))
                    }
                };
                let expr = self.bind(expr)?;
                require_timestamp(&expr, "EXTRACT")?;
                Ok(Expr::ScalarFunction {
                    func: ScalarFn::Extract(unit),
                    args: vec![expr],
                    data_type: DataType::Int64,
                    nullable: true,
                })
            }
            SqlExpr::Substring {
                expr,
                substring_from,
                substring_for,
                special,
                ..
            } => {
                let _ = special;
                let start = substring_from
                    .as_ref()
                    .ok_or_else(|| unsupported("SUBSTRING requires start and length arguments"))?;
                let length = substring_for
                    .as_ref()
                    .ok_or_else(|| unsupported("SUBSTRING requires start and length arguments"))?;
                let expr = self.bind(expr)?;
                let start = self.bind(start)?;
                let length = self.bind(length)?;
                require_string(&expr, "SUBSTRING")?;
                require_numeric(&start, "SUBSTRING start")?;
                require_numeric(&length, "SUBSTRING length")?;
                Ok(Expr::ScalarFunction {
                    func: ScalarFn::Substring,
                    args: vec![expr, start, length],
                    data_type: DataType::String,
                    nullable: true,
                })
            }
            SqlExpr::IsNull(inner) => Ok(Expr::IsNull(Box::new(self.bind(inner)?))),
            SqlExpr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(self.bind(inner)?))),
            SqlExpr::IsTrue(inner)
            | SqlExpr::IsNotTrue(inner)
            | SqlExpr::IsFalse(inner)
            | SqlExpr::IsNotFalse(inner) => {
                let e = self.bind(inner)?;
                require_bool(&e, "IS TRUE/FALSE")?;
                let (target, negated) = match expr {
                    SqlExpr::IsTrue(_) => (true, false),
                    SqlExpr::IsNotTrue(_) => (true, true),
                    SqlExpr::IsFalse(_) => (false, false),
                    _ => (false, true),
                };
                // x IS TRUE  == COALESCE(x, false) = true ; x IS NOT TRUE == NOT (x IS TRUE)
                let coalesced = Expr::ScalarFunction {
                    func: ScalarFn::Coalesce,
                    args: vec![e, Expr::Literal(Value::Bool(!target))],
                    data_type: DataType::Bool,
                    nullable: false,
                };
                let test = Expr::BinaryOp {
                    op: BinOp::Eq,
                    left: Box::new(coalesced),
                    right: Box::new(Expr::Literal(Value::Bool(target))),
                };
                Ok(if negated {
                    Expr::Not(Box::new(test))
                } else {
                    test
                })
            }
            SqlExpr::Like {
                negated,
                any,
                expr: inner,
                pattern,
                escape_char,
            } => {
                if *any {
                    return Err(unsupported("LIKE ANY not supported"));
                }
                if escape_char.is_some() {
                    return Err(unsupported("LIKE ... ESCAPE not supported"));
                }
                let e = self.bind(inner)?;
                let p = self.bind(pattern)?;
                for (x, what) in [(&e, "LIKE operand"), (&p, "LIKE pattern")] {
                    let t = x.expr_type();
                    if t.data_type != DataType::String && !t.is_permissive() {
                        return Err(invalid(format!(
                            "{what} must be a string, found {}",
                            t.data_type.name()
                        )));
                    }
                }
                Ok(Expr::Like {
                    expr: Box::new(e),
                    pattern: Box::new(p),
                    negated: *negated,
                })
            }
            SqlExpr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let e = self.bind(inner)?;
                let mut items = Vec::with_capacity(list.len());
                for item in list {
                    let b = self.bind(item)?;
                    check_comparable(&e, &b, "IN")?;
                    items.push(b);
                }
                Ok(Expr::In {
                    expr: Box::new(e),
                    list: items,
                    negated: *negated,
                })
            }
            SqlExpr::Between {
                expr: inner,
                negated,
                low,
                high,
            } => {
                let e = self.bind(inner)?;
                let l = self.bind(low)?;
                let h = self.bind(high)?;
                check_comparable(&e, &l, "BETWEEN")?;
                check_comparable(&e, &h, "BETWEEN")?;
                Ok(Expr::Between {
                    expr: Box::new(e),
                    low: Box::new(l),
                    high: Box::new(h),
                    negated: *negated,
                })
            }
            SqlExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let operand_b = match operand {
                    Some(o) => Some(Box::new(self.bind(o)?)),
                    None => None,
                };
                let mut branches = Vec::with_capacity(conditions.len());
                let mut results: Vec<Expr> = Vec::new();
                for when in conditions {
                    let c = self.bind(&when.condition)?;
                    match &operand_b {
                        Some(o) => check_comparable(o, &c, "CASE WHEN")?,
                        None => require_bool(&c, "CASE WHEN")?,
                    }
                    let r = self.bind(&when.result)?;
                    results.push(r.clone());
                    branches.push((c, r));
                }
                let else_b = match else_result {
                    Some(e) => {
                        let b = self.bind(e)?;
                        results.push(b.clone());
                        Some(Box::new(b))
                    }
                    None => None,
                };
                let (data_type, mut nullable) = common_type(&results, "CASE")?;
                if else_b.is_none() {
                    nullable = true;
                }
                Ok(Expr::Case {
                    operand: operand_b,
                    branches,
                    else_result: else_b,
                    data_type,
                    nullable,
                })
            }
            SqlExpr::Cast {
                kind,
                expr: inner,
                data_type,
                ..
            } => {
                if !matches!(kind, sql::CastKind::Cast) {
                    return Err(unsupported("only CAST(... AS ...) is supported"));
                }
                let e = self.bind(inner)?;
                let to = map_cast_type(data_type)?;
                if let Expr::Literal(v) = &e {
                    return Ok(Expr::Literal(cast_value(v.clone(), to)?));
                }
                Ok(Expr::Cast {
                    expr: Box::new(e),
                    to,
                })
            }
            SqlExpr::Function(func) => self.bind_function(func),
            SqlExpr::Subquery(q) => {
                let idx = self.bind_subquery(q, "scalar subquery")?;
                let cols = &self.subqueries[idx].output_columns;
                if cols.len() != 1 {
                    return Err(invalid(format!(
                        "scalar subquery must return exactly one column, found {}",
                        cols.len()
                    )));
                }
                Ok(Expr::ScalarSubquery {
                    index: idx,
                    data_type: cols[0].data_type,
                    correlated: self.subqueries[idx].correlated,
                })
            }
            SqlExpr::InSubquery {
                expr: inner,
                subquery,
                negated,
            } => {
                let e = self.bind(inner)?;
                let idx = self.bind_subquery(subquery, "IN subquery")?;
                let cols = &self.subqueries[idx].output_columns;
                if cols.len() != 1 {
                    return Err(invalid(format!(
                        "IN subquery must return exactly one column, found {}",
                        cols.len()
                    )));
                }
                let probe = Expr::ColumnRef {
                    slot: 0,
                    column: 0,
                    offset: 0,
                    name: cols[0].name.clone(),
                    data_type: cols[0].data_type,
                    nullable: true,
                };
                check_comparable(&e, &probe, "IN")?;
                Ok(Expr::InSubquery {
                    expr: Box::new(e),
                    index: idx,
                    negated: *negated,
                    correlated: self.subqueries[idx].correlated,
                })
            }
            SqlExpr::Exists { subquery, negated } => {
                let idx = self.bind_subquery(subquery, "EXISTS subquery")?;
                Ok(Expr::Exists {
                    index: idx,
                    negated: *negated,
                    correlated: self.subqueries[idx].correlated,
                })
            }
            SqlExpr::Tuple(_) => Err(unsupported("row/tuple expressions not supported")),
            other => Err(unsupported(format!("expression not supported: {other}"))),
        }
    }

    fn bind_subquery(&mut self, q: &Query, what: &str) -> Result<usize> {
        if !self.allow_subqueries {
            return Err(unsupported(format!("{what} not supported here")));
        }
        // Preserve ancestor slots after this query's slots so the child can distinguish
        // immediate correlation from unsupported references beyond one level.
        let mut outer = Vec::with_capacity(self.slots.len() + self.outer.len());
        outer.extend_from_slice(self.slots);
        outer.extend_from_slice(self.outer);
        let bound = bind_query_scoped(q, self.catalog, self.ctes, &outer, self.slots.len())?;
        self.subqueries.push(bound);
        Ok(self.subqueries.len() - 1)
    }

    fn bind_function(&mut self, func: &sql::Function) -> Result<Expr> {
        let name = object_name_single(&func.name)?.to_ascii_uppercase();
        if func.over.is_some() {
            return self.bind_window_function(func, &name);
        }
        if func.filter.is_some() {
            return Err(unsupported("aggregate FILTER clause not supported"));
        }
        if func.null_treatment.is_some() || !func.within_group.is_empty() || func.uses_odbc_syntax {
            return Err(unsupported("function modifiers not supported"));
        }
        if !matches!(func.parameters, FunctionArguments::None) {
            return Err(unsupported("parameterized functions not supported"));
        }
        let list = match &func.args {
            FunctionArguments::List(list) => list,
            FunctionArguments::None => {
                return Err(invalid(format!("function '{name}' requires arguments")))
            }
            FunctionArguments::Subquery(_) => {
                return Err(unsupported(
                    "subqueries in function arguments not supported",
                ))
            }
        };
        if !list.clauses.is_empty() {
            return Err(unsupported("clauses in function arguments not supported"));
        }
        let distinct = list.duplicate_treatment == Some(sql::DuplicateTreatment::Distinct);

        let agg = match name.as_str() {
            "COUNT" => Some(AggFn::Count),
            "SUM" => Some(AggFn::Sum),
            "AVG" => Some(AggFn::Avg),
            "MIN" => Some(AggFn::Min),
            "MAX" => Some(AggFn::Max),
            _ => None,
        };
        if let Some(agg) = agg {
            return self.bind_aggregate(agg, distinct, list, &name);
        }
        if distinct {
            return Err(invalid(format!(
                "DISTINCT is only valid in aggregate functions, not '{name}'"
            )));
        }
        let mut args = Vec::with_capacity(list.args.len());
        for arg in &list.args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => args.push(self.bind(e)?),
                _ => {
                    return Err(unsupported(format!(
                        "argument form not supported in '{name}'"
                    )))
                }
            }
        }
        let arity = |n: usize| -> Result<()> {
            if args.len() != n {
                Err(invalid(format!(
                    "{name} requires exactly {n} argument(s), found {}",
                    args.len()
                )))
            } else {
                Ok(())
            }
        };
        let (func, data_type, nullable) = match name.as_str() {
            "UPPER" | "UCASE" | "LOWER" | "LCASE" => {
                arity(1)?;
                require_string(&args[0], &name)?;
                let f = if name.starts_with('U') {
                    ScalarFn::Upper
                } else {
                    ScalarFn::Lower
                };
                (f, DataType::String, args[0].expr_type().nullable)
            }
            "LENGTH" | "OCTET_LENGTH" => {
                arity(1)?;
                let t = args[0].expr_type();
                if !matches!(t.data_type, DataType::String | DataType::Bytes) && !t.is_permissive()
                {
                    return Err(invalid(format!("{name} requires a string argument")));
                }
                (ScalarFn::Length, DataType::Int64, t.nullable)
            }
            "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
                arity(1)?;
                require_string(&args[0], &name)?;
                (
                    ScalarFn::CharLength,
                    DataType::Int64,
                    args[0].expr_type().nullable,
                )
            }
            "CONCAT" => {
                if args.is_empty() {
                    return Err(invalid("CONCAT requires at least one argument"));
                }
                let nullable = args.iter().any(|a| a.expr_type().nullable);
                (ScalarFn::Concat, DataType::String, nullable)
            }
            "ABS" => {
                arity(1)?;
                require_numeric(&args[0], &name)?;
                let t = args[0].expr_type();
                (ScalarFn::Abs, t.data_type, t.nullable)
            }
            "COALESCE" => {
                if args.is_empty() {
                    return Err(invalid("COALESCE requires at least one argument"));
                }
                let (dt, _) = common_type(&args, "COALESCE")?;
                let nullable = args.iter().all(|a| a.expr_type().nullable);
                (ScalarFn::Coalesce, dt, nullable)
            }
            "IFNULL" => {
                arity(2)?;
                let (dt, _) = common_type(&args, "IFNULL")?;
                (ScalarFn::IfNull, dt, args[1].expr_type().nullable)
            }
            "NULLIF" => {
                arity(2)?;
                check_comparable(&args[0], &args[1], "NULLIF")?;
                let t = args[0].expr_type();
                (ScalarFn::NullIf, t.data_type, true)
            }
            other => return Err(unsupported(format!("function '{other}' not supported"))),
        };
        Ok(Expr::ScalarFunction {
            func,
            args,
            data_type,
            nullable,
        })
    }

    fn bind_window_expr(&mut self, expr: &SqlExpr) -> Result<Expr> {
        let previous = self.in_window;
        self.in_window = true;
        let result = self.bind(expr);
        self.in_window = previous;
        result
    }

    fn bind_window_function(&mut self, func: &sql::Function, name: &str) -> Result<Expr> {
        if let Some(clause) = self.window_clause {
            return Err(invalid(format!(
                "window functions are not allowed in {clause}"
            )));
        }
        if self.in_window {
            return Err(invalid("window functions cannot be nested"));
        }
        if self.in_aggregate {
            return Err(invalid(
                "window functions cannot be used inside aggregate arguments",
            ));
        }
        if func.filter.is_some() {
            return Err(unsupported("FILTER is not supported for window functions"));
        }
        if func.null_treatment.is_some() {
            return Err(unsupported(
                "IGNORE NULLS / RESPECT NULLS is not supported for window functions",
            ));
        }
        if !func.within_group.is_empty() || func.uses_odbc_syntax {
            return Err(unsupported("function modifiers not supported"));
        }
        if !matches!(func.parameters, FunctionArguments::None) {
            return Err(unsupported("parameterized window functions not supported"));
        }

        let over = func.over.as_ref().expect("window function has OVER");
        let window = match over {
            WindowType::WindowSpec(spec) => spec,
            WindowType::NamedWindow(_) => {
                return Err(unsupported("named windows are not supported"))
            }
        };
        if window.window_name.is_some() {
            return Err(unsupported("named windows are not supported"));
        }

        let list = match &func.args {
            FunctionArguments::List(list) => list,
            FunctionArguments::None => {
                return Err(invalid(format!("function '{name}' requires parentheses")))
            }
            FunctionArguments::Subquery(_) => {
                return Err(unsupported(
                    "subqueries in window function arguments are not supported",
                ))
            }
        };
        if list.duplicate_treatment.is_some() {
            return Err(unsupported(
                "DISTINCT / ALL is not supported for ranking and offset window functions",
            ));
        }
        if !list.clauses.is_empty() {
            return Err(unsupported(
                "clauses in window function arguments are not supported",
            ));
        }

        // COUNT handles both COUNT(*) and COUNT(expr) in its dispatch arm below.
        // Skip generic extraction because it intentionally rejects wildcard arguments.
        let raw_args = if name == "COUNT" {
            Vec::new()
        } else {
            list.args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
                    _ => Err(unsupported(format!(
                        "argument form not supported in '{name}'"
                    ))),
                })
                .collect::<Result<Vec<_>>>()?
        };

        if matches!(
            name,
            "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" | "LAG" | "LEAD"
        ) && window.window_frame.is_some()
        {
            return Err(unsupported(format!(
                "explicit window frames are not supported for {name}"
            )));
        }

        let mut args = Vec::new();
        let (kind, data_type, nullable) = match name {
            "ROW_NUMBER" | "RANK" | "DENSE_RANK" => {
                if !raw_args.is_empty() {
                    return Err(invalid(format!(
                        "{name} requires exactly 0 arguments, found {}",
                        raw_args.len()
                    )));
                }
                let kind = match name {
                    "ROW_NUMBER" => WindowFunctionKind::RowNumber,
                    "RANK" => WindowFunctionKind::Rank,
                    _ => WindowFunctionKind::DenseRank,
                };
                (kind, DataType::Int64, false)
            }
            "NTILE" => {
                if raw_args.len() != 1 {
                    return Err(invalid(format!(
                        "NTILE requires exactly 1 argument, found {}",
                        raw_args.len()
                    )));
                }
                let value = match raw_args[0] {
                    SqlExpr::Value(value) => match &value.value {
                        sql::Value::Number(number, _) => number.parse::<u64>().ok(),
                        _ => None,
                    },
                    _ => None,
                }
                .filter(|value| *value > 0)
                .ok_or_else(|| invalid("NTILE argument must be a positive integer literal"))?;
                let value = i64::try_from(value)
                    .map_err(|_| invalid("NTILE argument must be a positive integer literal"))?;
                args.push(Expr::Literal(Value::Int64(value)));
                (WindowFunctionKind::Ntile, DataType::Int64, false)
            }
            "LAG" | "LEAD" => {
                if !(1..=3).contains(&raw_args.len()) {
                    return Err(invalid(format!(
                        "{name} requires 1 to 3 arguments, found {}",
                        raw_args.len()
                    )));
                }
                let value = self.bind_window_expr(raw_args[0])?;
                let offset = if let Some(offset) = raw_args.get(1) {
                    match offset {
                        SqlExpr::Value(value) => match &value.value {
                            sql::Value::Number(number, _)
                                if number.chars().all(|c| c.is_ascii_digit()) =>
                            {
                                number.parse::<u64>().ok()
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                    .ok_or_else(|| {
                        invalid(format!(
                            "{name} offset must be a non-negative integer literal"
                        ))
                    })?
                } else {
                    1
                };
                let offset = i64::try_from(offset).map_err(|_| {
                    invalid(format!(
                        "{name} offset must be a non-negative integer literal"
                    ))
                })?;
                let default = match raw_args.get(2) {
                    Some(default) => {
                        let default = self.bind_window_expr(default)?;
                        let value_type = value.expr_type();
                        let default_type = default.expr_type();
                        if !value_type.is_permissive()
                            && !default_type.is_permissive()
                            && union_type(value_type.data_type, default_type.data_type).is_none()
                        {
                            return Err(invalid(format!(
                                "{name} default has incompatible type {}; expected {}",
                                default_type.data_type.name(),
                                value_type.data_type.name()
                            )));
                        }
                        default
                    }
                    None => Expr::null(),
                };
                let value_type = value.expr_type();
                args.push(value);
                args.push(Expr::Literal(Value::Int64(offset)));
                args.push(default);
                (
                    if name == "LAG" {
                        WindowFunctionKind::Lag
                    } else {
                        WindowFunctionKind::Lead
                    },
                    value_type.data_type,
                    true,
                )
            }
            "COUNT" => match list.args.as_slice() {
                [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)] => {
                    (WindowFunctionKind::CountStar, DataType::Int64, false)
                }
                [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] => {
                    args.push(self.bind_window_expr(expr)?);
                    (WindowFunctionKind::Count, DataType::Int64, false)
                }
                args => {
                    return Err(invalid(format!(
                        "COUNT requires exactly 1 argument, found {}",
                        args.len()
                    )))
                }
            },
            "SUM" | "AVG" | "MIN" | "MAX" => {
                if raw_args.len() != 1 {
                    return Err(invalid(format!(
                        "{name} requires exactly 1 argument, found {}",
                        raw_args.len()
                    )));
                }
                let arg = self.bind_window_expr(raw_args[0])?;
                let arg_type = arg.expr_type();

                let (kind, data_type) = match name {
                    "SUM" => {
                        require_numeric(&arg, name)?;
                        let data_type = match arg_type.data_type {
                            DataType::Float64 => DataType::Float64,
                            DataType::Decimal { scale, .. } => DataType::Decimal {
                                precision: htap_common::types::MAX_DECIMAL_PRECISION,
                                scale,
                            },
                            _ => DataType::Int64,
                        };
                        (WindowFunctionKind::Sum, data_type)
                    }
                    "AVG" => {
                        require_numeric(&arg, name)?;
                        let data_type = match arg_type.data_type {
                            DataType::Decimal { scale, .. } => DataType::Decimal {
                                precision: htap_common::types::MAX_DECIMAL_PRECISION,
                                scale: scale
                                    .saturating_add(4)
                                    .min(htap_common::types::MAX_DECIMAL_PRECISION),
                            },
                            _ => DataType::Float64,
                        };
                        (WindowFunctionKind::Avg, data_type)
                    }
                    "MIN" => (WindowFunctionKind::Min, arg_type.data_type),
                    "MAX" => (WindowFunctionKind::Max, arg_type.data_type),
                    _ => unreachable!(),
                };

                args.push(arg);
                (kind, data_type, true)
            }
            "FIRST_VALUE" | "LAST_VALUE" => {
                if raw_args.len() != 1 {
                    return Err(invalid(format!(
                        "{name} requires exactly 1 argument, found {}",
                        raw_args.len()
                    )));
                }
                let arg = self.bind_window_expr(raw_args[0])?;
                let arg_type = arg.expr_type();
                args.push(arg);
                (
                    if name == "FIRST_VALUE" {
                        WindowFunctionKind::FirstValue
                    } else {
                        WindowFunctionKind::LastValue
                    },
                    arg_type.data_type,
                    true,
                )
            }
            _ => {
                return Err(unsupported(format!(
                    "window function '{name}' not supported"
                )))
            }
        };

        let mut partition_by = Vec::with_capacity(window.partition_by.len());
        for expr in &window.partition_by {
            partition_by.push(self.bind_window_expr(expr)?);
        }

        let mut order_by = Vec::with_capacity(window.order_by.len());
        for item in &window.order_by {
            if item.with_fill.is_some() {
                return Err(unsupported("WITH FILL is not supported in window ORDER BY"));
            }
            let asc = item.options.asc.unwrap_or(true);
            order_by.push(OrderItem {
                expr: self.bind_window_expr(&item.expr)?,
                asc,
                nulls_first: item.options.nulls_first.unwrap_or(asc),
            });
        }

        let frame = if matches!(
            name,
            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "FIRST_VALUE" | "LAST_VALUE"
        ) {
            self.bind_window_frame(window.window_frame.as_ref(), &order_by)?
        } else {
            WindowFrame::None
        };

        let windows = self
            .windows
            .as_deref_mut()
            .ok_or_else(|| invalid("window functions are only allowed in the select list"))?;
        let index = windows.len();
        windows.push(WindowSpec {
            func: kind,
            args,
            partition_by,
            order_by,
            frame,
            data_type,
            nullable,
        });
        Ok(Expr::WindowRef {
            index,
            data_type,
            nullable,
        })
    }

    fn bind_window_frame(
        &mut self,
        frame: Option<&sql::WindowFrame>,
        order_by: &[OrderItem],
    ) -> Result<WindowFrame> {
        let Some(frame) = frame else {
            return Ok(if order_by.is_empty() {
                WindowFrame::None
            } else {
                WindowFrame::PeerRange {
                    start: PeerFrameBound::Unbounded(WindowFrameDirection::Preceding),
                    end: PeerFrameBound::CurrentRow,
                }
            });
        };

        let end_bound = frame
            .end_bound
            .as_ref()
            .unwrap_or(&sql::WindowFrameBound::CurrentRow);

        match frame.units {
            sql::WindowFrameUnits::Rows => {
                let start = self.bind_row_frame_bound(&frame.start_bound)?;
                let end = self.bind_row_frame_bound(end_bound)?;
                validate_row_frame_bounds(&start, &end)?;
                Ok(WindowFrame::Rows { start, end })
            }
            sql::WindowFrameUnits::Range => {
                let has_offset = window_bound_has_offset(&frame.start_bound)
                    || window_bound_has_offset(end_bound);

                if !has_offset {
                    let start = bind_peer_frame_bound(&frame.start_bound)?;
                    let end = bind_peer_frame_bound(end_bound)?;
                    validate_peer_frame_bounds(start, end)?;
                    return Ok(WindowFrame::PeerRange { start, end });
                }

                if order_by.len() != 1 {
                    return Err(invalid(
                        "RANGE frames with offsets require exactly one ORDER BY expression",
                    ));
                }
                let order_type = order_by[0].expr.expr_type();
                if !is_numeric_type(&order_type) {
                    return Err(invalid(format!(
                        "RANGE frames with offsets require a numeric or timestamp ORDER BY expression, found {}",
                        order_type.data_type.name()
                    )));
                }

                let start = self.bind_value_frame_bound(&frame.start_bound)?;
                let end = self.bind_value_frame_bound(end_bound)?;
                validate_value_frame_bounds(&start, &end)?;
                Ok(WindowFrame::ValueRange { start, end })
            }
            sql::WindowFrameUnits::Groups => {
                Err(unsupported("GROUPS window frames are not supported"))
            }
        }
    }

    fn bind_row_frame_bound(&mut self, bound: &sql::WindowFrameBound) -> Result<RowFrameBound> {
        match bound {
            sql::WindowFrameBound::CurrentRow => Ok(RowFrameBound::CurrentRow),
            sql::WindowFrameBound::Preceding(None) => {
                Ok(RowFrameBound::Unbounded(WindowFrameDirection::Preceding))
            }
            sql::WindowFrameBound::Following(None) => {
                Ok(RowFrameBound::Unbounded(WindowFrameDirection::Following))
            }
            sql::WindowFrameBound::Preceding(Some(value)) => Ok(RowFrameBound::Offset {
                value: window_frame_u64(value, "ROWS frame offset")?,
                direction: WindowFrameDirection::Preceding,
            }),
            sql::WindowFrameBound::Following(Some(value)) => Ok(RowFrameBound::Offset {
                value: window_frame_u64(value, "ROWS frame offset")?,
                direction: WindowFrameDirection::Following,
            }),
        }
    }

    fn bind_value_frame_bound(&mut self, bound: &sql::WindowFrameBound) -> Result<ValueFrameBound> {
        match bound {
            sql::WindowFrameBound::CurrentRow => Ok(ValueFrameBound::CurrentRow),
            sql::WindowFrameBound::Preceding(None) => {
                Ok(ValueFrameBound::Unbounded(WindowFrameDirection::Preceding))
            }
            sql::WindowFrameBound::Following(None) => {
                Ok(ValueFrameBound::Unbounded(WindowFrameDirection::Following))
            }
            sql::WindowFrameBound::Preceding(Some(value)) => Ok(ValueFrameBound::Offset {
                value: self.bind_window_range_offset(value)?,
                direction: WindowFrameDirection::Preceding,
            }),
            sql::WindowFrameBound::Following(Some(value)) => Ok(ValueFrameBound::Offset {
                value: self.bind_window_range_offset(value)?,
                direction: WindowFrameDirection::Following,
            }),
        }
    }

    fn bind_window_range_offset(&mut self, expr: &SqlExpr) -> Result<Expr> {
        let bound = self.bind_window_expr(expr)?;
        match &bound {
            Expr::Literal(Value::Int32(value)) if *value >= 0 => Ok(bound),
            Expr::Literal(Value::Int64(value)) if *value >= 0 => Ok(bound),
            Expr::Literal(Value::Float64(value)) if value.is_finite() && *value >= 0.0 => Ok(bound),
            _ => Err(invalid(
                "RANGE frame offset must be a non-negative numeric literal",
            )),
        }
    }

    fn bind_aggregate(
        &mut self,
        func: AggFn,
        distinct: bool,
        list: &sql::FunctionArgumentList,
        name: &str,
    ) -> Result<Expr> {
        if !self.allow_aggregates {
            return Err(invalid(format!(
                "aggregate function {name} is not allowed in this clause"
            )));
        }
        if self.in_aggregate {
            return Err(invalid("aggregate functions cannot be nested"));
        }
        if list.args.len() != 1 {
            return Err(invalid(format!(
                "{name} requires exactly 1 argument, found {}",
                list.args.len()
            )));
        }
        let (func, arg, display) = match &list.args[0] {
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
                if func != AggFn::Count {
                    return Err(invalid(format!("{name}(*) is not valid")));
                }
                if distinct {
                    return Err(invalid("COUNT(DISTINCT *) is not valid"));
                }
                (AggFn::CountStar, None, "COUNT(*)".to_string())
            }
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                self.in_aggregate = true;
                let bound = self.bind(e);
                self.in_aggregate = false;
                let bound = bound?;
                let display = format!("{name}({}{e})", if distinct { "DISTINCT " } else { "" });
                (func, Some(bound), display)
            }
            _ => {
                return Err(unsupported(format!(
                    "argument form not supported in '{name}'"
                )))
            }
        };
        let (data_type, nullable) = match (func, &arg) {
            (AggFn::CountStar, _) | (AggFn::Count, _) => (DataType::Int64, false),
            (AggFn::Sum, Some(a)) => {
                let t = a.expr_type();
                require_numeric(a, name)?;
                (
                    match t.data_type {
                        DataType::Float64 => DataType::Float64,
                        DataType::Decimal { scale, .. } => DataType::Decimal {
                            precision: htap_common::types::MAX_DECIMAL_PRECISION,
                            scale,
                        },
                        _ => DataType::Int64,
                    },
                    true,
                )
            }
            (AggFn::Avg, Some(a)) => {
                require_numeric(a, name)?;
                (
                    match a.expr_type().data_type {
                        DataType::Decimal { scale, .. } => DataType::Decimal {
                            precision: htap_common::types::MAX_DECIMAL_PRECISION,
                            // AVG adds four fractional digits, capped by the maximum precision.
                            scale: scale
                                .saturating_add(4)
                                .min(htap_common::types::MAX_DECIMAL_PRECISION),
                        },
                        _ => DataType::Float64,
                    },
                    true,
                )
            }
            (AggFn::Min | AggFn::Max, Some(a)) => (a.expr_type().data_type, true),
            _ => unreachable!(),
        };
        let spec = AggregateSpec {
            func,
            distinct,
            arg,
            data_type,
            nullable,
            name: display,
        };
        let index = match self.aggregates.iter().position(|s| *s == spec) {
            Some(i) => i,
            None => {
                self.aggregates.push(spec);
                self.aggregates.len() - 1
            }
        };
        Ok(Expr::AggregateRef {
            index,
            data_type,
            nullable,
        })
    }
}

fn window_bound_has_offset(bound: &sql::WindowFrameBound) -> bool {
    matches!(
        bound,
        sql::WindowFrameBound::Preceding(Some(_)) | sql::WindowFrameBound::Following(Some(_))
    )
}

fn window_frame_u64(expr: &SqlExpr, what: &str) -> Result<u64> {
    match expr {
        SqlExpr::Value(value) => match &value.value {
            sql::Value::Number(number, _)
                if number.chars().all(|character| character.is_ascii_digit()) =>
            {
                number
                    .parse::<u64>()
                    .map_err(|_| invalid(format!("{what} is too large: {number}")))
            }
            _ => Err(invalid(format!(
                "{what} must be a non-negative integer literal"
            ))),
        },
        _ => Err(invalid(format!(
            "{what} must be a non-negative integer literal"
        ))),
    }
}

fn bind_peer_frame_bound(bound: &sql::WindowFrameBound) -> Result<PeerFrameBound> {
    match bound {
        sql::WindowFrameBound::CurrentRow => Ok(PeerFrameBound::CurrentRow),
        sql::WindowFrameBound::Preceding(None) => {
            Ok(PeerFrameBound::Unbounded(WindowFrameDirection::Preceding))
        }
        sql::WindowFrameBound::Following(None) => {
            Ok(PeerFrameBound::Unbounded(WindowFrameDirection::Following))
        }
        sql::WindowFrameBound::Preceding(Some(_)) | sql::WindowFrameBound::Following(Some(_)) => {
            Err(invalid("RANGE frame offsets require a value-based frame"))
        }
    }
}

fn row_frame_bound_position(bound: &RowFrameBound) -> i128 {
    match bound {
        RowFrameBound::Unbounded(WindowFrameDirection::Preceding) => i128::MIN,
        RowFrameBound::Offset {
            value,
            direction: WindowFrameDirection::Preceding,
        } => -i128::from(*value),
        RowFrameBound::CurrentRow => 0,
        RowFrameBound::Offset {
            value,
            direction: WindowFrameDirection::Following,
        } => i128::from(*value),
        RowFrameBound::Unbounded(WindowFrameDirection::Following) => i128::MAX,
    }
}

fn validate_row_frame_bounds(start: &RowFrameBound, end: &RowFrameBound) -> Result<()> {
    if matches!(
        start,
        RowFrameBound::Unbounded(WindowFrameDirection::Following)
    ) {
        return Err(invalid("window frame start cannot be UNBOUNDED FOLLOWING"));
    }
    if matches!(
        end,
        RowFrameBound::Unbounded(WindowFrameDirection::Preceding)
    ) {
        return Err(invalid("window frame end cannot be UNBOUNDED PRECEDING"));
    }
    if row_frame_bound_position(start) > row_frame_bound_position(end) {
        return Err(invalid(
            "window frame start cannot be after window frame end",
        ));
    }
    Ok(())
}

fn peer_frame_bound_position(bound: PeerFrameBound) -> i8 {
    match bound {
        PeerFrameBound::Unbounded(WindowFrameDirection::Preceding) => -1,
        PeerFrameBound::CurrentRow => 0,
        PeerFrameBound::Unbounded(WindowFrameDirection::Following) => 1,
    }
}

fn validate_peer_frame_bounds(start: PeerFrameBound, end: PeerFrameBound) -> Result<()> {
    if matches!(
        start,
        PeerFrameBound::Unbounded(WindowFrameDirection::Following)
    ) {
        return Err(invalid("window frame start cannot be UNBOUNDED FOLLOWING"));
    }
    if matches!(
        end,
        PeerFrameBound::Unbounded(WindowFrameDirection::Preceding)
    ) {
        return Err(invalid("window frame end cannot be UNBOUNDED PRECEDING"));
    }
    if peer_frame_bound_position(start) > peer_frame_bound_position(end) {
        return Err(invalid(
            "window frame start cannot be after window frame end",
        ));
    }
    Ok(())
}

fn value_frame_bound_position(bound: &ValueFrameBound) -> Result<f64> {
    match bound {
        ValueFrameBound::Unbounded(WindowFrameDirection::Preceding) => Ok(f64::NEG_INFINITY),
        ValueFrameBound::Offset {
            value,
            direction: WindowFrameDirection::Preceding,
        } => Ok(-numeric_frame_offset(value)?),
        ValueFrameBound::CurrentRow => Ok(0.0),
        ValueFrameBound::Offset {
            value,
            direction: WindowFrameDirection::Following,
        } => Ok(numeric_frame_offset(value)?),
        ValueFrameBound::Unbounded(WindowFrameDirection::Following) => Ok(f64::INFINITY),
    }
}

fn numeric_frame_offset(expr: &Expr) -> Result<f64> {
    match expr {
        Expr::Literal(Value::Int32(value)) if *value >= 0 => Ok(f64::from(*value)),
        Expr::Literal(Value::Int64(value)) if *value >= 0 => Ok(*value as f64),
        Expr::Literal(Value::Float64(value)) if value.is_finite() && *value >= 0.0 => Ok(*value),
        _ => Err(invalid(
            "RANGE frame offset must be a non-negative numeric literal",
        )),
    }
}

fn validate_value_frame_bounds(start: &ValueFrameBound, end: &ValueFrameBound) -> Result<()> {
    if matches!(
        start,
        ValueFrameBound::Unbounded(WindowFrameDirection::Following)
    ) {
        return Err(invalid("window frame start cannot be UNBOUNDED FOLLOWING"));
    }
    if matches!(
        end,
        ValueFrameBound::Unbounded(WindowFrameDirection::Preceding)
    ) {
        return Err(invalid("window frame end cannot be UNBOUNDED PRECEDING"));
    }
    if value_frame_bound_position(start)? > value_frame_bound_position(end)? {
        return Err(invalid(
            "window frame start cannot be after window frame end",
        ));
    }
    Ok(())
}

fn bind_literal(v: &sql::Value) -> Result<Expr> {
    Ok(Expr::Literal(match v {
        sql::Value::Null => Value::Null,
        sql::Value::Boolean(b) => Value::Bool(*b),
        sql::Value::Number(n, _) => {
            // Exact-value literals with a decimal point retain their declared scale. Numeric
            // literals using exponent notation remain approximate DOUBLE values.
            if n.contains('.') && !n.contains(['e', 'E']) {
                let (whole, fraction) = n
                    .split_once('.')
                    .ok_or_else(|| invalid(format!("invalid numeric literal {n}")))?;
                if !whole.chars().all(|c| c.is_ascii_digit())
                    || !fraction.chars().all(|c| c.is_ascii_digit())
                {
                    return Err(invalid(format!("invalid numeric literal {n}")));
                }

                let precision = whole.len() + fraction.len();
                if precision == 0
                    || precision > usize::from(htap_common::types::MAX_DECIMAL_PRECISION)
                {
                    return Err(invalid(format!(
                        "DECIMAL literal {n} exceeds maximum precision {}",
                        htap_common::types::MAX_DECIMAL_PRECISION
                    )));
                }

                let scale = u8::try_from(fraction.len())
                    .map_err(|_| invalid(format!("invalid numeric literal {n}")))?;
                let unscaled = format!("{whole}{fraction}")
                    .parse::<i64>()
                    .map_err(|_| invalid(format!("invalid numeric literal {n}")))?;
                Value::Decimal {
                    value: unscaled,
                    precision: precision as u8,
                    scale,
                }
            } else if let Ok(i) = n.parse::<i64>() {
                Value::Int64(i)
            } else {
                let value = n
                    .parse::<f64>()
                    .map_err(|_| invalid(format!("invalid numeric literal {n}")))?;
                if !value.is_finite() {
                    return Err(invalid("DOUBLE value is out of range in literal"));
                }
                Value::Float64(value)
            }
        }
        sql::Value::SingleQuotedString(s)
        | sql::Value::DoubleQuotedString(s)
        | sql::Value::NationalStringLiteral(s)
        | sql::Value::EscapedStringLiteral(s) => Value::String(s.clone()),
        sql::Value::HexStringLiteral(s) => {
            Value::Bytes(crate::binder::parse_hex_bytes(s, "literal")?)
        }
        sql::Value::Placeholder(_) => return Err(invalid("placeholders not supported")),
        other => return Err(unsupported(format!("literal not supported: {other}"))),
    }))
}

fn map_cast_type(dt: &sql::DataType) -> Result<DataType> {
    use sql::DataType as D;
    Ok(match dt {
        D::Boolean | D::Bool => DataType::Bool,
        D::TinyInt(_)
        | D::SmallInt(_)
        | D::Int(_)
        | D::Integer(_)
        | D::MediumInt(_)
        | D::Int4(_) => DataType::Int32,
        D::BigInt(_)
        | D::Int8(_)
        | D::Signed
        | D::SignedInteger
        | D::Unsigned
        | D::UnsignedInteger => DataType::Int64,
        D::Float(_) | D::Float4 | D::Float8 | D::Real | D::Double(_) | D::DoublePrecision => {
            DataType::Float64
        }
        D::Decimal(info) | D::Numeric(info) | D::Dec(info) => {
            let (precision, scale) = match info {
                sql::ExactNumberInfo::None => (10, 0),
                sql::ExactNumberInfo::Precision(precision) => (*precision, 0),
                sql::ExactNumberInfo::PrecisionAndScale(precision, scale) => (*precision, *scale),
            };
            let precision = u8::try_from(precision)
                .map_err(|_| invalid(format!("invalid DECIMAL precision: {precision}")))?;
            let scale = u8::try_from(scale)
                .map_err(|_| invalid(format!("invalid DECIMAL scale: {scale}")))?;
            let data_type = DataType::Decimal { precision, scale };
            data_type.validate()?;
            data_type
        }
        D::Char(_)
        | D::Character(_)
        | D::Varchar(_)
        | D::CharVarying(_)
        | D::CharacterVarying(_)
        | D::Text
        | D::String(_)
        | D::Nvarchar(_) => DataType::String,
        D::Binary(_) | D::Varbinary(_) | D::Blob(_) | D::Bytes(_) => DataType::Bytes,
        D::Timestamp(_, _) | D::Datetime(_) => DataType::Timestamp,
        other => {
            return Err(unsupported(format!(
                "CAST target type not supported: {other}"
            )))
        }
    })
}

fn bind_calendar_interval(interval: &sql::Interval) -> Result<(Expr, CalendarIntervalUnit)> {
    let unit = match &interval.leading_field {
        Some(sql::DateTimeField::Year) => CalendarIntervalUnit::Year,
        Some(sql::DateTimeField::Month) => CalendarIntervalUnit::Month,
        Some(sql::DateTimeField::Day) => CalendarIntervalUnit::Day,
        Some(other) => return Err(unsupported(format!("INTERVAL unit {other} not supported"))),
        None => return Err(invalid("INTERVAL requires a YEAR, MONTH, or DAY unit")),
    };

    let value = match interval.value.as_ref() {
        SqlExpr::Value(value) => match &value.value {
            sql::Value::Number(value, _)
            | sql::Value::SingleQuotedString(value)
            | sql::Value::DoubleQuotedString(value) => value,
            _ => return Err(invalid("INTERVAL magnitude must be an integer literal")),
        },
        _ => return Err(invalid("INTERVAL magnitude must be an integer literal")),
    };
    let quantity = value
        .parse::<i64>()
        .map_err(|_| invalid("INTERVAL magnitude must be an integer literal"))?;

    Ok((Expr::Literal(Value::Int64(quantity)), unit))
}

fn is_numeric_type(t: &ExprType) -> bool {
    matches!(
        t.data_type,
        DataType::Int32
            | DataType::Int64
            | DataType::Float64
            | DataType::Decimal { .. }
            | DataType::Timestamp
    )
}

fn require_timestamp(e: &Expr, what: &str) -> Result<()> {
    let t = e.expr_type();
    if t.data_type != DataType::Timestamp && !t.is_permissive() {
        return Err(invalid(format!(
            "{what} requires a timestamp operand, found {}",
            t.data_type.name()
        )));
    }
    Ok(())
}

fn require_numeric(e: &Expr, what: &str) -> Result<()> {
    let t = e.expr_type();
    if !is_numeric_type(&t) && !t.is_permissive() {
        return Err(invalid(format!(
            "{what} requires a numeric operand, found {}",
            t.data_type.name()
        )));
    }
    Ok(())
}

fn require_string(e: &Expr, what: &str) -> Result<()> {
    let t = e.expr_type();
    if t.data_type != DataType::String && !t.is_permissive() {
        return Err(invalid(format!(
            "{what} requires a string argument, found {}",
            t.data_type.name()
        )));
    }
    Ok(())
}

fn check_comparable(l: &Expr, r: &Expr, what: &str) -> Result<()> {
    let lt = l.expr_type();
    let rt = r.expr_type();
    if lt.is_permissive() || rt.is_permissive() {
        return Ok(());
    }
    if lt.data_type == rt.data_type || (is_numeric_type(&lt) && is_numeric_type(&rt)) {
        return Ok(());
    }
    Err(invalid(format!(
        "{what}: cannot compare {} with {}",
        lt.data_type.name(),
        rt.data_type.name()
    )))
}

/// Narrow coercion: if exactly one side of a comparison is BOOL-typed and the other is the
/// integer literal 0 or 1, rewrite that literal to the matching BOOL literal so the comparison
/// is type-homogeneous (MySQL clients commonly send BOOL as TINYINT(1), e.g. `boolcol = 1`).
fn coerce_bool_literal_pair(l: Expr, r: Expr) -> (Expr, Expr) {
    let l_is_bool = l.expr_type().data_type == DataType::Bool;
    let r_is_bool = r.expr_type().data_type == DataType::Bool;
    let l = if r_is_bool {
        coerce_int_literal_to_bool(l)
    } else {
        l
    };
    let r = if l_is_bool {
        coerce_int_literal_to_bool(r)
    } else {
        r
    };
    (l, r)
}

fn coerce_int_literal_to_bool(expr: Expr) -> Expr {
    match &expr {
        Expr::Literal(Value::Int32(0)) | Expr::Literal(Value::Int64(0)) => {
            Expr::Literal(Value::Bool(false))
        }
        Expr::Literal(Value::Int32(1)) | Expr::Literal(Value::Int64(1)) => {
            Expr::Literal(Value::Bool(true))
        }
        _ => expr,
    }
}

/// Narrow coercion: if exactly one side of a comparison is BYTES-typed and the other is a string
/// *literal*, encode that literal as UTF-8 bytes so the comparison is type-homogeneous. Mirrors
/// `coerce_to_column`'s INSERT/UPDATE assignment coercion for the same underlying reason: a bound
/// parameter (or literal) whose raw bytes happen to be valid UTF-8 decodes as `Value::String` (see
/// `htap_wire::binary_codec::decode_execute`'s doc comment), so a WHERE comparison against a BYTES
/// column would otherwise reject it even though INSERT/UPDATE already accept it.
fn coerce_bytes_literal_pair(l: Expr, r: Expr) -> (Expr, Expr) {
    let l_is_bytes = l.expr_type().data_type == DataType::Bytes;
    let r_is_bytes = r.expr_type().data_type == DataType::Bytes;
    let l = if r_is_bytes {
        coerce_string_literal_to_bytes(l)
    } else {
        l
    };
    let r = if l_is_bytes {
        coerce_string_literal_to_bytes(r)
    } else {
        r
    };
    (l, r)
}

fn coerce_string_literal_to_bytes(expr: Expr) -> Expr {
    match expr {
        Expr::Literal(Value::String(s)) => Expr::Literal(Value::Bytes(s.into_bytes())),
        other => other,
    }
}

fn check_binary_types(op: BinOp, l: &Expr, r: &Expr) -> Result<()> {
    match op {
        BinOp::And | BinOp::Or => {
            require_bool(l, &op.to_string())?;
            require_bool(r, &op.to_string())
        }
        BinOp::IntDiv => {
            let is_integer =
                |e: &Expr| matches!(e.expr_type().data_type, DataType::Int32 | DataType::Int64);
            if !is_integer(l) || !is_integer(r) {
                return Err(invalid("DIV requires both operands to be integers"));
            }
            Ok(())
        }
        op if op.is_comparison() => check_comparable(l, r, &op.to_string()),
        op => {
            require_numeric(l, &op.to_string())?;
            require_numeric(r, &op.to_string())?;
            arithmetic_result_type(op, l.expr_type().data_type, r.expr_type().data_type)?;
            Ok(())
        }
    }
}

/// Common result type of several branch expressions (`CASE`, `COALESCE`).
fn common_type(exprs: &[Expr], what: &str) -> Result<(DataType, bool)> {
    let mut result: Option<DataType> = None;
    let mut nullable = false;
    for e in exprs {
        let t = e.expr_type();
        nullable |= t.nullable;
        if t.is_permissive() {
            continue;
        }
        result = Some(match result {
            None => t.data_type,
            Some(cur) => union_type(cur, t.data_type).ok_or_else(|| {
                invalid(format!(
                    "{what} branches have incompatible types {} and {}",
                    cur.name(),
                    t.data_type.name()
                ))
            })?,
        });
    }
    Ok((result.unwrap_or(DataType::String), nullable))
}

/// Coerces a bound expression to a column type for assignment (UPDATE).
pub(crate) fn coerce_to_column(expr: Expr, col: &ColumnDef) -> Result<Expr> {
    let t = expr.expr_type();
    if t.is_null_literal {
        if !col.nullable {
            return Err(invalid(format!("column '{}' is NOT NULL", col.name)));
        }
        return Ok(expr);
    }
    if t.is_dynamic {
        // The value is only known at evaluation time: it may or may not be NULL, so the
        // NOT NULL constraint is enforced when the assignment executes, not here.
        return Ok(expr);
    }
    if t.data_type == col.data_type {
        return Ok(expr);
    }
    // Narrow coercion (bytes-vs-string codec fix, Phase 11): a bound parameter whose raw bytes
    // happen to be valid UTF-8 decodes as `Value::String` (see
    // `htap_wire::binary_codec::decode_execute`'s doc comment); accept a string literal for a
    // BYTES assignment target, encoded as UTF-8, exactly like `htap_sql::binder`'s typed INSERT
    // literal path does for the same reason. `coerce_bytes_literal_pair` applies the same
    // coercion to WHERE-clause comparisons (`check_comparable`'s `BinaryOp` callers), so the two
    // paths no longer disagree.
    if col.data_type == DataType::Bytes && t.data_type == DataType::String {
        if let Expr::Literal(Value::String(s)) = &expr {
            return Ok(Expr::Literal(Value::Bytes(s.as_bytes().to_vec())));
        }
    }
    // Narrow coercion: accept the integer literals 0/1 for a BOOL column (MySQL clients
    // commonly send BOOL as TINYINT(1)). Only literal 0/1 coerce; other integer values or
    // non-literal expressions of numeric type still fail below.
    if col.data_type == DataType::Bool {
        let literal_bool = match &expr {
            Expr::Literal(Value::Int32(0)) | Expr::Literal(Value::Int64(0)) => Some(false),
            Expr::Literal(Value::Int32(1)) | Expr::Literal(Value::Int64(1)) => Some(true),
            _ => None,
        };
        if let Some(b) = literal_bool {
            return Ok(Expr::Literal(Value::Bool(b)));
        }
    }
    let numeric = |d: DataType| {
        matches!(
            d,
            DataType::Int32
                | DataType::Int64
                | DataType::Float64
                | DataType::Decimal { .. }
                | DataType::Timestamp
        )
    };
    if numeric(t.data_type) && numeric(col.data_type) {
        if let Expr::Literal(v) = &expr {
            return Ok(Expr::Literal(
                cast_value(v.clone(), col.data_type)
                    .map_err(|e| invalid(format!("value for column '{}': {e}", col.name)))?,
            ));
        }
        return Ok(Expr::Cast {
            expr: Box::new(expr),
            to: col.data_type,
        });
    }
    Err(invalid(format!(
        "type mismatch for column '{}': expected {}, found {}",
        col.name,
        col.data_type.name(),
        t.data_type.name()
    )))
}

/// Validates that each `INSERT ... SELECT` output exactly matches its corresponding target type.
pub(crate) fn validate_insert_query_types(query: &BoundQuery, targets: &[ColumnDef]) -> Result<()> {
    if query.output_columns.len() != targets.len() {
        return Err(invalid(format!(
            "INSERT SELECT column count mismatch: expected {}, got {}",
            targets.len(),
            query.output_columns.len()
        )));
    }

    let source_exprs: Vec<Option<&Expr>> = match &query.body {
        QueryBody::Select(select) => select
            .projection
            .iter()
            .map(|item| Some(&item.expr))
            .collect(),
        _ => vec![None; query.output_columns.len()],
    };

    for ((source, source_expr), target) in query
        .output_columns
        .iter()
        .zip(source_exprs)
        .zip(targets.iter())
    {
        let permissive = source_expr.is_some_and(|expr| expr.expr_type().is_permissive());
        if source.data_type != target.data_type && !permissive {
            return Err(invalid(format!(
                "type mismatch for column '{}': expected {}, found {}",
                target.name,
                target.data_type.name(),
                source.data_type.name()
            )));
        }
        if source.nullable && !target.nullable && !permissive {
            return Err(invalid(format!("column '{}' is NOT NULL", target.name)));
        }
    }

    Ok(())
}

/// Binds `TRUNCATE`, implemented as an unfiltered table delete.
pub(crate) fn bind_truncate(
    truncate: &sql::Truncate,
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    if truncate.table_names.len() != 1 {
        return Err(unsupported(
            "TRUNCATE with multiple tables is not supported",
        ));
    }
    if truncate.partitions.is_some() {
        return Err(unsupported("TRUNCATE with partitions is not supported"));
    }
    if truncate.identity.is_some() {
        return Err(unsupported("TRUNCATE with identity is not supported"));
    }
    if truncate.cascade.is_some() {
        return Err(unsupported("TRUNCATE with cascade is not supported"));
    }
    if truncate.on_cluster.is_some() {
        return Err(unsupported("TRUNCATE with ON CLUSTER is not supported"));
    }

    let target = &truncate.table_names[0];
    if target.only {
        return Err(unsupported("TRUNCATE with ONLY is not supported"));
    }
    if target.has_asterisk {
        return Err(unsupported("TRUNCATE with an asterisk is not supported"));
    }

    let table_name = object_name_single(&target.name)?;
    if catalog.table_by_name(&table_name).is_none() && !truncate.if_exists {
        return Err(table_not_found(&table_name));
    }

    Ok(BoundStatement::Delete(DeleteStatement {
        table: table_name,
        target: DeleteTarget::Filter(None),
        if_exists: truncate.if_exists,
    }))
}

/// Binds a `DELETE` statement, including a general filter target.
pub(crate) fn bind_delete(
    delete: &sql::Delete,
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    if !delete.tables.is_empty() {
        return Err(unsupported("multi-table DELETE not supported"));
    }
    if delete.using.is_some() {
        return Err(unsupported("USING clause not supported in DELETE"));
    }
    if delete.returning.is_some() {
        return Err(unsupported("RETURNING clause not supported in DELETE"));
    }
    if !delete.order_by.is_empty() || delete.limit.is_some() {
        return Err(unsupported("DELETE with ORDER BY / LIMIT not supported"));
    }

    let tables = match &delete.from {
        sql::FromTable::WithFromKeyword(tables) | sql::FromTable::WithoutKeyword(tables) => tables,
    };
    if tables.len() != 1 {
        return Err(unsupported(
            "joins or multiple tables in DELETE not supported",
        ));
    }
    let table_with_joins = &tables[0];
    if !table_with_joins.joins.is_empty() {
        return Err(unsupported("JOIN not supported in DELETE"));
    }

    let (table_name, alias) = match &table_with_joins.relation {
        TableFactor::Table {
            name,
            alias,
            partitions,
            args,
            with_hints,
            version,
            with_ordinality,
            json_path,
            sample,
            index_hints,
        } => {
            if !partitions.is_empty()
                || args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
                || *with_ordinality
                || json_path.is_some()
                || sample.is_some()
                || !index_hints.is_empty()
            {
                return Err(unsupported("unsupported table factor options in DELETE"));
            }
            if let Some(alias) = alias {
                if !alias.columns.is_empty() {
                    return Err(unsupported("table alias column lists not supported"));
                }
            }
            (
                object_name_single(name)?,
                alias.as_ref().map(|alias| alias.name.value.clone()),
            )
        }
        _ => return Err(unsupported("DELETE target must be a table")),
    };

    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| table_not_found(&table_name))?;
    crate::binder::validate_table_descriptor_primary_key(table_desc)?;

    let slot = SlotInfo {
        alias: alias.unwrap_or_else(|| table_name.clone()),
        columns: table_desc.schema.columns().to_vec(),
        offset: 0,
    };
    let infos = vec![slot];
    let ctes = CteScope::default();
    let mut aggregates = Vec::new();
    let mut subqueries = Vec::new();

    let target = match &delete.selection {
        Some(selection) if crate::binder::is_pk_equality_where(selection, table_desc) => {
            DeleteTarget::PrimaryKey(crate::binder::bind_pk_where_predicate(
                table_desc,
                Some(selection),
            )?)
        }
        Some(selection) => {
            let mut binder = ExprBinder::new(
                &infos,
                catalog,
                &ctes,
                &[],
                0,
                &mut aggregates,
                &mut subqueries,
            );
            binder.allow_aggregates = false;
            binder.allow_subqueries = false;
            let filter = binder.bind(selection)?;
            require_bool(&filter, "WHERE")?;
            DeleteTarget::Filter(Some(filter))
        }
        None => DeleteTarget::Filter(None),
    };

    Ok(BoundStatement::Delete(DeleteStatement {
        table: table_name,
        target,
        if_exists: false,
    }))
}

/// Binds an `UPDATE` statement.
pub(crate) fn bind_update(
    update: &sql::Update,
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    if update.from.is_some() {
        return Err(unsupported("UPDATE ... FROM not supported"));
    }
    if update.returning.is_some() || update.output.is_some() {
        return Err(unsupported("UPDATE ... RETURNING/OUTPUT not supported"));
    }
    if update.or.is_some() {
        return Err(unsupported("UPDATE OR ... not supported"));
    }
    if !update.order_by.is_empty() || update.limit.is_some() {
        return Err(unsupported("UPDATE with ORDER BY / LIMIT not supported"));
    }
    if !update.table.joins.is_empty() {
        return Err(unsupported("multi-table UPDATE not supported"));
    }
    let (table_name, alias) = match &update.table.relation {
        TableFactor::Table { name, alias, .. } => (
            object_name_single(name)?,
            alias.as_ref().map(|a| a.name.value.clone()),
        ),
        _ => return Err(unsupported("UPDATE target must be a table")),
    };
    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| table_not_found(&table_name))?;
    crate::binder::validate_table_descriptor_primary_key(table_desc)?;

    let slot = SlotInfo {
        alias: alias.unwrap_or_else(|| table_name.clone()),
        columns: table_desc.schema.columns().to_vec(),
        offset: 0,
    };
    let infos = vec![slot];
    let ctes = CteScope::default();
    let mut aggregates = Vec::new();
    let mut subqueries = Vec::new();

    if update.assignments.is_empty() {
        return Err(invalid("UPDATE requires at least one assignment"));
    }
    let partition_key = table_desc.partitioning.as_ref().map(|p| p.key_column);
    let mut assignments = Vec::with_capacity(update.assignments.len());
    let mut seen = HashSet::new();
    for a in &update.assignments {
        let parts = match &a.target {
            sql::AssignmentTarget::ColumnName(n) => object_name_parts(n)?,
            sql::AssignmentTarget::Tuple(_) => {
                return Err(unsupported("tuple assignments not supported in UPDATE"))
            }
        };
        let col_name = match parts.as_slice() {
            [c] => c.clone(),
            [q, c] if q.eq_ignore_ascii_case(&infos[0].alias) => c.clone(),
            _ => {
                return Err(invalid(format!(
                    "unknown assignment target '{}'",
                    parts.join(".")
                )))
            }
        };
        let col_idx = table_desc.schema.column_index(&col_name).ok_or_else(|| {
            invalid(format!(
                "unknown column '{col_name}' in table '{table_name}'"
            ))
        })?;
        if !seen.insert(col_idx) {
            return Err(invalid(format!(
                "column '{col_name}' assigned more than once"
            )));
        }
        if table_desc.primary_key.contains(&col_idx) {
            return Err(unsupported(format!(
                "updating primary key column '{col_name}' is not supported"
            )));
        }
        if partition_key == Some(col_idx) {
            return Err(unsupported(format!(
                "updating partition key column '{col_name}' is not supported"
            )));
        }
        let mut binder = ExprBinder::new(
            &infos,
            catalog,
            &ctes,
            &[],
            0,
            &mut aggregates,
            &mut subqueries,
        );
        binder.allow_aggregates = false;
        binder.allow_subqueries = false;
        let value = binder.bind(&a.value)?;
        let col = &table_desc.schema.columns()[col_idx];
        assignments.push((col_idx, coerce_to_column(value, col)?));
    }

    let target = match &update.selection {
        Some(sel) if crate::binder::is_pk_equality_where(sel, table_desc) => {
            UpdateTarget::PrimaryKey(crate::binder::bind_pk_where_predicate(
                table_desc,
                Some(sel),
            )?)
        }
        Some(sel) => {
            let mut binder = ExprBinder::new(
                &infos,
                catalog,
                &ctes,
                &[],
                0,
                &mut aggregates,
                &mut subqueries,
            );
            binder.allow_aggregates = false;
            binder.allow_subqueries = false;
            let f = binder.bind(sel)?;
            require_bool(&f, "WHERE")?;
            UpdateTarget::Filter(Some(f))
        }
        None => UpdateTarget::Filter(None),
    };

    Ok(BoundStatement::Update(UpdateStatement {
        table: table_name,
        assignments,
        target,
    }))
}

/// Binds `DROP TABLE`.
pub(crate) fn bind_drop(statement: &Statement) -> Result<BoundStatement> {
    let Statement::Drop {
        object_type,
        if_exists,
        names,
        cascade,
        restrict: _,
        purge,
        temporary,
        table,
    } = statement
    else {
        return Err(unsupported("not a DROP statement"));
    };
    if *object_type != sql::ObjectType::Table {
        return Err(unsupported(format!("DROP {object_type} not supported")));
    }
    if names.len() != 1 {
        return Err(unsupported("DROP TABLE with multiple tables not supported"));
    }
    if *cascade || *purge || *temporary || table.is_some() {
        return Err(unsupported("DROP TABLE options not supported"));
    }
    Ok(BoundStatement::DropTable(DropTableStatement {
        table: object_name_single(&names[0])?,
        if_exists: *if_exists,
    }))
}

/// Binds `SHOW TABLES`, `SHOW DATABASES`, `SHOW COLUMNS FROM t`, and `DESCRIBE t`.
pub(crate) fn bind_show(
    statement: &Statement,
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    let resolve = |name: &ObjectName| -> Result<String> {
        let t = object_name_single(name)?;
        let _desc: &TableDescriptor = catalog
            .table_by_name(&t)
            .ok_or_else(|| table_not_found(&t))?;
        Ok(t)
    };
    match statement {
        Statement::ShowTables {
            terse: _,
            history,
            extended,
            full,
            external,
            show_options,
        } => {
            if *history || *extended || *full || *external {
                return Err(unsupported("SHOW TABLES modifiers not supported"));
            }
            if show_options.show_in.is_some()
                || show_options.starts_with.is_some()
                || show_options.limit.is_some()
                || show_options.limit_from.is_some()
            {
                return Err(unsupported("SHOW TABLES options not supported"));
            }
            let like = match &show_options.filter_position {
                None => None,
                Some(sql::ShowStatementFilterPosition::Infix(f))
                | Some(sql::ShowStatementFilterPosition::Suffix(f)) => match f {
                    sql::ShowStatementFilter::Like(p) => Some(p.clone()),
                    _ => return Err(unsupported("SHOW TABLES filter not supported")),
                },
            };
            Ok(BoundStatement::Show(ShowStatement::Tables { like }))
        }
        Statement::ShowDatabases { show_options, .. } => {
            if show_options.filter_position.is_some() || show_options.show_in.is_some() {
                return Err(unsupported("SHOW DATABASES options not supported"));
            }
            Ok(BoundStatement::Show(ShowStatement::Databases))
        }
        Statement::ShowColumns {
            extended,
            full,
            show_options,
        } => {
            if *extended || *full {
                return Err(unsupported("SHOW COLUMNS modifiers not supported"));
            }
            if show_options.filter_position.is_some() {
                return Err(unsupported("SHOW COLUMNS filters not supported"));
            }
            let show_in = show_options
                .show_in
                .as_ref()
                .ok_or_else(|| invalid("SHOW COLUMNS requires FROM <table>"))?;
            let name = show_in
                .parent_name
                .as_ref()
                .ok_or_else(|| invalid("SHOW COLUMNS requires FROM <table>"))?;
            Ok(BoundStatement::Show(ShowStatement::Columns {
                table: resolve(name)?,
            }))
        }
        Statement::ExplainTable {
            describe_alias,
            hive_format,
            table_name,
            ..
        } => {
            if hive_format.is_some() {
                return Err(unsupported("DESCRIBE FORMATTED/EXTENDED not supported"));
            }
            if matches!(describe_alias, sql::DescribeAlias::Explain) {
                return Err(unsupported("EXPLAIN not supported"));
            }
            Ok(BoundStatement::Show(ShowStatement::Describe {
                table: resolve(table_name)?,
            }))
        }
        other => Err(unsupported(format!("unsupported statement: {other}"))),
    }
}
