//! A non-audited TPC-H-derived schema and workload support crate.

pub mod params;
pub mod queries;
pub mod scale_factor;
pub mod schema;

pub use params::{fixed_parameters, QueryParameters};
pub use queries::query;
pub use scale_factor::{scale_factor, ScaleFactorError};
pub use schema::{ddl_statements, TABLE_NAMES};
