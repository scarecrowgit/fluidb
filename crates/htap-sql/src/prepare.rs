//! Prepared-statement support: placeholder (`?`) discovery, AST-level substitution, best-effort
//! placeholder type inference, and output-schema resolution.
//!
//! Parameterization is implemented as AST-level substitution: a placeholder `Expr::Value(Value
//! ::Placeholder(_))` node is replaced in place by a literal `Expr` built directly from a bound
//! parameter [`Value`], never by re-rendering the statement to text and reparsing it. This avoids
//! double-escaping string/byte parameters and the float/i64::MIN precision edge cases that a
//! text round-trip would introduce.
//!
//! The placeholder walk in this module intentionally mirrors the shape of the statement/query
//! AST the binder in [`crate::binder`] and [`crate::binder_query`] accepts: every clause that can
//! hold a literal there is walked here (INSERT VALUES rows; UPDATE SET and WHERE; DELETE WHERE;
//! SELECT projection/WHERE/HAVING/GROUP BY/ORDER BY/JOIN ON; subquery bodies; derived tables; CTE
//! bodies; UNION branches; LIMIT/OFFSET). [`count_placeholders`] and [`substitute_placeholders`]
//! share this walk, so they always agree with each other by construction. Whether the walk
//! agrees with the *raw* `?` token count in the original SQL text (i.e. whether some `?` sits in
//! a position the walk does not recognize as literal-bearing, such as a CREATE TABLE DEFAULT or a
//! SET target) is checked separately by [`checked_placeholder_count`].

use htap_catalog::CatalogSnapshot;
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Schema, Value};
use sqlparser::ast::{
    AssignmentTarget, BinaryOperator, Delete as SqlDelete, Expr as SqlExpr,
    Function as SqlFunction, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    Insert as SqlInsert, JoinConstraint, JoinOperator, LimitClause, ObjectName, ObjectNamePart,
    ObjectType, OrderByKind, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr,
    Statement, TableFactor, TableObject, TableWithJoins, Truncate as SqlTruncate, UnaryOperator,
    Update as SqlUpdate, UpdateTableFromKind, Value as SqlValue, WindowFrameBound, WindowSpec,
    WindowType,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::ast::BoundStatement;
use crate::binder::bind;
use crate::binder_query::object_name_single;

/// Visitor invoked for every placeholder `Expr` node found by the walk; it may mutate the node
/// in place (substitution) or merely observe it (counting).
type Visit<'a> = dyn FnMut(&mut SqlExpr) -> Result<()> + 'a;

// ---------------------------------------------------------------------------------------------
// count_placeholders / substitute_placeholders: shared, catalog-free, mutable AST walk.
// ---------------------------------------------------------------------------------------------

/// Counts the placeholders (`?`) in every literal-bearing position this module's walk
/// recognizes. See the module documentation for exactly which positions are covered; a `?` in
/// any other position is not counted here (use [`checked_placeholder_count`] to detect that).
pub fn count_placeholders(statement: &Statement) -> usize {
    let mut clone = statement.clone();
    let mut count = 0usize;
    walk_statement(&mut clone, &mut |_expr| {
        count += 1;
        Ok(())
    })
    .expect("counting visitor never fails");
    count
}

/// One bound parameter for [`substitute_placeholders_ext`] (finding 2 of the Phase 11 fix pass).
///
/// [`Value`] covers every ordinary bound parameter; [`ParamLiteral::NumericText`] exists
/// specifically for a wire-protocol `NEWDECIMAL`/`DECIMAL` parameter, whose exact fixed-point
/// text must survive substitution as a bare SQL numeric literal (`Value::Number` in the
/// tokenizer's own sense) instead of being rounded through `f64`/`i64` first — the same
/// precision loss `signed_integer_expr`/`signed_float_expr` already avoid for `Int64`/`Float64`
/// parameters by building the literal `Expr` directly from a formatted string rather than
/// re-parsing anything.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamLiteral {
    /// An ordinary bound value.
    Value(Value),
    /// Pre-validated ([`substitute_placeholders_ext`] re-validates it regardless) numeric literal
    /// text — optional leading sign, digits, an optional fractional part, and an optional
    /// exponent — substituted as a bare SQL numeric literal, exactly as if that text had been
    /// typed directly into the SQL. What the binder then does with a long/high-precision literal
    /// bound to an `Int64` or `Float64` column is ordinary binder behavior, identical to binding
    /// the same literal text in the SQL itself; there is no dedicated engine DECIMAL type (see
    /// `docs/LIMITATIONS.md`).
    NumericText(String),
}

impl From<Value> for ParamLiteral {
    fn from(value: Value) -> Self {
        ParamLiteral::Value(value)
    }
}

/// Substitutes every placeholder found by the walk (in left-to-right, depth-first order) with a
/// literal `Expr` built from the corresponding entry of `params`.
///
/// Returns [`HtapError::InvalidArgument`] if the number of placeholders found does not match
/// `params.len()`, if a [`ParamLiteral::Value`] cannot be represented as a SQL literal (e.g. a
/// non-finite float, which has no literal spelling and which the binder already rejects for typed
/// columns), or if a [`ParamLiteral::NumericText`] is not valid numeric literal text.
pub fn substitute_placeholders_ext(
    statement: &mut Statement,
    params: &[ParamLiteral],
) -> Result<()> {
    let mut idx = 0usize;
    walk_statement(statement, &mut |expr: &mut SqlExpr| {
        let Some(param) = params.get(idx) else {
            return Err(HtapError::InvalidArgument(format!(
                "expected {} parameter(s), found more placeholders in the statement",
                params.len()
            )));
        };
        *expr = match param {
            ParamLiteral::Value(value) => value_to_expr(value)?,
            ParamLiteral::NumericText(text) => numeric_text_to_expr(text)?,
        };
        idx += 1;
        Ok(())
    })?;
    if idx != params.len() {
        return Err(HtapError::InvalidArgument(format!(
            "expected {} parameter(s), found {idx} placeholder(s) in the statement",
            params.len()
        )));
    }
    Ok(())
}

/// [`substitute_placeholders_ext`] restricted to ordinary [`Value`] parameters (no
/// [`ParamLiteral::NumericText`]); kept as the common-case entry point so every existing caller
/// (and the wire layer's non-DECIMAL parameter types) is unaffected by amendment/finding 2's
/// `ParamLiteral` addition.
pub fn substitute_placeholders(statement: &mut Statement, values: &[Value]) -> Result<()> {
    let params: Vec<ParamLiteral> = values.iter().cloned().map(ParamLiteral::Value).collect();
    substitute_placeholders_ext(statement, &params)
}

/// Counts raw `?` placeholder tokens in `sql` using the MySQL-dialect tokenizer, independent of
/// how (or whether) the parser turns them into AST nodes.
pub fn tokenizer_placeholder_count(sql: &str) -> Result<usize> {
    let dialect = MySqlDialect {};
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|err| HtapError::InvalidArgument(format!("SQL tokenize error: {err}")))?;
    Ok(tokens
        .iter()
        .filter(|t| matches!(t, Token::Placeholder(_)))
        .count())
}

/// Collects the base-table names referenced by a preparable statement.
///
/// CTE names are excluded: references to a CTE are not catalog tables. A two-part name in the
/// default `htap` schema is returned as its table component, while other qualified names are
/// preserved for the binder to reject or resolve normally later.
pub fn referenced_table_names(statement: &Statement) -> Vec<String> {
    let mut names = Vec::new();
    walk_referenced_statement(statement, &[], &mut names);
    names
}

fn table_name_for_reference(name: &ObjectName) -> String {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(table)] => table.value.clone(),
        [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(table)]
            if schema.value.eq_ignore_ascii_case("htap") =>
        {
            table.value.clone()
        }
        _ => name.to_string(),
    }
}

fn walk_referenced_statement(statement: &Statement, ctes: &[String], names: &mut Vec<String>) {
    match statement {
        Statement::Insert(insert) => {
            if let TableObject::TableName(name) = &insert.table {
                names.push(table_name_for_reference(name));
            }
            if let Some(source) = &insert.source {
                walk_referenced_query(source, ctes, names);
            }
        }
        Statement::Update(update) => {
            walk_referenced_table_with_joins(&update.table, ctes, names);
            if let Some(from) = &update.from {
                match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => {
                        for table in tables {
                            walk_referenced_table_with_joins(table, ctes, names);
                        }
                    }
                }
            }
            for assignment in &update.assignments {
                walk_referenced_expr(&assignment.value, ctes, names);
            }
            if let Some(selection) = &update.selection {
                walk_referenced_expr(selection, ctes, names);
            }
        }
        Statement::Delete(delete) => {
            let tables = match &delete.from {
                sqlparser::ast::FromTable::WithFromKeyword(t)
                | sqlparser::ast::FromTable::WithoutKeyword(t) => t,
            };
            for table in tables {
                walk_referenced_table_with_joins(table, ctes, names);
            }
            if let Some(selection) = &delete.selection {
                walk_referenced_expr(selection, ctes, names);
            }
        }
        Statement::Truncate(truncate) => {
            for target in &truncate.table_names {
                names.push(table_name_for_reference(&target.name));
            }
        }
        Statement::Query(query) => walk_referenced_query(query, ctes, names),
        Statement::AlterTable(alter_table) => {
            names.push(table_name_for_reference(&alter_table.name));
        }
        Statement::Drop {
            object_type: ObjectType::Table,
            names: drop_names,
            ..
        } => {
            for name in drop_names {
                names.push(table_name_for_reference(name));
            }
        }
        Statement::ShowColumns { show_options, .. } => {
            if let Some(name) = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
            {
                names.push(table_name_for_reference(name));
            }
        }
        Statement::Explain { statement, .. } => {
            walk_referenced_statement(statement, ctes, names);
        }
        Statement::ExplainTable { table_name, .. } => {
            names.push(table_name_for_reference(table_name));
        }
        Statement::Analyze(analyze) => {
            if let Some(table_name) = &analyze.table_name {
                names.push(table_name_for_reference(table_name));
            }
        }
        _ => {}
    }
}

fn walk_referenced_query(query: &Query, ctes: &[String], names: &mut Vec<String>) {
    let mut visible_ctes = ctes.to_vec();
    if let Some(with) = &query.with {
        // A non-recursive CTE body sees only CTEs declared before it. A recursive CTE also
        // sees its own name, matching bind_query_scoped.
        for cte in &with.cte_tables {
            let cte_name = cte.alias.name.value.clone();
            let self_referencing =
                with.recursive && set_expr_references_cte(&cte.query.body, &cte_name);
            if self_referencing {
                visible_ctes.push(cte_name.clone());
            }
            walk_referenced_query(&cte.query, &visible_ctes, names);
            if !self_referencing {
                visible_ctes.push(cte_name);
            }
        }
    }
    walk_referenced_set_expr(&query.body, &visible_ctes, names);
    if let Some(order_by) = &query.order_by {
        if let OrderByKind::Expressions(items) = &order_by.kind {
            for item in items {
                walk_referenced_expr(&item.expr, &visible_ctes, names);
            }
        }
    }
    if let Some(limit_clause) = &query.limit_clause {
        match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(limit) = limit {
                    walk_referenced_expr(limit, &visible_ctes, names);
                }
                if let Some(offset) = offset {
                    walk_referenced_expr(&offset.value, &visible_ctes, names);
                }
                for expr in limit_by {
                    walk_referenced_expr(expr, &visible_ctes, names);
                }
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                walk_referenced_expr(offset, &visible_ctes, names);
                walk_referenced_expr(limit, &visible_ctes, names);
            }
        }
    }
}

fn set_expr_references_cte(body: &SetExpr, cte_name: &str) -> bool {
    match body {
        SetExpr::Select(select) => {
            select.projection.iter().any(|item| match item {
                SelectItem::UnnamedExpr(expr)
                | SelectItem::ExprWithAlias { expr, .. }
                | SelectItem::ExprWithAliases { expr, .. } => expr_references_cte(expr, cte_name),
                SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(expr), _) => {
                    expr_references_cte(expr, cte_name)
                }
                SelectItem::QualifiedWildcard(..) | SelectItem::Wildcard(_) => false,
            }) || select
                .from
                .iter()
                .any(|table| table_with_joins_references_cte(table, cte_name))
                || select
                    .selection
                    .as_ref()
                    .is_some_and(|expr| expr_references_cte(expr, cte_name))
                || matches!(
                    &select.group_by,
                    GroupByExpr::Expressions(expressions, _)
                        if expressions
                            .iter()
                            .any(|expr| expr_references_cte(expr, cte_name))
                )
                || select
                    .having
                    .as_ref()
                    .is_some_and(|expr| expr_references_cte(expr, cte_name))
        }
        SetExpr::Query(query) => query_references_cte(query, cte_name),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_references_cte(left, cte_name) || set_expr_references_cte(right, cte_name)
        }
        SetExpr::Values(values) => values.rows.iter().any(|row| {
            row.content
                .iter()
                .any(|expr| expr_references_cte(expr, cte_name))
        }),
        SetExpr::Insert(statement) | SetExpr::Update(statement) | SetExpr::Delete(statement) => {
            statement_references_cte(statement, cte_name)
        }
        SetExpr::Merge(_) | SetExpr::Table(_) => false,
    }
}

fn query_references_cte(query: &Query, cte_name: &str) -> bool {
    query.with.as_ref().is_some_and(|with| {
        with.cte_tables
            .iter()
            .any(|cte| query_references_cte(&cte.query, cte_name))
    }) || set_expr_references_cte(&query.body, cte_name)
        || query.order_by.as_ref().is_some_and(|order_by| {
            matches!(
                &order_by.kind,
                OrderByKind::Expressions(items)
                    if items
                        .iter()
                        .any(|item| expr_references_cte(&item.expr, cte_name))
            )
        })
        || query
            .limit_clause
            .as_ref()
            .is_some_and(|limit_clause| match limit_clause {
                LimitClause::LimitOffset {
                    limit,
                    offset,
                    limit_by,
                } => {
                    limit
                        .as_ref()
                        .is_some_and(|expr| expr_references_cte(expr, cte_name))
                        || offset
                            .as_ref()
                            .is_some_and(|offset| expr_references_cte(&offset.value, cte_name))
                        || limit_by
                            .iter()
                            .any(|expr| expr_references_cte(expr, cte_name))
                }
                LimitClause::OffsetCommaLimit { offset, limit } => {
                    expr_references_cte(offset, cte_name) || expr_references_cte(limit, cte_name)
                }
            })
}

fn statement_references_cte(statement: &Statement, cte_name: &str) -> bool {
    match statement {
        Statement::Insert(insert) => insert
            .source
            .as_ref()
            .is_some_and(|source| query_references_cte(source, cte_name)),
        Statement::Update(update) => {
            update
                .assignments
                .iter()
                .any(|assignment| expr_references_cte(&assignment.value, cte_name))
                || update
                    .selection
                    .as_ref()
                    .is_some_and(|expr| expr_references_cte(expr, cte_name))
        }
        Statement::Delete(delete) => delete
            .selection
            .as_ref()
            .is_some_and(|expr| expr_references_cte(expr, cte_name)),
        Statement::Query(query) => query_references_cte(query, cte_name),
        _ => false,
    }
}

fn table_with_joins_references_cte(table: &TableWithJoins, cte_name: &str) -> bool {
    table_factor_references_cte(&table.relation, cte_name)
        || table.joins.iter().any(|join| {
            table_factor_references_cte(&join.relation, cte_name)
                || match &join.join_operator {
                    JoinOperator::AsOf {
                        match_condition,
                        constraint,
                    } => {
                        expr_references_cte(match_condition, cte_name)
                            || matches!(
                                constraint,
                                JoinConstraint::On(expr)
                                    if expr_references_cte(expr, cte_name)
                            )
                    }
                    JoinOperator::Join(constraint)
                    | JoinOperator::Inner(constraint)
                    | JoinOperator::Left(constraint)
                    | JoinOperator::LeftOuter(constraint)
                    | JoinOperator::Right(constraint)
                    | JoinOperator::RightOuter(constraint)
                    | JoinOperator::FullOuter(constraint)
                    | JoinOperator::CrossJoin(constraint)
                    | JoinOperator::Semi(constraint)
                    | JoinOperator::LeftSemi(constraint)
                    | JoinOperator::RightSemi(constraint)
                    | JoinOperator::Anti(constraint)
                    | JoinOperator::LeftAnti(constraint)
                    | JoinOperator::RightAnti(constraint)
                    | JoinOperator::StraightJoin(constraint) => matches!(
                        constraint,
                        JoinConstraint::On(expr) if expr_references_cte(expr, cte_name)
                    ),
                    JoinOperator::CrossApply
                    | JoinOperator::OuterApply
                    | JoinOperator::ArrayJoin
                    | JoinOperator::LeftArrayJoin
                    | JoinOperator::InnerArrayJoin => false,
                }
        })
}

fn table_factor_references_cte(table: &TableFactor, cte_name: &str) -> bool {
    match table {
        TableFactor::Table { name, .. } => matches!(
            name.0.as_slice(),
            [ObjectNamePart::Identifier(identifier)]
                if identifier.value.eq_ignore_ascii_case(cte_name)
        ),
        TableFactor::Derived { subquery, .. } => query_references_cte(subquery, cte_name),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_references_cte(table_with_joins, cte_name),
        _ => false,
    }
}

fn expr_references_cte(expr: &SqlExpr, cte_name: &str) -> bool {
    match expr {
        SqlExpr::Nested(inner)
        | SqlExpr::UnaryOp { expr: inner, .. }
        | SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotFalse(inner)
        | SqlExpr::Cast { expr: inner, .. } => expr_references_cte(inner, cte_name),
        SqlExpr::BinaryOp { left, right, .. } => {
            expr_references_cte(left, cte_name) || expr_references_cte(right, cte_name)
        }
        SqlExpr::Like { expr, pattern, .. } => {
            expr_references_cte(expr, cte_name) || expr_references_cte(pattern, cte_name)
        }
        SqlExpr::InList { expr, list, .. } => {
            expr_references_cte(expr, cte_name)
                || list.iter().any(|item| expr_references_cte(item, cte_name))
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            expr_references_cte(expr, cte_name)
                || expr_references_cte(low, cte_name)
                || expr_references_cte(high, cte_name)
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|expr| expr_references_cte(expr, cte_name))
                || conditions.iter().any(|when| {
                    expr_references_cte(&when.condition, cte_name)
                        || expr_references_cte(&when.result, cte_name)
                })
                || else_result
                    .as_ref()
                    .is_some_and(|expr| expr_references_cte(expr, cte_name))
        }
        SqlExpr::Function(function) => {
            if let FunctionArguments::List(list) = &function.args {
                list.args.iter().any(|arg| {
                    matches!(
                        arg,
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                            if expr_references_cte(expr, cte_name)
                    )
                })
            } else {
                false
            }
        }
        SqlExpr::Subquery(query)
        | SqlExpr::Exists {
            subquery: query, ..
        } => query_references_cte(query, cte_name),
        SqlExpr::InSubquery { expr, subquery, .. } => {
            expr_references_cte(expr, cte_name) || query_references_cte(subquery, cte_name)
        }
        _ => false,
    }
}

fn walk_referenced_set_expr(body: &SetExpr, ctes: &[String], names: &mut Vec<String>) {
    match body {
        SetExpr::Select(select) => {
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr)
                    | SelectItem::ExprWithAlias { expr, .. }
                    | SelectItem::ExprWithAliases { expr, .. } => {
                        walk_referenced_expr(expr, ctes, names)
                    }
                    SelectItem::QualifiedWildcard(
                        SelectItemQualifiedWildcardKind::Expr(expr),
                        _,
                    ) => walk_referenced_expr(expr, ctes, names),
                    SelectItem::QualifiedWildcard(..) | SelectItem::Wildcard(_) => {}
                }
            }
            for table in &select.from {
                walk_referenced_table_with_joins(table, ctes, names);
            }
            if let Some(selection) = &select.selection {
                walk_referenced_expr(selection, ctes, names);
            }
            if let GroupByExpr::Expressions(expressions, _) = &select.group_by {
                for expr in expressions {
                    walk_referenced_expr(expr, ctes, names);
                }
            }
            if let Some(having) = &select.having {
                walk_referenced_expr(having, ctes, names);
            }
        }
        SetExpr::Query(query) => walk_referenced_query(query, ctes, names),
        SetExpr::SetOperation { left, right, .. } => {
            walk_referenced_set_expr(left, ctes, names);
            walk_referenced_set_expr(right, ctes, names);
        }
        SetExpr::Values(values) => {
            for row in &values.rows {
                for expr in &row.content {
                    walk_referenced_expr(expr, ctes, names);
                }
            }
        }
        SetExpr::Insert(statement) | SetExpr::Update(statement) | SetExpr::Delete(statement) => {
            walk_referenced_statement(statement, ctes, names)
        }
        SetExpr::Merge(_) | SetExpr::Table(_) => {}
    }
}

fn walk_referenced_table_with_joins(
    table: &TableWithJoins,
    ctes: &[String],
    names: &mut Vec<String>,
) {
    walk_referenced_table_factor(&table.relation, ctes, names);
    for join in &table.joins {
        walk_referenced_table_factor(&join.relation, ctes, names);
        match &join.join_operator {
            JoinOperator::AsOf {
                match_condition,
                constraint,
            } => {
                walk_referenced_expr(match_condition, ctes, names);
                if let JoinConstraint::On(expr) = constraint {
                    walk_referenced_expr(expr, ctes, names);
                }
            }
            JoinOperator::Join(constraint)
            | JoinOperator::Inner(constraint)
            | JoinOperator::Left(constraint)
            | JoinOperator::LeftOuter(constraint)
            | JoinOperator::Right(constraint)
            | JoinOperator::RightOuter(constraint)
            | JoinOperator::FullOuter(constraint)
            | JoinOperator::CrossJoin(constraint)
            | JoinOperator::Semi(constraint)
            | JoinOperator::LeftSemi(constraint)
            | JoinOperator::RightSemi(constraint)
            | JoinOperator::Anti(constraint)
            | JoinOperator::LeftAnti(constraint)
            | JoinOperator::RightAnti(constraint)
            | JoinOperator::StraightJoin(constraint) => {
                if let JoinConstraint::On(expr) = constraint {
                    walk_referenced_expr(expr, ctes, names);
                }
            }
            JoinOperator::CrossApply
            | JoinOperator::OuterApply
            | JoinOperator::ArrayJoin
            | JoinOperator::LeftArrayJoin
            | JoinOperator::InnerArrayJoin => {}
        }
    }
}

fn walk_referenced_table_factor(table: &TableFactor, ctes: &[String], names: &mut Vec<String>) {
    match table {
        TableFactor::Table { name, .. } => {
            let name = table_name_for_reference(name);
            if !ctes.iter().any(|cte| cte.eq_ignore_ascii_case(&name)) {
                names.push(name);
            }
        }
        TableFactor::Derived { subquery, .. } => walk_referenced_query(subquery, ctes, names),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => walk_referenced_table_with_joins(table_with_joins, ctes, names),
        _ => {}
    }
}

fn walk_referenced_expr(expr: &SqlExpr, ctes: &[String], names: &mut Vec<String>) {
    match expr {
        SqlExpr::Nested(inner)
        | SqlExpr::UnaryOp { expr: inner, .. }
        | SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotFalse(inner)
        | SqlExpr::Cast { expr: inner, .. } => walk_referenced_expr(inner, ctes, names),
        SqlExpr::BinaryOp { left, right, .. } => {
            walk_referenced_expr(left, ctes, names);
            walk_referenced_expr(right, ctes, names);
        }
        SqlExpr::Like { expr, pattern, .. } => {
            walk_referenced_expr(expr, ctes, names);
            walk_referenced_expr(pattern, ctes, names);
        }
        SqlExpr::InList { expr, list, .. } => {
            walk_referenced_expr(expr, ctes, names);
            for item in list {
                walk_referenced_expr(item, ctes, names);
            }
        }
        SqlExpr::Between {
            expr, low, high, ..
        } => {
            walk_referenced_expr(expr, ctes, names);
            walk_referenced_expr(low, ctes, names);
            walk_referenced_expr(high, ctes, names);
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                walk_referenced_expr(operand, ctes, names);
            }
            for when in conditions {
                walk_referenced_expr(&when.condition, ctes, names);
                walk_referenced_expr(&when.result, ctes, names);
            }
            if let Some(else_result) = else_result {
                walk_referenced_expr(else_result, ctes, names);
            }
        }
        SqlExpr::Function(function) => {
            if let FunctionArguments::List(list) = &function.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg {
                        walk_referenced_expr(expr, ctes, names);
                    }
                }
            }
        }
        SqlExpr::Subquery(query)
        | SqlExpr::Exists {
            subquery: query, ..
        } => walk_referenced_query(query, ctes, names),
        SqlExpr::InSubquery { expr, subquery, .. } => {
            walk_referenced_expr(expr, ctes, names);
            walk_referenced_query(subquery, ctes, names);
        }
        _ => {}
    }
}

/// Validates that the walk-based placeholder count for `statement` agrees with the raw `?`
/// token count in the original `sql` text it was parsed from. If they disagree, a placeholder
/// sits in a position the walk does not recognize as literal-bearing (e.g. a DDL default, a SET
/// target, or an identifier), and this returns [`HtapError::Unsupported`] rather than silently
/// under-substituting the statement.
pub fn checked_placeholder_count(sql: &str, statement: &Statement) -> Result<usize> {
    let walk_count = count_placeholders(statement);
    let raw_count = tokenizer_placeholder_count(sql)?;
    if walk_count != raw_count {
        return Err(HtapError::Unsupported(format!(
            "placeholder in an unsupported position: found {raw_count} '?' token(s) in the \
             statement text but only {walk_count} in a position this engine can substitute"
        )));
    }
    Ok(walk_count)
}

fn walk_statement(stmt: &mut Statement, visit: &mut Visit) -> Result<()> {
    match stmt {
        Statement::Insert(insert) => walk_insert(insert, visit),
        Statement::Update(update) => walk_update(update, visit),
        Statement::Delete(delete) => walk_delete(delete, visit),
        Statement::Truncate(truncate) => walk_truncate(truncate, visit),
        Statement::Query(query) => walk_query(query, visit),
        _ => Ok(()),
    }
}

fn walk_insert(insert: &mut SqlInsert, visit: &mut Visit) -> Result<()> {
    if let Some(source) = &mut insert.source {
        walk_query(source, visit)?;
    }
    Ok(())
}

fn walk_update(update: &mut SqlUpdate, visit: &mut Visit) -> Result<()> {
    for assignment in &mut update.assignments {
        walk_expr(&mut assignment.value, visit)?;
    }
    if let Some(selection) = &mut update.selection {
        walk_expr(selection, visit)?;
    }
    Ok(())
}

fn walk_delete(delete: &mut SqlDelete, visit: &mut Visit) -> Result<()> {
    if let Some(selection) = &mut delete.selection {
        walk_expr(selection, visit)?;
    }
    Ok(())
}

fn walk_truncate(truncate: &mut SqlTruncate, visit: &mut Visit) -> Result<()> {
    if let Some(partitions) = &mut truncate.partitions {
        for expr in partitions {
            walk_expr(expr, visit)?;
        }
    }
    Ok(())
}

fn walk_query(query: &mut Query, visit: &mut Visit) -> Result<()> {
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            walk_query(&mut cte.query, visit)?;
        }
    }
    walk_set_expr(&mut query.body, visit)?;
    if let Some(order_by) = &mut query.order_by {
        walk_order_by_kind(&mut order_by.kind, visit)?;
    }
    if let Some(limit_clause) = &mut query.limit_clause {
        walk_limit_clause(limit_clause, visit)?;
    }
    Ok(())
}

fn walk_set_expr(body: &mut SetExpr, visit: &mut Visit) -> Result<()> {
    match body {
        SetExpr::Select(select) => walk_select(select, visit),
        SetExpr::Query(q) => walk_query(q, visit),
        SetExpr::SetOperation { left, right, .. } => {
            walk_set_expr(left, visit)?;
            walk_set_expr(right, visit)
        }
        SetExpr::Values(values) => {
            for row in &mut values.rows {
                for e in &mut row.content {
                    walk_expr(e, visit)?;
                }
            }
            Ok(())
        }
        SetExpr::Insert(stmt) | SetExpr::Update(stmt) | SetExpr::Delete(stmt) => {
            walk_statement(stmt, visit)
        }
        SetExpr::Merge(_) | SetExpr::Table(_) => Ok(()),
    }
}

fn walk_select(select: &mut Select, visit: &mut Visit) -> Result<()> {
    for item in &mut select.projection {
        walk_select_item(item, visit)?;
    }
    for twj in &mut select.from {
        walk_table_with_joins(twj, visit)?;
    }
    if let Some(selection) = &mut select.selection {
        walk_expr(selection, visit)?;
    }
    walk_group_by(&mut select.group_by, visit)?;
    if let Some(having) = &mut select.having {
        walk_expr(having, visit)?;
    }
    Ok(())
}

fn walk_select_item(item: &mut SelectItem, visit: &mut Visit) -> Result<()> {
    match item {
        SelectItem::UnnamedExpr(e)
        | SelectItem::ExprWithAlias { expr: e, .. }
        | SelectItem::ExprWithAliases { expr: e, .. } => walk_expr(e, visit),
        SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(e), _) => {
            walk_expr(e, visit)
        }
        SelectItem::QualifiedWildcard(..) | SelectItem::Wildcard(_) => Ok(()),
    }
}

fn walk_table_with_joins(twj: &mut TableWithJoins, visit: &mut Visit) -> Result<()> {
    walk_table_factor(&mut twj.relation, visit)?;
    for join in &mut twj.joins {
        walk_table_factor(&mut join.relation, visit)?;
        walk_join_operator(&mut join.join_operator, visit)?;
    }
    Ok(())
}

fn walk_table_factor(tf: &mut TableFactor, visit: &mut Visit) -> Result<()> {
    match tf {
        TableFactor::Derived { subquery, .. } => walk_query(subquery, visit),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => walk_table_with_joins(table_with_joins, visit),
        _ => Ok(()),
    }
}

fn walk_join_operator(op: &mut JoinOperator, visit: &mut Visit) -> Result<()> {
    let constraint = match op {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => Some(c),
        JoinOperator::AsOf {
            match_condition,
            constraint,
        } => {
            walk_expr(match_condition, visit)?;
            Some(constraint)
        }
        JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => None,
    };
    if let Some(JoinConstraint::On(e)) = constraint {
        walk_expr(e, visit)?;
    }
    Ok(())
}

fn walk_group_by(gb: &mut GroupByExpr, visit: &mut Visit) -> Result<()> {
    if let GroupByExpr::Expressions(exprs, _) = gb {
        for e in exprs {
            walk_expr(e, visit)?;
        }
    }
    Ok(())
}

fn walk_order_by_kind(kind: &mut OrderByKind, visit: &mut Visit) -> Result<()> {
    if let OrderByKind::Expressions(items) = kind {
        for item in items {
            walk_expr(&mut item.expr, visit)?;
        }
    }
    Ok(())
}

fn walk_limit_clause(lc: &mut LimitClause, visit: &mut Visit) -> Result<()> {
    match lc {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if let Some(e) = limit {
                walk_expr(e, visit)?;
            }
            if let Some(off) = offset {
                walk_expr(&mut off.value, visit)?;
            }
            for e in limit_by {
                walk_expr(e, visit)?;
            }
            Ok(())
        }
        LimitClause::OffsetCommaLimit { offset, limit } => {
            walk_expr(offset, visit)?;
            walk_expr(limit, visit)
        }
    }
}

fn walk_function(func: &mut SqlFunction, visit: &mut Visit) -> Result<()> {
    if let FunctionArguments::List(list) = &mut func.args {
        for arg in &mut list.args {
            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                walk_expr(e, visit)?;
            }
        }
    }
    if let Some(WindowType::WindowSpec(spec)) = &mut func.over {
        walk_window_spec(spec, visit)?;
    }
    Ok(())
}

fn walk_window_spec(spec: &mut WindowSpec, visit: &mut Visit) -> Result<()> {
    for expr in &mut spec.partition_by {
        walk_expr(expr, visit)?;
    }
    for order_by in &mut spec.order_by {
        walk_expr(&mut order_by.expr, visit)?;
    }
    if let Some(frame) = &mut spec.window_frame {
        walk_window_frame_bound(&mut frame.start_bound, visit)?;
        if let Some(end_bound) = &mut frame.end_bound {
            walk_window_frame_bound(end_bound, visit)?;
        }
    }
    Ok(())
}

fn walk_window_frame_bound(bound: &mut WindowFrameBound, visit: &mut Visit) -> Result<()> {
    match bound {
        WindowFrameBound::Preceding(Some(expr)) | WindowFrameBound::Following(Some(expr)) => {
            walk_expr(expr, visit)
        }
        _ => Ok(()),
    }
}

/// Recursively walks every nested `Expr` form the binder accepts a literal within, invoking
/// `visit` on each placeholder found (in left-to-right, depth-first order).
fn walk_expr(expr: &mut SqlExpr, visit: &mut Visit) -> Result<()> {
    match expr {
        SqlExpr::Value(v) => {
            if matches!(v.value, SqlValue::Placeholder(_)) {
                visit(expr)?;
            }
            Ok(())
        }
        SqlExpr::Nested(inner) => walk_expr(inner, visit),
        SqlExpr::Identifier(_) | SqlExpr::CompoundIdentifier(_) => Ok(()),
        SqlExpr::UnaryOp { expr: inner, .. } => walk_expr(inner, visit),
        SqlExpr::BinaryOp { left, right, .. } => {
            walk_expr(left, visit)?;
            walk_expr(right, visit)
        }
        SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotFalse(inner) => walk_expr(inner, visit),
        SqlExpr::Like {
            expr: inner,
            pattern,
            ..
        } => {
            walk_expr(inner, visit)?;
            walk_expr(pattern, visit)
        }
        SqlExpr::InList {
            expr: inner, list, ..
        } => {
            walk_expr(inner, visit)?;
            for item in list {
                walk_expr(item, visit)?;
            }
            Ok(())
        }
        SqlExpr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            walk_expr(inner, visit)?;
            walk_expr(low, visit)?;
            walk_expr(high, visit)
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                walk_expr(o, visit)?;
            }
            for when in conditions {
                walk_expr(&mut when.condition, visit)?;
                walk_expr(&mut when.result, visit)?;
            }
            if let Some(e) = else_result {
                walk_expr(e, visit)?;
            }
            Ok(())
        }
        SqlExpr::Cast { expr: inner, .. } => walk_expr(inner, visit),
        SqlExpr::Function(func) => walk_function(func, visit),
        SqlExpr::Subquery(q) => walk_query(q, visit),
        SqlExpr::InSubquery {
            expr: inner,
            subquery,
            ..
        } => {
            walk_expr(inner, visit)?;
            walk_query(subquery, visit)
        }
        SqlExpr::Exists { subquery, .. } => walk_query(subquery, visit),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Value -> literal Expr substitution.
// ---------------------------------------------------------------------------------------------

fn value_to_expr(value: &Value) -> Result<SqlExpr> {
    Ok(match value {
        Value::Null => sql_value_expr(SqlValue::Null),
        Value::Bool(b) => sql_value_expr(SqlValue::Boolean(*b)),
        Value::Int32(i) => signed_integer_expr(i64::from(*i)),
        Value::Int64(i) => signed_integer_expr(*i),
        // The binder treats TIMESTAMP literals as raw integers (microseconds since the epoch),
        // not datetime strings; see `binder::parse_literal_value`'s `CommonDataType::Timestamp`
        // arm, which parses via the same integer path as INT64. No datetime-string formatting
        // (e.g. `htap_wire::result_codec::micros_to_datetime_string`) is involved here.
        Value::Timestamp(i) => signed_integer_expr(*i),
        Value::Float64(f) => signed_float_expr(*f)?,
        Value::Decimal { .. } => numeric_text_to_expr(&value.to_string())?,
        Value::String(s) => sql_value_expr(SqlValue::SingleQuotedString(s.clone())),
        Value::Bytes(b) => sql_value_expr(SqlValue::HexStringLiteral(hex_encode(b))),
    })
}

fn sql_value_expr(value: SqlValue) -> SqlExpr {
    SqlExpr::Value(value.into())
}

/// Builds the `Expr` for a [`ParamLiteral::NumericText`] parameter: a bare `Value::Number`
/// (magnitude only) for non-negative text, or `UnaryOp { Minus, Value::Number(magnitude) }` for a
/// leading `-` — the same shape [`signed_integer_expr`]/[`signed_float_expr`] use, so the text
/// substitutes exactly as if it had been typed into the SQL directly, at whatever precision it
/// was given (finding 2 of the Phase 11 fix pass: no `f64`/`i64` round trip).
///
/// # Errors
///
/// Returns [`HtapError::InvalidArgument`] if `text` is not valid numeric literal text: an
/// optional leading `+`/`-`, at least one digit (in the integer and/or fractional part), an
/// optional single `.` followed by digits, and an optional exponent (`e`/`E`, an optional sign,
/// and at least one digit) — with no other characters anywhere, leading or trailing.
fn numeric_text_to_expr(text: &str) -> Result<SqlExpr> {
    let magnitude = validate_numeric_text(text)?;
    let literal = sql_value_expr(SqlValue::Number(magnitude.to_string(), false));
    Ok(if text.starts_with('-') {
        negate(literal)
    } else {
        literal
    })
}

/// Validates `text` as numeric literal text (see [`numeric_text_to_expr`]'s doc comment for the
/// exact grammar) and returns the magnitude (the part after a leading sign, if any) on success.
fn validate_numeric_text(text: &str) -> Result<&str> {
    let bad =
        || HtapError::InvalidArgument(format!("invalid numeric parameter literal text: {text:?}"));
    let magnitude = text
        .strip_prefix('-')
        .or_else(|| text.strip_prefix('+'))
        .unwrap_or(text);
    let bytes = magnitude.as_bytes();
    let mut pos = 0usize;
    let mut saw_digit = false;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
        saw_digit = true;
    }
    if pos < bytes.len() && bytes[pos] == b'.' {
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return Err(bad());
    }
    if pos < bytes.len() && (bytes[pos] == b'e' || bytes[pos] == b'E') {
        pos += 1;
        if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
            pos += 1;
        }
        let exp_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == exp_start {
            return Err(bad());
        }
    }
    if pos != bytes.len() {
        return Err(bad());
    }
    Ok(magnitude)
}

/// Builds the `Expr` a real parser would build for the signed integer literal `v`: a bare
/// `Value::Number` for non-negative values, or `UnaryOp { Minus, Value::Number(magnitude) }` for
/// negative ones. This exactly mirrors what `Parser::parse_prefix` does for a `-`-prefixed
/// number (`vendor/sqlparser/src/parser/mod.rs`), which both the typed-column literal parser
/// (`binder::parse_literal_value` / `extract_number_parts`, which requires an all-digit `Number`
/// string) and the general expression binder (`binder_query::bind_literal`, whose `-9223372036
///854775808` overflows `i64` and falls back to `f64` for a *bare* `Number` string) depend on.
/// Using this shape for every negative integer keeps substitution behavior identical to binding
/// the literal SQL directly in both binders, including `i64::MIN`.
fn signed_integer_expr(v: i64) -> SqlExpr {
    if v < 0 {
        negate(sql_value_expr(SqlValue::Number(
            v.unsigned_abs().to_string(),
            false,
        )))
    } else {
        sql_value_expr(SqlValue::Number(v.to_string(), false))
    }
}

/// Builds the `Expr` for a float literal, using the same `UnaryOp`-wrapped-magnitude shape as
/// [`signed_integer_expr`] for the sign, and `{:?}` (not `{}`) for the magnitude so the token
/// text always contains a `.` or `e` (matching real tokenizer output) and is therefore always
/// parsed as a float rather than an integer by `binder_query::bind_literal`'s
/// `n.parse::<i64>()` probe.
fn signed_float_expr(v: f64) -> Result<SqlExpr> {
    if !v.is_finite() {
        return Err(HtapError::Unsupported(
            "non-finite float parameter (NaN/infinity) cannot be represented as a SQL literal"
                .into(),
        ));
    }
    let magnitude = format!("{:e}", v.abs());
    let literal = sql_value_expr(SqlValue::Number(magnitude, false));
    Ok(if v.is_sign_negative() {
        negate(literal)
    } else {
        literal
    })
}

fn negate(expr: SqlExpr) -> SqlExpr {
    SqlExpr::UnaryOp {
        op: UnaryOperator::Minus,
        expr: Box::new(expr),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// infer_placeholder_type_hints: best-effort, catalog-aware, per-placeholder type inference.
// ---------------------------------------------------------------------------------------------

/// Infers a `DataType` hint for every placeholder found by [`count_placeholders`], in the same
/// left-to-right, depth-first order, using only cheap local context (a target column's type in
/// INSERT VALUES / UPDATE SET, or the other operand's column type in a simple single-table
/// comparison / BETWEEN / IN / LIKE). `None` means the type could not be inferred locally: the
/// hint list always has exactly `count_placeholders(statement)` entries, but individual entries
/// may be `None`.
pub fn infer_placeholder_type_hints(
    statement: &Statement,
    catalog: &CatalogSnapshot,
) -> Result<Vec<Option<DataType>>> {
    let mut hints = Vec::new();
    match statement {
        Statement::Insert(insert) => infer_insert(insert, catalog, &mut hints),
        Statement::Update(update) => infer_update(update, catalog, &mut hints),
        Statement::Delete(delete) => infer_delete(delete, catalog, &mut hints),
        Statement::Truncate(truncate) => infer_truncate(truncate, catalog, &mut hints),
        Statement::Query(query) => infer_query(query, catalog, &mut hints),
        _ => {}
    }
    Ok(hints)
}

fn infer_insert(insert: &SqlInsert, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    let col_types = insert_column_types(insert, catalog);
    if let Some(source) = &insert.source {
        infer_query_with_values_hint(source, catalog, col_types.as_deref(), hints);
    }
}

fn insert_column_types(insert: &SqlInsert, catalog: &CatalogSnapshot) -> Option<Vec<DataType>> {
    let table_name = match &insert.table {
        TableObject::TableName(name) => object_name_single(name).ok()?,
        _ => return None,
    };
    let table = catalog.table_by_name(&table_name)?;
    insert
        .columns
        .iter()
        .map(|c| {
            object_name_single(c)
                .ok()
                .and_then(|name| table.schema.column_index(&name))
                .and_then(|idx| table.schema.column(idx))
                .map(|cd| cd.data_type)
        })
        .collect()
}

/// Like [`infer_query`], but if the query body is a bare `VALUES (...)` list (as in `INSERT ...
/// VALUES`), each cell in row position `i` gets the ambient hint `col_types[i]` instead of
/// `None`. Falls back to [`infer_set_expr`] for any other body shape (e.g. `INSERT ... SELECT`),
/// which the binder currently rejects but which the walk still traverses for count alignment.
fn infer_query_with_values_hint(
    query: &Query,
    catalog: &CatalogSnapshot,
    col_types: Option<&[DataType]>,
    hints: &mut Vec<Option<DataType>>,
) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            infer_query(&cte.query, catalog, hints);
        }
    }
    match query.body.as_ref() {
        SetExpr::Values(values) => {
            for row in &values.rows {
                for (i, expr) in row.content.iter().enumerate() {
                    let hint = col_types.and_then(|v| v.get(i).copied());
                    infer_expr(expr, hint, None, catalog, hints);
                }
            }
        }
        other => infer_set_expr(other, catalog, hints),
    }
    let schema = single_table_schema_of_set_expr(&query.body, catalog);
    if let Some(order_by) = &query.order_by {
        infer_order_by_kind(&order_by.kind, schema, catalog, hints);
    }
    if let Some(limit_clause) = &query.limit_clause {
        infer_limit_clause(limit_clause, catalog, hints);
    }
}

fn infer_update(update: &SqlUpdate, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    let schema = if update.table.joins.is_empty() {
        single_table_schema_of_factor(&update.table.relation, catalog)
    } else {
        None
    };
    for a in &update.assignments {
        let target_type = match &a.target {
            AssignmentTarget::ColumnName(name) => {
                let col_name = object_name_single(name).ok();
                match (schema, col_name) {
                    (Some(s), Some(n)) => s
                        .column_index(&n)
                        .and_then(|idx| s.column(idx))
                        .map(|cd| cd.data_type),
                    _ => None,
                }
            }
            AssignmentTarget::Tuple(_) => None,
        };
        infer_expr(&a.value, target_type, schema, catalog, hints);
    }
    if let Some(selection) = &update.selection {
        infer_expr(selection, None, schema, catalog, hints);
    }
}

fn infer_delete(delete: &SqlDelete, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    let tables = match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(t)
        | sqlparser::ast::FromTable::WithoutKeyword(t) => t,
    };
    let schema = if tables.len() == 1 && tables[0].joins.is_empty() {
        single_table_schema_of_factor(&tables[0].relation, catalog)
    } else {
        None
    };
    if let Some(selection) = &delete.selection {
        infer_expr(selection, None, schema, catalog, hints);
    }
}

fn infer_truncate(
    truncate: &SqlTruncate,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let Some(partitions) = &truncate.partitions {
        for expr in partitions {
            infer_expr(expr, None, None, catalog, hints);
        }
    }
}

fn infer_query(query: &Query, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    infer_query_with_values_hint(query, catalog, None, hints);
}

fn infer_set_expr(body: &SetExpr, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    match body {
        SetExpr::Select(select) => infer_select(select, catalog, hints),
        SetExpr::Query(q) => infer_query(q, catalog, hints),
        SetExpr::SetOperation { left, right, .. } => {
            infer_set_expr(left, catalog, hints);
            infer_set_expr(right, catalog, hints);
        }
        SetExpr::Values(values) => {
            for row in &values.rows {
                for e in &row.content {
                    infer_expr(e, None, None, catalog, hints);
                }
            }
        }
        SetExpr::Insert(stmt) => infer_insert_stmt(stmt, catalog, hints),
        SetExpr::Update(stmt) => infer_update_stmt(stmt, catalog, hints),
        SetExpr::Delete(stmt) => infer_delete_stmt(stmt, catalog, hints),
        SetExpr::Merge(_) | SetExpr::Table(_) => {}
    }
}

fn infer_insert_stmt(
    stmt: &Statement,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let Statement::Insert(insert) = stmt {
        infer_insert(insert, catalog, hints);
    }
}

fn infer_update_stmt(
    stmt: &Statement,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let Statement::Update(update) = stmt {
        infer_update(update, catalog, hints);
    }
}

fn infer_delete_stmt(
    stmt: &Statement,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let Statement::Delete(delete) = stmt {
        infer_delete(delete, catalog, hints);
    }
}

fn infer_select(select: &Select, catalog: &CatalogSnapshot, hints: &mut Vec<Option<DataType>>) {
    let schema = single_table_schema_of_select(select, catalog);
    for item in &select.projection {
        infer_select_item(item, schema, catalog, hints);
    }
    for twj in &select.from {
        infer_table_with_joins(twj, catalog, hints);
    }
    if let Some(selection) = &select.selection {
        infer_expr(selection, None, schema, catalog, hints);
    }
    infer_group_by(&select.group_by, schema, catalog, hints);
    if let Some(having) = &select.having {
        infer_expr(having, None, schema, catalog, hints);
    }
}

fn infer_select_item(
    item: &SelectItem,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    match item {
        SelectItem::UnnamedExpr(e)
        | SelectItem::ExprWithAlias { expr: e, .. }
        | SelectItem::ExprWithAliases { expr: e, .. } => {
            infer_expr(e, None, schema, catalog, hints)
        }
        SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(e), _) => {
            infer_expr(e, None, schema, catalog, hints)
        }
        SelectItem::QualifiedWildcard(..) | SelectItem::Wildcard(_) => {}
    }
}

fn infer_table_with_joins(
    twj: &TableWithJoins,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    infer_table_factor(&twj.relation, catalog, hints);
    for join in &twj.joins {
        infer_table_factor(&join.relation, catalog, hints);
        infer_join_operator(&join.join_operator, catalog, hints);
    }
}

fn infer_table_factor(
    tf: &TableFactor,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    match tf {
        TableFactor::Derived { subquery, .. } => infer_query(subquery, catalog, hints),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => infer_table_with_joins(table_with_joins, catalog, hints),
        _ => {}
    }
}

fn infer_join_operator(
    op: &JoinOperator,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    let constraint = match op {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => Some(c),
        JoinOperator::AsOf {
            match_condition,
            constraint,
        } => {
            infer_expr(match_condition, None, None, catalog, hints);
            Some(constraint)
        }
        JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => None,
    };
    if let Some(JoinConstraint::On(e)) = constraint {
        infer_expr(e, None, None, catalog, hints);
    }
}

fn infer_group_by(
    gb: &GroupByExpr,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let GroupByExpr::Expressions(exprs, _) = gb {
        for e in exprs {
            infer_expr(e, None, schema, catalog, hints);
        }
    }
}

/// Finding 5 of the Phase 11 fix pass: delegates to the catalog-aware [`infer_expr`] (which
/// mirrors every node [`walk_expr`] recognizes) rather than a separately-maintained subset. The
/// previous `infer_expr_no_catalog` helper only recognized `Value`/`Nested`/`BinaryOp`/`UnaryOp`,
/// so a placeholder inside any other expression shape `walk_expr` traverses — most importantly
/// `CASE` (`ORDER BY CASE WHEN ? = 1 THEN 0 ELSE 1 END` is a real MySQL client pattern) but also
/// `LIKE`/`IN`/`BETWEEN`/function calls/subqueries — produced *no* hint entry at all for that
/// placeholder, silently shortening the hint vector relative to [`count_placeholders`]'s count.
fn infer_order_by_kind(
    kind: &OrderByKind,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let OrderByKind::Expressions(items) = kind {
        for item in items {
            infer_expr(&item.expr, None, schema, catalog, hints);
        }
    }
}

/// See [`infer_order_by_kind`]'s doc comment (finding 5 of the Phase 11 fix pass): delegates to
/// the catalog-aware [`infer_expr`] instead of the removed `infer_expr_no_catalog`.
fn infer_limit_clause(
    lc: &LimitClause,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    match lc {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if let Some(e) = limit {
                infer_expr(e, Some(DataType::Int64), None, catalog, hints);
            }
            if let Some(off) = offset {
                infer_expr(&off.value, Some(DataType::Int64), None, catalog, hints);
            }
            for e in limit_by {
                infer_expr(e, None, None, catalog, hints);
            }
        }
        LimitClause::OffsetCommaLimit { offset, limit } => {
            infer_expr(offset, Some(DataType::Int64), None, catalog, hints);
            infer_expr(limit, Some(DataType::Int64), None, catalog, hints);
        }
    }
}

fn infer_function(
    func: &SqlFunction,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let FunctionArguments::List(list) = &func.args {
        for arg in &list.args {
            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                infer_expr(e, None, schema, catalog, hints);
            }
        }
    }
    if let Some(WindowType::WindowSpec(spec)) = &func.over {
        infer_window_spec(spec, schema, catalog, hints);
    }
}

fn infer_window_spec(
    spec: &WindowSpec,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    for expr in &spec.partition_by {
        infer_expr(expr, None, schema, catalog, hints);
    }
    for order_by in &spec.order_by {
        infer_expr(&order_by.expr, None, schema, catalog, hints);
    }
    if let Some(frame) = &spec.window_frame {
        infer_window_frame_bound(&frame.start_bound, schema, catalog, hints);
        if let Some(end_bound) = &frame.end_bound {
            infer_window_frame_bound(end_bound, schema, catalog, hints);
        }
    }
}

fn infer_window_frame_bound(
    bound: &WindowFrameBound,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    if let WindowFrameBound::Preceding(Some(expr)) | WindowFrameBound::Following(Some(expr)) = bound
    {
        infer_expr(expr, None, schema, catalog, hints);
    }
}

fn comparison_hints(
    op: &BinaryOperator,
    left: &SqlExpr,
    right: &SqlExpr,
    schema: Option<&Schema>,
) -> (Option<DataType>, Option<DataType>) {
    let is_cmp = matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    );
    if !is_cmp {
        return (None, None);
    }
    (
        schema.and_then(|s| column_data_type(right, s)),
        schema.and_then(|s| column_data_type(left, s)),
    )
}

/// Recursively infers placeholder type hints, mirroring [`walk_expr`]'s node set and traversal
/// order exactly (so hint indices line up with [`count_placeholders`] / [`substitute_placeholders`]
/// indices), while additionally tracking an `ambient` expected type (from the immediately
/// enclosing comparison/assignment/INSERT cell) and a single-table `schema` used to resolve bare
/// column identifiers for that ambient type.
fn infer_expr(
    expr: &SqlExpr,
    ambient: Option<DataType>,
    schema: Option<&Schema>,
    catalog: &CatalogSnapshot,
    hints: &mut Vec<Option<DataType>>,
) {
    match expr {
        SqlExpr::Value(v) => {
            if matches!(v.value, SqlValue::Placeholder(_)) {
                hints.push(ambient);
            }
        }
        SqlExpr::Nested(inner) => infer_expr(inner, ambient, schema, catalog, hints),
        SqlExpr::Identifier(_) | SqlExpr::CompoundIdentifier(_) => {}
        SqlExpr::UnaryOp { expr: inner, .. } => infer_expr(inner, None, schema, catalog, hints),
        SqlExpr::BinaryOp { left, op, right } => {
            let (left_hint, right_hint) = comparison_hints(op, left, right, schema);
            infer_expr(left, left_hint, schema, catalog, hints);
            infer_expr(right, right_hint, schema, catalog, hints);
        }
        SqlExpr::IsNull(inner)
        | SqlExpr::IsNotNull(inner)
        | SqlExpr::IsTrue(inner)
        | SqlExpr::IsNotTrue(inner)
        | SqlExpr::IsFalse(inner)
        | SqlExpr::IsNotFalse(inner) => infer_expr(inner, None, schema, catalog, hints),
        SqlExpr::Like {
            expr: inner,
            pattern,
            ..
        } => {
            infer_expr(inner, Some(DataType::String), schema, catalog, hints);
            infer_expr(pattern, Some(DataType::String), schema, catalog, hints);
        }
        SqlExpr::InList {
            expr: inner, list, ..
        } => {
            let item_hint = schema.and_then(|s| column_data_type(inner, s));
            infer_expr(inner, None, schema, catalog, hints);
            for item in list {
                infer_expr(item, item_hint, schema, catalog, hints);
            }
        }
        SqlExpr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            let bound_hint = schema.and_then(|s| column_data_type(inner, s));
            infer_expr(inner, None, schema, catalog, hints);
            infer_expr(low, bound_hint, schema, catalog, hints);
            infer_expr(high, bound_hint, schema, catalog, hints);
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                infer_expr(o, None, schema, catalog, hints);
            }
            for when in conditions {
                infer_expr(&when.condition, None, schema, catalog, hints);
                infer_expr(&when.result, None, schema, catalog, hints);
            }
            if let Some(e) = else_result {
                infer_expr(e, None, schema, catalog, hints);
            }
        }
        SqlExpr::Cast { expr: inner, .. } => infer_expr(inner, None, schema, catalog, hints),
        SqlExpr::Function(func) => infer_function(func, schema, catalog, hints),
        SqlExpr::Subquery(q) => infer_query(q, catalog, hints),
        SqlExpr::InSubquery {
            expr: inner,
            subquery,
            ..
        } => {
            infer_expr(inner, None, schema, catalog, hints);
            infer_query(subquery, catalog, hints);
        }
        SqlExpr::Exists { subquery, .. } => infer_query(subquery, catalog, hints),
        _ => {}
    }
}

fn column_data_type(expr: &SqlExpr, schema: &Schema) -> Option<DataType> {
    let mut cur = expr;
    while let SqlExpr::Nested(inner) = cur {
        cur = inner;
    }
    match cur {
        SqlExpr::Identifier(ident) => schema
            .column_index(&ident.value)
            .and_then(|idx| schema.column(idx))
            .map(|cd| cd.data_type),
        _ => None,
    }
}

fn single_table_schema_of_factor<'c>(
    tf: &TableFactor,
    catalog: &'c CatalogSnapshot,
) -> Option<&'c Schema> {
    match tf {
        TableFactor::Table { name, .. } => {
            let table_name = object_name_single(name).ok()?;
            catalog.table_by_name(&table_name).map(|t| &t.schema)
        }
        _ => None,
    }
}

fn single_table_schema_of_select<'c>(
    select: &Select,
    catalog: &'c CatalogSnapshot,
) -> Option<&'c Schema> {
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return None;
    }
    single_table_schema_of_factor(&select.from[0].relation, catalog)
}

fn single_table_schema_of_set_expr<'c>(
    se: &SetExpr,
    catalog: &'c CatalogSnapshot,
) -> Option<&'c Schema> {
    match se {
        SetExpr::Select(select) => single_table_schema_of_select(select, catalog),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// resolve_prepare_output_schema
// ---------------------------------------------------------------------------------------------

/// Resolves the output column schema of a statement, if it can be determined without knowing
/// the runtime parameter values.
///
/// - INSERT / UPDATE / DELETE always produce no result set: `Some(vec![])`.
/// - A `Statement::Query` (SELECT) resolves to the real output schema only when every
///   placeholder's type is statically inferable via [`infer_placeholder_type_hints`]; the
///   statement is bound against representative non-NULL probe values of those types through the
///   real, unmodified binder (never a special-cased schema-only path), so the result matches
///   exactly what executing the prepared statement would report. Any inference gap (e.g.
///   `SELECT ? AS x`, where the placeholder's type cannot be inferred from context) yields
///   `None`.
/// - Anything else (SHOW/DESCRIBE, DDL, and any statement kind not preparable in this engine)
///   yields `None`: this module cannot derive their output schema without executing them (SHOW's
///   result columns are computed in `htap-server`, outside this crate).
///
/// Errors from binding are only returned once every placeholder has a known type (i.e. they are
/// genuine errors independent of the missing-type-hint case, such as an unknown table).
pub fn resolve_prepare_output_schema(
    statement: &Statement,
    catalog: &CatalogSnapshot,
) -> Result<Option<Vec<ColumnDef>>> {
    match statement {
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => Ok(Some(Vec::new())),
        Statement::Query(_) => {
            let hints = infer_placeholder_type_hints(statement, catalog)?;
            // Defensive (finding 5 of the Phase 11 fix pass): `infer_placeholder_type_hints` is
            // meant to always produce exactly `count_placeholders(statement)` entries (mirroring
            // `walk_expr`'s traversal; see `infer_expr`'s doc comment), but if some future
            // expression shape ever falls out of sync between the two walks again, treat that as
            // "could not resolve" rather than let a length mismatch reach `substitute_placeholders`
            // below as a confusing `InvalidArgument` error.
            if hints.len() != count_placeholders(statement) || hints.iter().any(Option::is_none) {
                return Ok(None);
            }
            let probe_values: Vec<Value> = hints
                .into_iter()
                .map(|h| probe_value_for(h.expect("checked for None above")))
                .collect::<Result<Vec<_>>>()?;
            let mut probe = statement.clone();
            substitute_placeholders(&mut probe, &probe_values)?;
            let bound = bind(&probe, catalog)?;
            Ok(Some(output_schema_of(&bound, catalog)?))
        }
        _ => Ok(None),
    }
}

fn probe_value_for(dt: DataType) -> Result<Value> {
    match dt {
        DataType::Bool => Ok(Value::Bool(false)),
        DataType::Int32 => Ok(Value::Int32(0)),
        DataType::Int64 => Ok(Value::Int64(0)),
        DataType::Float64 => Ok(Value::Float64(0.0)),
        DataType::String => Ok(Value::String(String::new())),
        DataType::Bytes => Ok(Value::Bytes(Vec::new())),
        DataType::Timestamp => Ok(Value::Timestamp(0)),
        DataType::Decimal { precision, scale } => Ok(Value::Decimal {
            value: 0,
            precision,
            scale,
        }),
    }
}

fn output_schema_of(bound: &BoundStatement, catalog: &CatalogSnapshot) -> Result<Vec<ColumnDef>> {
    match bound {
        BoundStatement::Select(point) => {
            let table = catalog.table_by_name(&point.table).ok_or_else(|| {
                HtapError::Internal(format!(
                    "table '{}' missing from catalog after bind",
                    point.table
                ))
            })?;
            point
                .projection
                .iter()
                .map(|&idx| {
                    table.schema.column(idx).cloned().ok_or_else(|| {
                        HtapError::Internal(format!(
                            "projection index {idx} out of range for table '{}'",
                            point.table
                        ))
                    })
                })
                .collect()
        }
        BoundStatement::AnalyticSelect(sel) => Ok(sel.output_schema().columns().to_vec()),
        BoundStatement::Query(q) => Ok(q.as_ref().output_columns.clone()),
        other => Err(HtapError::Internal(format!(
            "unexpected bound statement kind for a Query: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_table_names_walks_queries_and_excludes_ctes() {
        let statement = crate::parse_one(
            "WITH x AS (SELECT id FROM a) \
             SELECT (SELECT id FROM b) FROM x JOIN c ON x.id = c.id \
             UNION SELECT id FROM d",
        )
        .unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn referenced_table_names_walks_insert_update_and_delete_subqueries() {
        let insert = crate::parse_one(
            "INSERT INTO dst (id) SELECT id FROM src WHERE id IN (SELECT id FROM x)",
        )
        .unwrap();
        assert_eq!(referenced_table_names(&insert), vec!["dst", "src", "x"]);

        let update = crate::parse_one(
            "UPDATE dst SET id = (SELECT id FROM src) WHERE id IN (SELECT id FROM x)",
        )
        .unwrap();
        assert_eq!(referenced_table_names(&update), vec!["dst", "src", "x"]);

        let delete = crate::parse_one("DELETE FROM dst WHERE id IN (SELECT id FROM src)").unwrap();
        assert_eq!(referenced_table_names(&delete), vec!["dst", "src"]);
    }

    #[test]
    fn referenced_table_names_simple_select_and_qualified_select() {
        let statement = crate::parse_one("SELECT * FROM a").unwrap();
        assert_eq!(referenced_table_names(&statement), vec!["a"]);

        let qualified = crate::parse_one("SELECT * FROM htap.a").unwrap();
        assert_eq!(referenced_table_names(&qualified), vec!["a"]);
    }

    #[test]
    fn referenced_table_names_walks_join_subquery_cte_and_union() {
        let join = crate::parse_one("SELECT * FROM a JOIN b ON a.id = b.id").unwrap();
        assert_eq!(referenced_table_names(&join), vec!["a", "b"]);

        let subquery = crate::parse_one("SELECT * FROM a WHERE id IN (SELECT id FROM b)").unwrap();
        assert_eq!(referenced_table_names(&subquery), vec!["a", "b"]);

        let cte =
            crate::parse_one("WITH x AS (SELECT id FROM a) SELECT * FROM x JOIN b ON x.id = b.id")
                .unwrap();
        assert_eq!(referenced_table_names(&cte), vec!["a", "b"]);

        let union = crate::parse_one("SELECT id FROM a UNION SELECT id FROM b").unwrap();
        assert_eq!(referenced_table_names(&union), vec!["a", "b"]);
    }

    #[test]
    fn referenced_table_names_cte_body_does_not_see_itself() {
        let statement =
            crate::parse_one("WITH secret AS (SELECT * FROM secret) SELECT * FROM secret").unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["secret"]);
    }

    #[test]
    fn referenced_table_names_recursive_cte_body_sees_itself() {
        let statement = crate::parse_one(
            "WITH RECURSIVE x AS (SELECT id FROM source UNION ALL SELECT id FROM x) \
             SELECT * FROM x",
        )
        .unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["source"]);
    }

    #[test]
    fn referenced_table_names_recursive_cte_body_sees_itself_in_exists_subquery() {
        let statement = crate::parse_one(
            "WITH RECURSIVE x AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t \
             WHERE EXISTS (SELECT * FROM x)) SELECT * FROM x",
        )
        .unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["t"]);
    }

    #[test]
    fn referenced_table_names_cte_body_does_not_see_later_ctes() {
        let statement = crate::parse_one(
            "WITH a AS (SELECT * FROM secret), secret AS (SELECT 1 AS id) SELECT * FROM a",
        )
        .unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["secret"]);
    }

    #[test]
    fn referenced_table_names_chained_ctes_exclude_prior_cte_names() {
        let statement = crate::parse_one(
            "WITH a AS (SELECT * FROM source), b AS (SELECT * FROM a) SELECT * FROM b",
        )
        .unwrap();

        assert_eq!(referenced_table_names(&statement), vec!["source"]);
    }

    #[test]
    fn referenced_table_names_walks_dml_sources() {
        let insert = crate::parse_one("INSERT INTO dst (id) SELECT id FROM src").unwrap();
        assert_eq!(referenced_table_names(&insert), vec!["dst", "src"]);

        let update = crate::parse_one(
            "UPDATE dst SET id = (SELECT id FROM src) WHERE id IN (SELECT id FROM x)",
        )
        .unwrap();
        assert_eq!(referenced_table_names(&update), vec!["dst", "src", "x"]);

        let delete = crate::parse_one("DELETE FROM dst WHERE id IN (SELECT id FROM src)").unwrap();
        assert_eq!(referenced_table_names(&delete), vec!["dst", "src"]);
    }
}
