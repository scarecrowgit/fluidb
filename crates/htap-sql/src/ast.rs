//! SQL Abstract Syntax Tree and bound statement definitions.

use htap_catalog::{PartitionAlteration, PrivilegeSet};
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Schema, Value};
use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::expr::Expr;
use crate::query::BoundQuery;

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

/// Parses SQL text containing one or more `;`-separated statements using [`MySqlDialect`]
/// (Phase 11 plan task 10, `CLIENT_MULTI_STATEMENTS`).
///
/// Empty statements produced by extra or trailing `;` (e.g. the second half of `"SELECT 1;;"`,
/// or all of `";"`) are dropped by the underlying parser and never appear in the result — verified
/// directly against `vendor/sqlparser`. Rejects input that is empty after trimming whitespace and
/// `;` with [`HtapError::InvalidArgument`], and maps parser syntax errors the same way
/// [`parse_one`] does (including statement shapes the shim answers, like `SET CHARACTER SET`,
/// which have no AST node at all and so fail the whole batch).
pub fn parse_many(sql: &str) -> Result<Vec<Statement>> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(HtapError::InvalidArgument("statement is empty".to_string()));
    }

    let dialect = MySqlDialect {};
    let statements = Parser::parse_sql(&dialect, trimmed)
        .map_err(|err| HtapError::InvalidArgument(format!("SQL parse error: {err}")))?;

    if statements.is_empty() {
        return Err(HtapError::InvalidArgument("statement is empty".to_string()));
    }
    Ok(statements)
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
    /// Analytical SELECT statement with column/aggregate projections, filters, and optional grouping.
    AnalyticSelect(AnalyticSelect),
    /// ALTER TABLE partition statement (ADD, DROP, REORGANIZE).
    AlterPartitions(AlterPartitions),
    /// General query (joins, expressions, subqueries, set operations, LIMIT, ...).
    Query(BoundQuery),
    /// UPDATE statement.
    Update(UpdateStatement),
    /// DROP TABLE statement.
    DropTable(DropTableStatement),
    /// SHOW / DESCRIBE statement (catalog metadata read).
    Show(ShowStatement),
    /// CREATE USER statement.
    CreateUser(CreateUserStatement),
    /// ALTER USER statement.
    AlterUser(AlterUserStatement),
    /// DROP USER statement.
    DropUser(DropUserStatement),
    /// GRANT privileges statement.
    GrantPrivileges(GrantStatement),
    /// REVOKE privileges statement.
    RevokePrivileges(RevokeStatement),
    /// SHOW GRANTS statement.
    ShowGrants(ShowGrantsStatement),
}

/// Scope to which a GRANT or REVOKE applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantScope {
    /// All objects in the default `htap` schema.
    Global,
    /// One table.
    Table(String),
}

/// Bound representation of CREATE USER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserStatement {
    /// Account username.
    pub username: String,
    /// Whether `IF NOT EXISTS` was specified.
    pub if_not_exists: bool,
    /// Plaintext password, hashed immediately before persistence by htap-server.
    pub password: Option<String>,
}

/// Bound representation of ALTER USER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterUserStatement {
    /// Account username.
    pub username: String,
    /// Whether `IF EXISTS` was specified.
    pub if_exists: bool,
    /// Plaintext replacement password, hashed immediately before persistence by htap-server.
    pub password: String,
}

/// Bound representation of DROP USER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropUserStatement {
    /// Account usernames.
    pub usernames: Vec<String>,
    /// Whether `IF EXISTS` was specified.
    pub if_exists: bool,
}

/// Bound representation of GRANT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStatement {
    /// Privileges to grant.
    pub privileges: PrivilegeSet,
    /// Object scope.
    pub scope: GrantScope,
    /// Account receiving the privileges.
    pub grantee: String,
}

/// Bound representation of REVOKE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeStatement {
    /// Privileges to revoke.
    pub privileges: PrivilegeSet,
    /// Object scope.
    pub scope: GrantScope,
    /// Account losing the privileges.
    pub grantee: String,
}

/// Bound representation of SHOW GRANTS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowGrantsStatement {
    /// Account whose grants are displayed; `None` means the current account.
    pub for_username: Option<String>,
}

/// Bound representation of an UPDATE statement.
///
/// Assignment expressions and the filter reference the target table as slot 0.
/// Assignments are applied left to right against the progressively updated row
/// (`SET a = a + 1, b = a` uses the new `a` for `b`).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    /// Target table name.
    pub table: String,
    /// `(column index, value expression)` pairs; values are already coerced to the column type.
    pub assignments: Vec<(usize, Expr)>,
    /// Which rows to update.
    pub target: UpdateTarget,
}

/// Row selection of an UPDATE.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateTarget {
    /// The WHERE clause names the complete primary key: a single point read-modify-write.
    PrimaryKey(Vec<Value>),
    /// Scan-based update; `None` updates every row.
    Filter(Option<Expr>),
}

/// Bound representation of a DROP TABLE statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTableStatement {
    /// Table name.
    pub table: String,
    /// `IF EXISTS`.
    pub if_exists: bool,
}

/// Bound representation of SHOW / DESCRIBE statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowStatement {
    /// `SHOW TABLES [LIKE pattern]`.
    Tables {
        /// Optional `LIKE` pattern.
        like: Option<String>,
    },
    /// `SHOW DATABASES`.
    Databases,
    /// `SHOW COLUMNS FROM table`.
    Columns {
        /// Table name.
        table: String,
    },
    /// `DESCRIBE table` / `DESC table`.
    Describe {
        /// Table name.
        table: String,
    },
}

/// Bound representation of an ALTER TABLE partition statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterPartitions {
    /// Target table name.
    pub table: String,
    /// Partition alteration specification reusing catalog PartitionAlteration types.
    pub alteration: PartitionAlteration,
}

impl AlterPartitions {
    /// Create a new [`AlterPartitions`] statement.
    pub fn new(table: impl Into<String>, alteration: PartitionAlteration) -> Self {
        Self {
            table: table.into(),
            alteration,
        }
    }
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
    /// Optional partitioning specification.
    pub partitioning: Option<BoundPartitioning>,
}

/// Bound partitioning specification on a CREATE TABLE statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundPartitioning {
    /// Range partitioning where partitions cover intervals `[lower, upper)`.
    Range {
        /// Column index in schema used as partition key.
        key_column: usize,
        /// Ordered partition bounds.
        partitions: Vec<BoundRangePartition>,
    },
    /// List partitioning where partitions cover disjoint sets of explicit values.
    List {
        /// Column index in schema used as partition key.
        key_column: usize,
        /// Explicit list value sets for each partition.
        partitions: Vec<BoundListPartition>,
    },
}

/// Bound range partition definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRangePartition {
    /// Partition name.
    pub name: String,
    /// Lower bound (None means -inf).
    pub lower: Option<Value>,
    /// Upper bound (None means MAXVALUE / +inf).
    pub upper: Option<Value>,
}

/// Bound list partition definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundListPartition {
    /// Partition name.
    pub name: String,
    /// Explicit values for this partition.
    pub values: Vec<Value>,
}

impl CreateTable {
    /// Create a new [`CreateTable`] bound statement.
    pub fn new(name: impl Into<String>, schema: Schema, primary_key: Vec<usize>) -> Self {
        Self {
            name: name.into(),
            schema,
            primary_key,
            partitioning: None,
        }
    }

    /// Set partitioning on [`CreateTable`].
    pub fn with_partitioning(mut self, partitioning: impl Into<Option<BoundPartitioning>>) -> Self {
        self.partitioning = partitioning.into();
        self
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

/// Comparison operators supported in analytical query filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOp {
    /// Equal (`=`).
    Eq,
    /// Not equal (`!=` or `<>`).
    NotEq,
    /// Less than (`<`).
    Lt,
    /// Less than or equal (`<=`).
    Lte,
    /// Greater than (`>`).
    Gt,
    /// Greater than or equal (`>=`).
    Gte,
}

impl std::fmt::Display for ComparisonOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eq => write!(f, "="),
            Self::NotEq => write!(f, "!="),
            Self::Lt => write!(f, "<"),
            Self::Lte => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::Gte => write!(f, ">="),
        }
    }
}

/// Documented typed filter tree for analytical queries.
///
/// In this execution slice, only conjunctions (`AND`) of column-vs-literal comparisons
/// and nullability checks (`IS NULL` / `IS NOT NULL`) are supported. Disjunctions (`OR`),
/// negations (`NOT`), expressions, subqueries, and cross-column comparisons are rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalyticFilter {
    /// Conjunction of filter expressions (logical AND).
    And(Vec<AnalyticFilter>),
    /// Column comparison with a typed literal value: `column op value`.
    Comparison {
        /// Zero-based column index in the source table schema.
        column: usize,
        /// Comparison operator.
        op: ComparisonOp,
        /// Non-NULL typed literal value.
        value: Value,
    },
    /// Nullability check: `column IS NULL`.
    IsNull {
        /// Zero-based column index in the source table schema.
        column: usize,
    },
    /// Nullability check: `column IS NOT NULL`.
    IsNotNull {
        /// Zero-based column index in the source table schema.
        column: usize,
    },
}

impl AnalyticFilter {
    /// Recursively collects all leaf predicates in this filter tree.
    pub fn leaves(&self) -> Vec<&AnalyticFilter> {
        match self {
            Self::And(children) => children.iter().flat_map(|c| c.leaves()).collect(),
            leaf => vec![leaf],
        }
    }
}

/// Supported aggregate functions in analytical queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    /// `COUNT(*)`: counts total matching rows. Output is non-nullable `Int64`.
    CountStar,
    /// `COUNT(column)`: counts non-null values of the specified column. Output is non-nullable `Int64`.
    Count,
    /// `SUM(column)`: sums values of a numeric column. Output is nullable `Int64` or `Float64`.
    Sum,
    /// `MIN(column)`: finds minimum value of a column. Output has source column's type, nullable.
    Min,
    /// `MAX(column)`: finds maximum value of a column. Output has source column's type, nullable.
    Max,
}

/// An analytical projection expression.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalyticExpr {
    /// Direct column projection from the source schema.
    Column {
        /// Zero-based column index in the table schema.
        index: usize,
        /// Column name.
        name: String,
        /// Data type.
        data_type: DataType,
        /// Nullability.
        nullable: bool,
    },
    /// Approved aggregate function.
    Aggregate {
        /// Aggregate function kind.
        function: AggregateFunction,
        /// Source column index in the table schema (`None` for `COUNT(*)`).
        column_index: Option<usize>,
        /// Projection column name (e.g., "COUNT(*)", "SUM(col)").
        name: String,
        /// Result data type.
        data_type: DataType,
        /// Result nullability.
        nullable: bool,
    },
}

impl AnalyticExpr {
    /// Returns the output column name for this expression.
    pub fn name(&self) -> &str {
        match self {
            Self::Column { name, .. } | Self::Aggregate { name, .. } => name,
        }
    }

    /// Returns the output data type for this expression.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. } | Self::Aggregate { data_type, .. } => *data_type,
        }
    }

    /// Returns whether the output expression can evaluate to NULL.
    pub fn nullable(&self) -> bool {
        match self {
            Self::Column { nullable, .. } | Self::Aggregate { nullable, .. } => *nullable,
        }
    }

    /// Returns `true` if this projection expression is an aggregate function.
    pub fn is_aggregate(&self) -> bool {
        matches!(self, Self::Aggregate { .. })
    }

    /// Returns the source column index, if applicable.
    pub fn column_index(&self) -> Option<usize> {
        match self {
            Self::Column { index, .. } => Some(*index),
            Self::Aggregate { column_index, .. } => *column_index,
        }
    }

    /// Converts this projection expression into an output [`ColumnDef`].
    pub fn to_column_def(&self) -> ColumnDef {
        ColumnDef {
            name: self.name().to_string(),
            data_type: self.data_type(),
            nullable: self.nullable(),
            primary_key: false,
        }
    }
}

/// Bound representation of an ORDER BY column specification in an analytical query.
///
/// In this strict execution slice, only simple unqualified column identifiers are supported.
/// Direction can be ASC or DESC, with explicit deterministic NULL ordering:
/// - ASC defaults to NULLS FIRST (NULL treated as lowest rank).
/// - DESC defaults to NULLS LAST (NULL treated as lowest rank, placed at the end).
/// - Explicit `NULLS FIRST` / `NULLS LAST` can override the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyticOrderBy {
    /// Zero-based column index in the table schema.
    pub column: usize,
    /// Sort ascending (`true`) or descending (`false`).
    pub asc: bool,
    /// Whether NULL values sort first (`true`) or last (`false`).
    pub nulls_first: bool,
}

impl AnalyticOrderBy {
    /// Create a new [`AnalyticOrderBy`] specification.
    pub fn new(column: usize, asc: bool, nulls_first: bool) -> Self {
        Self {
            column,
            asc,
            nulls_first,
        }
    }
}

/// Bound representation of an analytical SELECT query.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticSelect {
    /// Target table name.
    pub table: String,
    /// Projected expressions (columns and approved aggregates).
    pub projection: Vec<AnalyticExpr>,
    /// Optional typed filter tree (AND-only conjunctions of comparisons).
    pub filter: Option<AnalyticFilter>,
    /// Column indices in source table schema for `GROUP BY`.
    pub group_by: Vec<usize>,
    /// Order by column specifications.
    pub order_by: Vec<AnalyticOrderBy>,
    /// Pre-computed output schema for the analytical query result.
    pub output_schema: Schema,
}

impl AnalyticSelect {
    /// Create a new [`AnalyticSelect`] bound statement.
    pub fn new(
        table: impl Into<String>,
        projection: Vec<AnalyticExpr>,
        filter: Option<AnalyticFilter>,
        group_by: Vec<usize>,
        order_by: Vec<AnalyticOrderBy>,
        output_schema: Schema,
    ) -> Self {
        Self {
            table: table.into(),
            projection,
            filter,
            group_by,
            order_by,
            output_schema,
        }
    }

    /// Returns the output schema of the analytical query.
    pub fn output_schema(&self) -> &Schema {
        &self.output_schema
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

impl From<AnalyticSelect> for BoundStatement {
    fn from(stmt: AnalyticSelect) -> Self {
        Self::AnalyticSelect(stmt)
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

        let create = CreateTable::new("users", schema.clone(), vec![0]);
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

        let analytic = AnalyticSelect::new(
            "users",
            vec![AnalyticExpr::Column {
                index: 0,
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
            }],
            None,
            vec![],
            vec![],
            schema,
        );
        let bound_analytic: BoundStatement = analytic.clone().into();
        assert_eq!(bound_analytic, BoundStatement::AnalyticSelect(analytic));
    }
}
