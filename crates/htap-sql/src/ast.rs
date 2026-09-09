//! SQL Abstract Syntax Tree and bound statement definitions.

use htap_common::error::{HtapError, Result};
use htap_common::types::{Row, Schema, Value};
use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

/// Parses a single SQL statement using [`MySqlDialect`].
///
/// Rejects empty input or multi-statement input with [`HtapError::InvalidArgument`].
/// Also maps parser syntax errors to [`HtapError::InvalidArgument`].
pub fn parse_one(sql: &str) -> Result<Statement> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(HtapError::InvalidArgument("statement is empty".to_string()));
    }

    let dialect = MySqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, trimmed)
        .map_err(|err| HtapError::InvalidArgument(format!("SQL parse error: {err}")))?;

    match statements.len() {
        0 => Err(HtapError::InvalidArgument("statement is empty".to_string())),
        1 => Ok(statements.remove(0)),
        n => Err(HtapError::InvalidArgument(format!(
            "expected exactly one SQL statement, found {n}"
        ))),
    }
}

/// A validated, catalog-bound SQL statement ready for planning and execution.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundStatement {
    /// CREATE TABLE statement.
    CreateTable(CreateTable),
    /// INSERT statement.
    Insert(Insert),
    /// Single-row DELETE by primary key.
    Delete(DeleteByPrimaryKey),
    /// Single-row SELECT by primary key with column projection.
    Select(PointSelect),
}

/// Bound representation of a CREATE TABLE statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTable {
    /// Target table name.
    pub name: String,
    /// Table schema definition.
    pub schema: Schema,
    /// Indices of primary key columns in `schema`.
    pub primary_key: Vec<usize>,
}

impl CreateTable {
    /// Create a new [`CreateTable`] bound statement.
    pub fn new(name: impl Into<String>, schema: Schema, primary_key: Vec<usize>) -> Self {
        Self {
            name: name.into(),
            schema,
            primary_key,
        }
    }
}

/// Bound representation of an INSERT statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// Target table name.
    pub table: String,
    /// Rows to insert.
    pub rows: Vec<Row>,
}

impl Insert {
    /// Create a new [`Insert`] bound statement.
    pub fn new(table: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            table: table.into(),
            rows,
        }
    }
}

/// Bound representation of a single-row DELETE by primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteByPrimaryKey {
    /// Target table name.
    pub table: String,
    /// Primary key value tuple.
    pub key: Vec<Value>,
}

impl DeleteByPrimaryKey {
    /// Create a new [`DeleteByPrimaryKey`] bound statement.
    pub fn new(table: impl Into<String>, key: Vec<Value>) -> Self {
        Self {
            table: table.into(),
            key,
        }
    }
}

/// Bound representation of a single-row point SELECT by primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointSelect {
    /// Target table name.
    pub table: String,
    /// Projected column indices from schema.
    pub projection: Vec<usize>,
    /// Primary key value tuple.
    pub key: Vec<Value>,
}

impl PointSelect {
    /// Create a new [`PointSelect`] bound statement.
    pub fn new(table: impl Into<String>, projection: Vec<usize>, key: Vec<Value>) -> Self {
        Self {
            table: table.into(),
            projection,
            key,
        }
    }
}

impl From<CreateTable> for BoundStatement {
    fn from(stmt: CreateTable) -> Self {
        Self::CreateTable(stmt)
    }
}

impl From<Insert> for BoundStatement {
    fn from(stmt: Insert) -> Self {
        Self::Insert(stmt)
    }
}

impl From<DeleteByPrimaryKey> for BoundStatement {
    fn from(stmt: DeleteByPrimaryKey) -> Self {
        Self::Delete(stmt)
    }
}

impl From<PointSelect> for BoundStatement {
    fn from(stmt: PointSelect) -> Self {
        Self::Select(stmt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::types::{ColumnDef, DataType};

    #[test]
    fn test_bound_statement_conversions() {
        let schema = Schema::new(vec![ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        }])
        .unwrap();

        let create = CreateTable::new("users", schema, vec![0]);
        let bound_create: BoundStatement = create.clone().into();
        assert_eq!(bound_create, BoundStatement::CreateTable(create));

        let insert = Insert::new("users", vec![Row::new(vec![Value::Int64(1)])]);
        let bound_insert: BoundStatement = insert.clone().into();
        assert_eq!(bound_insert, BoundStatement::Insert(insert));

        let delete = DeleteByPrimaryKey::new("users", vec![Value::Int64(1)]);
        let bound_delete: BoundStatement = delete.clone().into();
        assert_eq!(bound_delete, BoundStatement::Delete(delete));

        let select = PointSelect::new("users", vec![0], vec![Value::Int64(1)]);
        let bound_select: BoundStatement = select.clone().into();
        assert_eq!(bound_select, BoundStatement::Select(select));
    }
}
