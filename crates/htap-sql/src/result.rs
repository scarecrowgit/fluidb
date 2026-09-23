//! Execution results for SQL statements.

use htap_common::types::{ColumnDef, Row, Value};
use htap_common::version::Version;
use serde::{Deserialize, Serialize};

/// Result of executing a SQL statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Sorts rows in-place by output column index, ascending direction, and NULL ordering,
    /// with deterministic tie-breaking using the full output row.
    ///
    /// # NULL Ordering Policy
    /// - If `nulls_first` is `true`, `Value::Null` sorts before any non-NULL value.
    /// - If `nulls_first` is `false`, `Value::Null` sorts after any non-NULL value.
    /// - When comparing two `Value::Null`s, they are considered equal.
    /// - Non-NULL values are compared according to `asc` (true for ascending, false for descending).
    /// - Equal comparisons on all specified columns are tie-broken deterministically by comparing
    ///   the full output row values.
    pub fn sort_by(&mut self, order_specs: &[(usize, bool, bool)]) {
        if order_specs.is_empty() {
            return;
        }
        self.rows.sort_by(|row_a, row_b| {
            for &(col_idx, asc, nulls_first) in order_specs {
                let val_a = row_a.get(col_idx).unwrap_or(&Value::Null);
                let val_b = row_b.get(col_idx).unwrap_or(&Value::Null);
                let cmp = match (val_a.is_null(), val_b.is_null()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => {
                        if nulls_first {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if nulls_first {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    }
                    (false, false) => {
                        let ord = val_a.cmp(val_b);
                        if asc {
                            ord
                        } else {
                            ord.reverse()
                        }
                    }
                };
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            row_a.values().cmp(row_b.values())
        });
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
    use serde_json;

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

    #[test]
    fn test_query_result_sorting_and_null_policy() {
        let cols = vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "val".into(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
            },
        ];

        let make_qr = || {
            QueryResult::new(
                cols.clone(),
                vec![
                    Row::new(vec![Value::Int32(1), Value::Int32(20)]),
                    Row::new(vec![Value::Int32(2), Value::Null]),
                    Row::new(vec![Value::Int32(3), Value::Int32(10)]),
                    Row::new(vec![Value::Int32(4), Value::Int32(20)]),
                ],
            )
        };

        // ASC, NULLS FIRST
        let mut qr_asc_nf = make_qr();
        qr_asc_nf.sort_by(&[(1, true, true)]);
        assert_eq!(
            qr_asc_nf.rows(),
            &[
                Row::new(vec![Value::Int32(2), Value::Null]),
                Row::new(vec![Value::Int32(3), Value::Int32(10)]),
                Row::new(vec![Value::Int32(1), Value::Int32(20)]), // tie-broken by id: 1 < 4
                Row::new(vec![Value::Int32(4), Value::Int32(20)]),
            ]
        );

        // ASC, NULLS LAST
        let mut qr_asc_nl = make_qr();
        qr_asc_nl.sort_by(&[(1, true, false)]);
        assert_eq!(
            qr_asc_nl.rows(),
            &[
                Row::new(vec![Value::Int32(3), Value::Int32(10)]),
                Row::new(vec![Value::Int32(1), Value::Int32(20)]),
                Row::new(vec![Value::Int32(4), Value::Int32(20)]),
                Row::new(vec![Value::Int32(2), Value::Null]),
            ]
        );

        // DESC, NULLS LAST
        let mut qr_desc_nl = make_qr();
        qr_desc_nl.sort_by(&[(1, false, false)]);
        assert_eq!(
            qr_desc_nl.rows(),
            &[
                Row::new(vec![Value::Int32(1), Value::Int32(20)]),
                Row::new(vec![Value::Int32(4), Value::Int32(20)]),
                Row::new(vec![Value::Int32(3), Value::Int32(10)]),
                Row::new(vec![Value::Int32(2), Value::Null]),
            ]
        );

        // DESC, NULLS FIRST
        let mut qr_desc_nf = make_qr();
        qr_desc_nf.sort_by(&[(1, false, true)]);
        assert_eq!(
            qr_desc_nf.rows(),
            &[
                Row::new(vec![Value::Int32(2), Value::Null]),
                Row::new(vec![Value::Int32(1), Value::Int32(20)]),
                Row::new(vec![Value::Int32(4), Value::Int32(20)]),
                Row::new(vec![Value::Int32(3), Value::Int32(10)]),
            ]
        );
    }

    #[test]
    fn test_statement_result_serializes_as_command() {
        let result = StatementResult::dml(3, Some(Version::new(17)));

        let json = serde_json::to_string(&result).expect("statement result should serialize");

        assert_eq!(json, r#"{"Command":{"Dml":{"affected":3,"version":17}}}"#);
    }

    #[test]
    fn test_statement_result_deserializes_as_query() {
        let expected = StatementResult::query(
            vec![ColumnDef {
                name: "count".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
            }],
            vec![Row::new(vec![Value::Int64(9)])],
        );

        let json = serde_json::to_string(&expected).expect("statement result should serialize");
        let result: StatementResult =
            serde_json::from_str(&json).expect("statement result should deserialize");

        assert_eq!(result, expected);
    }

    #[test]
    fn test_query_result_round_trip_serialization() {
        let result = QueryResult::new(
            vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                },
                ColumnDef {
                    name: "note".into(),
                    data_type: DataType::String,
                    nullable: true,
                    primary_key: false,
                },
            ],
            vec![
                Row::new(vec![Value::Int32(1), Value::String("ready".into())]),
                Row::new(vec![Value::Int32(2), Value::Null]),
            ],
        );

        let json = serde_json::to_string(&result).expect("query result should serialize");
        let decoded: QueryResult =
            serde_json::from_str(&json).expect("query result should deserialize");

        assert_eq!(decoded, result);
    }

    #[test]
    fn test_command_result_round_trip_serialization() {
        let cases = [
            CommandResult::ddl(2),
            CommandResult::dml(4, None),
            CommandResult::dml(6, Some(Version::new(23))),
        ];

        for result in cases {
            let json = serde_json::to_string(&result).expect("command result should serialize");
            let decoded: CommandResult =
                serde_json::from_str(&json).expect("command result should deserialize");

            assert_eq!(decoded, result);
        }
    }
}
