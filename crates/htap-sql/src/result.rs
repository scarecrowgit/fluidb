//! Execution results for SQL statements.

use htap_common::types::{ColumnDef, Row};
use htap_common::version::Version;

/// Result of executing a SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    /// Command execution result (DDL or DML).
    Command(CommandResult),
    /// Query execution result (tabular rows and column metadata).
    Query(QueryResult),
}

impl StatementResult {
    /// Create a DDL command result.
    pub fn ddl(affected: u64) -> Self {
        Self::Command(CommandResult::Ddl { affected })
    }

    /// Create a DML command result.
    pub fn dml(affected: u64, version: Option<Version>) -> Self {
        Self::Command(CommandResult::Dml { affected, version })
    }

    /// Create a query result.
    pub fn query(columns: Vec<ColumnDef>, rows: Vec<Row>) -> Self {
        Self::Query(QueryResult::new(columns, rows))
    }
}

/// Result of executing a mutating or schema-modifying command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// Result of a DDL statement (e.g., CREATE TABLE).
    Ddl {
        /// Number of schema objects affected.
        affected: u64,
    },
    /// Result of a DML statement (INSERT, DELETE, UPDATE).
    Dml {
        /// Number of rows affected.
        affected: u64,
        /// Commit version assigned if applicable.
        version: Option<Version>,
    },
}

impl CommandResult {
    /// Create a new DDL command result.
    pub fn ddl(affected: u64) -> Self {
        Self::Ddl { affected }
    }

    /// Create a new DML command result.
    pub fn dml(affected: u64, version: Option<Version>) -> Self {
        Self::Dml { affected, version }
    }

    /// Number of rows or schema objects affected by this command.
    pub fn affected(&self) -> u64 {
        match self {
            Self::Ddl { affected } => *affected,
            Self::Dml { affected, .. } => *affected,
        }
    }

    /// Commit version assigned by this command, if applicable.
    pub fn version(&self) -> Option<Version> {
        match self {
            Self::Ddl { .. } => None,
            Self::Dml { version, .. } => *version,
        }
    }
}

/// Tabular result of executing a query statement.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Column definitions describing the projected output columns.
    pub columns: Vec<ColumnDef>,
    /// Result rows conforming positionally to `columns`.
    pub rows: Vec<Row>,
}

impl QueryResult {
    /// Create a new query result from column definitions and rows.
    pub fn new(columns: Vec<ColumnDef>, rows: Vec<Row>) -> Self {
        Self { columns, rows }
    }

    /// Create an empty query result with given column definitions.
    pub fn empty(columns: Vec<ColumnDef>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Number of rows returned.
    pub fn num_rows(&self) -> usize {
        self.rows.len()
    }

    /// Whether no rows were returned.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Projected columns.
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Rows returned.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Deconstruct into columns and rows.
    pub fn into_parts(self) -> (Vec<ColumnDef>, Vec<Row>) {
        (self.columns, self.rows)
    }
}

impl From<CommandResult> for StatementResult {
    fn from(cmd: CommandResult) -> Self {
        Self::Command(cmd)
    }
}

impl From<QueryResult> for StatementResult {
    fn from(query: QueryResult) -> Self {
        Self::Query(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::types::{DataType, Value};

    #[test]
    fn test_command_result_accessors_and_conversions() {
        let ddl = CommandResult::ddl(1);
        assert_eq!(ddl.affected(), 1);
        assert_eq!(ddl.version(), None);
        let stmt_ddl: StatementResult = ddl.clone().into();
        assert_eq!(stmt_ddl, StatementResult::Command(ddl));

        let v = Version::new(42);
        let dml = CommandResult::dml(5, Some(v));
        assert_eq!(dml.affected(), 5);
        assert_eq!(dml.version(), Some(v));
        let stmt_dml: StatementResult = dml.clone().into();
        assert_eq!(stmt_dml, StatementResult::Command(dml));
    }

    #[test]
    fn test_query_result_accessors_and_conversions() {
        let cols = vec![ColumnDef {
            name: "val".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        }];
        let empty = QueryResult::empty(cols.clone());
        assert!(empty.is_empty());
        assert_eq!(empty.num_rows(), 0);
        assert_eq!(empty.columns().len(), 1);

        let rows = vec![Row::new(vec![Value::Int64(100)])];
        let non_empty = QueryResult::new(cols.clone(), rows.clone());
        assert!(!non_empty.is_empty());
        assert_eq!(non_empty.num_rows(), 1);
        assert_eq!(non_empty.rows().len(), 1);

        let (c, r) = non_empty.clone().into_parts();
        assert_eq!(c, cols);
        assert_eq!(r, rows);

        let stmt_query: StatementResult = non_empty.clone().into();
        assert_eq!(stmt_query, StatementResult::Query(non_empty));
    }
}
