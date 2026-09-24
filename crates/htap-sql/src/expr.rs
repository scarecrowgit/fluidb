//! General bound expression tree and its evaluator.
//!
//! [`Expr`] is the expression IR of the general query path ([`crate::query::BoundQuery`]).
//! Expressions are fully resolved at bind time: every column reference carries its flat
//! offset into the joined row, every aggregate refers to a precomputed aggregate slot, and
//! every subquery refers to a precomputed subquery result. Evaluation therefore needs no
//! catalog and no storage access; see [`EvalContext`].
//!
//! # Semantics
//!
//! - SQL three-valued logic: comparisons involving `NULL` yield `NULL`; `AND`/`OR`/`NOT`
//!   follow the Kleene tables; `IS [NOT] NULL` is the only operator that maps `NULL` to a
//!   definite boolean.
//! - Numeric promotion: `Int32` → `Int64` → `Float64`. Integer arithmetic is checked and
//!   overflows are reported as errors; `/` always produces `Float64`; `%` keeps the promoted
//!   operand type. `Timestamp` values take part in arithmetic and comparisons as their
//!   microsecond `Int64` representation.
//! - `LIKE` matches `%` and `_` case-insensitively (MySQL default collation behaviour).
//! - `CASE` evaluates lazily: only the selected branch is evaluated.

use chrono::{Datelike, NaiveDate};
use htap_common::error::{HtapError, Result};
use htap_common::types::{
    check_decimal_precision, parse_date_to_timestamp_micros, parse_decimal_text,
    timestamp_micros_to_date, DataType, Row, Value, MAX_DECIMAL_PRECISION,
};

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` (always floating point)
    Div,
    /// `DIV` (integer division)
    IntDiv,
    /// `%`
    Mod,
    /// `=`
    Eq,
    /// `<>` / `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `AND`
    And,
    /// `OR`
    Or,
}

impl BinOp {
    /// Whether the operator is a comparison producing a boolean.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::NotEq | Self::Lt | Self::Lte | Self::Gt | Self::Gte
        )
    }

    /// Whether the operator is arithmetic.
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::IntDiv | Self::Mod
        )
    }
}

impl std::fmt::Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::IntDiv => "DIV",
            Self::Mod => "%",
            Self::Eq => "=",
            Self::NotEq => "<>",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::And => "AND",
            Self::Or => "OR",
        };
        f.write_str(s)
    }
}

/// Supported scalar functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFn {
    /// `UPPER(s)`
    Upper,
    /// `LOWER(s)`
    Lower,
    /// `LENGTH(s)` (bytes)
    Length,
    /// `CHAR_LENGTH(s)` (characters)
    CharLength,
    /// `CONCAT(a, b, ...)`
    Concat,
    /// `ABS(n)`
    Abs,
    /// `COALESCE(a, b, ...)`
    Coalesce,
    /// `IFNULL(a, b)`
    IfNull,
    /// `NULLIF(a, b)`
    NullIf,
    /// `SUBSTRING(s, start[, length])`
    Substring,
    /// `EXTRACT(unit FROM timestamp)`
    Extract(CalendarIntervalUnit),
}

/// Units supported by calendar intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarIntervalUnit {
    /// Calendar years.
    Year,
    /// Calendar months.
    Month,
    /// Calendar days.
    Day,
}

/// Supported aggregate functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    /// `COUNT(*)`
    CountStar,
    /// `COUNT(expr)`
    Count,
    /// `SUM(expr)`
    Sum,
    /// `AVG(expr)`
    Avg,
    /// `MIN(expr)`
    Min,
    /// `MAX(expr)`
    Max,
}

/// An aggregate computed once per group. Referenced from expressions by index.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateSpec {
    /// Aggregate function.
    pub func: AggFn,
    /// `DISTINCT` modifier.
    pub distinct: bool,
    /// Argument (`None` for `COUNT(*)`), evaluated per input row.
    pub arg: Option<Expr>,
    /// Result type.
    pub data_type: DataType,
    /// Result nullability.
    pub nullable: bool,
    /// Display name, e.g. `SUM(x)`.
    pub name: String,
}

/// Static type of an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExprType {
    /// Data type. A bare `NULL` literal is typed as [`DataType::String`] and marked
    /// [`ExprType::is_null_literal`]. A [`Expr::Variable`] is also typed as
    /// [`DataType::String`] (nullable) and marked [`ExprType::is_dynamic`]: its true type is
    /// only known when it is evaluated, since user variables hold whatever was last assigned
    /// to them and system variables can be strings or integers.
    pub data_type: DataType,
    /// Whether the expression can evaluate to `NULL`.
    pub nullable: bool,
    /// Whether the expression is the literal `NULL`.
    pub is_null_literal: bool,
    /// Whether the expression is a variable ([`Expr::Variable`]): its reported `data_type` is
    /// a placeholder, not a real static type.
    pub is_dynamic: bool,
}

impl ExprType {
    fn new(data_type: DataType, nullable: bool) -> Self {
        Self {
            data_type,
            nullable,
            is_null_literal: false,
            is_dynamic: false,
        }
    }

    /// Whether the type is numeric (`Int32`, `Int64`, `Float64`).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.data_type,
            DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Decimal { .. }
        )
    }

    /// Whether static type checks should treat this expression as compatible with anything:
    /// `NULL` literals and variables, whose real type is only known at evaluation time.
    pub fn is_permissive(&self) -> bool {
        self.is_null_literal || self.is_dynamic
    }
}

/// Bound expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Column of a FROM slot, resolved to its offset in the flat joined row.
    ColumnRef {
        /// FROM slot index.
        slot: usize,
        /// Column index within the slot.
        column: usize,
        /// Offset within the flat joined row.
        offset: usize,
        /// Column name (for display).
        name: String,
        /// Data type.
        data_type: DataType,
        /// Nullability (already accounts for null-supplying outer joins).
        nullable: bool,
    },
    /// Reference to a column in the immediately enclosing query's input row.
    ///
    /// Correlated references are bound successfully but cannot execute until the query executor
    /// supplies an outer-row evaluation context.
    CorrelatedColumnRef {
        /// Offset within the immediately enclosing query's flat joined row.
        offset: usize,
        /// Column name (for display).
        name: String,
        /// Data type.
        data_type: DataType,
        /// Nullability in the enclosing query.
        nullable: bool,
    },
    /// Reference to an output (projected) column, used by `ORDER BY`/`HAVING` alias resolution.
    OutputColumn {
        /// Index in the projection.
        index: usize,
        /// Data type.
        data_type: DataType,
        /// Nullability.
        nullable: bool,
    },
    /// Literal value.
    Literal(Value),
    /// Binary operator.
    BinaryOp {
        /// Operator.
        op: BinOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// Logical `NOT`.
    Not(Box<Expr>),
    /// Unary minus.
    Negate(Box<Expr>),
    /// `expr IS NULL`.
    IsNull(Box<Expr>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<Expr>),
    /// `expr [NOT] LIKE pattern`.
    Like {
        /// Value expression.
        expr: Box<Expr>,
        /// Pattern expression.
        pattern: Box<Expr>,
        /// `NOT LIKE`.
        negated: bool,
    },
    /// `expr [NOT] IN (list)`.
    In {
        /// Value expression.
        expr: Box<Expr>,
        /// Candidate list.
        list: Vec<Expr>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Value expression.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
        /// `NOT BETWEEN`.
        negated: bool,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// Optional operand (simple CASE).
        operand: Option<Box<Expr>>,
        /// `(condition, result)` branches.
        branches: Vec<(Expr, Expr)>,
        /// `ELSE` result.
        else_result: Option<Box<Expr>>,
        /// Result type.
        data_type: DataType,
        /// Result nullability.
        nullable: bool,
    },
    /// `CAST(expr AS type)`.
    Cast {
        /// Operand.
        expr: Box<Expr>,
        /// Target type.
        to: DataType,
    },
    /// `expr + INTERVAL quantity unit` or `expr - INTERVAL quantity unit`.
    CalendarInterval {
        /// Timestamp operand.
        expr: Box<Expr>,
        /// Interval quantity.
        quantity: Box<Expr>,
        /// Calendar unit.
        unit: CalendarIntervalUnit,
        /// Whether to subtract rather than add the interval.
        negated: bool,
    },
    /// Scalar function call.
    ScalarFunction {
        /// Function.
        func: ScalarFn,
        /// Arguments.
        args: Vec<Expr>,
        /// Result type.
        data_type: DataType,
        /// Result nullability.
        nullable: bool,
    },
    /// Reference to a precomputed aggregate ([`AggregateSpec`]) of the enclosing query.
    AggregateRef {
        /// Index into the query's aggregate list.
        index: usize,
        /// Result type.
        data_type: DataType,
        /// Result nullability.
        nullable: bool,
    },
    /// Reference to a precomputed window function.
    WindowRef {
        /// Index into the query's window list.
        index: usize,
        /// Result type.
        data_type: DataType,
        /// Result nullability.
        nullable: bool,
    },
    /// Scalar subquery, precomputed by the executor unless correlated.
    ScalarSubquery {
        /// Index into the query's subquery list.
        index: usize,
        /// Result type.
        data_type: DataType,
        /// Whether the subquery must be executed against the current row.
        correlated: bool,
    },
    /// `expr [NOT] IN (subquery)`, precomputed by the executor unless correlated.
    InSubquery {
        /// Value expression.
        expr: Box<Expr>,
        /// Index into the query's subquery list.
        index: usize,
        /// `NOT IN`.
        negated: bool,
        /// Whether the subquery must be executed against the current row.
        correlated: bool,
    },
    /// `[NOT] EXISTS (subquery)`, precomputed by the executor unless correlated.
    Exists {
        /// Index into the query's subquery list.
        index: usize,
        /// `NOT EXISTS`.
        negated: bool,
        /// Whether the subquery must be executed against the current row.
        correlated: bool,
    },
    /// `@name` (user variable) or `@@[session.]name` (system variable), resolved at
    /// evaluation time via [`EvalContext::variables`]. `name` has any leading `@`/`@@` sigil
    /// and `session.` scope qualifier already stripped by the binder.
    Variable {
        /// Variable name, without sigils or scope qualifier.
        name: String,
        /// `true` for `@@name` (system variable), `false` for `@name` (user variable).
        is_system: bool,
    },
}

/// Runs a correlated subquery against one calling-query row.
///
/// This intentionally only exposes bound SQL values: implementations belong to the execution
/// layer, while this expression crate remains independent of storage and server types.
pub trait SubqueryRunner {
    /// Executes correlated subquery `index` using `outer_row` from its immediately enclosing
    /// query and returns its result rows.
    fn run(&self, index: usize, outer_row: &[Value]) -> Result<Vec<Row>>;
}

/// Shared limits for correlated-subquery execution within one statement.
///
/// Evaluation contexts deliberately carry separate callback fields for variables and subqueries.
/// If a third callback is needed, consolidate these into a single execution-services interface
/// rather than continuing to grow [`EvalContext`].
#[derive(Debug)]
pub struct SubqueryBudget {
    invocations: std::cell::Cell<usize>,
    depth: std::cell::Cell<usize>,
    invocation_cap: usize,
    depth_cap: usize,
}

impl SubqueryBudget {
    /// Creates a budget with explicit total-invocation and nesting-depth caps.
    pub fn new(invocation_cap: usize, depth_cap: usize) -> Self {
        Self {
            invocations: std::cell::Cell::new(0),
            depth: std::cell::Cell::new(0),
            invocation_cap,
            depth_cap,
        }
    }

    fn enter(&self) -> Result<()> {
        let invocations = self.invocations.get();
        if invocations >= self.invocation_cap {
            return Err(HtapError::InvalidArgument(format!(
                "correlated subquery invocation cap ({}) exceeded",
                self.invocation_cap
            )));
        }
        let depth = self.depth.get();
        if depth >= self.depth_cap {
            return Err(HtapError::InvalidArgument(format!(
                "correlated subquery nesting-depth cap ({}) exceeded",
                self.depth_cap
            )));
        }
        self.invocations.set(invocations + 1);
        self.depth.set(depth + 1);
        Ok(())
    }

    fn exit(&self) {
        self.depth.set(self.depth.get() - 1);
    }
}

/// Provides live values for [`Expr::Variable`] during evaluation.
///
/// Implemented by the session layer, which owns user variable storage and the live session
/// state needed to answer dynamic system variables (`autocommit`, `transaction_isolation`,
/// ...; see `htap_sql::variables`).
pub trait VariableLookup {
    /// Looks up `name` (already stripped of its `@`/`@@` sigil and, for system variables, any
    /// `session.` scope qualifier).
    ///
    /// A user variable (`is_system == false`) that was never assigned should return
    /// `Ok(Value::Null)`, mirroring MySQL. An unknown system variable should return
    /// `Err(HtapError::Unsupported(..))`.
    fn lookup(&self, name: &str, is_system: bool) -> Result<Value>;
}

/// Shared execution services required by expression evaluation.
#[derive(Clone, Copy)]
pub struct EvalServices<'a> {
    /// Source of values for [`Expr::Variable`].
    pub variables: Option<&'a dyn VariableLookup>,
    /// Correlated-subquery executor supplied by the query execution layer.
    pub subquery_runner: Option<&'a dyn SubqueryRunner>,
    /// Shared per-statement budget for correlated subquery invocations.
    pub subquery_budget: Option<&'a SubqueryBudget>,
}

impl EvalServices<'_> {
    /// Empty services for evaluation paths that cannot use variables or subqueries.
    pub const fn none() -> Self {
        Self {
            variables: None,
            subquery_runner: None,
            subquery_budget: None,
        }
    }
}

/// Values an [`Expr`] may need while being evaluated.
#[derive(Clone, Copy)]
pub struct EvalContext<'a> {
    /// Flat joined input row (all slots concatenated in slot order).
    pub row: &'a [Value],
    /// Row of the immediately enclosing query when evaluating a correlated subquery.
    pub current_outer_row: Option<&'a [Value]>,
    /// Aggregate results of the current group, indexed like the query's aggregate list.
    pub aggregates: &'a [Value],
    /// Projected output row of the current input row / group, if already computed.
    pub output: Option<&'a [Value]>,
    /// Precomputed subquery results, indexed like the query's subquery list.
    pub subqueries: &'a [Vec<Row>],
    /// Source of values for [`Expr::Variable`]. `None` when no session/variable context is
    /// available (for example constant folding or the narrow point/analytic paths, which never
    /// bind a `Variable`).
    pub variables: Option<&'a dyn VariableLookup>,
    /// Correlated-subquery executor supplied by the query execution layer.
    pub subquery_runner: Option<&'a dyn SubqueryRunner>,
    /// Shared per-statement budget for correlated subquery invocations.
    pub subquery_budget: Option<&'a SubqueryBudget>,
}

impl std::fmt::Debug for EvalContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalContext")
            .field("row", &self.row)
            .field("aggregates", &self.aggregates)
            .field("output", &self.output)
            .field("subqueries", &self.subqueries)
            .field("variables", &self.variables.map(|_| "<dyn VariableLookup>"))
            .finish()
    }
}

impl<'a> EvalContext<'a> {
    /// Creates an evaluation context from its row state and shared execution services.
    pub fn new(
        row: &'a [Value],
        current_outer_row: Option<&'a [Value]>,
        aggregates: &'a [Value],
        output: Option<&'a [Value]>,
        subqueries: &'a [Vec<Row>],
        services: EvalServices<'a>,
    ) -> Self {
        Self {
            row,
            current_outer_row,
            aggregates,
            output,
            subqueries,
            variables: services.variables,
            subquery_runner: services.subquery_runner,
            subquery_budget: services.subquery_budget,
        }
    }

    /// Context with only an input row.
    pub fn row_only(row: &'a [Value]) -> Self {
        Self::new(row, None, &[], None, &[], EvalServices::none())
    }
}

const EMPTY_ROW: &[Value] = &[];

impl Expr {
    /// Literal `NULL`.
    pub fn null() -> Expr {
        Expr::Literal(Value::Null)
    }

    /// Static type of the expression.
    pub fn expr_type(&self) -> ExprType {
        match self {
            Expr::ColumnRef {
                data_type,
                nullable,
                ..
            }
            | Expr::CorrelatedColumnRef {
                data_type,
                nullable,
                ..
            }
            | Expr::OutputColumn {
                data_type,
                nullable,
                ..
            }
            | Expr::AggregateRef {
                data_type,
                nullable,
                ..
            }
            | Expr::WindowRef {
                data_type,
                nullable,
                ..
            }
            | Expr::Case {
                data_type,
                nullable,
                ..
            }
            | Expr::ScalarFunction {
                data_type,
                nullable,
                ..
            } => ExprType::new(*data_type, *nullable),
            Expr::Literal(Value::Null) => ExprType {
                data_type: DataType::String,
                nullable: true,
                is_null_literal: true,
                is_dynamic: false,
            },
            Expr::Literal(v) => ExprType::new(v.data_type().unwrap_or(DataType::String), false),
            Expr::BinaryOp { op, left, right } => {
                let l = left.expr_type();
                let r = right.expr_type();
                let nullable = l.nullable || r.nullable;
                if op.is_comparison() || matches!(op, BinOp::And | BinOp::Or) {
                    ExprType::new(DataType::Bool, nullable)
                } else {
                    // The binder validates arithmetic result types before constructing a bound
                    // expression. Reaching this fallback therefore indicates an invalid IR.
                    ExprType::new(
                        arithmetic_result_type(*op, l.data_type, r.data_type)
                            .expect("bound arithmetic expression has an invalid result type"),
                        nullable,
                    )
                }
            }
            Expr::Not(inner) => ExprType::new(DataType::Bool, inner.expr_type().nullable),
            Expr::Negate(inner) => {
                let t = inner.expr_type();
                let dt = match t.data_type {
                    DataType::Int32 | DataType::Int64 | DataType::Timestamp => DataType::Int64,
                    other => other,
                };
                ExprType::new(dt, t.nullable)
            }
            Expr::IsNull(_) | Expr::IsNotNull(_) | Expr::Exists { .. } => {
                ExprType::new(DataType::Bool, false)
            }
            Expr::Like { expr, pattern, .. } => ExprType::new(
                DataType::Bool,
                expr.expr_type().nullable || pattern.expr_type().nullable,
            ),
            Expr::In { expr, list, .. } => ExprType::new(
                DataType::Bool,
                expr.expr_type().nullable || list.iter().any(|e| e.expr_type().nullable),
            ),
            Expr::Between {
                expr, low, high, ..
            } => ExprType::new(
                DataType::Bool,
                expr.expr_type().nullable || low.expr_type().nullable || high.expr_type().nullable,
            ),
            Expr::Cast { expr, to } => ExprType::new(*to, expr.expr_type().nullable),
            Expr::CalendarInterval { expr, quantity, .. } => ExprType::new(
                DataType::Timestamp,
                expr.expr_type().nullable || quantity.expr_type().nullable,
            ),
            Expr::ScalarSubquery { data_type, .. } => ExprType::new(*data_type, true),
            Expr::InSubquery { .. } => ExprType::new(DataType::Bool, true),
            Expr::Variable { .. } => ExprType {
                data_type: DataType::String,
                nullable: true,
                is_null_literal: false,
                is_dynamic: true,
            },
        }
    }

    /// Whether the expression contains an aggregate reference.
    pub fn contains_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if matches!(e, Expr::AggregateRef { .. }) {
                found = true;
            }
        });
        found
    }

    /// Whether the expression contains a window reference.
    pub fn contains_window(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if matches!(e, Expr::WindowRef { .. }) {
                found = true;
            }
        });
        found
    }

    /// Whether the expression references any input column.
    pub fn references_columns(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if matches!(e, Expr::ColumnRef { .. } | Expr::CorrelatedColumnRef { .. }) {
                found = true;
            }
        });
        found
    }

    /// Collects correlated references to the immediately enclosing query.
    pub fn correlated_outer_refs(&self) -> Vec<Expr> {
        let mut refs = Vec::new();
        self.walk(&mut |e| {
            if matches!(e, Expr::CorrelatedColumnRef { .. }) && !refs.contains(e) {
                refs.push(e.clone());
            }
        });
        refs
    }

    /// Collects the set of slots referenced by the expression.
    pub fn referenced_slots(&self) -> Vec<usize> {
        let mut slots = Vec::new();
        self.walk(&mut |e| {
            if let Expr::ColumnRef { slot, .. } = e {
                if !slots.contains(slot) {
                    slots.push(*slot);
                }
            }
        });
        slots.sort_unstable();
        slots
    }

    /// Collects `(slot, column)` pairs referenced by the expression.
    pub fn referenced_columns(&self) -> Vec<(usize, usize)> {
        let mut cols = Vec::new();
        self.walk(&mut |e| {
            if let Expr::ColumnRef { slot, column, .. } = e {
                if !cols.contains(&(*slot, *column)) {
                    cols.push((*slot, *column));
                }
            }
        });
        cols
    }

    /// Pre-order traversal.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        f(self);
        match self {
            Expr::ColumnRef { .. }
            | Expr::CorrelatedColumnRef { .. }
            | Expr::OutputColumn { .. }
            | Expr::Literal(_)
            | Expr::AggregateRef { .. }
            | Expr::WindowRef { .. }
            | Expr::ScalarSubquery { .. }
            | Expr::Exists { .. }
            | Expr::Variable { .. } => {}
            Expr::BinaryOp { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::Not(e) | Expr::Negate(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => e.walk(f),
            Expr::Like { expr, pattern, .. } => {
                expr.walk(f);
                pattern.walk(f);
            }
            Expr::In { expr, list, .. } => {
                expr.walk(f);
                for e in list {
                    e.walk(f);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.walk(f);
                low.walk(f);
                high.walk(f);
            }
            Expr::Case {
                operand,
                branches,
                else_result,
                ..
            } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (c, r) in branches {
                    c.walk(f);
                    r.walk(f);
                }
                if let Some(e) = else_result {
                    e.walk(f);
                }
            }
            Expr::Cast { expr, .. } => expr.walk(f),
            Expr::CalendarInterval { expr, quantity, .. } => {
                expr.walk(f);
                quantity.walk(f);
            }
            Expr::ScalarFunction { args, .. } => {
                for a in args {
                    a.walk(f);
                }
            }
            Expr::InSubquery { expr, .. } => expr.walk(f),
        }
    }

    /// Evaluates the expression.
    pub fn eval(&self, ctx: &EvalContext<'_>) -> Result<Value> {
        match self {
            Expr::ColumnRef { offset, name, .. } => {
                ctx.row.get(*offset).cloned().ok_or_else(|| {
                    HtapError::Internal(format!(
                        "column '{name}' offset {offset} out of bounds for row of width {}",
                        ctx.row.len()
                    ))
                })
            }
            Expr::CorrelatedColumnRef { offset, name, .. } => ctx
                .current_outer_row
                .ok_or_else(|| {
                    HtapError::Internal(format!(
                        "correlated column '{name}' evaluated without an outer row"
                    ))
                })?
                .get(*offset)
                .cloned()
                .ok_or_else(|| {
                    HtapError::Internal(format!(
                        "correlated column '{name}' offset {offset} out of bounds for outer row"
                    ))
                }),
            Expr::OutputColumn { index, .. } => ctx
                .output
                .and_then(|o| o.get(*index))
                .cloned()
                .ok_or_else(|| HtapError::Internal(format!("output column {index} not available"))),
            Expr::Literal(v) => Ok(v.clone()),
            Expr::BinaryOp { op, left, right } => eval_binary(*op, left, right, ctx),
            Expr::Not(inner) => Ok(match inner.eval(ctx)? {
                Value::Null => Value::Null,
                Value::Bool(b) => Value::Bool(!b),
                other => return Err(type_error("NOT", &other)),
            }),
            Expr::Negate(inner) => Ok(match inner.eval(ctx)? {
                Value::Null => Value::Null,
                Value::Int32(v) => Value::Int64(-(v as i64)),
                Value::Int64(v) => Value::Int64(v.checked_neg().ok_or_else(|| overflow("-"))?),
                Value::Timestamp(v) => Value::Int64(v.checked_neg().ok_or_else(|| overflow("-"))?),
                Value::Float64(v) => Value::Float64(-v),
                Value::Decimal {
                    value,
                    precision,
                    scale,
                } => Value::Decimal {
                    value: value.checked_neg().ok_or_else(|| overflow("-"))?,
                    precision,
                    scale,
                },
                other => return Err(type_error("unary -", &other)),
            }),
            Expr::IsNull(inner) => Ok(Value::Bool(inner.eval(ctx)?.is_null())),
            Expr::IsNotNull(inner) => Ok(Value::Bool(!inner.eval(ctx)?.is_null())),
            Expr::Like {
                expr,
                pattern,
                negated,
            } => {
                let v = expr.eval(ctx)?;
                let p = pattern.eval(ctx)?;
                Ok(match (v, p) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::String(s), Value::String(p)) => {
                        Value::Bool(like_match(&s, &p) != *negated)
                    }
                    (v, _) => return Err(type_error("LIKE", &v)),
                })
            }
            Expr::In {
                expr,
                list,
                negated,
            } => {
                let v = expr.eval(ctx)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for item in list {
                    let candidate = item.eval(ctx)?;
                    match compare(&v, &candidate)? {
                        None => saw_null = true,
                        Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(!*negated)),
                        Some(_) => {}
                    }
                }
                Ok(if saw_null {
                    Value::Null
                } else {
                    Value::Bool(*negated)
                })
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let v = expr.eval(ctx)?;
                let lo = low.eval(ctx)?;
                let hi = high.eval(ctx)?;
                let ge = compare(&v, &lo)?.map(|o| o != std::cmp::Ordering::Less);
                let le = compare(&v, &hi)?.map(|o| o != std::cmp::Ordering::Greater);
                Ok(match and3(ge, le) {
                    None => Value::Null,
                    Some(b) => Value::Bool(b != *negated),
                })
            }
            Expr::Case {
                operand,
                branches,
                else_result,
                data_type,
                ..
            } => {
                let operand_value = match operand {
                    Some(o) => Some(o.eval(ctx)?),
                    None => None,
                };
                for (cond, result) in branches {
                    let hit = match &operand_value {
                        Some(ov) => {
                            let cv = cond.eval(ctx)?;
                            matches!(compare(ov, &cv)?, Some(std::cmp::Ordering::Equal))
                        }
                        None => matches!(cond.eval(ctx)?, Value::Bool(true)),
                    };
                    if hit {
                        return conform(result.eval(ctx)?, *data_type);
                    }
                }
                match else_result {
                    Some(e) => conform(e.eval(ctx)?, *data_type),
                    None => Ok(Value::Null),
                }
            }
            Expr::Cast { expr, to } => cast_value(expr.eval(ctx)?, *to),
            Expr::CalendarInterval {
                expr,
                quantity,
                unit,
                negated,
            } => {
                let timestamp = expr.eval(ctx)?;
                let quantity = quantity.eval(ctx)?;
                match (timestamp, quantity) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::Timestamp(timestamp), quantity) => {
                        let quantity = match quantity {
                            Value::Int32(value) => value as i64,
                            Value::Int64(value) => value,
                            other => return Err(type_error("calendar interval", &other)),
                        };
                        add_calendar_interval(timestamp, quantity, *unit, *negated)
                    }
                    (other, _) => Err(type_error("calendar interval", &other)),
                }
            }
            Expr::ScalarFunction {
                func,
                args,
                data_type,
                ..
            } => conform(eval_scalar_fn(*func, args, ctx)?, *data_type),
            Expr::AggregateRef { index, .. } => ctx
                .aggregates
                .get(*index)
                .cloned()
                .ok_or_else(|| HtapError::Internal(format!("aggregate {index} not available"))),
            Expr::WindowRef { index, .. } => ctx
                .output
                .and_then(|output| output.get(*index))
                .cloned()
                .ok_or_else(|| HtapError::Internal(format!("window result {index} not available"))),
            Expr::ScalarSubquery {
                index, correlated, ..
            } => {
                let rows = subquery_result_rows(ctx, *index, *correlated)?;
                match rows.len() {
                    0 => Ok(Value::Null),
                    1 => rows[0]
                        .get(0)
                        .cloned()
                        .ok_or_else(|| HtapError::Internal("empty subquery row".into())),
                    n => Err(HtapError::InvalidArgument(format!(
                        "scalar subquery returned {n} rows, expected at most 1"
                    ))),
                }
            }
            Expr::InSubquery {
                expr,
                index,
                negated,
                correlated,
            } => {
                let v = expr.eval(ctx)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let rows = subquery_result_rows(ctx, *index, *correlated)?;
                let mut saw_null = false;
                for row in rows {
                    let candidate = row.get(0).cloned().unwrap_or(Value::Null);
                    match compare(&v, &candidate)? {
                        None => saw_null = true,
                        Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(!*negated)),
                        Some(_) => {}
                    }
                }
                Ok(if saw_null {
                    Value::Null
                } else {
                    Value::Bool(*negated)
                })
            }
            Expr::Exists {
                index,
                negated,
                correlated,
            } => {
                let rows = subquery_result_rows(ctx, *index, *correlated)?;
                Ok(Value::Bool(rows.is_empty() == *negated))
            }
            Expr::Variable { name, is_system } => {
                let is_system = *is_system;
                match ctx.variables {
                    Some(lookup) => lookup.lookup(name, is_system),
                    // No variable context: a user variable that was never assigned (which is
                    // indistinguishable from "no session at all" here) is NULL, matching MySQL.
                    None if !is_system => Ok(Value::Null),
                    None => Err(HtapError::Unsupported(format!(
                        "system variable '@@{name}' cannot be evaluated without a session"
                    ))),
                }
            }
        }
    }

    /// Evaluates a predicate: `true` only when the result is `TRUE` (`NULL` counts as false).
    pub fn eval_predicate(&self, ctx: &EvalContext<'_>) -> Result<bool> {
        Ok(matches!(self.eval(ctx)?, Value::Bool(true)))
    }

    /// Evaluates a constant expression (no columns, aggregates, or subqueries).
    pub fn eval_constant(&self) -> Result<Value> {
        self.eval(&EvalContext::row_only(EMPTY_ROW))
    }
}

/// Widens a numeric value to the statically inferred type of a multi-branch expression
/// (`CASE`, `COALESCE`, ...) so the runtime type always matches the declared column type.
fn conform(v: Value, data_type: DataType) -> Result<Value> {
    match (&v, data_type) {
        (Value::Null, _) => Ok(v),
        (Value::Int32(_), DataType::Int64 | DataType::Float64)
        | (Value::Int64(_), DataType::Float64) => cast_value(v, data_type),
        (
            Value::Decimal {
                value,
                scale: source_scale,
                ..
            },
            DataType::Decimal { scale, .. },
        ) if source_scale > &scale => {
            let factor = 10_i64
                .checked_pow(u32::from(*source_scale - scale))
                .ok_or_else(|| overflow("DECIMAL"))?;
            if value % factor != 0 {
                return Err(HtapError::InvalidArgument(format!(
                    "cannot exactly conform DECIMAL value {v} to {data_type}"
                )));
            }
            cast_value(v, data_type)
        }
        (_, DataType::Decimal { .. }) => cast_value(v, data_type),
        _ => Ok(v),
    }
}

fn subquery_rows<'a>(ctx: &EvalContext<'a>, index: usize) -> Result<&'a Vec<Row>> {
    ctx.subqueries
        .get(index)
        .ok_or_else(|| HtapError::Internal(format!("subquery {index} not available")))
}

/// Builds an [`EvalContext`] while ensuring all execution callbacks are supplied together.
#[macro_export]
macro_rules! eval_context {
    (
        row: $row:expr,
        current_outer_row: $current_outer_row:expr,
        aggregates: $aggregates:expr,
        output: $output:expr,
        subqueries: $subqueries:expr,
        variables: $variables:expr,
        subquery_runner: $subquery_runner:expr,
        subquery_budget: $subquery_budget:expr $(,)?
    ) => {
        $crate::expr::EvalContext::new(
            $row,
            $current_outer_row,
            $aggregates,
            $output,
            $subqueries,
            $crate::expr::EvalServices {
                variables: $variables,
                subquery_runner: $subquery_runner,
                subquery_budget: $subquery_budget,
            },
        )
    };
    (
        row: $row:expr,
        current_outer_row: $current_outer_row:expr,
        aggregates: $aggregates:expr,
        output: $output:expr,
        subqueries: $subqueries:ident,
        variables: $variables:ident,
        subquery_runner: $subquery_runner:ident,
        subquery_budget: $subquery_budget:ident $(,)?
    ) => {
        $crate::expr::EvalContext::new(
            $row,
            $current_outer_row,
            $aggregates,
            $output,
            $subqueries,
            $crate::expr::EvalServices {
                variables: $variables,
                subquery_runner: $subquery_runner,
                subquery_budget: $subquery_budget,
            },
        )
    };
}

fn subquery_result_rows(ctx: &EvalContext<'_>, index: usize, correlated: bool) -> Result<Vec<Row>> {
    if !correlated {
        return Ok(subquery_rows(ctx, index)?.clone());
    }

    let runner = ctx.subquery_runner.ok_or_else(|| {
        HtapError::Internal(format!(
            "correlated subquery {index} evaluated without a subquery runner"
        ))
    })?;
    let budget = ctx.subquery_budget.ok_or_else(|| {
        HtapError::Internal(format!(
            "correlated subquery {index} evaluated without a subquery budget"
        ))
    })?;

    budget.enter()?;
    let result = runner.run(index, ctx.row);
    budget.exit();
    result
}

fn add_calendar_interval(
    timestamp: i64,
    quantity: i64,
    unit: CalendarIntervalUnit,
    negated: bool,
) -> Result<Value> {
    let quantity = if negated {
        quantity.checked_neg().ok_or_else(|| overflow("INTERVAL"))?
    } else {
        quantity
    };
    let date_string = timestamp_micros_to_date(timestamp)?;
    let date = NaiveDate::parse_from_str(&date_string, "%Y-%m-%d").map_err(|error| {
        HtapError::Internal(format!(
            "failed to parse timestamp date '{date_string}': {error}"
        ))
    })?;
    let adjusted = match unit {
        CalendarIntervalUnit::Day => date
            .checked_add_signed(chrono::Duration::days(quantity))
            .ok_or_else(|| {
                HtapError::InvalidArgument("calendar interval result is out of range".into())
            })?,
        CalendarIntervalUnit::Month | CalendarIntervalUnit::Year => {
            let months = match unit {
                CalendarIntervalUnit::Month => quantity,
                CalendarIntervalUnit::Year => quantity
                    .checked_mul(12)
                    .ok_or_else(|| overflow("INTERVAL"))?,
                CalendarIntervalUnit::Day => unreachable!(),
            };
            let month_index = i64::from(date.year())
                .checked_mul(12)
                .and_then(|value| value.checked_add(i64::from(date.month0())))
                .and_then(|value| value.checked_add(months))
                .ok_or_else(|| {
                    HtapError::InvalidArgument("calendar interval result is out of range".into())
                })?;
            let year = i32::try_from(month_index.div_euclid(12)).map_err(|_| {
                HtapError::InvalidArgument("calendar interval result is out of range".into())
            })?;
            let month = u32::try_from(month_index.rem_euclid(12) + 1).map_err(|_| {
                HtapError::InvalidArgument("calendar interval result is out of range".into())
            })?;

            // Clamp to the last day of the target month, e.g. Jan 31 + 1 month = Feb 28/29.
            let last_day = if month == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1).and_then(|next| next.pred_opt())
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1).and_then(|next| next.pred_opt())
            }
            .ok_or_else(|| {
                HtapError::InvalidArgument("calendar interval result is out of range".into())
            })?;

            NaiveDate::from_ymd_opt(year, month, date.day().min(last_day.day())).ok_or_else(
                || HtapError::InvalidArgument("calendar interval result is out of range".into()),
            )?
        }
    };

    Ok(Value::Timestamp(parse_date_to_timestamp_micros(
        &adjusted.to_string(),
    )?))
}

fn type_error(what: &str, v: &Value) -> HtapError {
    HtapError::InvalidArgument(format!(
        "operator {what} cannot be applied to a {} value",
        v.data_type().map(|d| d.name()).unwrap_or("NULL")
    ))
}

fn overflow(op: &str) -> HtapError {
    HtapError::InvalidArgument(format!("integer overflow in '{op}'"))
}

/// Returns decimal precision and scale for decimal-compatible operands.
fn decimal_operand(data_type: DataType) -> Option<(u8, u8)> {
    match data_type {
        DataType::Decimal { precision, scale } => Some((precision, scale)),
        DataType::Int32 => Some((10, 0)),
        DataType::Int64 | DataType::Timestamp => Some((19, 0)),
        _ => None,
    }
}

/// Result type of arithmetic after numeric promotion.
pub fn arithmetic_result_type(op: BinOp, l: DataType, r: DataType) -> Result<DataType> {
    if !op.is_arithmetic() {
        return Err(HtapError::InvalidArgument(format!(
            "operator '{op}' is not an arithmetic operator"
        )));
    }
    if l == DataType::Float64 || r == DataType::Float64 {
        return Ok(DataType::Float64);
    }
    if let (Some((lp, ls)), Some((rp, rs))) = (decimal_operand(l), decimal_operand(r)) {
        if matches!(l, DataType::Decimal { .. }) || matches!(r, DataType::Decimal { .. }) {
            let decimal_result = |precision: u8, scale: u8| {
                let precision = precision.min(MAX_DECIMAL_PRECISION);
                let scale = scale.min(MAX_DECIMAL_PRECISION);
                let scale = if scale > precision {
                    // Drop fractional digits from the declared type when they do not fit, as MySQL does.
                    precision
                } else {
                    scale
                };
                Ok(DataType::Decimal { precision, scale })
            };
            let checked_add = |left: u8, right: u8| {
                left.checked_add(right).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "DECIMAL result precision overflow in '{op}'"
                    ))
                })
            };
            let integer_digits = |precision: u8, scale: u8| {
                precision.checked_sub(scale).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "invalid DECIMAL({precision},{scale}) operand for '{op}'"
                    ))
                })
            };

            let (precision, scale) = match op {
                BinOp::Add | BinOp::Sub => {
                    let scale = ls.max(rs);
                    let precision = checked_add(
                        checked_add(integer_digits(lp, ls)?.max(integer_digits(rp, rs)?), scale)?,
                        1,
                    )?;
                    (precision, scale)
                }
                BinOp::Mul => (checked_add(lp, rp)?, checked_add(ls, rs)?),
                BinOp::Div => (
                    MAX_DECIMAL_PRECISION,
                    ls.checked_add(4).ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "DECIMAL result precision overflow in '{op}'"
                        ))
                    })?,
                ),
                BinOp::IntDiv => (MAX_DECIMAL_PRECISION, 0),
                BinOp::Mod => {
                    let scale = ls.max(rs);
                    (
                        checked_add(integer_digits(lp, ls)?.min(integer_digits(rp, rs)?), scale)?,
                        scale,
                    )
                }
                _ => unreachable!(),
            };
            return decimal_result(precision, scale);
        }
    }
    if op == BinOp::Div {
        Ok(DataType::Float64)
    } else {
        Ok(DataType::Int64)
    }
}

/// Numeric view of a value for arithmetic and comparison.
enum Num {
    Int(i64),
    Float(f64),
}

fn as_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int32(i) => Some(Num::Int(*i as i64)),
        Value::Int64(i) | Value::Timestamp(i) => Some(Num::Int(*i)),
        Value::Float64(f) => Some(Num::Float(*f)),
        Value::Decimal { value, scale, .. } => {
            Some(Num::Float(*value as f64 / 10_f64.powi(i32::from(*scale))))
        }
        _ => None,
    }
}

fn eval_binary(op: BinOp, left: &Expr, right: &Expr, ctx: &EvalContext<'_>) -> Result<Value> {
    match op {
        BinOp::And => {
            let l = to_bool3(left.eval(ctx)?, "AND")?;
            if l == Some(false) {
                return Ok(Value::Bool(false));
            }
            let r = to_bool3(right.eval(ctx)?, "AND")?;
            Ok(bool3_to_value(and3(l, r)))
        }
        BinOp::Or => {
            let l = to_bool3(left.eval(ctx)?, "OR")?;
            if l == Some(true) {
                return Ok(Value::Bool(true));
            }
            let r = to_bool3(right.eval(ctx)?, "OR")?;
            Ok(bool3_to_value(or3(l, r)))
        }
        _ => {
            let l = left.eval(ctx)?;
            let r = right.eval(ctx)?;
            if l.is_null() || r.is_null() {
                return Ok(Value::Null);
            }
            if op.is_comparison() {
                let ord = compare(&l, &r)?;
                return Ok(match ord {
                    None => Value::Null,
                    Some(o) => Value::Bool(match op {
                        BinOp::Eq => o == std::cmp::Ordering::Equal,
                        BinOp::NotEq => o != std::cmp::Ordering::Equal,
                        BinOp::Lt => o == std::cmp::Ordering::Less,
                        BinOp::Lte => o != std::cmp::Ordering::Greater,
                        BinOp::Gt => o == std::cmp::Ordering::Greater,
                        BinOp::Gte => o != std::cmp::Ordering::Less,
                        _ => unreachable!(),
                    }),
                });
            }
            arithmetic(op, &l, &r)
        }
    }
}

fn arithmetic(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    // Float64 takes precedence over DECIMAL so mixed decimal/float arithmetic uses the
    // standard floating-point path rather than recursing through `decimal_arithmetic`.
    if !matches!(l, Value::Float64(_))
        && !matches!(r, Value::Float64(_))
        && (matches!(l, Value::Decimal { .. }) || matches!(r, Value::Decimal { .. }))
    {
        return decimal_arithmetic(op, l, r);
    }

    if op == BinOp::IntDiv {
        let (x, y) = match (l, r) {
            (Value::Int32(x), Value::Int32(y)) => (*x as i64, *y as i64),
            (Value::Int32(x), Value::Int64(y)) => (*x as i64, *y),
            (Value::Int64(x), Value::Int32(y)) => (*x, *y as i64),
            (Value::Int64(x), Value::Int64(y)) => (*x, *y),
            _ => {
                let bad = if matches!(l, Value::Int32(_) | Value::Int64(_)) {
                    r
                } else {
                    l
                };
                return Err(type_error("DIV", bad));
            }
        };
        if y == 0 {
            return Ok(Value::Null);
        }
        return Ok(Value::Int64(
            x.checked_div(y).ok_or_else(|| overflow("DIV"))?,
        ));
    }

    let (a, b) = match (as_num(l), as_num(r)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            let bad = if as_num(l).is_none() { l } else { r };
            return Err(type_error(&op.to_string(), bad));
        }
    };
    if op == BinOp::Div {
        let (x, y) = (num_to_f64(&a), num_to_f64(&b));
        if y == 0.0 {
            return Ok(Value::Null);
        }
        let result = x / y;
        if !result.is_finite() {
            return Err(HtapError::InvalidArgument(
                "DOUBLE value is out of range in '/'".into(),
            ));
        }
        return Ok(Value::Float64(result));
    }
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => {
            let opname = op.to_string();
            let v = match op {
                BinOp::Add => x.checked_add(y),
                BinOp::Sub => x.checked_sub(y),
                BinOp::Mul => x.checked_mul(y),
                BinOp::Mod => {
                    if y == 0 {
                        return Ok(Value::Null);
                    }
                    x.checked_rem(y)
                }
                _ => unreachable!(),
            };
            Ok(Value::Int64(v.ok_or_else(|| overflow(&opname))?))
        }
        (a, b) => {
            let (x, y) = (num_to_f64(&a), num_to_f64(&b));
            let result = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Mod => {
                    if y == 0.0 {
                        return Ok(Value::Null);
                    }
                    x % y
                }
                _ => unreachable!(),
            };
            if !result.is_finite() {
                return Err(HtapError::InvalidArgument(format!(
                    "DOUBLE value is out of range in '{}'",
                    op
                )));
            }
            Ok(Value::Float64(result))
        }
    }
}

fn decimal_arithmetic(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    let (left, left_precision, left_scale) = match l {
        Value::Decimal {
            value,
            precision,
            scale,
        } => (i128::from(*value), *precision, *scale),
        Value::Int32(value) => (i128::from(*value), 10, 0),
        Value::Int64(value) => (i128::from(*value), 19, 0),
        Value::Timestamp(value) => (i128::from(*value), 19, 0),
        Value::Float64(_) => return arithmetic(op, l, r),
        other => return Err(type_error(&op.to_string(), other)),
    };
    let (right, right_precision, right_scale) = match r {
        Value::Decimal {
            value,
            precision,
            scale,
        } => (i128::from(*value), *precision, *scale),
        Value::Int32(value) => (i128::from(*value), 10, 0),
        Value::Int64(value) => (i128::from(*value), 19, 0),
        Value::Timestamp(value) => (i128::from(*value), 19, 0),
        Value::Float64(_) => return arithmetic(op, l, r),
        other => return Err(type_error(&op.to_string(), other)),
    };

    let left_type = DataType::Decimal {
        precision: left_precision,
        scale: left_scale,
    };
    let right_type = DataType::Decimal {
        precision: right_precision,
        scale: right_scale,
    };
    let DataType::Decimal { precision, scale } = arithmetic_result_type(op, left_type, right_type)?
    else {
        return Err(HtapError::InvalidArgument(format!(
            "operator '{op}' does not produce a DECIMAL result"
        )));
    };

    let (value, value_scale) = match op {
        BinOp::Add | BinOp::Sub | BinOp::Mod => {
            let intermediate_scale = left_scale.max(right_scale);
            let left = decimal_rescale(left, left_scale, intermediate_scale)?;
            let right = decimal_rescale(right, right_scale, intermediate_scale)?;
            if op == BinOp::Mod && right == 0 {
                return Ok(Value::Null);
            }
            let value = match op {
                BinOp::Add => left.checked_add(right),
                BinOp::Sub => left.checked_sub(right),
                BinOp::Mod => left.checked_rem(right),
                _ => unreachable!(),
            }
            .ok_or_else(|| overflow(&op.to_string()))?;
            (value, intermediate_scale)
        }
        BinOp::Mul => (
            left.checked_mul(right).ok_or_else(|| overflow("*"))?,
            left_scale
                .checked_add(right_scale)
                .ok_or_else(|| overflow("*"))?,
        ),
        BinOp::Div => {
            if right == 0 {
                return Ok(Value::Null);
            }
            let exponent = scale
                .checked_add(right_scale)
                .and_then(|value| value.checked_sub(left_scale))
                .ok_or_else(|| overflow("/"))?;
            (
                decimal_div_round_half_away(decimal_rescale(left, 0, exponent)?, right)?,
                scale,
            )
        }
        BinOp::IntDiv => {
            if right == 0 {
                return Ok(Value::Null);
            }
            let intermediate_scale = left_scale.max(right_scale);
            let left = decimal_rescale(left, left_scale, intermediate_scale)?;
            let right = decimal_rescale(right, right_scale, intermediate_scale)?;
            let value = left.checked_div(right).ok_or_else(|| overflow("DIV"))?;
            return Ok(Value::Int64(
                i64::try_from(value).map_err(|_| overflow("DIV"))?,
            ));
        }
        _ => unreachable!(),
    };

    let value = decimal_rescale(value, value_scale, scale)?;
    let value = i64::try_from(value).map_err(|_| overflow(&op.to_string()))?;
    let value = check_decimal_precision(value, precision, scale)?;

    Ok(Value::Decimal {
        value,
        precision,
        scale,
    })
}

fn num_to_f64(n: &Num) -> f64 {
    match n {
        Num::Int(i) => *i as f64,
        Num::Float(f) => *f,
    }
}

fn decimal_rescale(value: i128, from_scale: u8, to_scale: u8) -> Result<i128> {
    if from_scale == to_scale {
        return Ok(value);
    }
    let factor = 10_i128
        .checked_pow(u32::from(from_scale.abs_diff(to_scale)))
        .ok_or_else(|| overflow("DECIMAL"))?;
    if from_scale < to_scale {
        value.checked_mul(factor).ok_or_else(|| overflow("DECIMAL"))
    } else {
        decimal_div_round_half_away(value, factor)
    }
}

/// Divides with DECIMAL's round-half-away-from-zero rule.
fn decimal_div_round_half_away(value: i128, divisor: i128) -> Result<i128> {
    let quotient = value
        .checked_div(divisor)
        .ok_or_else(|| overflow("DECIMAL"))?;
    let remainder = value
        .checked_rem(divisor)
        .ok_or_else(|| overflow("DECIMAL"))?;

    let remainder_magnitude = remainder.unsigned_abs();
    let divisor_magnitude = divisor.unsigned_abs();
    // Round when 2 * |remainder| >= |divisor| without overflowing the doubled remainder.
    if remainder_magnitude < divisor_magnitude.saturating_sub(remainder_magnitude) {
        return Ok(quotient);
    }
    // The truncated quotient can be zero, so use the exact quotient's sign.
    if (value < 0) != (divisor < 0) {
        quotient.checked_sub(1).ok_or_else(|| overflow("DECIMAL"))
    } else {
        quotient.checked_add(1).ok_or_else(|| overflow("DECIMAL"))
    }
}

/// SQL comparison. Returns `None` when either side is `NULL`.
///
/// Numeric values compare after promotion; other values must have the same type.
pub fn compare(l: &Value, r: &Value) -> Result<Option<std::cmp::Ordering>> {
    if l.is_null() || r.is_null() {
        return Ok(None);
    }
    if let (
        Value::Decimal {
            value: left,
            scale: left_scale,
            ..
        },
        Value::Decimal {
            value: right,
            scale: right_scale,
            ..
        },
    ) = (l, r)
    {
        let scale = (*left_scale).max(*right_scale);
        let left = decimal_rescale(i128::from(*left), *left_scale, scale)?;
        let right = decimal_rescale(i128::from(*right), *right_scale, scale)?;
        return Ok(Some(left.cmp(&right)));
    }
    match (l, r) {
        (Value::Decimal { value, scale, .. }, Value::Int32(integer)) => {
            let integer = decimal_rescale(i128::from(*integer), 0, *scale)?;
            return Ok(Some(i128::from(*value).cmp(&integer)));
        }
        (Value::Decimal { value, scale, .. }, Value::Int64(integer)) => {
            let integer = decimal_rescale(i128::from(*integer), 0, *scale)?;
            return Ok(Some(i128::from(*value).cmp(&integer)));
        }
        (Value::Int32(integer), Value::Decimal { value, scale, .. }) => {
            let integer = decimal_rescale(i128::from(*integer), 0, *scale)?;
            return Ok(Some(integer.cmp(&i128::from(*value))));
        }
        (Value::Int64(integer), Value::Decimal { value, scale, .. }) => {
            let integer = decimal_rescale(i128::from(*integer), 0, *scale)?;
            return Ok(Some(integer.cmp(&i128::from(*value))));
        }
        _ => {}
    }
    if let (Some(a), Some(b)) = (as_num(l), as_num(r)) {
        return Ok(Some(match (a, b) {
            (Num::Int(x), Num::Int(y)) => x.cmp(&y),
            (a, b) => num_to_f64(&a).total_cmp(&num_to_f64(&b)),
        }));
    }
    match (l, r) {
        (Value::Bool(a), Value::Bool(b)) => Ok(Some(a.cmp(b))),
        (Value::String(a), Value::String(b)) => Ok(Some(a.cmp(b))),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(Some(a.cmp(b))),
        _ => Err(HtapError::InvalidArgument(format!(
            "cannot compare {} with {}",
            l.data_type().map(|d| d.name()).unwrap_or("NULL"),
            r.data_type().map(|d| d.name()).unwrap_or("NULL")
        ))),
    }
}

fn to_bool3(v: Value, op: &str) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(b)),
        other => Err(type_error(op, &other)),
    }
}

fn bool3_to_value(b: Option<bool>) -> Value {
    match b {
        None => Value::Null,
        Some(b) => Value::Bool(b),
    }
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// `LIKE` matcher with `%` (any sequence) and `_` (one character), case-insensitive.
pub fn like_match(value: &str, pattern: &str) -> bool {
    let v: Vec<char> = value.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    like_rec(&v, &p)
}

fn like_rec(v: &[char], p: &[char]) -> bool {
    match p.split_first() {
        None => v.is_empty(),
        Some(('%', rest)) => (0..=v.len()).any(|i| like_rec(&v[i..], rest)),
        Some(('_', rest)) => !v.is_empty() && like_rec(&v[1..], rest),
        Some((c, rest)) => v.first() == Some(c) && like_rec(&v[1..], rest),
    }
}

/// Casts a value to `to`, following MySQL-like conversions for the supported types.
pub fn cast_value(v: Value, to: DataType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let bad = |v: &Value| {
        HtapError::InvalidArgument(format!(
            "cannot cast {} '{v}' to {}",
            v.data_type().map(|d| d.name()).unwrap_or("NULL"),
            to.name()
        ))
    };
    Ok(match to {
        DataType::Bool => match &v {
            Value::Bool(b) => Value::Bool(*b),
            Value::Int32(i) => Value::Bool(*i != 0),
            Value::Int64(i) | Value::Timestamp(i) => Value::Bool(*i != 0),
            Value::Float64(f) => Value::Bool(*f != 0.0),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" => Value::Bool(true),
                "0" | "false" => Value::Bool(false),
                _ => return Err(bad(&v)),
            },
            _ => return Err(bad(&v)),
        },
        DataType::Int32 => {
            let i = cast_to_i64(&v).ok_or_else(|| bad(&v))?;
            Value::Int32(i32::try_from(i).map_err(|_| bad(&v))?)
        }
        DataType::Int64 => Value::Int64(cast_to_i64(&v).ok_or_else(|| bad(&v))?),
        DataType::Timestamp => Value::Timestamp(cast_to_i64(&v).ok_or_else(|| bad(&v))?),
        DataType::Float64 => Value::Float64(match &v {
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Int32(i) => *i as f64,
            Value::Int64(i) | Value::Timestamp(i) => *i as f64,
            Value::Float64(f) => *f,
            Value::Decimal { value, scale, .. } => *value as f64 / 10_f64.powi(i32::from(*scale)),
            Value::String(s) => {
                let parsed: f64 = s.trim().parse().map_err(|_| bad(&v))?;
                if !parsed.is_finite() {
                    return Err(HtapError::InvalidArgument(
                        "DOUBLE value is out of range in 'CAST'".into(),
                    ));
                }
                parsed
            }
            _ => return Err(bad(&v)),
        }),
        DataType::Decimal { precision, scale } => {
            let unscaled = match &v {
                Value::Bool(value) => decimal_rescale(i128::from(*value as i64), 0, scale)?,
                Value::Int32(value) => decimal_rescale(i128::from(*value), 0, scale)?,
                Value::Int64(value) | Value::Timestamp(value) => {
                    decimal_rescale(i128::from(*value), 0, scale)?
                }
                Value::Float64(value) => {
                    if !value.is_finite() {
                        return Err(bad(&v));
                    }
                    let factor = 10_f64.powi(i32::from(scale));
                    let scaled = (value * factor).round();
                    if !scaled.is_finite() || scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
                        return Err(bad(&v));
                    }
                    i128::from(scaled as i64)
                }
                Value::Decimal {
                    value,
                    scale: source_scale,
                    ..
                } => decimal_rescale(i128::from(*value), *source_scale, scale)?,
                Value::String(value) => parse_decimal_text(value, scale).map_err(|_| bad(&v))?,
                _ => return Err(bad(&v)),
            };
            let value = i64::try_from(unscaled).map_err(|_| bad(&v))?;
            let value = check_decimal_precision(value, precision, scale).map_err(|_| bad(&v))?;
            Value::Decimal {
                value,
                precision,
                scale,
            }
        }
        DataType::String => Value::String(match &v {
            Value::Bool(b) => {
                if *b {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            Value::Bytes(b) => String::from_utf8(b.clone()).map_err(|_| bad(&v))?,
            other => other.to_string(),
        }),
        DataType::Bytes => match &v {
            Value::Bytes(b) => Value::Bytes(b.clone()),
            Value::String(s) => Value::Bytes(s.clone().into_bytes()),
            _ => return Err(bad(&v)),
        },
    })
}

fn cast_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Int32(i) => Some(*i as i64),
        Value::Int64(i) | Value::Timestamp(i) => Some(*i),
        Value::Float64(f) => {
            let r = f.round();
            if r.is_finite() && r >= i64::MIN as f64 && r < i64::MAX as f64 {
                Some(r as i64)
            } else {
                None
            }
        }
        Value::Decimal { value, scale, .. } => decimal_rescale(i128::from(*value), *scale, 0)
            .ok()
            .and_then(|value| i64::try_from(value).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn eval_scalar_fn(func: ScalarFn, args: &[Expr], ctx: &EvalContext<'_>) -> Result<Value> {
    let vals: Vec<Value> = args.iter().map(|a| a.eval(ctx)).collect::<Result<_>>()?;
    match func {
        ScalarFn::Upper | ScalarFn::Lower | ScalarFn::Length | ScalarFn::CharLength => {
            match &vals[0] {
                Value::Null => Ok(Value::Null),
                Value::String(s) => Ok(match func {
                    ScalarFn::Upper => Value::String(s.to_uppercase()),
                    ScalarFn::Lower => Value::String(s.to_lowercase()),
                    ScalarFn::Length => Value::Int64(s.len() as i64),
                    _ => Value::Int64(s.chars().count() as i64),
                }),
                Value::Bytes(b) if func == ScalarFn::Length => Ok(Value::Int64(b.len() as i64)),
                other => Err(type_error("string function", other)),
            }
        }
        ScalarFn::Substring => {
            if vals.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let string = match &vals[0] {
                Value::String(value) => value,
                other => return Err(type_error("SUBSTRING", other)),
            };
            let start = match vals[1] {
                Value::Int32(value) => i64::from(value),
                Value::Int64(value) => value,
                ref other => return Err(type_error("SUBSTRING", other)),
            };
            let length = match vals.get(2) {
                Some(Value::Int32(value)) => Some(i64::from(*value)),
                Some(Value::Int64(value)) => Some(*value),
                Some(other) => return Err(type_error("SUBSTRING", other)),
                None => None,
            };
            if length.is_some_and(|value| value <= 0) || start == 0 {
                return Ok(Value::String(String::new()));
            }

            let chars: Vec<char> = string.chars().collect();
            let start = if start > 0 {
                usize::try_from(start - 1).unwrap_or(usize::MAX)
            } else {
                let offset = start.unsigned_abs();
                chars
                    .len()
                    .saturating_sub(usize::try_from(offset).unwrap_or(usize::MAX))
            };
            let end = length
                .map(|value| start.saturating_add(usize::try_from(value).unwrap_or(usize::MAX)))
                .unwrap_or(chars.len())
                .min(chars.len());

            Ok(Value::String(
                chars.get(start..end).unwrap_or(&[]).iter().collect(),
            ))
        }
        ScalarFn::Extract(unit) => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Timestamp(timestamp) => {
                let date_string = timestamp_micros_to_date(*timestamp)?;
                let date =
                    NaiveDate::parse_from_str(&date_string, "%Y-%m-%d").map_err(|error| {
                        HtapError::Internal(format!(
                            "failed to parse timestamp date '{date_string}': {error}"
                        ))
                    })?;
                let value = match unit {
                    CalendarIntervalUnit::Year => i64::from(date.year()),
                    CalendarIntervalUnit::Month => i64::from(date.month()),
                    CalendarIntervalUnit::Day => i64::from(date.day()),
                };
                Ok(Value::Int64(value))
            }
            other => Err(type_error("EXTRACT", other)),
        },
        ScalarFn::Concat => {
            let mut out = String::new();
            for v in &vals {
                match v {
                    Value::Null => return Ok(Value::Null),
                    Value::Bytes(b) => match std::str::from_utf8(b) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(type_error("CONCAT", v)),
                    },
                    Value::Bool(b) => out.push(if *b { '1' } else { '0' }),
                    other => out.push_str(&other.to_string()),
                }
            }
            Ok(Value::String(out))
        }
        ScalarFn::Abs => Ok(match &vals[0] {
            Value::Null => Value::Null,
            Value::Int32(i) => Value::Int32(i.checked_abs().ok_or_else(|| overflow("ABS"))?),
            Value::Int64(i) => Value::Int64(i.checked_abs().ok_or_else(|| overflow("ABS"))?),
            Value::Float64(f) => Value::Float64(f.abs()),
            Value::Decimal {
                value,
                precision,
                scale,
            } => Value::Decimal {
                value: value.checked_abs().ok_or_else(|| overflow("ABS"))?,
                precision: *precision,
                scale: *scale,
            },
            other => return Err(type_error("ABS", other)),
        }),
        ScalarFn::Coalesce => Ok(vals
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null)),
        ScalarFn::IfNull => Ok(if vals[0].is_null() {
            vals[1].clone()
        } else {
            vals[0].clone()
        }),
        ScalarFn::NullIf => Ok(match compare(&vals[0], &vals[1])? {
            Some(std::cmp::Ordering::Equal) => Value::Null,
            _ => vals[0].clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: Value) -> Expr {
        Expr::Literal(v)
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::BinaryOp {
            op,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    #[test]
    fn three_valued_logic_tables() {
        let t = || lit(Value::Bool(true));
        let f = || lit(Value::Bool(false));
        let n = || Expr::null();
        let ev = |e: Expr| e.eval_constant().unwrap();
        assert_eq!(ev(bin(BinOp::And, t(), n())), Value::Null);
        assert_eq!(ev(bin(BinOp::And, f(), n())), Value::Bool(false));
        assert_eq!(ev(bin(BinOp::And, n(), f())), Value::Bool(false));
        assert_eq!(ev(bin(BinOp::Or, t(), n())), Value::Bool(true));
        assert_eq!(ev(bin(BinOp::Or, n(), t())), Value::Bool(true));
        assert_eq!(ev(bin(BinOp::Or, f(), n())), Value::Null);
        assert_eq!(ev(Expr::Not(Box::new(n()))), Value::Null);
        assert_eq!(ev(Expr::Not(Box::new(t()))), Value::Bool(false));
        assert_eq!(ev(bin(BinOp::Eq, lit(Value::Int64(1)), n())), Value::Null);
        assert_eq!(ev(Expr::IsNull(Box::new(n()))), Value::Bool(true));
        assert_eq!(ev(Expr::IsNotNull(Box::new(n()))), Value::Bool(false));
        assert!(!bin(BinOp::Eq, lit(Value::Int64(1)), n())
            .eval_predicate(&EvalContext::row_only(&[]))
            .unwrap());
    }

    #[test]
    fn numeric_promotion_and_overflow() {
        let ev = |e: Expr| e.eval_constant().unwrap();
        assert_eq!(
            ev(bin(BinOp::Add, lit(Value::Int32(1)), lit(Value::Int64(2)))),
            Value::Int64(3)
        );
        assert_eq!(
            ev(bin(
                BinOp::Add,
                lit(Value::Int64(1)),
                lit(Value::Float64(0.5))
            )),
            Value::Float64(1.5)
        );
        assert_eq!(
            ev(bin(BinOp::Div, lit(Value::Int64(7)), lit(Value::Int64(2)))),
            Value::Float64(3.5)
        );
        assert_eq!(
            ev(bin(BinOp::Div, lit(Value::Int64(7)), lit(Value::Int64(0)))),
            Value::Null
        );
        assert_eq!(
            ev(bin(
                BinOp::IntDiv,
                lit(Value::Int64(-7)),
                lit(Value::Int64(2))
            )),
            Value::Int64(-3)
        );
        assert_eq!(
            ev(bin(
                BinOp::IntDiv,
                lit(Value::Int64(7)),
                lit(Value::Int64(0))
            )),
            Value::Null
        );
        assert!(bin(
            BinOp::IntDiv,
            lit(Value::Int64(i64::MIN)),
            lit(Value::Int64(-1))
        )
        .eval_constant()
        .is_err());
        assert_eq!(
            ev(bin(BinOp::Mod, lit(Value::Int64(7)), lit(Value::Int64(3)))),
            Value::Int64(1)
        );
        assert!(bin(
            BinOp::Add,
            lit(Value::Int64(i64::MAX)),
            lit(Value::Int64(1))
        )
        .eval_constant()
        .is_err());
        assert!(bin(
            BinOp::Add,
            lit(Value::String("a".into())),
            lit(Value::Int64(1))
        )
        .eval_constant()
        .is_err());
        assert_eq!(
            ev(bin(
                BinOp::Lt,
                lit(Value::Int32(1)),
                lit(Value::Float64(1.5))
            )),
            Value::Bool(true)
        );
        assert_eq!(
            ev(Expr::Negate(Box::new(lit(Value::Int32(5))))),
            Value::Int64(-5)
        );
        assert_eq!(
            arithmetic_result_type(BinOp::Add, DataType::Int32, DataType::Int32).unwrap(),
            DataType::Int64
        );
        assert_eq!(
            arithmetic_result_type(BinOp::Div, DataType::Int32, DataType::Int32).unwrap(),
            DataType::Float64
        );
    }

    #[test]
    fn test_decimal_exact_sum_and_float_difference() {
        let decimal_4_2 = DataType::Decimal {
            precision: 4,
            scale: 2,
        };
        let penny = Value::Decimal {
            value: 1,
            precision: 4,
            scale: 2,
        };
        let mut decimal_sum = Value::Decimal {
            value: 0,
            precision: 4,
            scale: 2,
        };
        let mut float_sum = 0.0_f64;

        for _ in 0..100 {
            // Keep the accumulator at its declared type rather than letting derived
            // arithmetic precision grow once per addition.
            decimal_sum = cast_value(
                arithmetic(BinOp::Add, &decimal_sum, &penny).unwrap(),
                decimal_4_2,
            )
            .unwrap();
            float_sum += 0.01_f64;
        }

        assert_eq!(
            decimal_sum,
            Value::Decimal {
                value: 100,
                precision: 4,
                scale: 2,
            }
        );
        assert_eq!(decimal_sum.to_string(), "1.00");
        assert_ne!(float_sum, 1.0_f64);
    }

    #[test]
    fn test_decimal_rounding_half_away_from_zero_for_rescale_division_and_cast() {
        let assert_decimal = |result: Value, expected_value, expected_precision, expected_scale| {
            let Value::Decimal {
                value,
                precision,
                scale,
            } = result
            else {
                panic!("expected DECIMAL");
            };
            assert_eq!(value, expected_value);
            assert_eq!(precision, expected_precision);
            assert_eq!(scale, expected_scale);
        };

        // Arithmetic rescaling uses DECIMAL's round-half-away-from-zero rule.
        assert_eq!(decimal_rescale(1_005, 3, 2).unwrap(), 101);
        assert_eq!(decimal_rescale(-1_005, 3, 2).unwrap(), -101);

        let one = Value::Decimal {
            value: 1,
            precision: 1,
            scale: 0,
        };
        let three = Value::Decimal {
            value: 3,
            precision: 1,
            scale: 0,
        };
        assert_decimal(
            arithmetic(BinOp::Div, &one, &three).unwrap(),
            3_333,
            MAX_DECIMAL_PRECISION,
            4,
        );
        assert_decimal(
            arithmetic(
                BinOp::Div,
                &Value::Decimal {
                    value: 2,
                    precision: 1,
                    scale: 0,
                },
                &three,
            )
            .unwrap(),
            6_667,
            MAX_DECIMAL_PRECISION,
            4,
        );
        assert_decimal(
            arithmetic(
                BinOp::Div,
                &Value::Decimal {
                    value: 12_345,
                    precision: 5,
                    scale: 2,
                },
                &one,
            )
            .unwrap(),
            123_450_000,
            MAX_DECIMAL_PRECISION,
            6,
        );

        let decimal_5_2 = DataType::Decimal {
            precision: 5,
            scale: 2,
        };
        assert_decimal(
            cast_value(Value::String("1.005".into()), decimal_5_2).unwrap(),
            101,
            5,
            2,
        );
        assert_decimal(
            cast_value(Value::String("-1.005".into()), decimal_5_2).unwrap(),
            -101,
            5,
            2,
        );
    }

    #[test]
    fn decimal_casts_integral_sources_at_target_scale() {
        let decimal_5_2 = DataType::Decimal {
            precision: 5,
            scale: 2,
        };

        assert_eq!(
            cast_value(Value::Int32(42), decimal_5_2).unwrap(),
            Value::Decimal {
                value: 4_200,
                precision: 5,
                scale: 2,
            }
        );
        assert_eq!(
            cast_value(Value::Int64(-42), decimal_5_2).unwrap(),
            Value::Decimal {
                value: -4_200,
                precision: 5,
                scale: 2,
            }
        );
        assert_eq!(
            cast_value(Value::Timestamp(7), decimal_5_2).unwrap(),
            Value::Decimal {
                value: 700,
                precision: 5,
                scale: 2,
            }
        );
    }

    #[test]
    fn d9_decimal_division_rounding_direction_uses_exact_quotient_sign() {
        let assert_decimal = |result: Value, expected_value| {
            let Value::Decimal {
                value,
                precision,
                scale,
            } = result
            else {
                panic!("expected DECIMAL");
            };
            assert_eq!(value, expected_value);
            assert_eq!(precision, MAX_DECIMAL_PRECISION);
            assert_eq!(scale, 4);
        };

        assert_eq!(decimal_rescale(-5, 2, 1).unwrap(), -1);
        assert_eq!(decimal_rescale(5, 2, 1).unwrap(), 1);

        let decimal = |value| Value::Decimal {
            value,
            precision: 1,
            scale: 0,
        };

        // Cover all dividend/divisor sign combinations through the division path. The first
        // division is exact; the last truncates to zero before rounding away from zero.
        assert_decimal(
            arithmetic(BinOp::Div, &decimal(1), &decimal(2)).unwrap(),
            5_000,
        );
        assert_decimal(
            arithmetic(BinOp::Div, &decimal(-1), &decimal(3)).unwrap(),
            -3_333,
        );
        assert_decimal(
            arithmetic(BinOp::Div, &decimal(1), &decimal(-3)).unwrap(),
            -3_333,
        );
        assert_decimal(
            arithmetic(BinOp::Div, &decimal(-1), &decimal(-1_500_000)).unwrap(),
            0,
        );
    }

    #[test]
    fn test_decimal_arithmetic_overflow_and_precision_errors() {
        // A DECIMAL result that fits its at-most-18-digit declaration also fits i64, so
        // stored-integer overflow of a valid result is unreachable. Derived precision is clamped.
        let precision_overflow = arithmetic(
            BinOp::Mul,
            &Value::Decimal {
                value: 999_999_999_999_999_999,
                precision: 18,
                scale: 0,
            },
            &Value::Decimal {
                value: 2,
                precision: 1,
                scale: 0,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            precision_overflow.contains("1999999999999999998")
                && precision_overflow.contains("DECIMAL(18,0)"),
            "{precision_overflow}"
        );

        let cast_precision_overflow = cast_value(
            Value::Int64(12_345),
            DataType::Decimal {
                precision: 3,
                scale: 2,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            cast_precision_overflow.contains("cannot cast"),
            "{cast_precision_overflow}"
        );
    }

    #[test]
    fn test_decimal_required_precision_is_clamped_to_supported_bound() {
        assert_eq!(
            arithmetic_result_type(
                BinOp::Mul,
                DataType::Decimal {
                    precision: 18,
                    scale: 0,
                },
                DataType::Decimal {
                    precision: 18,
                    scale: 0,
                },
            )
            .unwrap(),
            DataType::Decimal {
                precision: 18,
                scale: 0,
            }
        );
    }

    #[test]
    fn test_decimal_string_parsing_is_exact_and_rejects_non_decimal_syntax() {
        let decimal_18_2 = DataType::Decimal {
            precision: 18,
            scale: 2,
        };

        assert_eq!(
            cast_value(Value::String("1234567890123456.78".into()), decimal_18_2).unwrap(),
            Value::Decimal {
                value: 123_456_789_012_345_678,
                precision: 18,
                scale: 2,
            }
        );
        assert_eq!(
            cast_value(Value::String("000001.20".into()), decimal_18_2).unwrap(),
            Value::Decimal {
                value: 120,
                precision: 18,
                scale: 2,
            }
        );
        assert_eq!(
            cast_value(Value::String("1.2".into()), decimal_18_2).unwrap(),
            Value::Decimal {
                value: 120,
                precision: 18,
                scale: 2,
            }
        );
        assert!(cast_value(Value::String("1e-2".into()), decimal_18_2).is_err());
        assert!(cast_value(Value::String("not-a-number".into()), decimal_18_2).is_err());
    }

    #[test]
    fn d7_decimal_division_rounds_non_tie_remainders_correctly() {
        let third = Value::Decimal {
            value: 3,
            precision: 1,
            scale: 0,
        };
        let expected_positive = Value::Decimal {
            value: 3_333,
            precision: MAX_DECIMAL_PRECISION,
            scale: 4,
        };
        let expected_negative = Value::Decimal {
            value: -3_333,
            precision: MAX_DECIMAL_PRECISION,
            scale: 4,
        };

        assert_eq!(
            arithmetic(
                BinOp::Div,
                &Value::Decimal {
                    value: 1,
                    precision: 1,
                    scale: 0,
                },
                &third,
            )
            .unwrap(),
            expected_positive
        );
        assert_eq!(
            arithmetic(
                BinOp::Div,
                &Value::Decimal {
                    value: -1,
                    precision: 1,
                    scale: 0,
                },
                &third,
            )
            .unwrap(),
            expected_negative
        );
    }

    #[test]
    fn decimal_arithmetic_and_comparison() {
        let ev = |e: Expr| e.eval_constant().unwrap();

        assert_eq!(
            ev(bin(
                BinOp::Add,
                lit(Value::Decimal {
                    value: 125,
                    precision: 3,
                    scale: 2,
                }),
                lit(Value::Decimal {
                    value: 25,
                    precision: 2,
                    scale: 1,
                }),
            )),
            Value::Decimal {
                value: 375,
                precision: 4,
                scale: 2,
            }
        );
        assert_eq!(
            ev(bin(
                BinOp::Mul,
                lit(Value::Decimal {
                    value: 125,
                    precision: 3,
                    scale: 2,
                }),
                lit(Value::Decimal {
                    value: 20,
                    precision: 2,
                    scale: 1,
                }),
            )),
            Value::Decimal {
                value: 2500,
                precision: 6,
                scale: 3,
            }
        );
        assert_eq!(
            ev(bin(
                BinOp::Div,
                lit(Value::Decimal {
                    value: 1000,
                    precision: 4,
                    scale: 2,
                }),
                lit(Value::Decimal {
                    value: 400,
                    precision: 3,
                    scale: 2,
                }),
            )),
            Value::Decimal {
                value: 2_500_000,
                precision: MAX_DECIMAL_PRECISION,
                scale: 6,
            }
        );
        assert_eq!(
            ev(bin(
                BinOp::Eq,
                lit(Value::Decimal {
                    value: 100,
                    precision: 3,
                    scale: 2,
                }),
                lit(Value::Decimal {
                    value: 10,
                    precision: 2,
                    scale: 1,
                }),
            )),
            Value::Bool(true)
        );
        assert_eq!(
            arithmetic_result_type(
                BinOp::Add,
                DataType::Decimal {
                    precision: 5,
                    scale: 2,
                },
                DataType::Int32,
            )
            .unwrap(),
            DataType::Decimal {
                precision: 13,
                scale: 2,
            }
        );
    }

    #[test]
    fn like_in_between_case_cast() {
        let ev = |e: Expr| e.eval_constant().unwrap();
        assert!(like_match("Apple pie", "a%"));
        assert!(like_match("abc", "a_c"));
        assert!(!like_match("abc", "a_"));
        assert!(like_match("", "%"));
        let like = Expr::Like {
            expr: Box::new(lit(Value::String("hello".into()))),
            pattern: Box::new(lit(Value::String("h%o".into()))),
            negated: true,
        };
        assert_eq!(ev(like), Value::Bool(false));

        let in_list = |v: Value, negated: bool| Expr::In {
            expr: Box::new(lit(v)),
            list: vec![lit(Value::Int64(1)), Expr::null(), lit(Value::Int64(3))],
            negated,
        };
        assert_eq!(ev(in_list(Value::Int64(3), false)), Value::Bool(true));
        assert_eq!(ev(in_list(Value::Int64(2), false)), Value::Null);
        assert_eq!(ev(in_list(Value::Int64(3), true)), Value::Bool(false));
        assert_eq!(
            ev(Expr::In {
                expr: Box::new(lit(Value::Int64(2))),
                list: vec![lit(Value::Int64(1))],
                negated: true
            }),
            Value::Bool(true)
        );

        let between = |v: Value| Expr::Between {
            expr: Box::new(lit(v)),
            low: Box::new(lit(Value::Int64(1))),
            high: Box::new(lit(Value::Int64(3))),
            negated: false,
        };
        assert_eq!(ev(between(Value::Int64(3))), Value::Bool(true));
        assert_eq!(ev(between(Value::Int64(4))), Value::Bool(false));
        assert_eq!(ev(between(Value::Null)), Value::Null);

        let case = Expr::Case {
            operand: None,
            branches: vec![(
                bin(BinOp::Gt, lit(Value::Int64(1)), lit(Value::Int64(0))),
                lit(Value::String("pos".into())),
            )],
            else_result: Some(Box::new(bin(
                BinOp::Add,
                lit(Value::String("boom".into())),
                lit(Value::Int64(1)),
            ))),
            data_type: DataType::String,
            nullable: true,
        };
        // The ELSE branch would error if evaluated eagerly.
        assert_eq!(ev(case), Value::String("pos".into()));
        let simple_case = Expr::Case {
            operand: Some(Box::new(lit(Value::Int64(2)))),
            branches: vec![
                (lit(Value::Int64(1)), lit(Value::String("one".into()))),
                (lit(Value::Int64(2)), lit(Value::String("two".into()))),
            ],
            else_result: None,
            data_type: DataType::String,
            nullable: true,
        };
        assert_eq!(ev(simple_case), Value::String("two".into()));

        assert_eq!(
            cast_value(Value::String(" 42 ".into()), DataType::Int32).unwrap(),
            Value::Int32(42)
        );
        assert_eq!(
            cast_value(Value::Float64(2.6), DataType::Int64).unwrap(),
            Value::Int64(3)
        );
        assert_eq!(
            cast_value(Value::Int64(7), DataType::String).unwrap(),
            Value::String("7".into())
        );
        assert_eq!(
            cast_value(Value::Bool(true), DataType::Float64).unwrap(),
            Value::Float64(1.0)
        );
        assert!(cast_value(Value::String("x".into()), DataType::Int64).is_err());
        assert!(cast_value(Value::Int64(1 << 40), DataType::Int32).is_err());
        assert_eq!(
            cast_value(Value::Null, DataType::Int32).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn scalar_functions() {
        let ev = |e: Expr| e.eval_constant().unwrap();
        let sf = |func, args: Vec<Expr>, dt| Expr::ScalarFunction {
            func,
            args,
            data_type: dt,
            nullable: true,
        };
        assert_eq!(
            ev(sf(
                ScalarFn::Upper,
                vec![lit(Value::String("ab".into()))],
                DataType::String
            )),
            Value::String("AB".into())
        );
        assert_eq!(
            ev(sf(
                ScalarFn::Length,
                vec![lit(Value::String("héllo".into()))],
                DataType::Int64
            )),
            Value::Int64(6)
        );
        assert_eq!(
            ev(sf(
                ScalarFn::CharLength,
                vec![lit(Value::String("héllo".into()))],
                DataType::Int64
            )),
            Value::Int64(5)
        );
        assert_eq!(
            ev(sf(
                ScalarFn::Concat,
                vec![
                    lit(Value::String("a".into())),
                    lit(Value::Int64(1)),
                    lit(Value::Bool(true))
                ],
                DataType::String
            )),
            Value::String("a11".into())
        );
        assert_eq!(
            ev(sf(
                ScalarFn::Concat,
                vec![lit(Value::String("a".into())), Expr::null()],
                DataType::String
            )),
            Value::Null
        );
        assert_eq!(
            ev(sf(
                ScalarFn::Coalesce,
                vec![Expr::null(), lit(Value::Int64(2))],
                DataType::Int64
            )),
            Value::Int64(2)
        );
        assert_eq!(
            ev(sf(
                ScalarFn::NullIf,
                vec![lit(Value::Int64(2)), lit(Value::Int64(2))],
                DataType::Int64
            )),
            Value::Null
        );
        assert_eq!(
            ev(sf(
                ScalarFn::Abs,
                vec![lit(Value::Float64(-1.5))],
                DataType::Float64
            )),
            Value::Float64(1.5)
        );
    }

    #[test]
    fn calendar_intervals_extract_and_substring() {
        let date = |value| parse_date_to_timestamp_micros(value).unwrap();
        let date_expr = |value, quantity, unit, negated| Expr::CalendarInterval {
            expr: Box::new(lit(Value::Timestamp(date(value)))),
            quantity: Box::new(lit(Value::Int64(quantity))),
            unit,
            negated,
        };
        let as_date = |expr: Expr| match expr.eval_constant().unwrap() {
            Value::Timestamp(value) => timestamp_micros_to_date(value).unwrap(),
            other => panic!("expected timestamp, found {other:?}"),
        };

        assert_eq!(
            as_date(date_expr(
                "2023-01-31",
                1,
                CalendarIntervalUnit::Month,
                false
            )),
            "2023-02-28"
        );
        assert_eq!(
            as_date(date_expr(
                "2024-01-31",
                1,
                CalendarIntervalUnit::Month,
                false
            )),
            "2024-02-29"
        );
        assert_eq!(
            as_date(date_expr(
                "2024-02-29",
                1,
                CalendarIntervalUnit::Year,
                false
            )),
            "2025-02-28"
        );
        assert_eq!(
            as_date(date_expr("2024-03-01", 1, CalendarIntervalUnit::Day, true)),
            "2024-02-29"
        );
        assert_eq!(
            Expr::CalendarInterval {
                expr: Box::new(Expr::null()),
                quantity: Box::new(lit(Value::Int64(1))),
                unit: CalendarIntervalUnit::Day,
                negated: false,
            }
            .eval_constant()
            .unwrap(),
            Value::Null
        );

        let extract = |unit| Expr::ScalarFunction {
            func: ScalarFn::Extract(unit),
            args: vec![lit(Value::Timestamp(date("2024-02-29")))],
            data_type: DataType::Int64,
            nullable: true,
        };
        assert_eq!(
            extract(CalendarIntervalUnit::Year).eval_constant().unwrap(),
            Value::Int64(2024)
        );
        assert_eq!(
            extract(CalendarIntervalUnit::Month)
                .eval_constant()
                .unwrap(),
            Value::Int64(2)
        );
        assert_eq!(
            extract(CalendarIntervalUnit::Day).eval_constant().unwrap(),
            Value::Int64(29)
        );
        assert_eq!(
            Expr::ScalarFunction {
                func: ScalarFn::Extract(CalendarIntervalUnit::Day),
                args: vec![Expr::null()],
                data_type: DataType::Int64,
                nullable: true,
            }
            .eval_constant()
            .unwrap(),
            Value::Null
        );

        let substring = |value: &str, start: i64, length: i64| Expr::ScalarFunction {
            func: ScalarFn::Substring,
            args: vec![
                lit(Value::String(value.into())),
                lit(Value::Int64(start)),
                lit(Value::Int64(length)),
            ],
            data_type: DataType::String,
            nullable: true,
        };
        assert_eq!(
            substring("abcdef", 2, 3).eval_constant().unwrap(),
            Value::String("bcd".into())
        );
        assert_eq!(
            substring("abcdef", 20, 3).eval_constant().unwrap(),
            Value::String(String::new())
        );
        assert_eq!(
            substring("abcdef", 2, 0).eval_constant().unwrap(),
            Value::String(String::new())
        );
        assert_eq!(
            substring("abcdef", 2, -1).eval_constant().unwrap(),
            Value::String(String::new())
        );
        assert_eq!(
            substring("héllo", 2, 2).eval_constant().unwrap(),
            Value::String("él".into())
        );
    }

    #[test]
    fn subquery_and_aggregate_context() {
        let subqueries = vec![
            vec![Row::new(vec![Value::Int64(5)])],
            vec![],
            vec![
                Row::new(vec![Value::Int64(1)]),
                Row::new(vec![Value::Int64(2)]),
            ],
        ];
        let aggregates = vec![Value::Int64(9)];
        let ctx = EvalContext {
            row: &[],
            current_outer_row: None,
            aggregates: &aggregates,
            output: None,
            subqueries: &subqueries,
            variables: None,
            subquery_runner: None,
            subquery_budget: None,
        };
        let scalar = |index| Expr::ScalarSubquery {
            index,
            data_type: DataType::Int64,
            correlated: false,
        };
        assert_eq!(scalar(0).eval(&ctx).unwrap(), Value::Int64(5));
        assert_eq!(scalar(1).eval(&ctx).unwrap(), Value::Null);
        assert!(scalar(2).eval(&ctx).is_err());
        assert_eq!(
            Expr::Exists {
                index: 1,
                negated: false,
                correlated: false,
            }
            .eval(&ctx)
            .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            Expr::InSubquery {
                expr: Box::new(lit(Value::Int64(2))),
                index: 2,
                negated: false,
                correlated: false,
            }
            .eval(&ctx)
            .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            Expr::AggregateRef {
                index: 0,
                data_type: DataType::Int64,
                nullable: true
            }
            .eval(&ctx)
            .unwrap(),
            Value::Int64(9)
        );
    }

    struct EchoOuterRow;

    impl SubqueryRunner for EchoOuterRow {
        fn run(&self, _index: usize, outer_row: &[Value]) -> Result<Vec<Row>> {
            Ok(vec![Row::new(vec![outer_row[0].clone()])])
        }
    }

    #[test]
    fn subquery_runner_trait_and_correlation_eval() {
        let budget = SubqueryBudget::new(10, 5);
        let runner = EchoOuterRow;
        let outer_row = [Value::Int64(42)];
        let expr = Expr::ScalarSubquery {
            index: 0,
            data_type: DataType::Int64,
            correlated: true,
        };
        let ctx = EvalContext {
            row: &[Value::Int64(999)],
            current_outer_row: Some(&outer_row),
            aggregates: &[],
            output: None,
            subqueries: &[],
            variables: None,
            subquery_runner: Some(&runner),
            subquery_budget: Some(&budget),
        };

        // Correlated subqueries receive the immediate parent's current row (`ctx.row`),
        // not the enclosing grandparent row (`current_outer_row`).
        assert_eq!(expr.eval(&ctx).unwrap(), Value::Int64(999));

        let unwired = EvalContext::row_only(&[]);
        let err = expr.eval(&unwired).unwrap_err().to_string();
        assert!(err.contains("without a subquery runner"), "{err}");
    }

    struct RecursiveRunner<'a> {
        budget: &'a SubqueryBudget,
        calls: std::cell::Cell<usize>,
    }

    impl SubqueryRunner for RecursiveRunner<'_> {
        fn run(&self, index: usize, outer_row: &[Value]) -> Result<Vec<Row>> {
            self.calls.set(self.calls.get() + 1);
            let expr = Expr::ScalarSubquery {
                index,
                data_type: DataType::Int64,
                correlated: true,
            };
            let ctx = EvalContext {
                row: &[],
                current_outer_row: Some(outer_row),
                aggregates: &[],
                output: None,
                subqueries: &[],
                variables: None,
                subquery_runner: Some(self),
                subquery_budget: Some(self.budget),
            };
            expr.eval(&ctx)?;
            unreachable!("the nesting-depth cap must stop recursion")
        }
    }

    #[test]
    fn subquery_invocation_and_nesting_caps() {
        let depth_budget = SubqueryBudget::new(100, 5);
        let runner = RecursiveRunner {
            budget: &depth_budget,
            calls: std::cell::Cell::new(0),
        };
        let outer_row = [Value::Int64(1)];
        let expr = Expr::ScalarSubquery {
            index: 0,
            data_type: DataType::Int64,
            correlated: true,
        };
        let ctx = EvalContext {
            row: &[],
            current_outer_row: Some(&outer_row),
            aggregates: &[],
            output: None,
            subqueries: &[],
            variables: None,
            subquery_runner: Some(&runner),
            subquery_budget: Some(&depth_budget),
        };
        let err = expr.eval(&ctx).unwrap_err().to_string();
        assert!(err.contains("nesting-depth cap (5) exceeded"), "{err}");
        assert_eq!(runner.calls.get(), 5);

        let invocation_budget = SubqueryBudget::new(10_000, 1);
        for _ in 0..10_000 {
            invocation_budget.enter().unwrap();
            invocation_budget.exit();
        }
        let err = invocation_budget.enter().unwrap_err().to_string();
        assert!(err.contains("invocation cap (10000) exceeded"), "{err}");
    }

    #[test]
    fn expr_type_inference() {
        let col = Expr::ColumnRef {
            slot: 0,
            column: 1,
            offset: 1,
            name: "a".into(),
            data_type: DataType::Int32,
            nullable: true,
        };
        assert_eq!(
            bin(BinOp::Add, col.clone(), lit(Value::Int64(1))).expr_type(),
            ExprType::new(DataType::Int64, true)
        );
        assert_eq!(
            bin(BinOp::Gt, col.clone(), lit(Value::Int64(1))).expr_type(),
            ExprType::new(DataType::Bool, true)
        );
        assert_eq!(
            Expr::IsNull(Box::new(col.clone())).expr_type(),
            ExprType::new(DataType::Bool, false)
        );
        assert!(Expr::null().expr_type().is_null_literal);
        assert_eq!(col.referenced_columns(), vec![(0, 1)]);
        assert!(!col.contains_aggregate());
        assert_eq!(
            Expr::Cast {
                expr: Box::new(col),
                to: DataType::String
            }
            .expr_type()
            .data_type,
            DataType::String
        );
        let window = Expr::WindowRef {
            index: 0,
            data_type: DataType::Int64,
            nullable: false,
        };
        assert_eq!(window.expr_type(), ExprType::new(DataType::Int64, false));
        assert!(window.contains_window());
        assert!(!window.contains_aggregate());

        let var = Expr::Variable {
            name: "x".into(),
            is_system: false,
        };
        let t = var.expr_type();
        assert!(t.is_dynamic);
        assert!(t.is_permissive());
        assert!(t.nullable);
    }

    struct FakeVars(std::collections::BTreeMap<&'static str, Value>);

    impl VariableLookup for FakeVars {
        fn lookup(&self, name: &str, is_system: bool) -> Result<Value> {
            match self.0.get(name) {
                Some(v) => Ok(v.clone()),
                None if is_system => Err(HtapError::Unsupported(format!(
                    "unknown system variable '{name}'"
                ))),
                None => Ok(Value::Null),
            }
        }
    }

    fn assert_decimal(
        value: Value,
        expected_value: i64,
        expected_precision: u8,
        expected_scale: u8,
    ) {
        let Value::Decimal {
            value,
            precision,
            scale,
        } = value
        else {
            panic!("expected DECIMAL");
        };
        assert_eq!(value, expected_value);
        assert_eq!(precision, expected_precision);
        assert_eq!(scale, expected_scale);
    }

    #[test]
    fn decimal_cast_negation_and_abs() {
        let decimal = |value, precision, scale| Value::Decimal {
            value,
            precision,
            scale,
        };

        assert_eq!(
            cast_value(decimal(1_234, 4, 2), DataType::Int64).unwrap(),
            Value::Int64(12)
        );
        assert_eq!(
            cast_value(decimal(1_250, 4, 2), DataType::Int64).unwrap(),
            Value::Int64(13)
        );
        assert_eq!(
            cast_value(decimal(-1_250, 4, 2), DataType::Int64).unwrap(),
            Value::Int64(-13)
        );

        let column = Expr::ColumnRef {
            slot: 0,
            column: 0,
            offset: 0,
            name: "amount".into(),
            data_type: DataType::Decimal {
                precision: 18,
                scale: 0,
            },
            nullable: false,
        };
        let row = [decimal(-999_999_999_999_999_999, 18, 0)];
        assert_decimal(
            Expr::Negate(Box::new(column))
                .eval(&EvalContext::row_only(&row))
                .unwrap(),
            999_999_999_999_999_999,
            18,
            0,
        );

        let expression = Expr::Negate(Box::new(bin(
            BinOp::Add,
            lit(decimal(125, 3, 2)),
            lit(decimal(25, 2, 1)),
        )));
        assert_decimal(expression.eval_constant().unwrap(), -375, 4, 2);

        let abs = |value| Expr::ScalarFunction {
            func: ScalarFn::Abs,
            args: vec![lit(decimal(value, 4, 2))],
            data_type: DataType::Decimal {
                precision: 4,
                scale: 2,
            },
            nullable: false,
        };
        assert_decimal(abs(123).eval_constant().unwrap(), 123, 4, 2);
        assert_decimal(abs(-123).eval_constant().unwrap(), 123, 4, 2);
        assert_decimal(abs(0).eval_constant().unwrap(), 0, 4, 2);
    }

    #[test]
    fn conditional_integer_branch_conforms_to_declared_decimal_type() {
        let expression = Expr::Case {
            operand: None,
            branches: vec![(lit(Value::Bool(true)), lit(Value::Int64(7)))],
            else_result: Some(Box::new(lit(Value::Decimal {
                value: 125,
                precision: 5,
                scale: 2,
            }))),
            data_type: DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            nullable: false,
        };

        assert_decimal(expression.eval_constant().unwrap(), 700, 12, 2);
    }

    #[test]
    fn null_coalescing_integer_column_conforms_to_declared_decimal_type() {
        let expression = Expr::ScalarFunction {
            func: ScalarFn::Coalesce,
            args: vec![
                Expr::ColumnRef {
                    slot: 0,
                    column: 0,
                    offset: 0,
                    name: "whole_amount".into(),
                    data_type: DataType::Int64,
                    nullable: true,
                },
                Expr::ColumnRef {
                    slot: 0,
                    column: 1,
                    offset: 1,
                    name: "decimal_amount".into(),
                    data_type: DataType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    nullable: true,
                },
            ],
            data_type: DataType::Decimal {
                precision: 12,
                scale: 2,
            },
            nullable: true,
        };
        let row = [
            Value::Int64(7),
            Value::Decimal {
                value: 125,
                precision: 12,
                scale: 2,
            },
        ];

        assert_decimal(
            expression.eval(&EvalContext::row_only(&row)).unwrap(),
            700,
            12,
            2,
        );
    }

    #[test]
    fn variable_evaluation() {
        let user_var = Expr::Variable {
            name: "x".into(),
            is_system: false,
        };
        let sys_var = Expr::Variable {
            name: "autocommit".into(),
            is_system: true,
        };

        // No variable context at all: user variable defaults to NULL, system variable errors.
        assert_eq!(
            user_var.eval(&EvalContext::row_only(&[])).unwrap(),
            Value::Null
        );
        assert!(sys_var.eval(&EvalContext::row_only(&[])).is_err());

        // With a lookup: values come from it, and unknown system variables still error.
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("x", Value::Int64(7));
        vars.insert("autocommit", Value::Int64(1));
        let fake = FakeVars(vars);
        let ctx = EvalContext {
            row: &[],
            current_outer_row: None,
            aggregates: &[],
            output: None,
            subqueries: &[],
            variables: Some(&fake),
            subquery_runner: None,
            subquery_budget: None,
        };
        assert_eq!(user_var.eval(&ctx).unwrap(), Value::Int64(7));
        assert_eq!(sys_var.eval(&ctx).unwrap(), Value::Int64(1));
        let unknown = Expr::Variable {
            name: "no_such_var".into(),
            is_system: true,
        };
        assert!(unknown.eval(&ctx).is_err());
    }
}
