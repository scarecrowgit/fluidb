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
use htap_common::types::{ColumnDef, DataType, Value};
use sqlparser::ast::{
    self as sql, Expr as SqlExpr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, LimitClause, ObjectName, ObjectNamePart, OrderByKind, Query,
    Select, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement, TableFactor,
    TableWithJoins,
};

use crate::ast::{
    BoundStatement, DropTableStatement, ShowStatement, UpdateStatement, UpdateTarget,
};
use crate::expr::{
    arithmetic_result_type, cast_value, AggFn, AggregateSpec, BinOp, Expr, ExprType, ScalarFn,
};
use crate::query::{
    BoundQuery, JoinKind, JoinSpec, OrderItem, ProjectionItem, QueryBody, SelectBody, SetOpKind,
    TableSlot,
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

/// Common table expressions visible to a query, innermost scope last.
#[derive(Default)]
struct CteScope {
    ctes: Vec<(String, BoundQuery)>,
}

impl CteScope {
    fn lookup(&self, name: &str) -> Option<&BoundQuery> {
        self.ctes
            .iter()
            .rev()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, q)| q)
    }
}

/// Binds a general `SELECT` query.
pub(crate) fn bind_query(query: &Query, catalog: &CatalogSnapshot) -> Result<BoundQuery> {
    bind_query_scoped(query, catalog, &CteScope::default(), &[])
}

fn bind_query_scoped(
    query: &Query,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
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

    // Inline CTEs: each may reference the ones declared before it.
    let mut local_scope = CteScope {
        ctes: ctes.ctes.clone(),
    };
    if let Some(with) = &query.with {
        if with.recursive {
            return Err(unsupported("recursive CTEs (WITH RECURSIVE) not supported"));
        }
        for cte in &with.cte_tables {
            if cte.from.is_some() {
                return Err(unsupported("CTE FROM clause not supported"));
            }
            if !cte.alias.columns.is_empty() {
                return Err(unsupported("CTE column lists not supported"));
            }
            let bound = bind_query_scoped(&cte.query, catalog, &local_scope, outer)?;
            ensure_unique_output_names(&bound, &cte.alias.name.value)?;
            local_scope.ctes.push((cte.alias.name.value.clone(), bound));
        }
    }

    let mut subqueries = Vec::new();
    let (body, order_scope) =
        bind_set_expr(&query.body, catalog, &local_scope, outer, &mut subqueries)?;
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

    Ok(BoundQuery {
        body,
        order_by,
        limit,
        offset,
        subqueries,
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
    }
}

/// Common type of two set-operation branch columns.
fn union_type(l: DataType, r: DataType) -> Option<DataType> {
    if l == r {
        return Some(l);
    }
    let numeric = |d: DataType| matches!(d, DataType::Int32 | DataType::Int64 | DataType::Float64);
    if numeric(l) && numeric(r) {
        if l == DataType::Float64 || r == DataType::Float64 {
            Some(DataType::Float64)
        } else {
            Some(DataType::Int64)
        }
    } else {
        None
    }
}

/// What `ORDER BY` may resolve against.
enum OrderScope {
    /// A select block: slots, aggregates, and output aliases.
    Select {
        slots: Vec<SlotInfo>,
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
    subqueries: &mut Vec<BoundQuery>,
) -> Result<(QueryBody, OrderScope)> {
    match body {
        SetExpr::Select(select) => {
            let (sel, scope) = bind_select_body(select, catalog, ctes, outer, subqueries)?;
            Ok((QueryBody::Select(sel), scope))
        }
        SetExpr::Query(inner) => {
            // Parenthesized branch with its own ORDER BY / LIMIT.
            let bound = bind_query_scoped(inner, catalog, ctes, outer)?;
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
                (SetOperator::Union, other) => {
                    return Err(unsupported(format!("UNION {other} not supported")))
                }
                (SetOperator::Except, _) | (SetOperator::Intersect, _) => {
                    return Err(unsupported("EXCEPT / INTERSECT not supported"))
                }
                (other, _) => {
                    return Err(unsupported(format!("set operator {other} not supported")))
                }
            };
            let left_q = bind_branch(left, catalog, ctes, outer)?;
            let right_q = bind_branch(right, catalog, ctes, outer)?;
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
) -> Result<BoundQuery> {
    match branch {
        SetExpr::Query(inner) => bind_query_scoped(inner, catalog, ctes, outer),
        other => {
            let mut subqueries = Vec::new();
            let (body, _) = bind_set_expr(other, catalog, ctes, outer, &mut subqueries)?;
            let output_columns = body_output_columns(&body);
            Ok(BoundQuery {
                body,
                order_by: Vec::new(),
                limit: None,
                offset: None,
                subqueries,
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
        output_columns: q.output_columns.clone(),
    }
}

fn bind_select_body(
    select: &Select,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
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

    for (i, twj) in select.from.iter().enumerate() {
        bind_table_with_joins(
            twj,
            i > 0,
            catalog,
            ctes,
            outer,
            &mut slots,
            &mut infos,
            &mut joins,
            &mut aggregates,
            subqueries,
        )?;
    }

    // WHERE
    let filter = match &select.selection {
        Some(expr) => {
            let mut binder =
                ExprBinder::new(&infos, catalog, ctes, outer, &mut aggregates, subqueries);
            binder.allow_aggregates = false;
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
                let mut binder =
                    ExprBinder::new(&infos, catalog, ctes, outer, &mut aggregates, subqueries);
                binder.allow_aggregates = false;
                let bound = match e {
                    SqlExpr::Value(v) if is_integer_literal(&v.value) => {
                        return Err(unsupported(format!(
                            "GROUP BY ordinal {} not supported",
                            v.value
                        )))
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
                if infos.is_empty() {
                    return Err(invalid("SELECT * requires a FROM clause"));
                }
                for info in &infos {
                    for (ci, col) in info.columns.iter().enumerate() {
                        projection.push(ProjectionItem {
                            expr: column_ref(info, 0, ci, &infos),
                            name: col.name.clone(),
                        });
                    }
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
                let mut binder =
                    ExprBinder::new(&infos, catalog, ctes, outer, &mut aggregates, subqueries);
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
                let mut binder =
                    ExprBinder::new(&infos, catalog, ctes, outer, &mut aggregates, subqueries);
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
            let mut binder =
                ExprBinder::new(&infos, catalog, ctes, outer, &mut aggregates, subqueries);
            binder.output = Some(&output_defs);
            binder.alias_priority = false;
            let bound = binder.bind(expr)?;
            require_bool(&bound, "HAVING")?;
            Some(bound)
        }
        None => None,
    };

    let is_aggregate = !group_by.is_empty() || !aggregates.is_empty();
    if is_aggregate {
        for p in &projection {
            check_grouped(&p.expr, &group_by, &p.name)?;
        }
        if let Some(h) = &having {
            check_grouped(h, &group_by, "HAVING")?;
        }
    } else if having.is_some() {
        // HAVING without GROUP BY / aggregates behaves like WHERE over the projected row.
    }

    let body = SelectBody {
        slots,
        joins,
        filter,
        group_by: group_by.clone(),
        aggregates: aggregates.clone(),
        having,
        projection,
        distinct,
    };
    Ok((
        body,
        OrderScope::Select {
            slots: infos,
            is_aggregate,
            aggregates,
            group_by,
        },
    ))
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

/// Checks that a non-aggregate expression only depends on grouped expressions.
fn check_grouped(expr: &Expr, group_by: &[Expr], what: &str) -> Result<()> {
    if group_by.contains(expr) {
        return Ok(());
    }
    match expr {
        Expr::ColumnRef { name, .. } => Err(invalid(format!(
            "column '{name}' in '{what}' must appear in the GROUP BY clause or be used in an aggregate function"
        ))),
        Expr::OutputColumn { .. }
        | Expr::Literal(_)
        | Expr::AggregateRef { .. }
        | Expr::ScalarSubquery { .. }
        | Expr::Exists { .. }
        | Expr::Variable { .. } => Ok(()),
        Expr::BinaryOp { left, right, .. } => {
            check_grouped(left, group_by, what)?;
            check_grouped(right, group_by, what)
        }
        Expr::Not(e) | Expr::Negate(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            check_grouped(e, group_by, what)
        }
        Expr::Like { expr, pattern, .. } => {
            check_grouped(expr, group_by, what)?;
            check_grouped(pattern, group_by, what)
        }
        Expr::In { expr, list, .. } => {
            check_grouped(expr, group_by, what)?;
            list.iter().try_for_each(|e| check_grouped(e, group_by, what))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            check_grouped(expr, group_by, what)?;
            check_grouped(low, group_by, what)?;
            check_grouped(high, group_by, what)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                check_grouped(o, group_by, what)?;
            }
            for (c, r) in branches {
                check_grouped(c, group_by, what)?;
                check_grouped(r, group_by, what)?;
            }
            if let Some(e) = else_result {
                check_grouped(e, group_by, what)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => check_grouped(expr, group_by, what),
        Expr::ScalarFunction { args, .. } => {
            args.iter().try_for_each(|e| check_grouped(e, group_by, what))
        }
        Expr::InSubquery { expr, .. } => check_grouped(expr, group_by, what),
    }
}

#[allow(clippy::too_many_arguments)]
fn bind_table_with_joins(
    twj: &TableWithJoins,
    comma_joined: bool,
    catalog: &CatalogSnapshot,
    ctes: &CteScope,
    outer: &[SlotInfo],
    slots: &mut Vec<TableSlot>,
    infos: &mut Vec<SlotInfo>,
    joins: &mut Vec<JoinSpec>,
    aggregates: &mut Vec<AggregateSpec>,
    subqueries: &mut Vec<BoundQuery>,
) -> Result<()> {
    let first = bind_table_factor(&twj.relation, catalog, ctes, outer)?;
    push_slot(first, slots, infos)?;
    if comma_joined {
        joins.push(JoinSpec {
            kind: JoinKind::Cross,
            right_slot: slots.len() - 1,
            on: None,
        });
    }
    for join in &twj.joins {
        if join.global {
            return Err(unsupported("GLOBAL joins not supported"));
        }
        let (kind, constraint) = match &join.join_operator {
            JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinKind::Inner, c),
            JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
            JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinKind::Right, c),
            JoinOperator::CrossJoin(c) => (JoinKind::Cross, c),
            JoinOperator::FullOuter(_) => return Err(unsupported("FULL OUTER JOIN not supported")),
            other => {
                return Err(unsupported(format!(
                    "join type not supported: {}",
                    join_operator_name(other)
                )))
            }
        };
        let slot = bind_table_factor(&join.relation, catalog, ctes, outer)?;
        push_slot(slot, slots, infos)?;
        let right_slot = slots.len() - 1;
        // Null-supplying sides become nullable for everything bound afterwards.
        match kind {
            JoinKind::Left => mark_nullable(&mut infos[right_slot]),
            JoinKind::Right => infos[..right_slot].iter_mut().for_each(mark_nullable),
            JoinKind::Inner | JoinKind::Cross => {}
        }
        let (kind, on) = match constraint {
            JoinConstraint::On(expr) => {
                let mut binder =
                    ExprBinder::new(infos, catalog, ctes, outer, aggregates, subqueries);
                binder.allow_aggregates = false;
                let bound = binder.bind(expr)?;
                require_bool(&bound, "JOIN ON")?;
                (kind, Some(bound))
            }
            JoinConstraint::None => match kind {
                JoinKind::Inner | JoinKind::Cross => (JoinKind::Cross, None),
                JoinKind::Left | JoinKind::Right => {
                    return Err(invalid("outer joins require an ON condition"))
                }
            },
            JoinConstraint::Using(_) => return Err(unsupported("JOIN ... USING not supported")),
            JoinConstraint::Natural => return Err(unsupported("NATURAL JOIN not supported")),
        };
        if kind == JoinKind::Cross && on.is_some() {
            return Err(invalid("CROSS JOIN cannot have an ON condition"));
        }
        joins.push(JoinSpec {
            kind,
            right_slot,
            on,
        });
    }
    Ok(())
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

fn bind_table_factor(
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
                return Ok(TableSlot::Derived {
                    columns: cte.output_columns.clone(),
                    query: Box::new(cte.clone()),
                    alias: alias_name,
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
            let bound = bind_query_scoped(subquery, catalog, ctes, outer)?;
            ensure_unique_output_names(&bound, &alias.name.value)?;
            Ok(TableSlot::Derived {
                columns: bound.output_columns.clone(),
                query: Box::new(bound),
                alias: alias.name.value.clone(),
            })
        }
        TableFactor::NestedJoin { .. } => Err(unsupported(
            "parenthesized (nested) join expressions not supported; use a left-deep join chain",
        )),
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
        let expr = match scope {
            OrderScope::Output => {
                let mut no_aggs = Vec::new();
                let mut binder =
                    ExprBinder::new(&[], catalog, ctes, outer, &mut no_aggs, subqueries);
                binder.output = Some(output);
                binder.alias_priority = true;
                binder.output_only = true;
                binder.bind(&ob.expr)?
            }
            OrderScope::Select {
                slots,
                is_aggregate,
                aggregates,
                group_by,
            } => {
                let mut aggs = aggregates.clone();
                let mut binder =
                    ExprBinder::new(slots, catalog, ctes, outer, &mut aggs, subqueries);
                binder.output = Some(output);
                binder.alias_priority = true;
                let bound = binder.bind(&ob.expr)?;
                if aggs.len() != aggregates.len() {
                    return Err(invalid(
                        "ORDER BY aggregate expressions must also appear in the select list",
                    ));
                }
                if *is_aggregate {
                    check_grouped(&bound, group_by, "ORDER BY")?;
                }
                bound
            }
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
    aggregates: &'a mut Vec<AggregateSpec>,
    subqueries: &'a mut Vec<BoundQuery>,
    /// Output columns available for alias / ordinal resolution.
    output: Option<&'a [ColumnDef]>,
    /// Resolve aliases before source columns (ORDER BY) or after (HAVING).
    alias_priority: bool,
    /// Only output columns may be referenced (ORDER BY of a set operation).
    output_only: bool,
    allow_aggregates: bool,
    allow_subqueries: bool,
    in_aggregate: bool,
}

impl<'a> ExprBinder<'a> {
    fn new(
        slots: &'a [SlotInfo],
        catalog: &'a CatalogSnapshot,
        ctes: &'a CteScope,
        outer: &'a [SlotInfo],
        aggregates: &'a mut Vec<AggregateSpec>,
        subqueries: &'a mut Vec<BoundQuery>,
    ) -> Self {
        Self {
            slots,
            catalog,
            ctes,
            outer,
            aggregates,
            subqueries,
            output: None,
            alias_priority: false,
            output_only: false,
            allow_aggregates: true,
            allow_subqueries: true,
            in_aggregate: false,
        }
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
        let candidates: Vec<(usize, usize)> = match qualifier {
            Some(q) => {
                let (si, info) = match find_slot(self.slots, q) {
                    Ok(found) => found,
                    Err(e) => {
                        if self.outer.iter().any(|o| o.alias.eq_ignore_ascii_case(q)) {
                            return Err(unsupported(format!(
                                "correlated subqueries are not supported: column '{q}.{name}' refers to the outer query"
                            )));
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
                let in_outer = self.outer.iter().any(|info| {
                    qualifier.is_none_or(|q| info.alias.eq_ignore_ascii_case(q))
                        && info
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(name))
                });
                if in_outer {
                    return Err(unsupported(format!(
                        "correlated subqueries are not supported: column '{display}' refers to the outer query"
                    )));
                }
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
            SqlExpr::TypedString(_) | SqlExpr::Interval(_) => {
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
                let op = match op {
                    sql::BinaryOperator::Plus => BinOp::Add,
                    sql::BinaryOperator::Minus => BinOp::Sub,
                    sql::BinaryOperator::Multiply => BinOp::Mul,
                    sql::BinaryOperator::Divide => BinOp::Div,
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
                })
            }
            SqlExpr::Exists { subquery, negated } => {
                let idx = self.bind_subquery(subquery, "EXISTS subquery")?;
                Ok(Expr::Exists {
                    index: idx,
                    negated: *negated,
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
        // Inner queries see the CTEs but not the enclosing slots; the enclosing slots are
        // passed only so that correlated references produce a clear error.
        let mut outer: Vec<SlotInfo> = self.outer.to_vec();
        outer.extend(self.slots.iter().cloned());
        let bound = bind_query_scoped(q, self.catalog, self.ctes, &outer)?;
        self.subqueries.push(bound);
        Ok(self.subqueries.len() - 1)
    }

    fn bind_function(&mut self, func: &sql::Function) -> Result<Expr> {
        let name = object_name_single(&func.name)?.to_ascii_uppercase();
        if func.over.is_some() {
            return Err(unsupported("window functions (OVER clause) not supported"));
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
                    if t.data_type == DataType::Float64 {
                        DataType::Float64
                    } else {
                        DataType::Int64
                    },
                    true,
                )
            }
            (AggFn::Avg, Some(a)) => {
                require_numeric(a, name)?;
                (DataType::Float64, true)
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

fn bind_literal(v: &sql::Value) -> Result<Expr> {
    Ok(Expr::Literal(match v {
        sql::Value::Null => Value::Null,
        sql::Value::Boolean(b) => Value::Bool(*b),
        sql::Value::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Value::Int64(i)
            } else {
                Value::Float64(
                    n.parse::<f64>()
                        .map_err(|_| invalid(format!("invalid numeric literal {n}")))?,
                )
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
        D::Float(_)
        | D::Float4
        | D::Float8
        | D::Real
        | D::Double(_)
        | D::DoublePrecision
        | D::Decimal(_)
        | D::Numeric(_)
        | D::Dec(_) => DataType::Float64,
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

fn is_numeric_type(t: &ExprType) -> bool {
    matches!(
        t.data_type,
        DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Timestamp
    )
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
        op if op.is_comparison() => check_comparable(l, r, &op.to_string()),
        op => {
            require_numeric(l, &op.to_string())?;
            require_numeric(r, &op.to_string())?;
            let _ = arithmetic_result_type(op, l.expr_type().data_type, r.expr_type().data_type);
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
fn coerce_to_column(expr: Expr, col: &ColumnDef) -> Result<Expr> {
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
            DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Timestamp
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
