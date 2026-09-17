//! SQL parsing, binding, result types, and routing for the HTAP storage engine.
//!
//! Statement execution is implemented in `htap-server`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
pub mod binder;
mod binder_query;
pub mod expr;
pub mod query;
pub mod result;
pub mod route;
pub mod variables;

pub use ast::{
    parse_one, AggregateFunction, AlterPartitions, AnalyticExpr, AnalyticFilter, AnalyticOrderBy,
    AnalyticSelect, BoundStatement, ComparisonOp, CreateTable, DeleteByPrimaryKey,
    DropTableStatement, Insert, PointSelect, ShowStatement, UpdateStatement, UpdateTarget,
};
pub use binder::bind;
pub use expr::{
    AggFn, AggregateSpec, BinOp, EvalContext, Expr, ExprType, ScalarFn, VariableLookup,
};
pub use query::{
    BoundQuery, JoinKind, JoinSpec, OrderItem, ProjectionItem, QueryBody, SelectBody, SetOpKind,
    TableSlot,
};
pub use result::{CommandResult, QueryResult, StatementResult};
pub use route::{classify_route, Route};
pub use variables::{
    classify_set_target, parse_autocommit_value, system_variable_value, validate_isolation_level,
    SessionVarsView, SetClass, SetScope, REPORTED_VERSION,
};
