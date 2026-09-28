//! A non-audited TPC-H-derived schema and workload support crate.

pub mod drivers;
pub mod generate;
pub mod load;
pub mod params;
pub mod queries;
pub mod refresh;
pub mod scale_factor;
pub mod schema;

pub use drivers::{
    power_test, query_order, table_11_stream_count, throughput_test, DriverError, PowerTestReport,
    QueryTiming, RefreshTiming, StreamReport, ThroughputTestReport, POWER_METRIC_LABEL,
    THROUGHPUT_METRIC_LABEL,
};
pub use generate::{generate, Dataset};
pub use load::{load_dataset, LoadOptions, LoadReport};
pub use params::{fixed_parameters, QueryParameters};
pub use queries::{query, QueryError};
pub use refresh::{
    apply_one_order_delete, apply_one_order_insert, generate_rf1_rows, generate_rf2_plan,
    rf1_new_sales, rf2_old_sales, RefreshError, Rf1Rows,
};
pub use scale_factor::{scale_factor, ScaleFactorError};
pub use schema::{ddl_statements, TABLE_NAMES};
