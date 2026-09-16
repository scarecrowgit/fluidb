//! SQL parsing, binding, result types, and routing for the HTAP storage engine.
//!
//! Statement execution is implemented in `htap-server`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
pub mod binder;
pub mod result;
pub mod route;

pub use ast::{
    parse_one, AggregateFunction, AlterPartitions, AnalyticExpr, AnalyticFilter, AnalyticOrderBy,
    AnalyticSelect, BoundStatement, ComparisonOp, CreateTable, DeleteByPrimaryKey, Insert,
    PointSelect,
};
pub use binder::bind;
pub use result::{CommandResult, QueryResult, StatementResult};
pub use route::{classify_route, Route};
