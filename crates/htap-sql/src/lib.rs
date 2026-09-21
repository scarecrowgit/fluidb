//! SQL parsing, binding, result types, and routing for the HTAP storage engine.
//!
//! Statement execution is implemented in `htap-server`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
pub mod binder;
mod binder_query;
pub mod expr;
pub mod optimize;
pub mod prepare;
pub mod query;
pub mod result;
pub mod route;
pub mod variables;

/// Constructs the canonical missing-table error used by binding and privilege masking.
pub fn table_not_found(name: &str) -> htap_common::error::HtapError {
    htap_common::error::HtapError::NotFound(format!("table '{name}' not found"))
}

pub use ast::{
    parse_many, parse_one, AggregateFunction, AlterPartitions, AlterUserStatement, AnalyticExpr,
    AnalyticFilter, AnalyticOrderBy, AnalyticSelect, BoundStatement, ComparisonOp, CreateTable,
    CreateUserStatement, DeleteStatement, DeleteTarget, DropTableStatement, DropUserStatement,
    GrantScope, GrantStatement, Insert, InsertSource, PointSelect, RevokeStatement,
    ShowGrantsStatement, ShowStatement, UpdateStatement, UpdateTarget,
};
pub use binder::bind;
pub use expr::{
    AggFn, AggregateSpec, BinOp, EvalContext, Expr, ExprType, ScalarFn, VariableLookup,
};
pub use prepare::{
    checked_placeholder_count, count_placeholders, infer_placeholder_type_hints,
    referenced_table_names, resolve_prepare_output_schema, substitute_placeholders,
    substitute_placeholders_ext, tokenizer_placeholder_count, ParamLiteral,
};
pub use query::{
    BoundQuery, JoinKind, JoinSpec, JoinTree, OrderItem, PeerFrameBound, ProjectionItem, QueryBody,
    RowFrameBound, SelectBody, SetOpKind, TableSlot, ValueFrameBound, WindowFrame,
    WindowFrameDirection, WindowFunctionKind, WindowSpec,
};
pub use result::{CommandResult, QueryResult, StatementResult};
pub use route::{classify_route, Route};
pub use variables::{
    classify_set_target, parse_autocommit_value, system_variable_value, validate_isolation_level,
    SessionVarsView, SetClass, SetScope, DEFAULT_MAX_ALLOWED_PACKET, REPORTED_VERSION,
};
