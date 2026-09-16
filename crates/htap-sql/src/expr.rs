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

use htap_common::error::{HtapError, Result};
use htap_common::types::{DataType, Row, Value};

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
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod
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
    /// [`ExprType::is_null_literal`].
    pub data_type: DataType,
    /// Whether the expression can evaluate to `NULL`.
    pub nullable: bool,
    /// Whether the expression is the literal `NULL`.
    pub is_null_literal: bool,
}

impl ExprType {
    fn new(data_type: DataType, nullable: bool) -> Self {
        Self {
            data_type,
            nullable,
            is_null_literal: false,
        }
    }

    /// Whether the type is numeric (`Int32`, `Int64`, `Float64`).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.data_type,
            DataType::Int32 | DataType::Int64 | DataType::Float64
        )
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
    /// Uncorrelated scalar subquery, precomputed by the executor.
    ScalarSubquery {
        /// Index into the query's subquery list.
        index: usize,
        /// Result type.
        data_type: DataType,
    },
    /// `expr [NOT] IN (subquery)`, subquery precomputed by the executor.
    InSubquery {
        /// Value expression.
        expr: Box<Expr>,
        /// Index into the query's subquery list.
        index: usize,
        /// `NOT IN`.
        negated: bool,
    },
    /// `[NOT] EXISTS (subquery)`, subquery precomputed by the executor.
    Exists {
        /// Index into the query's subquery list.
        index: usize,
        /// `NOT EXISTS`.
        negated: bool,
    },
}

/// Values an [`Expr`] may need while being evaluated.
#[derive(Debug, Clone, Copy)]
pub struct EvalContext<'a> {
    /// Flat joined input row (all slots concatenated in slot order).
    pub row: &'a [Value],
    /// Aggregate results of the current group, indexed like the query's aggregate list.
    pub aggregates: &'a [Value],
    /// Projected output row of the current input row / group, if already computed.
    pub output: Option<&'a [Value]>,
    /// Precomputed subquery results, indexed like the query's subquery list.
    pub subqueries: &'a [Vec<Row>],
}

impl<'a> EvalContext<'a> {
    /// Context with only an input row.
    pub fn row_only(row: &'a [Value]) -> Self {
        Self {
            row,
            aggregates: &[],
            output: None,
            subqueries: &[],
        }
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
            },
            Expr::Literal(v) => ExprType::new(v.data_type().unwrap_or(DataType::String), false),
            Expr::BinaryOp { op, left, right } => {
                let l = left.expr_type();
                let r = right.expr_type();
                let nullable = l.nullable || r.nullable;
                if op.is_comparison() || matches!(op, BinOp::And | BinOp::Or) {
                    ExprType::new(DataType::Bool, nullable)
                } else {
                    ExprType::new(
                        arithmetic_result_type(*op, l.data_type, r.data_type),
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
            Expr::ScalarSubquery { data_type, .. } => ExprType::new(*data_type, true),
            Expr::InSubquery { .. } => ExprType::new(DataType::Bool, true),
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

    /// Whether the expression references any input column.
    pub fn references_columns(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if matches!(e, Expr::ColumnRef { .. }) {
                found = true;
            }
        });
        found
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
            | Expr::OutputColumn { .. }
            | Expr::Literal(_)
            | Expr::AggregateRef { .. }
            | Expr::ScalarSubquery { .. }
            | Expr::Exists { .. } => {}
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
            Expr::ScalarSubquery { index, .. } => {
                let rows = subquery_rows(ctx, *index)?;
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
            } => {
                let v = expr.eval(ctx)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                let rows = subquery_rows(ctx, *index)?;
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
            Expr::Exists { index, negated } => {
                let rows = subquery_rows(ctx, *index)?;
                Ok(Value::Bool(rows.is_empty() == *negated))
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
        _ => Ok(v),
    }
}

fn subquery_rows<'a>(ctx: &EvalContext<'a>, index: usize) -> Result<&'a Vec<Row>> {
    ctx.subqueries
        .get(index)
        .ok_or_else(|| HtapError::Internal(format!("subquery {index} not available")))
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

/// Result type of arithmetic after numeric promotion.
pub fn arithmetic_result_type(op: BinOp, l: DataType, r: DataType) -> DataType {
    if op == BinOp::Div {
        return DataType::Float64;
    }
    if l == DataType::Float64 || r == DataType::Float64 {
        DataType::Float64
    } else {
        DataType::Int64
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
        return Ok(Value::Float64(x / y));
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
            Ok(Value::Float64(match op {
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
            }))
        }
    }
}

fn num_to_f64(n: &Num) -> f64 {
    match n {
        Num::Int(i) => *i as f64,
        Num::Float(f) => *f,
    }
}

/// SQL comparison. Returns `None` when either side is `NULL`.
///
/// Numeric values compare after promotion; other values must have the same type.
pub fn compare(l: &Value, r: &Value) -> Result<Option<std::cmp::Ordering>> {
    if l.is_null() || r.is_null() {
        return Ok(None);
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
            Value::String(s) => s.trim().parse().map_err(|_| bad(&v))?,
            _ => return Err(bad(&v)),
        }),
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
            arithmetic_result_type(BinOp::Add, DataType::Int32, DataType::Int32),
            DataType::Int64
        );
        assert_eq!(
            arithmetic_result_type(BinOp::Div, DataType::Int32, DataType::Int32),
            DataType::Float64
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
            aggregates: &aggregates,
            output: None,
            subqueries: &subqueries,
        };
        let scalar = |index| Expr::ScalarSubquery {
            index,
            data_type: DataType::Int64,
        };
        assert_eq!(scalar(0).eval(&ctx).unwrap(), Value::Int64(5));
        assert_eq!(scalar(1).eval(&ctx).unwrap(), Value::Null);
        assert!(scalar(2).eval(&ctx).is_err());
        assert_eq!(
            Expr::Exists {
                index: 1,
                negated: false
            }
            .eval(&ctx)
            .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            Expr::InSubquery {
                expr: Box::new(lit(Value::Int64(2))),
                index: 2,
                negated: false
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
    }
}
