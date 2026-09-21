//! Bound representation of a general (multi-table) query.
//!
//! A [`BoundQuery`] is fully resolved: every table reference is a [`TableSlot`], every join
//! is a left-deep [`JoinSpec`], every expression is an [`Expr`] with flat row offsets, and
//! aggregates and subqueries are hoisted into indexed lists so the executor computes them
//! once. Execution lives in `htap-server`.
//!
//! The flat joined row is the concatenation of the slot rows in slot order; a slot's width
//! is the number of columns of its table or derived query.

use htap_common::types::{ColumnDef, DataType};

use crate::expr::{AggregateSpec, Expr};

/// A FROM item.
#[derive(Debug, Clone, PartialEq)]
pub enum TableSlot {
    /// A catalog table.
    Base {
        /// Table name in the catalog.
        table: String,
        /// Name used to qualify columns (alias or table name).
        alias: String,
        /// Table columns.
        columns: Vec<ColumnDef>,
    },
    /// A derived table (subquery in FROM or an inlined CTE).
    Derived {
        /// The subquery.
        query: Box<BoundQuery>,
        /// Alias (mandatory for derived tables; the CTE name for CTEs).
        alias: String,
        /// Output columns of the subquery.
        columns: Vec<ColumnDef>,
    },
    /// The previous iteration of a recursive CTE.
    WorkingTableSlot {
        /// Name used to qualify columns.
        alias: String,
        /// Output columns of the recursive CTE.
        columns: Vec<ColumnDef>,
    },
}

impl TableSlot {
    /// Name used to qualify columns.
    pub fn alias(&self) -> &str {
        match self {
            Self::Base { alias, .. }
            | Self::Derived { alias, .. }
            | Self::WorkingTableSlot { alias, .. } => alias,
        }
    }

    /// Columns of the slot.
    pub fn columns(&self) -> &[ColumnDef] {
        match self {
            Self::Base { columns, .. }
            | Self::Derived { columns, .. }
            | Self::WorkingTableSlot { columns, .. } => columns,
        }
    }

    /// Number of columns.
    pub fn width(&self) -> usize {
        self.columns().len()
    }
}

/// Join kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `INNER JOIN` / comma join with `ON`.
    Inner,
    /// `LEFT [OUTER] JOIN`: every left row is preserved.
    Left,
    /// `RIGHT [OUTER] JOIN`: every right row is preserved.
    Right,
    /// `FULL [OUTER] JOIN`: every row from both inputs is preserved.
    Full,
    /// `CROSS JOIN` / comma join without a constraint.
    Cross,
}

/// Recursive FROM/join structure. Leaf indices refer to [`SelectBody::slots`].
#[derive(Debug, Clone, PartialEq)]
pub enum JoinTree {
    /// One FROM slot.
    Leaf(usize),
    /// A join between two FROM subtrees.
    Join {
        /// Join kind.
        kind: JoinKind,
        /// Left input.
        left: Box<JoinTree>,
        /// Right input.
        right: Box<JoinTree>,
        /// Join condition (`None` for cross joins).
        ///
        /// This is a rebased copy whose offsets are relative to this subtree's base offset.
        /// It is regenerable on demand and must never be cached.
        on: Option<Expr>,
    },
}

/// A column visible from a join subtree.
#[derive(Debug, Clone, PartialEq)]
pub enum VisibleColumn {
    /// A physical column in a FROM slot.
    Physical {
        /// Slot index.
        slot: usize,
        /// Column index in the slot.
        column: usize,
    },
    /// A merged column, such as one produced by a future `JOIN ... USING`.
    Merged {
        /// Expression producing the visible value.
        expr: Expr,
        /// Visible name.
        name: String,
        /// Value type.
        data_type: DataType,
        /// Whether the value may be NULL.
        nullable: bool,
    },
}

/// Visible columns from one join subtree.
pub type VisibleSchema = Vec<VisibleColumn>;

/// One step of the left-deep join chain: joins slot `right_slot` onto the accumulated left
/// side (slots `0..right_slot`).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinSpec {
    /// Join kind.
    pub kind: JoinKind,
    /// Slot index of the right input.
    pub right_slot: usize,
    /// `ON` condition (`None` for cross joins).
    pub on: Option<Expr>,
    /// Canonical numeric type for each extracted equi-join key.
    ///
    /// `None` marks a non-numeric key and causes the executor to use a nested-loop join.
    pub equi_key_types: Vec<Option<DataType>>,
}

/// A projected column.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    /// Expression.
    pub expr: Expr,
    /// Output column name.
    pub name: String,
}

/// An `ORDER BY` item.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// Sort expression (may reference output columns via [`Expr::OutputColumn`]).
    pub expr: Expr,
    /// Ascending?
    pub asc: bool,
    /// `NULL`s first?
    pub nulls_first: bool,
}

/// Supported window functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunctionKind {
    /// `ROW_NUMBER()`.
    RowNumber,
    /// `RANK()`.
    Rank,
    /// `DENSE_RANK()`.
    DenseRank,
    /// `NTILE(n)`.
    Ntile,
    /// `LAG(value [, offset [, default]])`.
    Lag,
    /// `LEAD(value [, offset [, default]])`.
    Lead,
    /// `FIRST_VALUE(value)`.
    FirstValue,
    /// `LAST_VALUE(value)`.
    LastValue,
    /// `COUNT(*) OVER (...)`.
    CountStar,
    /// `COUNT(value) OVER (...)`.
    Count,
    /// `SUM(value) OVER (...)`.
    Sum,
    /// `AVG(value) OVER (...)`.
    Avg,
    /// `MIN(value) OVER (...)`.
    Min,
    /// `MAX(value) OVER (...)`.
    Max,
}

/// Direction of a bounded window-frame endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFrameDirection {
    /// Rows or values earlier in comparator order.
    Preceding,
    /// Rows or values later in comparator order.
    Following,
}

/// Endpoint of a row-count-based window frame.
#[derive(Debug, Clone, PartialEq)]
pub enum RowFrameBound {
    /// The start or end of the partition.
    Unbounded(WindowFrameDirection),
    /// The current row.
    CurrentRow,
    /// A non-negative row offset.
    Offset {
        /// Number of rows.
        value: u64,
        /// Endpoint direction.
        direction: WindowFrameDirection,
    },
}

/// Endpoint of a peer-based `RANGE` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFrameBound {
    /// The start or end of the partition.
    Unbounded(WindowFrameDirection),
    /// The current peer group.
    CurrentRow,
}

/// Endpoint of a value-offset `RANGE` frame.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueFrameBound {
    /// The start or end of the partition.
    Unbounded(WindowFrameDirection),
    /// The current peer group.
    CurrentRow,
    /// A non-negative numeric literal offset.
    Offset {
        /// Offset expression.
        value: Expr,
        /// Endpoint direction.
        direction: WindowFrameDirection,
    },
}

/// Window frame.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowFrame {
    /// No frame, meaning the whole partition.
    None,
    /// A `ROWS` frame.
    Rows {
        /// Frame start.
        start: RowFrameBound,
        /// Frame end.
        end: RowFrameBound,
    },
    /// A peer-based `RANGE` frame using only unbounded/current-row endpoints.
    PeerRange {
        /// Frame start.
        start: PeerFrameBound,
        /// Frame end.
        end: PeerFrameBound,
    },
    /// A value-offset `RANGE` frame over one numeric or timestamp ordering key.
    ValueRange {
        /// Frame start.
        start: ValueFrameBound,
        /// Frame end.
        end: ValueFrameBound,
    },
}

/// A window function computed after grouping and HAVING.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// Function kind.
    pub func: WindowFunctionKind,
    /// Function-specific arguments.
    pub args: Vec<Expr>,
    /// Partitioning expressions.
    pub partition_by: Vec<Expr>,
    /// Ordering expressions.
    pub order_by: Vec<OrderItem>,
    /// Window frame.
    pub frame: WindowFrame,
    /// Result type.
    pub data_type: DataType,
    /// Result nullability.
    pub nullable: bool,
}

/// Set operation combining two query bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    /// `UNION ALL`
    UnionAll,
    /// `UNION [DISTINCT]`
    UnionDistinct,
    /// `EXCEPT ALL`
    ExceptAll,
    /// `EXCEPT [DISTINCT]`
    ExceptDistinct,
    /// `INTERSECT ALL`
    IntersectAll,
    /// `INTERSECT [DISTINCT]`
    IntersectDistinct,
}

/// A `SELECT ... FROM ... [WHERE] [GROUP BY] [HAVING]` block.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectBody {
    /// FROM slots in order (empty for `SELECT <constants>`).
    pub slots: Vec<TableSlot>,
    /// Recursive FROM/join structure. A FROM-less select uses a synthetic leaf.
    pub join_tree: JoinTree,
    /// Visible schema for every join-tree node, in post-order.
    pub visible_schemas: Vec<VisibleSchema>,
    /// `WHERE` predicate.
    pub filter: Option<Expr>,
    /// `GROUP BY` expressions.
    pub group_by: Vec<Expr>,
    /// Aggregates referenced by projection/having/order (indexed by [`Expr::AggregateRef`]).
    pub aggregates: Vec<AggregateSpec>,
    /// Window functions referenced by the projection (indexed by [`Expr::WindowRef`]).
    pub windows: Vec<WindowSpec>,
    /// `HAVING` predicate (may reference output columns).
    pub having: Option<Expr>,
    /// Projected columns.
    pub projection: Vec<ProjectionItem>,
    /// `SELECT DISTINCT`.
    pub distinct: bool,
    /// References to columns of this query's immediately enclosing query.
    ///
    /// These are retained so the enclosing aggregate-query validator can require that a
    /// correlated subquery only depends on grouped columns.
    pub correlated_outer_refs: Vec<Expr>,
}

impl SelectBody {
    /// Whether the block is an aggregate query (has GROUP BY or aggregates).
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty() || !self.aggregates.is_empty()
    }

    /// Total width of the flat joined row.
    pub fn row_width(&self) -> usize {
        self.slots.iter().map(TableSlot::width).sum()
    }

    /// Offset of the first column of `slot` in the flat joined row.
    pub fn slot_offset(&self, slot: usize) -> usize {
        self.slots[..slot].iter().map(TableSlot::width).sum()
    }
}

/// Query body: a select block or a set operation over two queries.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum QueryBody {
    /// Plain select block.
    Select(SelectBody),
    /// Set operation. Each side is a full query (a parenthesized branch may carry its own
    /// `ORDER BY`/`LIMIT`); rows of a side whose column type differs from the output type
    /// are cast by the executor.
    SetOp {
        /// Operation.
        kind: SetOpKind,
        /// Left input.
        left: Box<BoundQuery>,
        /// Right input.
        right: Box<BoundQuery>,
    },
    /// Recursive CTE body, evaluated from its anchor to a fixed point.
    RecursiveQueryBody {
        /// Non-recursive seed query.
        anchor: Box<BoundQuery>,
        /// Query evaluated once for each prior iteration.
        recursive_term: Box<BoundQuery>,
        /// `true` for `UNION`, `false` for `UNION ALL`.
        distinct: bool,
        /// Reconciled recursive CTE output schema.
        output_columns: Vec<ColumnDef>,
    },
}

/// A bound query: body plus ordering and limits.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundQuery {
    /// Body.
    pub body: QueryBody,
    /// `ORDER BY` items. For set operations these only reference output columns.
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`.
    pub limit: Option<u64>,
    /// `OFFSET`.
    pub offset: Option<u64>,
    /// Subqueries appearing in expressions, indexed by the `index` fields of
    /// [`Expr::ScalarSubquery`], [`Expr::InSubquery`], and [`Expr::Exists`].
    pub subqueries: Vec<BoundQuery>,
    /// Whether this query refers to its immediately enclosing query.
    pub correlated: bool,
    /// References in the immediately enclosing query on which this query is correlated.
    pub correlated_outer_refs: Vec<Expr>,
    /// Output columns. Names may repeat for a top-level query (`SELECT a.id, b.id`);
    /// derived tables and CTEs require unique names.
    pub output_columns: Vec<ColumnDef>,
}

impl BoundQuery {
    /// Output data types.
    pub fn output_types(&self) -> Vec<DataType> {
        self.output_columns.iter().map(|c| c.data_type).collect()
    }
}
