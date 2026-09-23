//! Executor for the general query path ([`BoundQuery`]).
//!
//! # Model
//!
//! Every base table side is materialized as logical rows through the same storage path the
//! narrow analytic scan uses ([`scan_partition_compact`]): `Row` partitions come from the
//! LSM rowstore, `Column` and manifest-bearing `Converting` partitions from the columnar
//! segments with the rowstore delta overlaid. Joins, filters, grouping, ordering and set
//! operations then run over those rows in memory as separate, sequential stages:
//!
//! 1. subqueries (uncorrelated, executed once),
//! 2. slot materialization (per-slot column projection, partition pruning, single-leaf
//!    predicate pushdown derived from the `WHERE` conjuncts of that slot),
//! 3. left-deep joins (hash join on equi-conjuncts, nested loop otherwise),
//! 4. `WHERE`, 5. grouping and aggregation, 6. projection, 7. `HAVING`,
//! 8. `DISTINCT`, 9. `ORDER BY`, 10. `LIMIT`/`OFFSET`, 11. `UNION`.
//!
//! # Snapshot
//!
//! One MVCC [`Snapshot`] is taken per statement and used for every slot, every partition,
//! and every subquery, so all sides of a join across storage formats observe the same
//! committed version. Partition pruning only chooses which partitions are scanned; it
//! never suppresses the rowstore delta overlay inside a scanned partition.
//!
//! # Determinism
//!
//! Without `ORDER BY` the output order is: slot-0 rows in scan order (partition order, then
//! primary-key order within a partition), each joined with its matching right rows in the
//! right input's scan order; unmatched preserved rows of an outer join come after the
//! matched output. Groups are emitted in ascending order of their `GROUP BY` key tuple.
//!
//! # Limits
//!
//! Intermediate results (scanned rows, hash tables, groups) are held in memory without
//! bounds; there is no spilling and no cost-based planning.

use std::cell::Cell;
use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use htap_catalog::{CatalogSnapshot, TableDescriptor};
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_rowstore::Snapshot;
use htap_sql::ast::{AnalyticFilter, ComparisonOp};
use htap_sql::expr::{
    cast_value, compare, AggFn, AggregateSpec, BinOp, EvalContext, Expr, SubqueryBudget,
    SubqueryRunner, VariableLookup,
};
use htap_sql::optimize::{optimize, BuildSide, StatsLookup};
use htap_sql::query::{
    BoundQuery, JoinKind, JoinSpec, JoinTree, OrderItem, PeerFrameBound, QueryBody, RowFrameBound,
    SelectBody, SetOpKind, TableSlot, ValueFrameBound, WindowFrame, WindowFrameDirection,
    WindowFunctionKind,
};
use htap_sql::result::StatementResult;

use crate::memory_budget::MemoryBudget;
use crate::session::WriteSet;
use crate::spill::{OperatorKind, SpillDir, SpillHeader, SpillKind, SpillReader, SpillWriter};
use crate::{olap, scan_partition_compact, OwnedServer};

// Cap concurrent spill writers to limit file descriptors and unbudgeted writer buffers.
const HASH_JOIN_MAX_SPILL_PARTITIONS: usize = 128;
const HASH_JOIN_PARALLEL_THRESHOLD: usize = 10_000;
const GROUP_BY_SPILL_PARTITIONS: usize = 16;
const GROUP_BY_PARALLEL_THRESHOLD: usize = 10_000;
const SET_OPERATION_SPILL_PARTITIONS: usize = 16;
const SORT_MERGE_FAN_IN: usize = 4;
static NEXT_SPILL_STATEMENT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    // Query execution is synchronous on its caller thread. Tests read this after execute()
    // returns, while workers only update it indirectly through the parent-stage gate.
    static LAST_QUERY_PARALLEL_WORKERS: Cell<usize> = const { Cell::new(1) };
    // Spill creation occurs on the statement thread before an operator writes scratch files.
    static LAST_QUERY_SPILLED_OPERATORS: Cell<u8> = const { Cell::new(0) };
    // Optimizer planning occurs on the statement thread before a query body is executed.
    // This lets regression tests distinguish prepared correlated subqueries from per-row plans.
    static LAST_QUERY_OPTIMIZER_INVOCATIONS: Cell<usize> = const { Cell::new(0) };
}

impl OwnedServer {
    /// Returns the largest worker count used by the caller thread's most recent query.
    ///
    /// This is intentionally lightweight execution telemetry for integration tests: it avoids
    /// changing query results or EXPLAIN output merely to prove an operator took its parallel path.
    pub fn last_query_parallel_workers(&self) -> usize {
        let _ = self;
        LAST_QUERY_PARALLEL_WORKERS.with(Cell::get)
    }

    /// Returns whether the hash join operator spilled in the caller thread's most recent query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_hash_join_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::HashJoin)
    }

    /// Returns whether the GROUP BY operator spilled in the caller thread's most recent query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_group_by_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::GroupBy)
    }

    /// Returns whether the sort operator spilled in the caller thread's most recent query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_sort_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::Sort)
    }

    /// Returns whether the DISTINCT operator spilled in the caller thread's most recent query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_distinct_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::Distinct)
    }

    /// Returns whether the set-operation operator spilled in the caller thread's most recent
    /// query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_set_operation_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::SetOperation)
    }

    /// Returns whether the window operator spilled in the caller thread's most recent query.
    ///
    /// This is lightweight execution telemetry for integration tests and does not affect query
    /// results or EXPLAIN output.
    pub fn last_query_window_spilled(&self) -> bool {
        let _ = self;
        last_query_spilled_operator(OperatorKind::Window)
    }

    /// Returns the optimizer invocation count for the caller thread's most recent query.
    ///
    /// This is test telemetry used to verify that correlated subqueries are prepared once per
    /// statement rather than re-optimized for every outer row.
    pub fn last_query_optimizer_invocations(&self) -> usize {
        let _ = self;
        LAST_QUERY_OPTIMIZER_INVOCATIONS.with(Cell::get)
    }
}

fn reset_parallel_workers() {
    LAST_QUERY_PARALLEL_WORKERS.with(|workers| workers.set(1));
}

fn record_parallel_workers(worker_count: usize) {
    LAST_QUERY_PARALLEL_WORKERS.with(|workers| {
        workers.set(workers.get().max(worker_count));
    });
}

fn spill_operator_bit(operator: OperatorKind) -> u8 {
    1 << (operator as u8 - 1)
}

fn reset_spill_count() {
    LAST_QUERY_SPILLED_OPERATORS.with(|operators| operators.set(0));
}

fn record_spill(operator: OperatorKind) {
    LAST_QUERY_SPILLED_OPERATORS.with(|operators| {
        operators.set(operators.get() | spill_operator_bit(operator));
    });
}

fn last_query_spilled_operator(operator: OperatorKind) -> bool {
    LAST_QUERY_SPILLED_OPERATORS
        .with(|operators| operators.get() & spill_operator_bit(operator) != 0)
}

fn reset_optimizer_invocations() {
    LAST_QUERY_OPTIMIZER_INVOCATIONS.with(|count| count.set(0));
}

fn record_optimizer_invocation() {
    LAST_QUERY_OPTIMIZER_INVOCATIONS.with(|count| count.set(count.get() + 1));
}

/// Per-statement execution context.
pub(crate) struct ExecContext<'a> {
    pub server: &'a OwnedServer,
    pub catalog: &'a CatalogSnapshot,
    pub snapshot: Snapshot,
    /// Open transaction's buffered write set to overlay below relational operators, or `None`
    /// in autocommit mode (byte-for-byte the pre-Phase-10 read path).
    pub write_set: Option<&'a WriteSet>,
    /// Source of values for `@name`/`@@name` expressions (Phase 10 task 7): the session's user
    /// variables and live state, or [`crate::session::DefaultVariables`] when there is no
    /// session (`LocalServer::execute`).
    pub variables: Option<&'a dyn VariableLookup>,
    /// Rows produced by the current recursive CTE iteration. Present only while evaluating a
    /// recursive term containing a [`TableSlot::WorkingTableSlot`].
    pub working_rows: Option<&'a [Row]>,
    /// Expected column count of each row in `working_rows`.
    pub working_width: usize,
    /// Statement-scoped memory budget shared by relational operators.
    pub memory_budget: Arc<MemoryBudget>,
    /// Canonical server data root used for statement-scoped scratch files.
    pub spill_root: &'a Path,
    /// Maximum worker count shared by all parallel relational stages in this statement.
    pub parallelism: usize,
    /// Test/benchmark switch controlling optimizer execution for this statement and descendants.
    pub optimization_mode: OptimizationMode,
}

/// Selects whether the test/benchmark query path executes the optimizer output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptimizationMode {
    /// Disables optimization for differential execution against the identity plan.
    #[allow(dead_code)]
    Disabled,
    /// Enables optimizer planning before query execution.
    #[default]
    Enabled,
}

struct CatalogStats<'a>(&'a CatalogSnapshot);

impl StatsLookup for CatalogStats<'_> {
    fn table_stats(&self, table: &str) -> Option<&htap_catalog::TableStats> {
        self.0
            .table_by_name(table)
            .and_then(|table| table.stats.as_ref())
    }
}

/// Inputs required to execute a query with a selected optimization mode.
pub(crate) struct ExecuteQueryInput<'a> {
    /// Server providing storage access and query configuration.
    pub server: &'a OwnedServer,
    /// Bound query plan to execute.
    pub query: &'a BoundQuery,
    /// Catalog snapshot used to resolve table metadata.
    pub catalog: &'a CatalogSnapshot,
    /// MVCC snapshot used for all query reads.
    pub snapshot: Snapshot,
    /// Optional transactional writes to overlay on query reads.
    pub write_set: Option<&'a WriteSet>,
    /// Optional session variable lookup for expression evaluation.
    pub variables: Option<&'a dyn VariableLookup>,
    /// Selects whether the optimizer is applied before execution.
    pub optimization_mode: OptimizationMode,
    /// Maximum memory available to relational operators.
    pub memory_budget: usize,
    /// Maximum number of workers available to parallel operators.
    pub parallelism: usize,
}

/// Executes a bound query and returns its result set.
pub(crate) fn execute_query(
    server: &OwnedServer,
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    snapshot: Snapshot,
    write_set: Option<&WriteSet>,
    variables: Option<&dyn VariableLookup>,
) -> Result<StatementResult> {
    execute_query_with_mode(ExecuteQueryInput {
        server,
        query,
        catalog,
        snapshot,
        write_set,
        variables,
        optimization_mode: OptimizationMode::default(),
        memory_budget: server.query_memory_budget.load(Ordering::Relaxed),
        parallelism: server.query_parallelism.load(Ordering::Relaxed),
    })
}

/// Test/benchmark entry point for differential execution of optimized and identity plans.
pub(crate) fn execute_query_with_mode(input: ExecuteQueryInput<'_>) -> Result<StatementResult> {
    reset_parallel_workers();
    reset_spill_count();
    reset_optimizer_invocations();

    let ExecuteQueryInput {
        server,
        query,
        catalog,
        snapshot,
        write_set,
        variables,
        optimization_mode,
        memory_budget,
        parallelism,
    } = input;
    let ctx = ExecContext {
        server,
        catalog,
        snapshot,
        write_set,
        variables,
        working_rows: None,
        working_width: 0,
        memory_budget: Arc::new(MemoryBudget::new(memory_budget)),
        spill_root: server.data_root(),
        parallelism: parallelism.max(1),
        optimization_mode,
    };
    let rows = run_query(&ctx, query)?;
    let dynamic = dynamic_output_flags(&query.body);
    let columns = infer_dynamic_column_types(query.output_columns.clone(), &dynamic, &rows);
    Ok(StatementResult::query(columns, rows))
}

/// Per-output-column flags mirroring `htap_sql::binder_query::body_output_columns`'s structure:
/// `true` where the projection expression is [`htap_sql::expr::ExprType::is_dynamic`] (an
/// `Expr::Variable`, whose bind-time static type is a placeholder, not its real type).
fn dynamic_output_flags(body: &QueryBody) -> Vec<bool> {
    match body {
        QueryBody::Select(sel) => sel
            .projection
            .iter()
            .map(|p| p.expr.expr_type().is_dynamic)
            .collect(),
        QueryBody::SetOp { left, right, .. } => {
            let l = dynamic_output_flags(&left.body);
            let r = dynamic_output_flags(&right.body);
            l.iter().zip(r.iter()).map(|(a, b)| *a || *b).collect()
        }
        QueryBody::RecursiveQueryBody { output_columns, .. } => {
            vec![false; output_columns.len()]
        }
    }
}

/// Storage-reviewer finding F10: a result column whose bind-time static type is
/// [`htap_sql::expr::ExprType::is_dynamic`] (`@name`/`@@name`) is reported as a nullable string
/// by `htap_sql::binder_query::body_output_columns` regardless of the variable's actual value, so
/// a wire client that trusts the declared column type (as the MySQL text protocol requires: every
/// value is sent as text and decoded according to its column's declared type) would decode an
/// integer-valued system variable like `@@max_allowed_packet` as a string instead of an integer,
/// unlike before variables were resolved as real expressions.
///
/// For each `dynamic[i]` column, this instead reports the type inferred from `rows`' actual
/// values at that position: the shared [`DataType`] of every non-null value if they all agree,
/// `DataType::String` if they disagree (or there were no rows/values to look at at all), and
/// nullable if any row's value was `NULL` or there was no evidence either way. A `SELECT
/// @x`/`SELECT @@sysvar` variable expression evaluates to the same value on every row (it never
/// depends on row data), so in practice this is exactly that one value's type.
fn infer_dynamic_column_types(
    mut columns: Vec<ColumnDef>,
    dynamic: &[bool],
    rows: &[Row],
) -> Vec<ColumnDef> {
    for (i, col) in columns.iter_mut().enumerate() {
        if !dynamic.get(i).copied().unwrap_or(false) {
            continue;
        }
        let mut inferred: Option<DataType> = None;
        let mut consistent = true;
        let mut saw_null = false;
        for row in rows {
            let Some(value) = row.get(i) else { continue };
            match value.data_type() {
                None => saw_null = true,
                Some(dt) => match inferred {
                    None => inferred = Some(dt),
                    Some(existing) if existing == dt => {}
                    Some(_) => consistent = false,
                },
            }
        }
        col.data_type = match (consistent, inferred) {
            (true, Some(dt)) => dt,
            _ => DataType::String,
        };
        col.nullable = saw_null || inferred.is_none();
    }
    columns
}

/// A produced row together with its `ORDER BY` sort keys.
struct Keyed {
    output: Vec<Value>,
    keys: Vec<Value>,
}

/// Inputs to one binary join operation.
struct JoinRowsInput<'a> {
    left: Vec<Vec<Value>>,
    left_width: usize,
    right: Vec<Vec<Value>>,
    right_width: usize,
    left_slots: &'a [usize],
    right_slots: &'a [usize],
    join: &'a JoinSpec,
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
    memory_budget: &'a Arc<MemoryBudget>,
    spill_root: &'a Path,
    parallelism: usize,
    build_side: BuildSide,
}

/// Runs a query to completion, applying ordering and limits.
pub(crate) fn run_query(ctx: &ExecContext<'_>, query: &BoundQuery) -> Result<Vec<Row>> {
    let budget = SubqueryBudget::new(10_000, 20);
    run_query_with_budget(ctx, query, &budget)
}

fn run_query_with_budget(
    ctx: &ExecContext<'_>,
    query: &BoundQuery,
    budget: &SubqueryBudget,
) -> Result<Vec<Row>> {
    run_query_with_outer(ctx, query, None, budget)
}

struct PreparedCorrelatedQuery {
    query: BoundQuery,
    join_build_sides: Option<HashMap<usize, BuildSide>>,
}

struct CorrelatedRunner<'a> {
    ctx: &'a ExecContext<'a>,
    subqueries: &'a [BoundQuery],
    prepared: &'a [Option<PreparedCorrelatedQuery>],
    budget: &'a SubqueryBudget,
}

impl SubqueryRunner for CorrelatedRunner<'_> {
    fn run(&self, index: usize, outer_row: &[Value]) -> Result<Vec<Row>> {
        let query = self
            .subqueries
            .get(index)
            .ok_or_else(|| HtapError::Internal(format!("subquery {index} not available")))?;
        if let Some(prepared) = self.prepared.get(index).and_then(Option::as_ref) {
            run_query_with_outer_prepared(
                self.ctx,
                &prepared.query,
                Some(outer_row),
                self.budget,
                prepared.join_build_sides.as_ref(),
            )
        } else if query.correlated {
            run_query_with_outer(self.ctx, query, Some(outer_row), self.budget)
        } else {
            run_query_with_budget(self.ctx, query, self.budget)
        }
    }
}

fn run_query_with_outer(
    ctx: &ExecContext<'_>,
    query: &BoundQuery,
    current_outer_row: Option<&[Value]>,
    budget: &SubqueryBudget,
) -> Result<Vec<Row>> {
    let physical = (ctx.optimization_mode == OptimizationMode::Enabled).then(|| {
        record_optimizer_invocation();
        optimize(query, &CatalogStats(ctx.catalog))
    });
    let optimized_query = match physical.as_ref() {
        Some(physical) => match (&query.body, physical.select.as_ref()) {
            (QueryBody::Select(_), Some(select)) => {
                let mut optimized = query.clone();
                optimized.body = QueryBody::Select(select.clone());
                optimized
            }
            _ => query.clone(),
        },
        None => query.clone(),
    };
    run_query_with_outer_prepared(
        ctx,
        &optimized_query,
        current_outer_row,
        budget,
        physical
            .as_ref()
            .filter(|physical| !physical.fallback)
            .map(|physical| &physical.join_build_sides),
    )
}

fn run_query_with_outer_prepared(
    ctx: &ExecContext<'_>,
    query: &BoundQuery,
    current_outer_row: Option<&[Value]>,
    budget: &SubqueryBudget,
    join_build_sides: Option<&HashMap<usize, BuildSide>>,
) -> Result<Vec<Row>> {
    let subqueries: Vec<Vec<Row>> = query
        .subqueries
        .iter()
        .map(|q| {
            if q.correlated {
                Ok(Vec::new())
            } else {
                run_query_with_budget(ctx, q, budget)
            }
        })
        .collect::<Result<_>>()?;
    let prepared_subqueries = query
        .subqueries
        .iter()
        .map(|subquery| {
            if !subquery.correlated {
                return None;
            }
            let physical = (ctx.optimization_mode == OptimizationMode::Enabled).then(|| {
                record_optimizer_invocation();
                optimize(subquery, &CatalogStats(ctx.catalog))
            });
            let query = match physical.as_ref() {
                Some(physical) => match (&subquery.body, physical.select.as_ref()) {
                    (QueryBody::Select(_), Some(select)) => {
                        let mut optimized = subquery.clone();
                        optimized.body = QueryBody::Select(select.clone());
                        optimized
                    }
                    _ => subquery.clone(),
                },
                None => subquery.clone(),
            };
            Some(PreparedCorrelatedQuery {
                query,
                join_build_sides: physical
                    .filter(|physical| !physical.fallback)
                    .map(|physical| physical.join_build_sides),
            })
        })
        .collect::<Vec<_>>();
    let runner = CorrelatedRunner {
        ctx,
        subqueries: &query.subqueries,
        prepared: &prepared_subqueries,
        budget,
    };

    let mut keyed = match &query.body {
        QueryBody::Select(sel) => run_select(
            ctx,
            sel,
            RunSelectContext {
                bound_subqueries: &query.subqueries,
                order_by: &query.order_by,
                subqueries: &subqueries,
                current_outer_row,
                subquery_runner: Some(&runner),
                subquery_budget: Some(budget),
                join_build_sides,
            },
        )?,
        QueryBody::SetOp { kind, left, right } => {
            let cast_side = |side: &BoundQuery| -> Result<Vec<Vec<Value>>> {
                let mut rows = Vec::new();
                for row in run_query_with_outer(ctx, side, current_outer_row, budget)? {
                    let mut values = row.into_values();
                    for (i, target) in query.output_columns.iter().enumerate() {
                        if values[i].data_type().is_some_and(|d| d != target.data_type) {
                            values[i] = cast_value(values[i].clone(), target.data_type)?;
                        }
                    }
                    rows.push(values);
                }
                Ok(rows)
            };
            let left_rows = cast_side(left)?;
            let right_rows = cast_side(right)?;
            let rows = execute_set_operation(
                *kind,
                left_rows,
                right_rows,
                &ctx.memory_budget,
                ctx.spill_root,
            )?;
            let mut out = Vec::with_capacity(rows.len());
            for output in rows {
                let keys =
                    order_keys_from_output(&query.order_by, &output, &subqueries, ctx.variables)?;
                out.push(Keyed { output, keys });
            }
            out
        }
        QueryBody::RecursiveQueryBody {
            anchor,
            recursive_term,
            distinct,
            output_columns,
        } => {
            const MAX_RECURSIVE_ITERATIONS: usize = 1_000;
            const MAX_RECURSIVE_ROWS: usize = 1_000_000;
            const MAX_RECURSIVE_BYTES: usize = 256 * 1024 * 1024;

            fn cast_recursive_rows(rows: Vec<Row>, columns: &[ColumnDef]) -> Result<Vec<Row>> {
                rows.into_iter()
                    .map(|row| {
                        let mut values = row.into_values();
                        for (i, column) in columns.iter().enumerate() {
                            if values[i]
                                .data_type()
                                .is_some_and(|data_type| data_type != column.data_type)
                            {
                                values[i] = cast_value(values[i].clone(), column.data_type)?;
                            }
                        }
                        Ok(Row::new(values))
                    })
                    .collect()
            }

            fn estimated_rows_bytes(rows: &[Row], width: usize) -> usize {
                rows.iter()
                    .map(|row| {
                        let values = (0..width)
                            .filter_map(|i| row.get(i))
                            .map(|value| std::mem::size_of::<Value>() + value.to_string().len())
                            .sum::<usize>();
                        std::mem::size_of::<Row>() + values
                    })
                    .sum()
            }

            fn check_recursive_caps(row_count: usize, byte_count: usize) -> Result<()> {
                if row_count > MAX_RECURSIVE_ROWS {
                    return Err(HtapError::InvalidArgument(format!(
                        "recursive CTE accumulated-row-count cap of \
                         {MAX_RECURSIVE_ROWS} rows exceeded"
                    )));
                }
                if byte_count > MAX_RECURSIVE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "recursive CTE approximate accumulated-byte cap of \
                         {MAX_RECURSIVE_BYTES} bytes exceeded"
                    )));
                }
                Ok(())
            }

            let anchor_rows = cast_recursive_rows(
                run_query_with_outer(ctx, anchor, current_outer_row, budget)?,
                output_columns,
            )?;
            let mut seen = BTreeSet::new();
            let mut accumulated = Vec::new();
            let mut working = Vec::new();

            for row in anchor_rows {
                let values = row.into_values();
                if !*distinct || seen.insert(values.clone()) {
                    working.push(Row::new(values.clone()));
                    accumulated.push(Row::new(values));
                }
            }

            let mut accumulated_bytes = estimated_rows_bytes(&accumulated, output_columns.len());
            check_recursive_caps(accumulated.len(), accumulated_bytes)?;

            let mut iterations = 0;
            while !working.is_empty() {
                if iterations == MAX_RECURSIVE_ITERATIONS {
                    return Err(HtapError::InvalidArgument(format!(
                        "recursive CTE iteration cap of \
                         {MAX_RECURSIVE_ITERATIONS} iterations exceeded"
                    )));
                }
                iterations += 1;

                let iteration_ctx = ExecContext {
                    server: ctx.server,
                    catalog: ctx.catalog,
                    snapshot: ctx.snapshot,
                    write_set: ctx.write_set,
                    variables: ctx.variables,
                    working_rows: Some(&working),
                    working_width: output_columns.len(),
                    memory_budget: Arc::clone(&ctx.memory_budget),
                    spill_root: ctx.spill_root,
                    parallelism: ctx.parallelism,
                    optimization_mode: ctx.optimization_mode,
                };
                let candidates = cast_recursive_rows(
                    run_query_with_outer(
                        &iteration_ctx,
                        recursive_term,
                        current_outer_row,
                        budget,
                    )?,
                    output_columns,
                )?;

                let mut next = Vec::new();
                for row in candidates {
                    let values = row.into_values();
                    if !*distinct || seen.insert(values.clone()) {
                        next.push(Row::new(values));
                    }
                }
                if next.is_empty() {
                    break;
                }

                accumulated_bytes = accumulated_bytes
                    .saturating_add(estimated_rows_bytes(&next, output_columns.len()));
                let next_row_count = accumulated.len().saturating_add(next.len());
                check_recursive_caps(next_row_count, accumulated_bytes)?;

                accumulated.extend(next.iter().cloned());
                working = next;
            }

            let mut out = Vec::with_capacity(accumulated.len());
            for row in accumulated {
                let output = row.into_values();
                let keys =
                    order_keys_from_output(&query.order_by, &output, &subqueries, ctx.variables)?;
                out.push(Keyed { output, keys });
            }
            out
        }
    };

    if !query.order_by.is_empty() {
        sort_keyed(
            &mut keyed,
            &query.order_by,
            &ctx.memory_budget,
            ctx.spill_root,
        )?;
    }
    let offset = query.offset.unwrap_or(0) as usize;
    let rows = keyed
        .into_iter()
        .skip(offset)
        .take(query.limit.map(|l| l as usize).unwrap_or(usize::MAX))
        .map(|k| Row::new(k.output))
        .collect();
    Ok(rows)
}

fn order_keys_from_output(
    order_by: &[OrderItem],
    output: &[Value],
    subqueries: &[Vec<Row>],
    variables: Option<&dyn VariableLookup>,
) -> Result<Vec<Value>> {
    let ctx = htap_sql::eval_context! {
        row: &[],
        current_outer_row: None,
        aggregates: &[],
        output: Some(output),
        subqueries: subqueries,
        variables: variables,
        subquery_runner: None,
        subquery_budget: None,
    };
    order_by.iter().map(|o| o.expr.eval(&ctx)).collect()
}

fn memory_budget_exceeded(error: &HtapError) -> bool {
    matches!(
        error,
        HtapError::InvalidArgument(message)
            if message.starts_with("query memory budget exceeded")
    )
}

fn estimate_values_bytes(values: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>()
        + values
            .iter()
            .map(MemoryBudget::estimate_value_bytes)
            .sum::<usize>()
}

fn estimate_keyed_bytes(row: &Keyed) -> usize {
    std::mem::size_of::<Keyed>()
        .saturating_add(estimate_values_bytes(&row.output))
        .saturating_add(estimate_values_bytes(&row.keys))
}

fn keyed_cmp(left: &Keyed, right: &Keyed, order_by: &[OrderItem]) -> std::cmp::Ordering {
    for (i, item) in order_by.iter().enumerate() {
        let (left_key, right_key) = (&left.keys[i], &right.keys[i]);
        let ordering = match (left_key.is_null(), right_key.is_null()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => {
                if item.nulls_first {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            (false, true) => {
                if item.nulls_first {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            }
            (false, false) => {
                let ordering = compare(left_key, right_key)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| left_key.cmp(right_key));
                if item.asc {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    left.output.cmp(&right.output)
}

fn encode_keyed(row: &Keyed) -> Row {
    let mut values = Vec::with_capacity(row.output.len() + row.keys.len());
    values.extend(row.output.iter().cloned());
    values.extend(row.keys.iter().cloned());
    Row::new(values)
}

fn decode_keyed(row: Row, key_count: usize) -> Result<Keyed> {
    let mut values = row.into_values();
    let output_len = values
        .len()
        .checked_sub(key_count)
        .ok_or_else(|| HtapError::Corruption("sort spill row has too few values".into()))?;
    let keys = values.split_off(output_len);
    Ok(Keyed {
        output: values,
        keys,
    })
}

fn validate_spill_header(
    header: SpillHeader,
    statement_id: u64,
    kind: SpillKind,
    operator_kind: OperatorKind,
) -> Result<()> {
    if header.statement_id != statement_id
        || header.kind != kind
        || header.operator_kind != operator_kind
    {
        return Err(HtapError::Corruption(
            "spill header does not match its statement and operator".into(),
        ));
    }
    Ok(())
}

struct MergeSortRunsInput<'a> {
    spill_dir: &'a mut SpillDir,
    statement_id: u64,
    input_paths: &'a [std::path::PathBuf],
    output_path: &'a std::path::Path,
    output_side: &'a str,
    output_index: usize,
    key_count: usize,
    order_by: &'a [OrderItem],
    memory_budget: &'a Arc<MemoryBudget>,
}

fn merge_sort_runs(input: MergeSortRunsInput<'_>) -> Result<std::path::PathBuf> {
    let MergeSortRunsInput {
        spill_dir,
        statement_id,
        input_paths,
        output_path,
        output_side,
        output_index,
        key_count,
        order_by,
        memory_budget,
    } = input;
    let mut readers = Vec::with_capacity(input_paths.len());
    let mut heads = Vec::with_capacity(input_paths.len());
    let mut head_reservations = Vec::with_capacity(input_paths.len());
    for path in input_paths {
        let mut reader = SpillReader::open(path)?;
        validate_spill_header(
            reader.read_header()?,
            statement_id,
            SpillKind::Sort,
            OperatorKind::Sort,
        )?;
        let head = reader
            .read_row()?
            .map(|row| decode_keyed(row, key_count))
            .transpose()?;
        let reservation = head
            .as_ref()
            .map(|row| memory_budget.try_reserve(estimate_keyed_bytes(row)))
            .transpose()?;
        heads.push(head);
        head_reservations.push(reservation);
        readers.push(reader);
    }

    let path = spill_dir.partition_file_path(OperatorKind::Sort, output_side, output_index);
    debug_assert_eq!(path, output_path);
    let mut writer = SpillWriter::create(
        &path,
        SpillHeader::new(SpillKind::Sort, statement_id, OperatorKind::Sort),
    )?;

    loop {
        let next = heads
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.as_ref().map(|row| (index, row)))
            .min_by(|(left_index, left), (right_index, right)| {
                keyed_cmp(left, right, order_by).then_with(|| left_index.cmp(right_index))
            })
            .map(|(index, _)| index);
        let Some(next) = next else {
            break;
        };

        let row = heads[next]
            .take()
            .ok_or_else(|| HtapError::Internal("sort merge head disappeared".into()))?;
        head_reservations[next] = None;
        writer.append_row(&encode_keyed(&row))?;
        let head = readers[next]
            .read_row()?
            .map(|row| decode_keyed(row, key_count))
            .transpose()?;
        head_reservations[next] = head
            .as_ref()
            .map(|row| memory_budget.try_reserve(estimate_keyed_bytes(row)))
            .transpose()?;
        heads[next] = head;
    }

    writer.finish()?;
    Ok(path)
}

fn sort_keyed(
    rows: &mut Vec<Keyed>,
    order_by: &[OrderItem],
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
) -> Result<()> {
    let estimated_bytes = rows
        .iter()
        .map(estimate_keyed_bytes)
        .fold(0usize, usize::saturating_add);

    if let Ok(_reservation) = memory_budget.try_reserve(estimated_bytes) {
        rows.sort_by(|left, right| keyed_cmp(left, right, order_by));
        return Ok(());
    }

    let statement_id = NEXT_SPILL_STATEMENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut spill_dir = SpillDir::create(spill_root, statement_id)?;
    record_spill(OperatorKind::Sort);
    let run_limit = memory_budget
        .remaining()
        .checked_div(SORT_MERGE_FAN_IN)
        .unwrap_or(0)
        .max(1);

    let mut input = std::mem::take(rows).into_iter();
    let mut run_paths = Vec::new();
    let mut pending = input.next();

    while pending.is_some() {
        let mut run = Vec::new();
        let mut run_bytes = 0usize;
        while let Some(row) = pending.take() {
            let row_bytes = estimate_keyed_bytes(&row);
            if row_bytes > run_limit {
                return Err(HtapError::InvalidArgument(format!(
                    "query memory budget exceeded: one sort row requires {row_bytes} bytes"
                )));
            }
            if !run.is_empty() && run_bytes.saturating_add(row_bytes) > run_limit {
                pending = Some(row);
                break;
            }
            run_bytes = run_bytes.saturating_add(row_bytes);
            run.push(row);
            pending = input.next();
        }

        let _reservation = memory_budget.try_reserve(run_bytes)?;
        run.sort_by(|left, right| keyed_cmp(left, right, order_by));
        let run_index = run_paths.len();
        let path = spill_dir.partition_file_path(OperatorKind::Sort, "run-0", run_index);
        let mut writer = SpillWriter::create(
            &path,
            SpillHeader::new(SpillKind::Sort, statement_id, OperatorKind::Sort),
        )?;
        for row in &run {
            writer.append_row(&encode_keyed(row))?;
        }
        writer.finish()?;
        run_paths.push(path);
    }

    let mut pass = 1usize;
    while run_paths.len() > 1 {
        let mut next_paths = Vec::new();
        for (batch_index, batch) in run_paths.chunks(SORT_MERGE_FAN_IN).enumerate() {
            let side = format!("run-{pass}");
            let output_path = spill_dir.partition_file_path(OperatorKind::Sort, &side, batch_index);
            next_paths.push(merge_sort_runs(MergeSortRunsInput {
                spill_dir: &mut spill_dir,
                statement_id,
                input_paths: batch,
                output_path: &output_path,
                output_side: &side,
                output_index: batch_index,
                key_count: order_by.len(),
                order_by,
                memory_budget,
            })?);
        }
        run_paths = next_paths;
        pass += 1;
    }

    if let Some(path) = run_paths.pop() {
        let mut reader = SpillReader::open(&path)?;
        validate_spill_header(
            reader.read_header()?,
            statement_id,
            SpillKind::Sort,
            OperatorKind::Sort,
        )?;
        while let Some(row) = reader.read_row()? {
            rows.push(decode_keyed(row, order_by.len())?);
        }
    }
    Ok(())
}

fn dedup_keyed(
    rows: Vec<Keyed>,
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
) -> Result<Vec<Keyed>> {
    let estimated_bytes = rows
        .iter()
        .map(|row| estimate_values_bytes(&row.output))
        .fold(0usize, usize::saturating_add);
    match memory_budget.try_reserve(estimated_bytes) {
        Ok(_reservation) => {
            let mut seen = BTreeSet::new();
            return Ok(rows
                .into_iter()
                .filter(|row| seen.insert(row.output.clone()))
                .collect());
        }
        Err(error) if memory_budget_exceeded(&error) => {}
        Err(error) => return Err(error),
    }

    record_spill(OperatorKind::Distinct);
    let outputs = rows.iter().map(|row| row.output.clone()).collect();
    let unique = execute_set_operation(
        SetOpKind::UnionDistinct,
        outputs,
        Vec::new(),
        memory_budget,
        spill_root,
    )?;
    let unique = unique.into_iter().collect::<BTreeSet<_>>();
    let mut emitted = BTreeSet::new();
    Ok(rows
        .into_iter()
        .filter(|row| unique.contains(&row.output) && emitted.insert(row.output.clone()))
        .collect())
}

fn execute_set_operation(
    kind: SetOpKind,
    left_rows: Vec<Vec<Value>>,
    right_rows: Vec<Vec<Value>>,
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
) -> Result<Vec<Vec<Value>>> {
    if kind == SetOpKind::UnionAll {
        let mut rows = left_rows;
        rows.extend(right_rows);
        return Ok(rows);
    }

    let estimated_bytes = left_rows
        .iter()
        .chain(&right_rows)
        .map(|row| estimate_values_bytes(row))
        .fold(0usize, usize::saturating_add);
    match memory_budget.try_reserve(estimated_bytes) {
        Ok(_reservation) => return Ok(set_operation_in_memory(kind, left_rows, right_rows)),
        Err(error) if memory_budget_exceeded(&error) => {}
        Err(error) => return Err(error),
    }

    spill_set_operation(kind, &left_rows, &right_rows, memory_budget, spill_root)
}

fn set_operation_in_memory(
    kind: SetOpKind,
    left_rows: Vec<Vec<Value>>,
    right_rows: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    if kind == SetOpKind::UnionDistinct {
        let mut seen = BTreeSet::new();
        return left_rows
            .into_iter()
            .chain(right_rows)
            .filter(|row| seen.insert(row.clone()))
            .collect();
    }

    let mut left_counts = BTreeMap::new();
    let mut left_order = Vec::new();
    for row in left_rows {
        let count = left_counts.entry(row.clone()).or_insert(0usize);
        if *count == 0 {
            left_order.push(row);
        }
        *count += 1;
    }

    let mut right_counts = BTreeMap::new();
    for row in right_rows {
        *right_counts.entry(row).or_insert(0usize) += 1;
    }

    let mut output = Vec::new();
    for row in left_order {
        let left_count = left_counts[&row];
        let right_count = right_counts.get(&row).copied().unwrap_or(0);
        let copies = match kind {
            SetOpKind::ExceptDistinct => usize::from(right_count == 0),
            SetOpKind::ExceptAll => left_count.saturating_sub(right_count),
            SetOpKind::IntersectDistinct => usize::from(right_count > 0),
            SetOpKind::IntersectAll => left_count.min(right_count),
            SetOpKind::UnionAll | SetOpKind::UnionDistinct => unreachable!(),
        };
        output.extend(std::iter::repeat_n(row, copies));
    }
    output
}

fn spill_set_operation(
    kind: SetOpKind,
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
) -> Result<Vec<Vec<Value>>> {
    let statement_id = NEXT_SPILL_STATEMENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut spill_dir = SpillDir::create(spill_root, statement_id)?;
    record_spill(OperatorKind::SetOperation);
    let mut left_paths = Vec::with_capacity(SET_OPERATION_SPILL_PARTITIONS);
    let mut right_paths = Vec::with_capacity(SET_OPERATION_SPILL_PARTITIONS);
    let mut left_writers = Vec::with_capacity(SET_OPERATION_SPILL_PARTITIONS);
    let mut right_writers = Vec::with_capacity(SET_OPERATION_SPILL_PARTITIONS);

    // Spill files retain append order, so these source indexes let decoded rows retain their
    // original statement-order position even when spill serialization changes their values.
    let mut left_source_indexes = vec![Vec::new(); SET_OPERATION_SPILL_PARTITIONS];
    let mut right_source_indexes = vec![Vec::new(); SET_OPERATION_SPILL_PARTITIONS];

    for partition in 0..SET_OPERATION_SPILL_PARTITIONS {
        let left_path =
            spill_dir.partition_file_path(OperatorKind::SetOperation, "left", partition);
        let right_path =
            spill_dir.partition_file_path(OperatorKind::SetOperation, "right", partition);
        left_writers.push(SpillWriter::create(
            &left_path,
            SpillHeader::new(
                SpillKind::SetOperation,
                statement_id,
                OperatorKind::SetOperation,
            ),
        )?);
        right_writers.push(SpillWriter::create(
            &right_path,
            SpillHeader::new(
                SpillKind::SetOperation,
                statement_id,
                OperatorKind::SetOperation,
            ),
        )?);
        left_paths.push(left_path);
        right_paths.push(right_path);
    }

    for (source_index, row) in left_rows.iter().enumerate() {
        let partition = hash_join_partition_for(row, SET_OPERATION_SPILL_PARTITIONS);
        left_writers[partition].append_row(&Row::new(row.clone()))?;
        left_source_indexes[partition].push(source_index);
    }
    for (right_index, row) in right_rows.iter().enumerate() {
        let partition = hash_join_partition_for(row, SET_OPERATION_SPILL_PARTITIONS);
        right_writers[partition].append_row(&Row::new(row.clone()))?;
        right_source_indexes[partition].push(left_rows.len().saturating_add(right_index));
    }
    for writer in left_writers {
        writer.finish()?;
    }
    for writer in right_writers {
        writer.finish()?;
    }

    let read_rows =
        |path: &std::path::Path, source_indexes: &[usize]| -> Result<Vec<(Vec<Value>, usize)>> {
            let mut reader = SpillReader::open(path)?;
            validate_spill_header(
                reader.read_header()?,
                statement_id,
                SpillKind::SetOperation,
                OperatorKind::SetOperation,
            )?;

            let mut rows = Vec::with_capacity(source_indexes.len());
            for &source_index in source_indexes {
                let row = reader.read_row()?.ok_or_else(|| {
                    HtapError::Corruption(
                        "set-operation spill file has fewer rows than its source index list".into(),
                    )
                })?;
                rows.push((row.into_values(), source_index));
            }
            if reader.read_row()?.is_some() {
                return Err(HtapError::Corruption(
                    "set-operation spill file has more rows than its source index list".into(),
                ));
            }
            Ok(rows)
        };

    // Store each decoded result and its multiplicity. Rows must be ordered by the earliest
    // source position associated with their decoded value, rather than by an equality comparison
    // against the pre-spill row representation.
    let mut partition_outputs = BTreeMap::<Vec<Value>, usize>::new();
    let mut first_source_order = BTreeMap::<Vec<Value>, usize>::new();

    for partition in 0..SET_OPERATION_SPILL_PARTITIONS {
        let left = read_rows(&left_paths[partition], &left_source_indexes[partition])?;
        let right = read_rows(&right_paths[partition], &right_source_indexes[partition])?;

        let partition_bytes = left
            .iter()
            .chain(&right)
            .map(|(row, _)| estimate_values_bytes(row))
            .fold(0usize, usize::saturating_add);
        let _reservation = memory_budget.try_reserve(partition_bytes)?;

        // Associate every decoded value with its first applicable source position before
        // evaluating the operation. UNION DISTINCT may emit a value from either side; all
        // remaining set operations emit only values that originated on the left.
        let mut decoded_source_order = BTreeMap::<Vec<Value>, usize>::new();
        for (row, source_index) in &left {
            decoded_source_order
                .entry(row.clone())
                .and_modify(|first| *first = (*first).min(*source_index))
                .or_insert(*source_index);
        }
        if kind == SetOpKind::UnionDistinct {
            for (row, source_index) in &right {
                decoded_source_order
                    .entry(row.clone())
                    .and_modify(|first| *first = (*first).min(*source_index))
                    .or_insert(*source_index);
            }
        }

        let left_values = left.iter().map(|(row, _)| row.clone()).collect();
        let right_values = right.iter().map(|(row, _)| row.clone()).collect();
        for row in set_operation_in_memory(kind, left_values, right_values) {
            let source_index = decoded_source_order.get(&row).copied().ok_or_else(|| {
                HtapError::Corruption(
                    "set-operation output has no decoded source-order position".into(),
                )
            })?;
            *partition_outputs.entry(row.clone()).or_insert(0) += 1;
            first_source_order
                .entry(row)
                .and_modify(|first| *first = (*first).min(source_index))
                .or_insert(source_index);
        }
    }

    let mut output = partition_outputs
        .into_iter()
        .map(|(row, copies)| {
            let source_index = first_source_order.get(&row).copied().ok_or_else(|| {
                HtapError::Internal(
                    "set-operation result is missing its recorded source-order position".into(),
                )
            })?;
            Ok((source_index, row, copies))
        })
        .collect::<Result<Vec<_>>>()?;
    output.sort_by_key(|(source_index, _, _)| *source_index);

    let mut rows = Vec::new();
    for (_, row, copies) in output {
        rows.extend(std::iter::repeat_n(row, copies));
    }
    Ok(rows)
}

/// Shared inputs used while executing one select block.
struct RunSelectContext<'a> {
    bound_subqueries: &'a [BoundQuery],
    order_by: &'a [OrderItem],
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
    join_build_sides: Option<&'a HashMap<usize, BuildSide>>,
}

/// Executes one select block, returning projected rows with their sort keys.
fn run_select(
    ctx: &ExecContext<'_>,
    sel: &SelectBody,
    input: RunSelectContext<'_>,
) -> Result<Vec<Keyed>> {
    let RunSelectContext {
        bound_subqueries,
        order_by,
        subqueries,
        current_outer_row,
        subquery_runner,
        subquery_budget,
        join_build_sides,
    } = input;
    // 1. Which columns each slot must provide.
    let mut needed: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); sel.slots.len()];
    let mut note = |e: &Expr| {
        for (slot, col) in e.referenced_columns() {
            needed[slot].insert(col);
        }
    };
    fn note_join_tree_on(tree: &JoinTree, note: &mut impl FnMut(&Expr)) {
        match tree {
            JoinTree::Leaf(_) => {}
            JoinTree::Join {
                left, right, on, ..
            } => {
                note_join_tree_on(left, note);
                note_join_tree_on(right, note);
                if let Some(on) = on {
                    note(on);
                }
            }
        }
    }

    if !sel.slots.is_empty() {
        note_join_tree_on(&sel.join_tree, &mut note);
    }
    if let Some(f) = &sel.filter {
        note(f);
    }
    for g in &sel.group_by {
        note(g);
    }
    for a in &sel.aggregates {
        if let Some(arg) = &a.arg {
            note(arg);
        }
    }
    if let Some(h) = &sel.having {
        note(h);
    }
    for p in &sel.projection {
        note(&p.expr);
    }
    for o in order_by {
        note(&o.expr);
    }
    for window in &sel.windows {
        for expr in &window.partition_by {
            note(expr);
        }
        for item in &window.order_by {
            note(&item.expr);
        }
        for arg in &window.args {
            note(arg);
        }
        if let WindowFrame::ValueRange { start, end } = &window.frame {
            for bound in [start, end] {
                if let ValueFrameBound::Offset { value, .. } = bound {
                    note(value);
                }
            }
        }
    }

    // Correlated references are stored as offsets into this select's flattened input row.
    // Include their physical source columns even when no expression in this select uses them.
    for subquery in bound_subqueries.iter().filter(|query| query.correlated) {
        for outer_ref in &subquery.correlated_outer_refs {
            let Expr::CorrelatedColumnRef { offset, .. } = outer_ref else {
                continue;
            };
            let mut base = 0;
            let mut found = false;
            for (slot_index, slot) in sel.slots.iter().enumerate() {
                let width = slot.width();
                if *offset >= base && *offset < base + width {
                    needed[slot_index].insert(*offset - base);
                    found = true;
                    break;
                }
                base += width;
            }
            if !found {
                return Err(HtapError::Internal(format!(
                    "correlated column offset {offset} is outside outer row width {base}"
                )));
            }
        }
    }

    // 2. Materialize slots. WHERE conjuncts that touch exactly one slot are offered to
    //    that slot's scan for pruning/pushdown, except for slots on the null-supplying side
    //    of an outer join: their WHERE predicates apply after null padding (for example
    //    `LEFT JOIN c ... WHERE c.id IS NULL`), so they must stay residual.
    let mut null_supplying = vec![false; sel.slots.len()];
    if !sel.slots.is_empty() {
        mark_tree_null_supplying(&sel.join_tree, false, &mut null_supplying);
    }
    let where_conjuncts: Vec<&Expr> = sel.filter.as_ref().map(conjuncts).unwrap_or_default();
    let mut slot_rows: Vec<Vec<Vec<Value>>> = Vec::with_capacity(sel.slots.len());
    for (i, slot) in sel.slots.iter().enumerate() {
        let rows = match slot {
            TableSlot::Base { table, .. } => {
                let single_slot_conjuncts: Vec<&Expr> = if null_supplying[i] {
                    Vec::new()
                } else {
                    where_conjuncts
                        .iter()
                        .copied()
                        .filter(|c| c.referenced_slots() == [i])
                        .collect()
                };
                scan_base_table(ctx, table, &needed[i], i, &single_slot_conjuncts)?
            }
            TableSlot::Derived { query, .. } => {
                let rows = match subquery_budget {
                    Some(budget) => run_query_with_outer(ctx, query, current_outer_row, budget)?,
                    None => run_query(ctx, query)?,
                };
                rows.into_iter().map(Row::into_values).collect()
            }
            TableSlot::WorkingTableSlot { columns, .. } => {
                let working_rows = ctx.working_rows.ok_or_else(|| {
                    HtapError::Internal(
                        "recursive working table used outside recursive CTE iteration".into(),
                    )
                })?;
                if ctx.working_width != columns.len() {
                    return Err(HtapError::Internal(format!(
                        "recursive working table width mismatch: context has {}, slot expects {}",
                        ctx.working_width,
                        columns.len()
                    )));
                }

                working_rows
                    .iter()
                    .map(|row| {
                        if row.values().len() != ctx.working_width {
                            return Err(HtapError::Internal(format!(
                                "recursive working row has {} values, expected {}",
                                row.values().len(),
                                ctx.working_width
                            )));
                        }
                        Ok(row.values().to_vec())
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        slot_rows.push(rows);
    }

    // 3. Joins.
    let mut current = if sel.slots.is_empty() {
        vec![Vec::new()]
    } else {
        let mut join_optimization = JoinOptimizationContext {
            build_sides: join_build_sides,
            next_join_id: 0,
        };
        let rows = evaluate_join_tree(
            sel,
            &sel.join_tree,
            &mut slot_rows,
            EvalInputs {
                subqueries,
                current_outer_row,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
                spill_root: ctx.spill_root,
            },
            &ctx.memory_budget,
            ctx.parallelism,
            &mut join_optimization,
        )?;
        let mut physical_slots = Vec::new();
        tree_slots(&sel.join_tree, &mut physical_slots);
        rows.into_iter()
            .map(|row| row_to_logical_slot_order(row, &physical_slots, &sel.slots))
            .collect::<Result<Vec<_>>>()?
    };

    // 4. WHERE.
    if let Some(filter) = &sel.filter {
        let mut kept = Vec::with_capacity(current.len());
        for row in current {
            let c = htap_sql::eval_context! {
                row: &row,
                current_outer_row: current_outer_row,
                aggregates: &[],
                output: None,
                subqueries: subqueries,
                variables: ctx.variables,
                subquery_runner: subquery_runner,
                subquery_budget: subquery_budget,
            };
            if filter.eval_predicate(&c)? {
                kept.push(row);
            }
        }
        current = kept;
    }

    // 5. Aggregate and retain each input row with its aggregate values.
    let mut window_inputs: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    if sel.is_aggregate() {
        window_inputs = aggregate_rows(
            sel,
            current,
            EvalInputs {
                subqueries,
                current_outer_row,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
                spill_root: ctx.spill_root,
            },
            &ctx.memory_budget,
            ctx.spill_root,
            ctx.parallelism,
        )?;
    } else {
        window_inputs.extend(current.into_iter().map(|row| (row, Vec::new())));
    }

    // 6-7. Compute only projection aliases needed by HAVING. Window-bearing projection
    // items cannot be evaluated until after HAVING has filtered the grouped rows.
    if let Some(having) = &sel.having {
        let mut needed_outputs = BTreeSet::new();
        having.walk(&mut |expr| {
            if let Expr::OutputColumn { index, .. } = expr {
                needed_outputs.insert(*index);
            }
        });

        let mut kept = Vec::with_capacity(window_inputs.len());
        for (row, aggregates) in window_inputs {
            let projection_ctx = htap_sql::eval_context! {
                row: &row,
                current_outer_row: current_outer_row,
                aggregates: &aggregates,
                output: None,
                subqueries: subqueries,
                variables: ctx.variables,
                subquery_runner: subquery_runner,
                subquery_budget: subquery_budget,
            };
            let mut output = vec![Value::Null; sel.projection.len()];
            for index in needed_outputs.iter().copied() {
                let projection = sel.projection.get(index).ok_or_else(|| {
                    HtapError::Internal(format!("HAVING output column {index} not available"))
                })?;
                if !projection.expr.contains_window() {
                    output[index] = projection.expr.eval(&projection_ctx)?;
                }
            }

            let having_ctx = htap_sql::eval_context! {
                row: &row,
                current_outer_row: current_outer_row,
                aggregates: &aggregates,
                output: Some(&output),
                subqueries: subqueries,
                variables: ctx.variables,
                subquery_runner: subquery_runner,
                subquery_budget: subquery_budget,
            };
            if having.eval_predicate(&having_ctx)? {
                kept.push((row, aggregates));
            }
        }
        window_inputs = kept;
    }

    // 8. Windows are evaluated only over rows that survived HAVING.
    let window_values = evaluate_windows(
        sel,
        &window_inputs,
        EvaluateWindowsContext {
            subqueries,
            current_outer_row,
            variables: ctx.variables,
            subquery_runner,
            subquery_budget,
            memory_budget: &ctx.memory_budget,
        },
        ctx.spill_root,
    )?;

    // 9. Re-evaluate the final projection and compute top-level ORDER BY keys.
    let mut produced = Vec::with_capacity(window_inputs.len());
    for ((row, aggregates), windows) in window_inputs.into_iter().zip(window_values.iter()) {
        produced.push(project_row(
            sel,
            order_by,
            &row,
            &aggregates,
            windows,
            EvalInputs {
                subqueries,
                current_outer_row,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
                spill_root: ctx.spill_root,
            },
        )?);
    }

    // 10. DISTINCT.
    if sel.distinct {
        produced = dedup_keyed(produced, &ctx.memory_budget, ctx.spill_root)?;
    }
    Ok(produced)
}

/// Shared expression-evaluation inputs used by window functions.
struct EvaluateWindowsContext<'a> {
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
    memory_budget: &'a Arc<MemoryBudget>,
}

fn evaluate_windows(
    sel: &SelectBody,
    inputs: &[(Vec<Value>, Vec<Value>)],
    context: EvaluateWindowsContext<'_>,
    spill_root: &Path,
) -> Result<Vec<Vec<Value>>> {
    evaluate_windows_inner(sel, inputs, context, spill_root, true)
}

fn estimate_window_evaluation_bytes(
    inputs: &[(Vec<Value>, Vec<Value>)],
    windows: &[htap_sql::query::WindowSpec],
) -> usize {
    let input_count = inputs.len();
    let total_row_values = inputs.iter().map(|(row, _)| row.len()).sum::<usize>();
    let average_row_value_count = if input_count == 0 {
        0.0
    } else {
        total_row_values as f64 / input_count as f64
    };
    let first_row_bytes = inputs
        .first()
        .map(|(row, _)| estimate_values_bytes(row))
        .unwrap_or(0);
    let window_overhead_per_row = windows
        .iter()
        .map(|window| {
            std::mem::size_of::<Vec<Value>>().saturating_add(
                window
                    .partition_by
                    .len()
                    .saturating_add(window.order_by.len())
                    .saturating_mul(std::mem::size_of::<Value>()),
            )
        })
        .sum::<usize>();
    let result_matrix_overhead = input_count
        .saturating_mul(windows.len())
        .saturating_mul(std::mem::size_of::<Value>());
    let estimated_bytes = inputs
        .iter()
        .map(|(row, aggregates)| {
            estimate_values_bytes(row)
                .saturating_add(estimate_values_bytes(aggregates))
                .saturating_add(window_overhead_per_row)
        })
        .fold(0usize, usize::saturating_add)
        .saturating_add(result_matrix_overhead);

    let _ = (
        average_row_value_count,
        first_row_bytes,
        window_overhead_per_row,
        result_matrix_overhead,
    );
    estimated_bytes
}

fn evaluate_windows_inner(
    sel: &SelectBody,
    inputs: &[(Vec<Value>, Vec<Value>)],
    context: EvaluateWindowsContext<'_>,
    spill_root: &Path,
    allow_spill: bool,
) -> Result<Vec<Vec<Value>>> {
    let EvaluateWindowsContext {
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
        memory_budget,
    } = context;

    let estimated_bytes = estimate_window_evaluation_bytes(inputs, &sel.windows);

    let _reservation = match memory_budget.try_reserve(estimated_bytes) {
        Ok(reservation) => Some(reservation),
        Err(error) if allow_spill && memory_budget_exceeded(&error) => {
            eprintln!(
                "window evaluation reservation exceeded: inputs={}, windows={}, bytes={estimated_bytes}",
                inputs.len(),
                sel.windows.len(),
            );
            return spill_window_partitions(
                sel,
                inputs,
                EvaluateWindowsContext {
                    subqueries,
                    current_outer_row,
                    variables,
                    subquery_runner,
                    subquery_budget,
                    memory_budget,
                },
                spill_root,
            );
        }
        Err(error) if !allow_spill && memory_budget_exceeded(&error) => return Err(error),
        Err(error) => return Err(error),
    };
    let window_context = WindowFunctionContext {
        inputs,
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
    };
    let mut results = vec![vec![Value::Null; sel.windows.len()]; inputs.len()];
    for (window_index, window) in sel.windows.iter().enumerate() {
        let mut partitions: BTreeMap<Vec<Value>, Vec<usize>> = BTreeMap::new();
        let mut order_keys = vec![Vec::new(); inputs.len()];

        for (input_index, (row, aggregates)) in inputs.iter().enumerate() {
            let eval = htap_sql::eval_context! {
                row: row,
                current_outer_row: current_outer_row,
                aggregates: aggregates,
                output: None,
                subqueries: subqueries,
                variables: variables,
                subquery_runner: subquery_runner,
                subquery_budget: subquery_budget,
            };
            let partition_key = window
                .partition_by
                .iter()
                .map(|expr| expr.eval(&eval))
                .collect::<Result<Vec<_>>>()?;
            order_keys[input_index] = window
                .order_by
                .iter()
                .map(|item| item.expr.eval(&eval))
                .collect::<Result<Vec<_>>>()?;
            partitions
                .entry(partition_key)
                .or_default()
                .push(input_index);
        }

        for partition in partitions.values_mut() {
            if !window.order_by.is_empty() {
                partition.sort_by(|left, right| {
                    compare_order_keys(&order_keys[*left], &order_keys[*right], &window.order_by)
                        .then_with(|| left.cmp(right))
                });
            }

            let mut rank = 1usize;
            let mut dense_rank = 1usize;
            for position in 0..partition.len() {
                if position > 0
                    && !order_keys_equal(
                        &order_keys[partition[position - 1]],
                        &order_keys[partition[position]],
                        &window.order_by,
                    )
                {
                    rank = position + 1;
                    dense_rank += 1;
                }

                let input_index = partition[position];
                let value = match window.func {
                    WindowFunctionKind::RowNumber => Value::Int64((position + 1) as i64),
                    WindowFunctionKind::Rank => Value::Int64(rank as i64),
                    WindowFunctionKind::DenseRank => Value::Int64(dense_rank as i64),
                    WindowFunctionKind::Ntile => {
                        let buckets = window_arg_u64(window, 0, input_index, &window_context)?;
                        if buckets == 0 {
                            return Err(HtapError::InvalidArgument(
                                "NTILE bucket count must be positive".into(),
                            ));
                        }
                        let row_count = partition.len() as u64;
                        let position = position as u64;
                        let larger = row_count % buckets;
                        let larger_size = row_count / buckets + 1;
                        let bucket = if position < larger * larger_size {
                            position / larger_size + 1
                        } else {
                            larger + (position - larger * larger_size) / (row_count / buckets) + 1
                        };
                        Value::Int64(bucket as i64)
                    }
                    WindowFunctionKind::Lag | WindowFunctionKind::Lead => {
                        let offset = if window.args.len() >= 2 {
                            window_arg_u64(window, 1, input_index, &window_context)?
                        } else {
                            1
                        };
                        let target = match window.func {
                            WindowFunctionKind::Lag => position.checked_sub(offset as usize),
                            WindowFunctionKind::Lead => position.checked_add(offset as usize),
                            _ => unreachable!(),
                        }
                        .filter(|target| *target < partition.len());

                        match target {
                            Some(target) => {
                                let target_index = partition[target];
                                eval_window_arg(window, 0, target_index, &window_context)?
                            }
                            None if window.args.len() >= 3 => {
                                eval_window_arg(window, 2, input_index, &window_context)?
                            }
                            None => Value::Null,
                        }
                    }
                    WindowFunctionKind::FirstValue | WindowFunctionKind::LastValue => {
                        let frame = window_frame_range(
                            window,
                            partition,
                            position,
                            &order_keys,
                            input_index,
                            &window_context,
                        )?;
                        match frame {
                            Some((start, end)) => {
                                let frame_position =
                                    if window.func == WindowFunctionKind::FirstValue {
                                        start
                                    } else {
                                        end
                                    };
                                eval_window_arg(
                                    window,
                                    0,
                                    partition[frame_position],
                                    &window_context,
                                )?
                            }
                            None => Value::Null,
                        }
                    }
                    WindowFunctionKind::CountStar
                    | WindowFunctionKind::Count
                    | WindowFunctionKind::Sum
                    | WindowFunctionKind::Avg
                    | WindowFunctionKind::Min
                    | WindowFunctionKind::Max => {
                        let frame = window_frame_range(
                            window,
                            partition,
                            position,
                            &order_keys,
                            input_index,
                            &window_context,
                        )?;
                        let aggregate_func = match window.func {
                            WindowFunctionKind::CountStar => AggFn::CountStar,
                            WindowFunctionKind::Count => AggFn::Count,
                            WindowFunctionKind::Sum => AggFn::Sum,
                            WindowFunctionKind::Avg => AggFn::Avg,
                            WindowFunctionKind::Min => AggFn::Min,
                            WindowFunctionKind::Max => AggFn::Max,
                            _ => unreachable!(),
                        };
                        let spec = AggregateSpec {
                            func: aggregate_func,
                            distinct: false,
                            arg: window.args.first().cloned(),
                            data_type: window.data_type,
                            nullable: window.nullable,
                            name: "window aggregate".into(),
                        };
                        let mut state = AggState::new(&spec);
                        if let Some((start, end)) = frame {
                            for &frame_input_index in partition[start..=end].iter() {
                                let value = if aggregate_func == AggFn::CountStar {
                                    Value::Int64(1)
                                } else {
                                    eval_window_arg(window, 0, frame_input_index, &window_context)?
                                };
                                state.accumulate(&spec, value)?;
                            }
                        }
                        state.finish(&spec)?
                    }
                };
                results[input_index][window_index] = value;
            }
        }
    }
    Ok(results)
}

fn spill_window_partitions(
    sel: &SelectBody,
    inputs: &[(Vec<Value>, Vec<Value>)],
    context: EvaluateWindowsContext<'_>,
    spill_root: &Path,
) -> Result<Vec<Vec<Value>>> {
    let statement_id = NEXT_SPILL_STATEMENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut spill_dir = SpillDir::create(spill_root, statement_id)?;
    record_spill(OperatorKind::Window);

    let estimated_input_bytes = estimate_window_evaluation_bytes(inputs, &sel.windows);
    // Reserve 40% of the remaining statement budget for a hash bucket, leaving a 60% margin
    // for grouping, per-window evaluation, and result materialization. This mirrors hash join's
    // bounded partition sizing and caps descriptor use at 128 files per window.
    let available_bytes = context.memory_budget.remaining();
    let bytes_per_partition = available_bytes
        .saturating_mul(40)
        .checked_div(100)
        .unwrap_or(0)
        .max(1);
    let partition_count = (estimated_input_bytes / bytes_per_partition)
        .saturating_add(usize::from(
            !estimated_input_bytes.is_multiple_of(bytes_per_partition),
        ))
        .clamp(1, HASH_JOIN_MAX_SPILL_PARTITIONS);

    let mut results = vec![vec![Value::Null; sel.windows.len()]; inputs.len()];
    for (window_index, window) in sel.windows.iter().enumerate() {
        let mut paths = Vec::with_capacity(partition_count);
        let mut writers = Vec::with_capacity(partition_count);
        for partition in 0..partition_count {
            let path = spill_dir.partition_file_path(
                OperatorKind::Window,
                &format!("window-{window_index}"),
                partition,
            );
            writers.push(SpillWriter::create(
                &path,
                SpillHeader::new(SpillKind::Window, statement_id, OperatorKind::Window),
            )?);
            paths.push(path);
        }

        for (input_index, (row, aggregates)) in inputs.iter().enumerate() {
            let eval = htap_sql::eval_context! {
                row: row,
                current_outer_row: context.current_outer_row,
                aggregates: aggregates,
                output: None,
                subqueries: context.subqueries,
                variables: context.variables,
                subquery_runner: context.subquery_runner,
                subquery_budget: context.subquery_budget,
            };
            let partition_key = window
                .partition_by
                .iter()
                .map(|expr| expr.eval(&eval))
                .collect::<Result<Vec<_>>>()?;

            let partition = hash_join_partition_for(&partition_key, partition_count);

            let row_width = i64::try_from(row.len()).map_err(|_| {
                HtapError::InvalidArgument("window input row width exceeds i64".into())
            })?;
            let input_index = i64::try_from(input_index)
                .map_err(|_| HtapError::InvalidArgument("window input index exceeds i64".into()))?;
            let mut values = Vec::with_capacity(2 + row.len() + aggregates.len());
            values.push(Value::Int64(input_index));
            values.push(Value::Int64(row_width));
            values.extend(row.iter().cloned());
            values.extend(aggregates.iter().cloned());
            writers[partition].append_row(&Row::new(values))?;
        }
        for writer in writers {
            writer.finish()?;
        }

        let mut partition_sel = sel.clone();
        partition_sel.windows = vec![window.clone()];
        for path in paths {
            let mut reader = SpillReader::open(&path)?;
            validate_spill_header(
                reader.read_header()?,
                statement_id,
                SpillKind::Window,
                OperatorKind::Window,
            )?;

            // Charge each hash bucket as it is decoded. A bucket that cannot be held on its
            // own fails immediately rather than causing recursive spill partitioning.
            let mut reservations = Vec::new();
            let mut grouped = BTreeMap::<Vec<Value>, Vec<(usize, Vec<Value>, Vec<Value>)>>::new();
            while let Some(row) = reader.read_row()? {
                let mut values = row.into_values();
                let reservation = context
                    .memory_budget
                    .try_reserve(estimate_values_bytes(&values))?;
                let [Value::Int64(input_index), Value::Int64(row_width)] =
                    values.get(..2).ok_or_else(|| {
                        HtapError::Corruption(
                            "window spill row is missing its input index or width".into(),
                        )
                    })?
                else {
                    return Err(HtapError::Corruption(
                        "window spill row has an invalid input index or width".into(),
                    ));
                };
                let input_index = usize::try_from(*input_index).map_err(|_| {
                    HtapError::Corruption("window spill row has an invalid input index".into())
                })?;
                let row_width = usize::try_from(*row_width).map_err(|_| {
                    HtapError::Corruption("window spill row has an invalid input-row width".into())
                })?;
                values.drain(..2);
                if values.len() < row_width {
                    return Err(HtapError::Corruption(
                        "window spill row is shorter than its input-row width".into(),
                    ));
                }
                let aggregates = values.split_off(row_width);
                let eval = htap_sql::eval_context! {
                    row: &values,
                    current_outer_row: context.current_outer_row,
                    aggregates: &aggregates,
                    output: None,
                    subqueries: context.subqueries,
                    variables: context.variables,
                    subquery_runner: context.subquery_runner,
                    subquery_budget: context.subquery_budget,
                };
                let key = window
                    .partition_by
                    .iter()
                    .map(|expr| expr.eval(&eval))
                    .collect::<Result<Vec<_>>>()?;
                reservations.push(reservation);
                grouped
                    .entry(key)
                    .or_default()
                    .push((input_index, values, aggregates));
            }

            // The evaluator charges its complete per-window working set. Release the decoded
            // bucket charge before evaluating a logical partition so it is not double charged.
            drop(reservations);
            for (_, rows) in grouped {
                let mut input_indexes = Vec::with_capacity(rows.len());
                let mut partition_inputs = Vec::with_capacity(rows.len());
                for (input_index, row, aggregates) in rows {
                    input_indexes.push(input_index);
                    partition_inputs.push((row, aggregates));
                }
                let values = evaluate_windows_inner(
                    &partition_sel,
                    &partition_inputs,
                    EvaluateWindowsContext {
                        subqueries: context.subqueries,
                        current_outer_row: context.current_outer_row,
                        variables: context.variables,
                        subquery_runner: context.subquery_runner,
                        subquery_budget: context.subquery_budget,
                        memory_budget: context.memory_budget,
                    },
                    spill_root,
                    false,
                )?;
                for (input_index, values) in input_indexes.into_iter().zip(values) {
                    results[input_index][window_index] =
                        values.into_iter().next().ok_or_else(|| {
                            HtapError::Internal(
                                "window partition result is missing its value".into(),
                            )
                        })?;
                }
            }
        }
    }

    Ok(results)
}

fn estimate_group_entry_bytes(
    key: &[Value],
    representative: &[Value],
    states: &[AggState],
) -> usize {
    std::mem::size_of::<Vec<Value>>() * 2
        + key
            .iter()
            .map(MemoryBudget::estimate_value_bytes)
            .sum::<usize>()
        + representative
            .iter()
            .map(MemoryBudget::estimate_value_bytes)
            .sum::<usize>()
        + states.iter().map(AggState::estimated_bytes).sum::<usize>()
}

#[derive(Debug)]
struct AggregateGroup {
    representative: Vec<Value>,
    representative_index: usize,
    states: Vec<AggState>,
    reserved_bytes: usize,
    reservations: Vec<crate::memory_budget::MemoryReservation>,
}

fn aggregate_rows(
    sel: &SelectBody,
    rows: Vec<Vec<Value>>,
    inputs: EvalInputs<'_>,
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
    parallelism: usize,
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    match aggregate_rows_in_memory(sel, &rows, inputs, memory_budget, parallelism) {
        Ok(result) => Ok(result),
        Err(HtapError::InvalidArgument(message))
            if message.starts_with("query memory budget exceeded:") =>
        {
            spill_group_by(sel, &rows, inputs, memory_budget, spill_root)
        }
        Err(error) => Err(error),
    }
}

fn build_group_map<'a>(
    sel: &SelectBody,
    rows: impl Iterator<Item = (usize, &'a [Value])>,
    inputs: EvalInputs<'_>,
    memory_budget: &Arc<MemoryBudget>,
) -> Result<BTreeMap<Vec<Value>, AggregateGroup>> {
    let mut groups = BTreeMap::new();

    for (row_index, row) in rows {
        let eval = htap_sql::eval_context! {
            row: row,
            current_outer_row: inputs.current_outer_row,
            aggregates: &[],
            output: None,
            subqueries: inputs.subqueries,
            variables: inputs.variables,
            subquery_runner: inputs.subquery_runner,
            subquery_budget: inputs.subquery_budget,
        };
        let key = sel
            .group_by
            .iter()
            .map(|expr| expr.eval(&eval))
            .collect::<Result<Vec<_>>>()?;

        if !groups.contains_key(&key) {
            let states = sel.aggregates.iter().map(AggState::new).collect::<Vec<_>>();
            let reserved_bytes = estimate_group_entry_bytes(&key, row, &states);
            let reservation = memory_budget.try_reserve(reserved_bytes)?;
            groups.insert(
                key.clone(),
                AggregateGroup {
                    representative: row.to_vec(),
                    representative_index: row_index,
                    states,
                    reserved_bytes,
                    reservations: vec![reservation],
                },
            );
        }

        let group = groups
            .get_mut(&key)
            .expect("group inserted before aggregate accumulation");
        let old_state_bytes = group
            .states
            .iter()
            .map(AggState::estimated_bytes)
            .sum::<usize>();

        for (state, spec) in group.states.iter_mut().zip(&sel.aggregates) {
            let value = match &spec.arg {
                Some(arg) => arg.eval(&eval)?,
                None => Value::Int64(1),
            };
            state.accumulate(spec, value)?;
        }

        resize_group_reservation(group, old_state_bytes, memory_budget)?;
    }

    Ok(groups)
}

fn resize_group_reservation(
    group: &mut AggregateGroup,
    old_state_bytes: usize,
    memory_budget: &Arc<MemoryBudget>,
) -> Result<()> {
    let new_state_bytes = group
        .states
        .iter()
        .map(AggState::estimated_bytes)
        .sum::<usize>();
    if new_state_bytes > old_state_bytes {
        let additional = new_state_bytes - old_state_bytes;
        let reservation = memory_budget.try_reserve(additional)?;
        group.reserved_bytes = group.reserved_bytes.saturating_add(additional);
        group.reservations.push(reservation);
    }
    Ok(())
}

fn merge_group_maps(
    sel: &SelectBody,
    target: &mut BTreeMap<Vec<Value>, AggregateGroup>,
    source: BTreeMap<Vec<Value>, AggregateGroup>,
    memory_budget: &Arc<MemoryBudget>,
) -> Result<()> {
    for (key, mut source_group) in source {
        let Some(target_group) = target.get_mut(&key) else {
            target.insert(key, source_group);
            continue;
        };

        if source_group.representative_index < target_group.representative_index {
            target_group.representative = std::mem::take(&mut source_group.representative);
            target_group.representative_index = source_group.representative_index;
        }

        let old_state_bytes = target_group
            .states
            .iter()
            .map(AggState::estimated_bytes)
            .sum::<usize>();
        for ((target_state, source_state), spec) in target_group
            .states
            .iter_mut()
            .zip(source_group.states)
            .zip(&sel.aggregates)
        {
            target_state.merge(spec, source_state)?;
        }
        resize_group_reservation(target_group, old_state_bytes, memory_budget)?;
    }
    Ok(())
}

fn finish_group_map(
    groups: BTreeMap<Vec<Value>, AggregateGroup>,
    aggregates: &[AggregateSpec],
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    groups
        .into_values()
        .map(|group| {
            let values = group
                .states
                .iter()
                .zip(aggregates)
                .map(|(state, spec)| state.finish(spec))
                .collect::<Result<Vec<_>>>()?;
            Ok((group.representative, values))
        })
        .collect()
}

fn expr_requires_serial_worker_evaluation(expr: &Expr) -> bool {
    let mut requires_serial = false;
    expr.walk(&mut |expr| {
        if matches!(
            expr,
            Expr::Variable { .. }
                | Expr::ScalarSubquery { .. }
                | Expr::InSubquery { .. }
                | Expr::Exists { .. }
                | Expr::CorrelatedColumnRef { .. }
        ) {
            requires_serial = true;
        }
    });
    requires_serial
}

fn aggregate_rows_in_memory(
    sel: &SelectBody,
    rows: &[Vec<Value>],
    inputs: EvalInputs<'_>,
    memory_budget: &Arc<MemoryBudget>,
    parallelism: usize,
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    let worker_count = parallelism.max(1).min(rows.len().max(1));
    // Record the chosen degree on the statement thread so integration tests can prove the
    // parallel branch ran without making EXPLAIN output part of the execution contract.
    let can_parallelize = !sel
        .group_by
        .iter()
        .chain(
            sel.aggregates
                .iter()
                .filter_map(|aggregate| aggregate.arg.as_ref()),
        )
        .any(expr_requires_serial_worker_evaluation);
    let subqueries = inputs.subqueries;
    let current_outer_row = inputs.current_outer_row;
    let mut groups =
        if rows.len() >= GROUP_BY_PARALLEL_THRESHOLD && worker_count > 1 && can_parallelize {
            record_parallel_workers(worker_count);
            let partials = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(worker_count);
                for worker_index in 0..worker_count {
                    handles.push(scope.spawn(move || {
                        let worker_inputs = EvalInputs {
                            subqueries,
                            current_outer_row,
                            variables: None,
                            subquery_runner: None,
                            subquery_budget: None,
                            spill_root: inputs.spill_root,
                        };
                        build_group_map(
                            sel,
                            rows.iter()
                                .enumerate()
                                .skip(worker_index)
                                .step_by(worker_count)
                                .map(|(row_index, row)| (row_index, row.as_slice())),
                            worker_inputs,
                            memory_budget,
                        )
                    }));
                }

                let mut partials = Vec::with_capacity(worker_count);
                for handle in handles {
                    partials.push(handle.join().map_err(|_| {
                        HtapError::Internal("GROUP BY worker thread panicked".into())
                    })??);
                }
                Ok::<_, HtapError>(partials)
            })?;

            let mut merged = BTreeMap::new();
            for partial in partials {
                merge_group_maps(sel, &mut merged, partial, memory_budget)?;
            }
            merged
        } else {
            build_group_map(
                sel,
                rows.iter()
                    .enumerate()
                    .map(|(row_index, row)| (row_index, row.as_slice())),
                inputs,
                memory_budget,
            )?
        };

    if rows.is_empty() && sel.group_by.is_empty() {
        let states = sel.aggregates.iter().map(AggState::new).collect::<Vec<_>>();
        let reserved_bytes = estimate_group_entry_bytes(&[], &[], &states);
        let reservation = memory_budget.try_reserve(reserved_bytes)?;
        groups.insert(
            Vec::new(),
            AggregateGroup {
                representative: Vec::new(),
                representative_index: 0,
                states,
                reserved_bytes,
                reservations: vec![reservation],
            },
        );
    }

    finish_group_map(groups, &sel.aggregates)
}

fn spill_group_by(
    sel: &SelectBody,
    rows: &[Vec<Value>],
    inputs: EvalInputs<'_>,
    memory_budget: &Arc<MemoryBudget>,
    spill_root: &Path,
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    // An ungrouped aggregate has one constant-size aggregate state. Partitioning it
    // would manufacture an empty aggregate result for every unused partition.
    if sel.group_by.is_empty() {
        return aggregate_rows_in_memory(sel, rows, inputs, memory_budget, 1);
    }

    let partition_count = GROUP_BY_SPILL_PARTITIONS;

    let statement_id = NEXT_SPILL_STATEMENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut spill_dir = SpillDir::create(spill_root, statement_id)?;
    record_spill(OperatorKind::GroupBy);
    let mut paths = Vec::with_capacity(partition_count);
    let mut writers = Vec::with_capacity(partition_count);

    for partition in 0..partition_count {
        let path = spill_dir.partition_file_path(OperatorKind::GroupBy, "input", partition);
        writers.push(SpillWriter::create(
            &path,
            SpillHeader::new(SpillKind::GroupBy, statement_id, OperatorKind::GroupBy),
        )?);
        paths.push(path);
    }

    for row in rows {
        let eval = htap_sql::eval_context! {
            row: row,
            current_outer_row: inputs.current_outer_row,
            aggregates: &[],
            output: None,
            subqueries: inputs.subqueries,
            variables: inputs.variables,
            subquery_runner: inputs.subquery_runner,
            subquery_budget: inputs.subquery_budget,
        };
        let key = sel
            .group_by
            .iter()
            .map(|expr| expr.eval(&eval))
            .collect::<Result<Vec<_>>>()?;
        let partition = hash_join_partition_for(&key, partition_count);
        writers[partition].append_row(&Row::new(row.clone()))?;
    }
    for writer in writers {
        writer.finish()?;
    }

    let mut output = Vec::new();
    for path in paths {
        let mut reader = SpillReader::open(&path)?;
        let header = reader.read_header()?;
        if header.statement_id != statement_id
            || header.kind != SpillKind::GroupBy
            || header.operator_kind != OperatorKind::GroupBy
        {
            return Err(HtapError::Corruption(
                "GROUP BY spill header does not match its statement".into(),
            ));
        }

        // Keep the decoded partition charged while its aggregate state is built. This bounds
        // read-back memory and rejects a skewed partition that cannot fit by itself.
        let mut partition_rows = Vec::new();
        let mut partition_reservations = Vec::new();
        while let Some(row) = reader.read_row()? {
            let values = row.into_values();
            partition_reservations.push(memory_budget.try_reserve(estimate_values_bytes(&values))?);
            partition_rows.push(values);
        }
        // The aggregate operator charges its own group state; release decoded input rows first
        // so this partition is not double charged during aggregation.
        drop(partition_reservations);
        output.extend(aggregate_rows_in_memory(
            sel,
            &partition_rows,
            inputs,
            memory_budget,
            1,
        )?);
    }

    if rows.is_empty() && sel.group_by.is_empty() {
        output.extend(aggregate_rows_in_memory(
            sel,
            &[],
            inputs,
            memory_budget,
            1,
        )?);
    }

    output.sort_by(|left, right| {
        let left_eval = EvalContext::row_only(&left.0);
        let right_eval = EvalContext::row_only(&right.0);
        let left_key = sel
            .group_by
            .iter()
            .map(|expr| expr.eval(&left_eval).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        let right_key = sel
            .group_by
            .iter()
            .map(|expr| expr.eval(&right_eval).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        left_key.cmp(&right_key)
    });
    Ok(output)
}

/// Shared inputs used while evaluating window arguments and frame boundaries.
struct WindowFunctionContext<'a> {
    inputs: &'a [(Vec<Value>, Vec<Value>)],
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
}

fn window_frame_range(
    window: &htap_sql::query::WindowSpec,
    partition: &[usize],
    position: usize,
    order_keys: &[Vec<Value>],
    input_index: usize,
    context: &WindowFunctionContext<'_>,
) -> Result<Option<(usize, usize)>> {
    let len = partition.len();
    if len == 0 {
        return Ok(None);
    }

    let peer_range = || {
        let mut start = position;
        while start > 0
            && order_keys_equal(
                &order_keys[partition[start - 1]],
                &order_keys[partition[position]],
                &window.order_by,
            )
        {
            start -= 1;
        }
        let mut end = position;
        while end + 1 < len
            && order_keys_equal(
                &order_keys[partition[end + 1]],
                &order_keys[partition[position]],
                &window.order_by,
            )
        {
            end += 1;
        }
        (start, end)
    };

    let range = match &window.frame {
        WindowFrame::None => (0, len - 1),
        WindowFrame::Rows { start, end } => {
            let endpoint = |bound: &RowFrameBound| -> i128 {
                match bound {
                    RowFrameBound::Unbounded(WindowFrameDirection::Preceding) => i128::MIN,
                    RowFrameBound::Unbounded(WindowFrameDirection::Following) => i128::MAX,
                    RowFrameBound::CurrentRow => position as i128,
                    RowFrameBound::Offset { value, direction } => match direction {
                        WindowFrameDirection::Preceding => position as i128 - *value as i128,
                        WindowFrameDirection::Following => position as i128 + *value as i128,
                    },
                }
            };
            let start = endpoint(start).max(0);
            let end = endpoint(end).min(len as i128 - 1);
            if start > end || start >= len as i128 || end < 0 {
                return Ok(None);
            }
            (start as usize, end as usize)
        }
        WindowFrame::PeerRange { start, end } => {
            let (peer_start, peer_end) = peer_range();
            let endpoint = |bound: &PeerFrameBound, is_start: bool| match bound {
                PeerFrameBound::Unbounded(WindowFrameDirection::Preceding) => 0,
                PeerFrameBound::Unbounded(WindowFrameDirection::Following) => len - 1,
                PeerFrameBound::CurrentRow => {
                    if is_start {
                        peer_start
                    } else {
                        peer_end
                    }
                }
            };
            (endpoint(start, true), endpoint(end, false))
        }
        WindowFrame::ValueRange { start, end } => {
            if window.order_by.len() != 1 {
                return Err(HtapError::Internal(
                    "value RANGE frame requires exactly one ORDER BY key".into(),
                ));
            }
            let current = &order_keys[input_index][0];
            let (peer_start, peer_end) = peer_range();
            let endpoint = |bound: &ValueFrameBound, is_start: bool| -> Result<usize> {
                match bound {
                    ValueFrameBound::Unbounded(WindowFrameDirection::Preceding) => Ok(0),
                    ValueFrameBound::Unbounded(WindowFrameDirection::Following) => Ok(len - 1),
                    ValueFrameBound::CurrentRow => Ok(if is_start { peer_start } else { peer_end }),
                    ValueFrameBound::Offset { value, direction } if current.is_null() => {
                        let _ = value;
                        let _ = direction;
                        Ok(if is_start { peer_start } else { peer_end })
                    }
                    ValueFrameBound::Offset { value, direction } => {
                        let (row, aggregates) = &context.inputs[input_index];
                        let offset = value.eval(&htap_sql::eval_context! {
                            row: row,
                            current_outer_row: context.current_outer_row,
                            aggregates: aggregates,
                            output: None,
                            subqueries: context.subqueries,
                            variables: context.variables,
                            subquery_runner: context.subquery_runner,
                            subquery_budget: context.subquery_budget,
                        })?;
                        let boundary = value_range_boundary(
                            current,
                            &offset,
                            *direction,
                            window.order_by[0].asc,
                        )?;
                        let boundary_keys = [boundary];

                        if is_start {
                            Ok(partition
                                .iter()
                                .position(|index| {
                                    !order_keys[*index][0].is_null()
                                        && compare_order_keys(
                                            &order_keys[*index],
                                            &boundary_keys,
                                            &window.order_by,
                                        ) != std::cmp::Ordering::Less
                                })
                                .unwrap_or(len))
                        } else {
                            Ok(partition
                                .iter()
                                .rposition(|index| {
                                    !order_keys[*index][0].is_null()
                                        && compare_order_keys(
                                            &order_keys[*index],
                                            &boundary_keys,
                                            &window.order_by,
                                        ) != std::cmp::Ordering::Greater
                                })
                                .unwrap_or(len))
                        }
                    }
                }
            };
            (endpoint(start, true)?, endpoint(end, false)?)
        }
    };

    if range.0 >= len || range.1 >= len || range.0 > range.1 {
        Ok(None)
    } else {
        Ok(Some(range))
    }
}

fn value_range_boundary(
    current: &Value,
    offset: &Value,
    direction: WindowFrameDirection,
    ascending: bool,
) -> Result<Value> {
    let subtract = matches!(direction, WindowFrameDirection::Preceding) == ascending;
    let invalid = || {
        HtapError::InvalidArgument(format!(
            "RANGE offset must be a non-negative finite numeric value, got {offset}"
        ))
    };
    let overflow = || HtapError::InvalidArgument("window RANGE boundary overflow".into());

    match current {
        Value::Int32(value) => {
            let offset = match offset {
                Value::Int32(value) if *value >= 0 => *value as i64,
                Value::Int64(value) if *value >= 0 => *value,
                _ => return Err(invalid()),
            };
            let value = *value as i64;
            Ok(Value::Int64(if subtract {
                value.checked_sub(offset).ok_or_else(overflow)?
            } else {
                value.checked_add(offset).ok_or_else(overflow)?
            }))
        }
        Value::Int64(value) => {
            let offset = match offset {
                Value::Int32(value) if *value >= 0 => *value as i64,
                Value::Int64(value) if *value >= 0 => *value,
                _ => return Err(invalid()),
            };
            Ok(Value::Int64(if subtract {
                value.checked_sub(offset).ok_or_else(overflow)?
            } else {
                value.checked_add(offset).ok_or_else(overflow)?
            }))
        }
        Value::Timestamp(value) => {
            let offset = match offset {
                Value::Int32(value) if *value >= 0 => *value as i64,
                Value::Int64(value) if *value >= 0 => *value,
                _ => return Err(invalid()),
            };
            Ok(Value::Timestamp(if subtract {
                value.checked_sub(offset).ok_or_else(overflow)?
            } else {
                value.checked_add(offset).ok_or_else(overflow)?
            }))
        }
        Value::Float64(value) if value.is_finite() => {
            let offset = match offset {
                Value::Int32(value) if *value >= 0 => *value as f64,
                Value::Int64(value) if *value >= 0 => *value as f64,
                Value::Float64(value) if value.is_finite() && *value >= 0.0 => *value,
                _ => return Err(invalid()),
            };
            let boundary = if subtract {
                *value - offset
            } else {
                *value + offset
            };
            if !boundary.is_finite() {
                return Err(overflow());
            }
            Ok(Value::Float64(boundary))
        }
        _ => Err(HtapError::InvalidArgument(format!(
            "value RANGE requires a numeric or timestamp ORDER BY key, got {current}"
        ))),
    }
}

fn compare_order_keys(
    left: &[Value],
    right: &[Value],
    order_by: &[OrderItem],
) -> std::cmp::Ordering {
    for ((left, right), item) in left.iter().zip(right).zip(order_by) {
        let ordering = match (left.is_null(), right.is_null()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => {
                if item.nulls_first {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            (false, true) => {
                if item.nulls_first {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            }
            (false, false) => {
                let ordering = compare(left, right)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| left.cmp(right));
                if item.asc {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

fn order_keys_equal(left: &[Value], right: &[Value], order_by: &[OrderItem]) -> bool {
    compare_order_keys(left, right, order_by) == std::cmp::Ordering::Equal
}

fn eval_window_arg(
    window: &htap_sql::query::WindowSpec,
    argument: usize,
    input_index: usize,
    context: &WindowFunctionContext<'_>,
) -> Result<Value> {
    let (row, aggregates) = &context.inputs[input_index];
    window.args[argument].eval(&htap_sql::eval_context! {
        row: row,
        current_outer_row: context.current_outer_row,
        aggregates: aggregates,
        output: None,
        subqueries: context.subqueries,
        variables: context.variables,
        subquery_runner: context.subquery_runner,
        subquery_budget: context.subquery_budget,
    })
}

fn window_arg_u64(
    window: &htap_sql::query::WindowSpec,
    argument: usize,
    input_index: usize,
    context: &WindowFunctionContext<'_>,
) -> Result<u64> {
    match eval_window_arg(window, argument, input_index, context)? {
        Value::Int32(value) if value >= 0 => Ok(value as u64),
        Value::Int64(value) if value >= 0 => Ok(value as u64),
        value => Err(HtapError::InvalidArgument(format!(
            "window offset or bucket count must be a non-negative integer, got {value}"
        ))),
    }
}

/// Shared expression-evaluation inputs used by projection and join evaluation.
#[derive(Clone, Copy)]
struct EvalInputs<'a> {
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
    spill_root: &'a Path,
}

/// Projects one input row (or group) and computes top-level sort keys.
fn project_row(
    sel: &SelectBody,
    order_by: &[OrderItem],
    row: &[Value],
    aggregates: &[Value],
    windows: &[Value],
    inputs: EvalInputs<'_>,
) -> Result<Keyed> {
    let EvalInputs {
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
        spill_root: _,
    } = inputs;
    let c = htap_sql::eval_context! {
        row: row,
        current_outer_row: current_outer_row,
        aggregates: aggregates,
        output: Some(windows),
        subqueries: subqueries,
        variables: variables,
        subquery_runner: subquery_runner,
        subquery_budget: subquery_budget,
    };
    let output: Vec<Value> = sel
        .projection
        .iter()
        .map(|p| p.expr.eval(&c))
        .collect::<Result<_>>()?;
    let c = htap_sql::eval_context! {
        row: row,
        current_outer_row: current_outer_row,
        aggregates: aggregates,
        output: Some(&output),
        subqueries: subqueries,
        variables: variables,
        subquery_runner: subquery_runner,
        subquery_budget: subquery_budget,
    };
    let keys: Vec<Value> = order_by
        .iter()
        .map(|o| o.expr.eval(&c))
        .collect::<Result<_>>()?;
    Ok(Keyed { output, keys })
}

/// Splits an expression into its top-level `AND` conjuncts.
fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::BinaryOp {
            op: BinOp::And,
            left,
            right,
        } => {
            let mut v = conjuncts(left);
            v.extend(conjuncts(right));
            v
        }
        other => vec![other],
    }
}

/// Converts single-table conjuncts into the narrow `AnalyticFilter` shape used for
/// partition pruning and single-leaf predicate pushdown. Conjuncts that do not fit are
/// simply left to the residual `WHERE` evaluation.
fn pushdown_filter(conjuncts: &[&Expr], table: &TableDescriptor) -> Option<AnalyticFilter> {
    let mut leaves = Vec::new();
    for c in conjuncts {
        let leaf = match c {
            Expr::IsNull(inner) => match inner.as_ref() {
                Expr::ColumnRef { column, .. } => Some(AnalyticFilter::IsNull { column: *column }),
                _ => None,
            },
            Expr::IsNotNull(inner) => match inner.as_ref() {
                Expr::ColumnRef { column, .. } => {
                    Some(AnalyticFilter::IsNotNull { column: *column })
                }
                _ => None,
            },
            Expr::BinaryOp { op, left, right } if op.is_comparison() => {
                let (column, value, op) = match (left.as_ref(), right.as_ref()) {
                    (Expr::ColumnRef { column, .. }, Expr::Literal(v)) => (*column, v, *op),
                    (Expr::Literal(v), Expr::ColumnRef { column, .. }) => {
                        let flipped = match op {
                            BinOp::Lt => BinOp::Gt,
                            BinOp::Lte => BinOp::Gte,
                            BinOp::Gt => BinOp::Lt,
                            BinOp::Gte => BinOp::Lte,
                            other => *other,
                        };
                        (*column, v, flipped)
                    }
                    _ => continue,
                };
                if value.is_null() {
                    continue;
                }
                let col_type = table.schema.column(column).map(|c| c.data_type);
                let Some(col_type) = col_type else { continue };
                let Ok(value) = cast_value(value.clone(), col_type) else {
                    continue;
                };
                if value.data_type() != Some(col_type) {
                    continue;
                }
                // Casting a fractional literal to an integer column would change the predicate.
                if let (Value::Float64(f), DataType::Int32 | DataType::Int64) =
                    (value_source(left, right), col_type)
                {
                    if f.fract() != 0.0 {
                        continue;
                    }
                }
                let cmp = match op {
                    BinOp::Eq => ComparisonOp::Eq,
                    BinOp::NotEq => ComparisonOp::NotEq,
                    BinOp::Lt => ComparisonOp::Lt,
                    BinOp::Lte => ComparisonOp::Lte,
                    BinOp::Gt => ComparisonOp::Gt,
                    BinOp::Gte => ComparisonOp::Gte,
                    _ => continue,
                };
                Some(AnalyticFilter::Comparison {
                    column,
                    op: cmp,
                    value,
                })
            }
            _ => None,
        };
        if let Some(l) = leaf {
            leaves.push(l);
        }
    }
    match leaves.len() {
        0 => None,
        1 => leaves.pop(),
        _ => Some(AnalyticFilter::And(leaves)),
    }
}

fn value_source(left: &Expr, right: &Expr) -> Value {
    match (left, right) {
        (Expr::Literal(v), _) | (_, Expr::Literal(v)) => v.clone(),
        _ => Value::Null,
    }
}

/// Reads a base table at the statement snapshot, returning full-width rows in which
/// columns not in `needed` (and not part of the primary key) are `NULL`.
pub(crate) fn scan_base_table(
    ctx: &ExecContext<'_>,
    table: &str,
    needed: &BTreeSet<usize>,
    slot: usize,
    conjuncts: &[&Expr],
) -> Result<Vec<Vec<Value>>> {
    let (table_desc, partitions) = ctx
        .server
        .resolve_table_and_all_partitions(table, ctx.catalog)?;
    let width = table_desc.schema.len();
    let mut source_columns: BTreeSet<usize> = needed.clone();
    source_columns.extend(table_desc.primary_key.iter().copied());
    let source_columns: Vec<usize> = source_columns.into_iter().collect();

    let filter = pushdown_filter(conjuncts, table_desc);
    let selected = olap::prune_partitions(table_desc, &partitions, filter.as_ref());
    let pushdown = olap::select_pushdown_predicate(filter.as_ref());
    let _ = slot;

    let mut out = Vec::new();
    for partition in selected {
        let rows = scan_partition_compact(
            &ctx.server.engine,
            &ctx.server.colstore_dir,
            ctx.catalog,
            partition,
            ctx.snapshot,
            &source_columns,
            &table_desc.primary_key,
            pushdown.as_ref(),
            ctx.write_set,
        )?;
        for row in rows {
            let mut full = vec![Value::Null; width];
            for (i, &col) in source_columns.iter().enumerate() {
                full[col] = row.get(i).cloned().ok_or_else(|| {
                    HtapError::Internal(format!("compact row missing column {col}"))
                })?;
            }
            out.push(full);
        }
    }
    Ok(out)
}

/// Marks leaves below an outer join's null-supplying input. A leaf remains ineligible for
/// single-table WHERE pushdown when any ancestor can NULL-pad it.
fn mark_tree_null_supplying(tree: &JoinTree, inherited: bool, null_supplying: &mut [bool]) {
    match tree {
        JoinTree::Leaf(slot) => null_supplying[*slot] |= inherited,
        JoinTree::Join {
            kind, left, right, ..
        } => {
            let left_supplied = inherited || matches!(kind, JoinKind::Right | JoinKind::Full);
            let right_supplied = inherited || matches!(kind, JoinKind::Left | JoinKind::Full);
            mark_tree_null_supplying(left, left_supplied, null_supplying);
            mark_tree_null_supplying(right, right_supplied, null_supplying);
        }
    }
}

/// Returns the inclusive slot range covered by a join subtree.
/// Returns the slots covered by a join subtree in physical row-concatenation order.
fn tree_slots(tree: &JoinTree, slots: &mut Vec<usize>) {
    match tree {
        JoinTree::Leaf(slot) => slots.push(*slot),
        JoinTree::Join { left, right, .. } => {
            tree_slots(left, slots);
            tree_slots(right, slots);
        }
    }
}

fn tree_width(tree: &JoinTree, slots: &[TableSlot]) -> usize {
    match tree {
        JoinTree::Leaf(slot) => slots[*slot].width(),
        JoinTree::Join { left, right, .. } => {
            tree_width(left, slots).saturating_add(tree_width(right, slots))
        }
    }
}

/// Reorders a physical join row back into the binder's stable slot order.
fn row_to_logical_slot_order(
    row: Vec<Value>,
    physical_slots: &[usize],
    slots: &[TableSlot],
) -> Result<Vec<Value>> {
    let mut by_slot = vec![None; slots.len()];
    let mut offset: usize = 0;
    for &slot in physical_slots {
        let width = slots[slot].width();
        let end = offset.saturating_add(width);
        if end > row.len() {
            return Err(HtapError::Internal(
                "reordered join row is shorter than its slot widths".into(),
            ));
        }
        by_slot[slot] = Some(row[offset..end].to_vec());
        offset = end;
    }
    if offset != row.len() {
        return Err(HtapError::Internal(
            "reordered join row is longer than its slot widths".into(),
        ));
    }

    let mut logical = Vec::with_capacity(row.len());
    for (slot, values) in by_slot.into_iter().enumerate() {
        logical.extend(values.ok_or_else(|| {
            HtapError::Internal(format!("reordered join row is missing slot {slot}"))
        })?);
    }
    Ok(logical)
}

/// Optimizer hints and traversal state shared while evaluating a join tree.
struct JoinOptimizationContext<'a> {
    build_sides: Option<&'a HashMap<usize, BuildSide>>,
    next_join_id: usize,
}

/// Evaluates a nested join subtree bottom-up. Each result row is the concatenation of its
/// leaves' full-width slot rows, which is also the global slot order at the root.
fn evaluate_join_tree(
    sel: &SelectBody,
    tree: &JoinTree,
    slot_rows: &mut [Vec<Vec<Value>>],
    inputs: EvalInputs<'_>,
    memory_budget: &Arc<MemoryBudget>,
    parallelism: usize,
    join_optimization: &mut JoinOptimizationContext<'_>,
) -> Result<Vec<Vec<Value>>> {
    let EvalInputs {
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
        spill_root,
    } = inputs;
    match tree {
        JoinTree::Leaf(slot) => Ok(std::mem::take(&mut slot_rows[*slot])),
        JoinTree::Join {
            kind,
            left,
            right,
            on,
        } => {
            let left_rows = evaluate_join_tree(
                sel,
                left,
                slot_rows,
                EvalInputs {
                    subqueries,
                    current_outer_row,
                    variables,
                    subquery_runner,
                    subquery_budget,
                    spill_root,
                },
                memory_budget,
                parallelism,
                &mut *join_optimization,
            )?;
            let right_rows = evaluate_join_tree(
                sel,
                right,
                slot_rows,
                EvalInputs {
                    subqueries,
                    current_outer_row,
                    variables,
                    subquery_runner,
                    subquery_budget,
                    spill_root,
                },
                memory_budget,
                parallelism,
                &mut *join_optimization,
            )?;

            let join_id = join_optimization.next_join_id;
            join_optimization.next_join_id += 1;
            let build_side = join_optimization
                .build_sides
                .and_then(|build_sides| build_sides.get(&join_id))
                .copied()
                .unwrap_or(BuildSide::Right);

            let mut left_slots = Vec::new();
            let mut right_slots = Vec::new();
            tree_slots(left, &mut left_slots);
            tree_slots(right, &mut right_slots);

            let left_width = tree_width(left, &sel.slots);
            let right_width = tree_width(right, &sel.slots);
            let equi_key_types = join_equi_key_types(on.as_ref(), &left_slots, &right_slots);
            let right_slot = right_slots.first().copied().ok_or_else(|| {
                HtapError::Internal("join right subtree contains no table slots".into())
            })?;
            let join = JoinSpec {
                kind: *kind,
                right_slot,
                on: on.clone(),
                equi_key_types,
            };
            join_rows(JoinRowsInput {
                left: left_rows,
                left_width,
                right: right_rows,
                right_width,
                left_slots: &left_slots,
                right_slots: &right_slots,
                join: &join,
                subqueries: inputs.subqueries,
                current_outer_row: inputs.current_outer_row,
                variables: inputs.variables,
                subquery_runner: inputs.subquery_runner,
                subquery_budget: inputs.subquery_budget,
                memory_budget,
                spill_root,
                parallelism,
                build_side,
            })
        }
    }
}

fn join_equi_key_types(
    on: Option<&Expr>,
    left_operand_slots: &[usize],
    right_operand_slots: &[usize],
) -> Vec<Option<DataType>> {
    let Some(on) = on else {
        return Vec::new();
    };
    let is_left = |slots: &[usize]| {
        !slots.is_empty() && slots.iter().all(|slot| left_operand_slots.contains(slot))
    };
    let is_right = |slots: &[usize]| {
        !slots.is_empty() && slots.iter().all(|slot| right_operand_slots.contains(slot))
    };

    conjuncts(on)
        .into_iter()
        .filter_map(|term| {
            let Expr::BinaryOp {
                op: BinOp::Eq,
                left,
                right,
            } = term
            else {
                return None;
            };
            let left_slots = left.referenced_slots();
            let right_slots = right.referenced_slots();
            let (left, right) = if is_left(&left_slots) && is_right(&right_slots) {
                (left.as_ref(), right.as_ref())
            } else if is_right(&left_slots) && is_left(&right_slots) {
                (right.as_ref(), left.as_ref())
            } else {
                return None;
            };

            let left_type = left.expr_type().data_type;
            let right_type = right.expr_type().data_type;
            let numeric = |data_type| {
                matches!(
                    data_type,
                    DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Timestamp
                )
            };
            Some(if numeric(left_type) && numeric(right_type) {
                Some(
                    if left_type == DataType::Float64 || right_type == DataType::Float64 {
                        DataType::Float64
                    } else {
                        DataType::Int64
                    },
                )
            } else {
                None
            })
        })
        .collect()
}

fn estimate_join_build_bytes(rows: &[Vec<Value>]) -> usize {
    rows.iter()
        .map(|values| {
            MemoryBudget::estimate_row_bytes(&Row::new(values.clone()))
                .saturating_add(std::mem::size_of::<Vec<Value>>())
                .saturating_add(std::mem::size_of::<usize>())
        })
        .fold(0usize, usize::saturating_add)
}

fn normalized_join_key(
    row: &[Value],
    key_exprs: &[&Expr],
    canonical_types: &[DataType],
) -> Result<Option<Vec<Value>>> {
    let context = EvalContext::row_only(row);
    let mut key = Vec::with_capacity(key_exprs.len());
    for (expr, canonical_type) in key_exprs.iter().zip(canonical_types) {
        let value = expr.eval(&context)?;
        if value.is_null() {
            return Ok(None);
        }
        key.push(normalize_key(value, *canonical_type)?);
    }
    Ok(Some(key))
}

fn hash_join_partition_for(key: &[Value], partition_count: usize) -> usize {
    debug_assert!(partition_count > 0);
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % partition_count
}

struct SpillHashJoinInput<'a> {
    left: &'a [Vec<Value>],
    left_width: usize,
    right: &'a [Vec<Value>],
    right_width: usize,
    left_slots: &'a [usize],
    right_slots: &'a [usize],
    join: &'a JoinSpec,
    left_keys: &'a [&'a Expr],
    right_keys: &'a [&'a Expr],
    canonical_types: &'a [DataType],
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
    memory_budget: &'a Arc<MemoryBudget>,
    spill_root: &'a Path,
}

fn spill_hash_join(input: SpillHashJoinInput<'_>) -> Result<Vec<Vec<Value>>> {
    let SpillHashJoinInput {
        left,
        left_width,
        right,
        right_width,
        left_slots,
        right_slots,
        join,
        left_keys,
        right_keys,
        canonical_types,
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
        memory_budget,
        spill_root,
    } = input;

    // Each spilled partition rebuilds its hash table from `right` in `join_rows_inner`.
    // Reserve 60% of the currently available statement budget for that partition's probe rows
    // and output, leaving 40% for the build-side hash table.
    let build_bytes = estimate_join_build_bytes(right);
    let available_bytes = memory_budget.remaining();
    let build_bytes_per_partition = available_bytes
        .saturating_mul(40)
        .checked_div(100)
        .unwrap_or(0)
        .max(1);
    let partition_count = (build_bytes / build_bytes_per_partition)
        .saturating_add(usize::from(
            !build_bytes.is_multiple_of(build_bytes_per_partition),
        ))
        .clamp(1, HASH_JOIN_MAX_SPILL_PARTITIONS);

    let statement_id = NEXT_SPILL_STATEMENT_ID.fetch_add(1, Ordering::Relaxed);
    let mut spill_dir = SpillDir::create(spill_root, statement_id)?;
    record_spill(OperatorKind::HashJoin);
    let mut left_paths = Vec::with_capacity(partition_count);
    let mut right_paths = Vec::with_capacity(partition_count);
    let mut left_writers = Vec::with_capacity(partition_count);
    let mut right_writers = Vec::with_capacity(partition_count);

    for partition in 0..partition_count {
        let left_path = spill_dir.partition_file_path(OperatorKind::HashJoin, "left", partition);
        let right_path = spill_dir.partition_file_path(OperatorKind::HashJoin, "right", partition);
        left_writers.push(SpillWriter::create(
            &left_path,
            SpillHeader::new(SpillKind::HashJoin, statement_id, OperatorKind::HashJoin),
        )?);
        right_writers.push(SpillWriter::create(
            &right_path,
            SpillHeader::new(SpillKind::HashJoin, statement_id, OperatorKind::HashJoin),
        )?);
        left_paths.push(left_path);
        right_paths.push(right_path);
    }

    for values in left {
        let partition = normalized_join_key(values, left_keys, canonical_types)?
            .map(|key| hash_join_partition_for(&key, partition_count))
            .unwrap_or(0);
        left_writers[partition].append_row(&Row::new(values.clone()))?;
    }

    let left_padding = vec![Value::Null; left_width];
    for values in right {
        let mut padded = left_padding.clone();
        padded.extend_from_slice(values);
        let partition = normalized_join_key(&padded, right_keys, canonical_types)?
            .map(|key| hash_join_partition_for(&key, partition_count))
            .unwrap_or(0);
        right_writers[partition].append_row(&Row::new(values.clone()))?;
    }

    for writer in left_writers {
        writer.finish()?;
    }
    for writer in right_writers {
        writer.finish()?;
    }

    let mut partition_outputs = Vec::with_capacity(partition_count);
    for partition in 0..partition_count {
        let read_rows = |path: &std::path::Path| -> Result<Vec<Vec<Value>>> {
            let mut reader = SpillReader::open(path)?;
            validate_spill_header(
                reader.read_header()?,
                statement_id,
                SpillKind::HashJoin,
                OperatorKind::HashJoin,
            )?;

            let mut rows = Vec::new();
            while let Some(row) = reader.read_row()? {
                rows.push(row.into_values());
            }
            Ok(rows)
        };

        let left_rows = read_rows(&left_paths[partition])?;
        let right_rows = read_rows(&right_paths[partition])?;
        partition_outputs.push(join_rows_inner(
            JoinRowsInput {
                left: left_rows,
                left_width,
                right: right_rows,
                right_width,
                left_slots,
                right_slots,
                join,
                subqueries,
                current_outer_row,
                variables,
                subquery_runner,
                subquery_budget,
                memory_budget,
                spill_root,
                parallelism: 1,
                build_side: BuildSide::Right,
            },
            false,
        )?);
    }

    Ok(partition_outputs.into_iter().flatten().collect())
}

struct ParallelHashJoinInput<'a> {
    left: &'a [Vec<Value>],
    right: &'a [Vec<Value>],
    left_width: usize,
    left_keys: &'a [&'a Expr],
    right_keys: &'a [&'a Expr],
    canonical_types: &'a [DataType],
    on: &'a Expr,
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    parallelism: usize,
}

fn parallel_cross_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    parallelism: usize,
) -> Result<Vec<Vec<Value>>> {
    let worker_count = parallelism.max(1).min(left.len().max(1));
    let mut partials = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for worker_index in 0..worker_count {
            handles.push(scope.spawn(move || {
                let mut output = Vec::new();
                for (left_index, left_row) in left
                    .iter()
                    .enumerate()
                    .skip(worker_index)
                    .step_by(worker_count)
                {
                    let mut matches = Vec::with_capacity(right.len());
                    for right_row in right {
                        let mut row = Vec::with_capacity(left_row.len() + right_row.len());
                        row.extend_from_slice(left_row);
                        row.extend_from_slice(right_row);
                        matches.push(row);
                    }
                    output.push((left_index, matches));
                }
                output
            }));
        }

        let mut partials = Vec::with_capacity(worker_count);
        for handle in handles {
            partials.push(
                handle
                    .join()
                    .map_err(|_| HtapError::Internal("hash join worker thread panicked".into()))?,
            );
        }
        Ok::<_, HtapError>(partials)
    })?;

    let mut tagged = Vec::with_capacity(left.len());
    for partial in partials.drain(..) {
        tagged.extend(partial);
    }
    tagged.sort_by_key(|(left_index, _)| *left_index);
    Ok(tagged
        .into_iter()
        .flat_map(|(_, matches)| matches)
        .collect())
}

fn parallel_hash_join(input: ParallelHashJoinInput<'_>) -> Result<Vec<Vec<Value>>> {
    let ParallelHashJoinInput {
        left,
        right,
        left_width,
        left_keys,
        right_keys,
        canonical_types,
        on,
        subqueries,
        current_outer_row,
        parallelism,
    } = input;

    let worker_count = parallelism.clamp(1, HASH_JOIN_MAX_SPILL_PARTITIONS.min(right.len().max(1)));
    let mut left_shards = vec![Vec::<(usize, Vec<Value>)>::new(); worker_count];
    let mut right_shards = vec![Vec::<(usize, Vec<Value>)>::new(); worker_count];

    for (left_index, values) in left.iter().enumerate() {
        if let Some(key) = normalized_join_key(values, left_keys, canonical_types)? {
            let shard = hash_join_partition_for(&key, worker_count);
            left_shards[shard].push((left_index, values.clone()));
        }
    }

    let left_padding = vec![Value::Null; left_width];
    for (right_index, values) in right.iter().enumerate() {
        let mut padded = left_padding.clone();
        padded.extend_from_slice(values);
        if let Some(key) = normalized_join_key(&padded, right_keys, canonical_types)? {
            let shard = hash_join_partition_for(&key, worker_count);
            right_shards[shard].push((right_index, values.clone()));
        }
    }

    let partials = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for (left_shard, right_shard) in left_shards.into_iter().zip(right_shards) {
            handles.push(
                scope.spawn(move || -> Result<Vec<(usize, Vec<Vec<Value>>)>> {
                    let mut table: HashMap<Vec<Value>, Vec<(usize, Vec<Value>)>> = HashMap::new();
                    for (right_index, right_row) in right_shard {
                        let mut padded = vec![Value::Null; left_width];
                        padded.extend_from_slice(&right_row);
                        if let Some(key) =
                            normalized_join_key(&padded, right_keys, canonical_types)?
                        {
                            table.entry(key).or_default().push((right_index, right_row));
                        }
                    }

                    let mut output = Vec::with_capacity(left_shard.len());
                    for (left_index, left_row) in left_shard {
                        let Some(key) = normalized_join_key(&left_row, left_keys, canonical_types)?
                        else {
                            continue;
                        };
                        let mut matches = Vec::new();
                        if let Some(candidates) = table.get(&key) {
                            for (_, right_row) in candidates {
                                let mut row = Vec::with_capacity(left_row.len() + right_row.len());
                                row.extend_from_slice(&left_row);
                                row.extend_from_slice(right_row);
                                if on.eval_predicate(&htap_sql::eval_context! {
                                    row: &row,
                                    current_outer_row: current_outer_row,
                                    aggregates: &[],
                                    output: None,
                                    subqueries: subqueries,
                                    variables: None,
                                    subquery_runner: None,
                                    subquery_budget: None,
                                })? {
                                    matches.push(row);
                                }
                            }
                        }
                        if !matches.is_empty() {
                            output.push((left_index, matches));
                        }
                    }
                    Ok(output)
                }),
            );
        }

        let mut partials = Vec::with_capacity(worker_count);
        for handle in handles {
            partials.push(
                handle.join().map_err(|_| {
                    HtapError::Internal("hash join worker thread panicked".into())
                })??,
            );
        }
        Ok::<_, HtapError>(partials)
    })?;

    let mut tagged = Vec::new();
    for partial in partials {
        tagged.extend(partial);
    }
    tagged.sort_by_key(|(left_index, _)| *left_index);
    Ok(tagged
        .into_iter()
        .flat_map(|(_, matches)| matches)
        .collect())
}

/// Joins the accumulated left rows with the right relation's rows.
fn join_rows(input: JoinRowsInput<'_>) -> Result<Vec<Vec<Value>>> {
    join_rows_inner(input, true)
}

fn join_rows_inner(input: JoinRowsInput<'_>, allow_spill: bool) -> Result<Vec<Vec<Value>>> {
    let JoinRowsInput {
        left,
        left_width,
        right,
        right_width,
        left_slots,
        right_slots,
        join,
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
        memory_budget,
        spill_root,
        parallelism,
        build_side,
    } = input;
    debug_assert!(left.iter().all(|row| row.len() == left_width));
    debug_assert!(right.iter().all(|row| row.len() == right_width));

    let null_right = vec![Value::Null; right_width];
    let mut out: Vec<(usize, Vec<Vec<Value>>)> = Vec::new();
    let combine = |l: &[Value], r: &[Value]| {
        let mut v = Vec::with_capacity(l.len() + r.len());
        v.extend_from_slice(l);
        v.extend_from_slice(r);
        v
    };

    let on = match (&join.on, join.kind) {
        (None, _) | (_, JoinKind::Cross) => {
            if right.len() >= HASH_JOIN_PARALLEL_THRESHOLD
                && parallelism > 1
                && !left.is_empty()
                && !right.is_empty()
            {
                return parallel_cross_join(&left, &right, parallelism);
            }
            for (left_index, left_row) in left.iter().enumerate() {
                let mut matches = Vec::with_capacity(right.len());
                for right_row in &right {
                    matches.push(combine(left_row, right_row));
                }
                if !matches.is_empty() {
                    out.push((left_index, matches));
                }
            }
            out.sort_by_key(|(left_index, _)| *left_index);
            return Ok(out.into_iter().flat_map(|(_, matches)| matches).collect());
        }
        (Some(on), _) => on,
    };

    // Split equi-conjuncts `left_expr = right_expr`.
    let mut left_keys: Vec<&Expr> = Vec::new();
    let mut right_keys: Vec<&Expr> = Vec::new();
    for c in conjuncts(on) {
        if let Expr::BinaryOp {
            op: BinOp::Eq,
            left: a,
            right: b,
        } = c
        {
            let (sa, sb) = (a.referenced_slots(), b.referenced_slots());
            let is_left = |slots: &[usize]| {
                !slots.is_empty() && slots.iter().all(|slot| left_slots.contains(slot))
            };
            let is_right = |slots: &[usize]| {
                !slots.is_empty() && slots.iter().all(|slot| right_slots.contains(slot))
            };
            if is_left(&sa) && is_right(&sb) {
                left_keys.push(a);
                right_keys.push(b);
            } else if is_right(&sa) && is_left(&sb) {
                left_keys.push(b);
                right_keys.push(a);
            }
        }
    }

    let eval_on = |row: &[Value]| -> Result<bool> {
        on.eval_predicate(&htap_sql::eval_context! {
            row: row,
            current_outer_row: current_outer_row,
            aggregates: &[],
            output: None,
            subqueries: subqueries,
            variables: variables,
            subquery_runner: subquery_runner,
            subquery_budget: subquery_budget,
        })
    };

    let mut left_matched = vec![false; left.len()];
    let mut right_matched = vec![false; right.len()];

    let canonical_types = join
        .equi_key_types
        .iter()
        .copied()
        .collect::<Option<Vec<_>>>();
    if left_keys.is_empty()
        || canonical_types
            .as_ref()
            .is_none_or(|types| types.len() != left_keys.len())
    {
        for (left_index, left_row) in left.iter().enumerate() {
            let mut matches = Vec::new();
            for (right_index, right_row) in right.iter().enumerate() {
                let row = combine(left_row, right_row);
                if eval_on(&row)? {
                    left_matched[left_index] = true;
                    right_matched[right_index] = true;
                    matches.push(row);
                }
            }
            if !matches.is_empty() {
                out.push((left_index, matches));
            }
        }
    } else {
        let canonical_types = canonical_types.expect("canonical key types checked above");

        // Workers receive no execution services. Inspect exactly the expressions they evaluate
        // instead of treating the presence of session services as a reason to disable parallelism.
        let can_parallelize = join.kind == JoinKind::Inner
            && right.len() >= HASH_JOIN_PARALLEL_THRESHOLD
            && parallelism > 1
            && !expr_requires_serial_worker_evaluation(on)
            && !left_keys
                .iter()
                .chain(right_keys.iter())
                .any(|expr| expr_requires_serial_worker_evaluation(expr));
        let execution_build_side = if can_parallelize {
            BuildSide::Right
        } else {
            build_side
        };
        let build_rows = match execution_build_side {
            BuildSide::Left => &left,
            BuildSide::Right => &right,
        };
        let build_bytes = estimate_join_build_bytes(build_rows);
        let _build_reservation = match memory_budget.try_reserve(build_bytes) {
            Ok(reservation) => reservation,
            Err(_) if allow_spill => {
                return spill_hash_join(SpillHashJoinInput {
                    left: &left,
                    left_width,
                    right: &right,
                    right_width,
                    left_slots,
                    right_slots,
                    join,
                    left_keys: &left_keys,
                    right_keys: &right_keys,
                    canonical_types: &canonical_types,
                    subqueries,
                    current_outer_row,
                    variables,
                    subquery_runner,
                    subquery_budget,
                    memory_budget,
                    spill_root,
                });
            }
            Err(error) => return Err(error),
        };
        if can_parallelize {
            let worker_count =
                parallelism.clamp(1, HASH_JOIN_MAX_SPILL_PARTITIONS.min(right.len().max(1)));
            record_parallel_workers(worker_count);
            return parallel_hash_join(ParallelHashJoinInput {
                left: &left,
                right: &right,
                left_width,
                left_keys: &left_keys,
                right_keys: &right_keys,
                canonical_types: &canonical_types,
                on,
                subqueries,
                current_outer_row,
                parallelism,
            });
        }

        match execution_build_side {
            BuildSide::Right => {
                let mut table: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
                let pad = vec![Value::Null; left_width];
                for (right_index, right_row) in right.iter().enumerate() {
                    let padded = combine(&pad, right_row);
                    if let Some(key) = normalized_join_key(&padded, &right_keys, &canonical_types)?
                    {
                        table.entry(key).or_default().push(right_index);
                    }
                }

                for (left_index, left_row) in left.iter().enumerate() {
                    let Some(key) = normalized_join_key(left_row, &left_keys, &canonical_types)?
                    else {
                        continue;
                    };

                    let mut matches = Vec::new();
                    if let Some(candidates) = table.get(&key) {
                        for &right_index in candidates {
                            let row = combine(left_row, &right[right_index]);
                            if eval_on(&row)? {
                                left_matched[left_index] = true;
                                right_matched[right_index] = true;
                                matches.push(row);
                            }
                        }
                    }
                    if !matches.is_empty() {
                        out.push((left_index, matches));
                    }
                }
            }
            BuildSide::Left => {
                let mut table: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
                for (left_index, left_row) in left.iter().enumerate() {
                    if let Some(key) = normalized_join_key(left_row, &left_keys, &canonical_types)?
                    {
                        table.entry(key).or_default().push(left_index);
                    }
                }

                let mut matches_by_left = vec![Vec::new(); left.len()];
                let pad = vec![Value::Null; left_width];
                for (right_index, right_row) in right.iter().enumerate() {
                    let padded = combine(&pad, right_row);
                    let Some(key) = normalized_join_key(&padded, &right_keys, &canonical_types)?
                    else {
                        continue;
                    };

                    if let Some(candidates) = table.get(&key) {
                        for &left_index in candidates {
                            let row = combine(&left[left_index], right_row);
                            if eval_on(&row)? {
                                left_matched[left_index] = true;
                                right_matched[right_index] = true;
                                matches_by_left[left_index].push(row);
                            }
                        }
                    }
                }

                out.extend(
                    matches_by_left
                        .into_iter()
                        .enumerate()
                        .filter(|(_, matches)| !matches.is_empty()),
                );
            }
        }
    }

    match join.kind {
        JoinKind::Left => {
            for (left_index, left_row) in left.iter().enumerate() {
                if !left_matched[left_index] {
                    out.push((left_index, vec![combine(left_row, &null_right)]));
                }
            }
        }
        JoinKind::Right => {
            let null_left = vec![Value::Null; left_width];
            let mut unmatched_index = left.len();
            for (right_index, right_row) in right.iter().enumerate() {
                if !right_matched[right_index] {
                    out.push((unmatched_index, vec![combine(&null_left, right_row)]));
                    unmatched_index += 1;
                }
            }
        }
        JoinKind::Full => {
            for (left_index, left_row) in left.iter().enumerate() {
                if !left_matched[left_index] {
                    out.push((left_index, vec![combine(left_row, &null_right)]));
                }
            }

            let null_left = vec![Value::Null; left_width];
            let mut unmatched_index = left.len();
            for (right_index, right_row) in right.iter().enumerate() {
                if !right_matched[right_index] {
                    out.push((unmatched_index, vec![combine(&null_left, right_row)]));
                    unmatched_index += 1;
                }
            }
        }
        JoinKind::Inner | JoinKind::Cross => {}
    }

    out.sort_by_key(|(left_index, _)| *left_index);
    Ok(out.into_iter().flat_map(|(_, matches)| matches).collect())
}

/// Normalizes both sides of an equi-join key to the binder-selected numeric type.
fn normalize_key(value: Value, canonical_type: DataType) -> Result<Value> {
    match canonical_type {
        DataType::Int64 => match value {
            Value::Int32(value) => Ok(Value::Int64(value as i64)),
            Value::Int64(value) | Value::Timestamp(value) => Ok(Value::Int64(value)),
            other => cast_value(other, DataType::Int64),
        },
        DataType::Float64 => match value {
            Value::Int32(value) => Ok(Value::Float64(value as f64)),
            Value::Int64(value) | Value::Timestamp(value) => Ok(Value::Float64(value as f64)),
            Value::Float64(value) => Ok(Value::Float64(value)),
            other => cast_value(other, DataType::Float64),
        },
        other => Err(HtapError::Internal(format!(
            "unsupported canonical join key type {}",
            other.name()
        ))),
    }
}

/// Running state of one aggregate within a group.
#[derive(Debug)]
enum AggState {
    Count(i64),
    SumInt(Option<i64>),
    SumFloat(Option<f64>),
    Avg { sum: f64, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
    Distinct(BTreeSet<Value>),
}

impl AggState {
    fn new(spec: &AggregateSpec) -> Self {
        if spec.distinct {
            return AggState::Distinct(BTreeSet::new());
        }
        match spec.func {
            AggFn::CountStar | AggFn::Count => AggState::Count(0),
            AggFn::Sum => {
                if spec.data_type == DataType::Float64 {
                    AggState::SumFloat(None)
                } else {
                    AggState::SumInt(None)
                }
            }
            AggFn::Avg => AggState::Avg { sum: 0.0, count: 0 },
            AggFn::Min => AggState::Min(None),
            AggFn::Max => AggState::Max(None),
        }
    }

    fn estimated_bytes(&self) -> usize {
        match self {
            AggState::Count(_)
            | AggState::SumInt(_)
            | AggState::SumFloat(_)
            | AggState::Avg { .. } => std::mem::size_of::<Self>(),
            AggState::Min(value) | AggState::Max(value) => {
                std::mem::size_of::<Self>()
                    + value
                        .as_ref()
                        .map(MemoryBudget::estimate_value_bytes)
                        .unwrap_or(0)
            }
            AggState::Distinct(values) => {
                std::mem::size_of::<Self>()
                    + values
                        .iter()
                        .map(MemoryBudget::estimate_value_bytes)
                        .sum::<usize>()
            }
        }
    }

    fn accumulate(&mut self, spec: &AggregateSpec, v: Value) -> Result<()> {
        if v.is_null() && spec.func != AggFn::CountStar {
            return Ok(());
        }
        match self {
            AggState::Count(n) => *n += 1,
            AggState::SumInt(acc) => {
                let i = to_i64(&v)?;
                *acc = Some(match acc {
                    Some(a) => a
                        .checked_add(i)
                        .ok_or_else(|| HtapError::InvalidArgument("SUM overflow".into()))?,
                    None => i,
                });
            }
            AggState::SumFloat(acc) => {
                let f = to_f64(&v)?;
                let next = acc.unwrap_or(0.0) + f;
                if !next.is_finite() {
                    return Err(HtapError::InvalidArgument(
                        "DOUBLE value is out of range in 'SUM'".into(),
                    ));
                }
                *acc = Some(next);
            }
            AggState::Avg { sum, count } => {
                *sum += to_f64(&v)?;
                if !sum.is_finite() {
                    return Err(HtapError::InvalidArgument(
                        "DOUBLE value is out of range in 'AVG'".into(),
                    ));
                }
                *count += 1;
            }
            AggState::Min(cur) => {
                let replace = match cur {
                    Some(c) => matches!(compare(&v, c)?, Some(std::cmp::Ordering::Less)),
                    None => true,
                };
                if replace {
                    *cur = Some(v);
                }
            }
            AggState::Max(cur) => {
                let replace = match cur {
                    Some(c) => matches!(compare(&v, c)?, Some(std::cmp::Ordering::Greater)),
                    None => true,
                };
                if replace {
                    *cur = Some(v);
                }
            }
            AggState::Distinct(set) => {
                set.insert(v);
            }
        }
        Ok(())
    }

    fn merge(&mut self, spec: &AggregateSpec, other: AggState) -> Result<()> {
        match (self, other) {
            (AggState::Count(target), AggState::Count(source)) => {
                *target = target
                    .checked_add(source)
                    .ok_or_else(|| HtapError::InvalidArgument("COUNT overflow".into()))?;
            }
            (AggState::SumInt(target), AggState::SumInt(source)) => {
                if let Some(source) = source {
                    *target = Some(match *target {
                        Some(current) => current
                            .checked_add(source)
                            .ok_or_else(|| HtapError::InvalidArgument("SUM overflow".into()))?,
                        None => source,
                    });
                }
            }
            (AggState::SumFloat(target), AggState::SumFloat(source)) => {
                if let Some(source) = source {
                    let merged = target.unwrap_or(0.0) + source;
                    if !merged.is_finite() {
                        return Err(HtapError::InvalidArgument(
                            "DOUBLE value is out of range in 'SUM'".into(),
                        ));
                    }
                    *target = Some(merged);
                }
            }
            (
                AggState::Avg {
                    sum: target_sum,
                    count: target_count,
                },
                AggState::Avg {
                    sum: source_sum,
                    count: source_count,
                },
            ) => {
                let merged_sum = *target_sum + source_sum;
                if !merged_sum.is_finite() {
                    return Err(HtapError::InvalidArgument(
                        "DOUBLE value is out of range in 'AVG'".into(),
                    ));
                }
                *target_sum = merged_sum;
                *target_count = target_count
                    .checked_add(source_count)
                    .ok_or_else(|| HtapError::InvalidArgument("AVG count overflow".into()))?;
            }
            (AggState::Min(target), AggState::Min(source)) => {
                if let Some(value) = source {
                    let replace = match target {
                        Some(current) => {
                            matches!(compare(&value, current)?, Some(std::cmp::Ordering::Less))
                        }
                        None => true,
                    };
                    if replace {
                        *target = Some(value);
                    }
                }
            }
            (AggState::Max(target), AggState::Max(source)) => {
                if let Some(value) = source {
                    let replace = match target {
                        Some(current) => {
                            matches!(compare(&value, current)?, Some(std::cmp::Ordering::Greater))
                        }
                        None => true,
                    };
                    if replace {
                        *target = Some(value);
                    }
                }
            }
            (AggState::Distinct(target), AggState::Distinct(source)) => {
                target.extend(source);
            }
            _ => {
                return Err(HtapError::Internal(format!(
                    "cannot merge incompatible states for aggregate {}",
                    spec.name
                )));
            }
        }
        Ok(())
    }

    fn finish(&self, spec: &AggregateSpec) -> Result<Value> {
        Ok(match self {
            AggState::Count(n) => Value::Int64(*n),
            AggState::SumInt(acc) => acc.map(Value::Int64).unwrap_or(Value::Null),
            AggState::SumFloat(acc) => acc.map(Value::Float64).unwrap_or(Value::Null),
            AggState::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Float64(sum / *count as f64)
                }
            }
            AggState::Min(v) | AggState::Max(v) => v.clone().unwrap_or(Value::Null),
            AggState::Distinct(set) => {
                // Re-run the plain aggregate over the distinct values.
                let plain = AggregateSpec {
                    distinct: false,
                    ..spec.clone()
                };
                let mut inner = AggState::new(&plain);
                for v in set {
                    inner.accumulate(&plain, v.clone())?;
                }
                inner.finish(&plain)?
            }
        })
    }
}

fn to_i64(v: &Value) -> Result<i64> {
    match v {
        Value::Int32(i) => Ok(*i as i64),
        Value::Int64(i) | Value::Timestamp(i) => Ok(*i),
        Value::Float64(f) => Ok(*f as i64),
        other => Err(HtapError::InvalidArgument(format!(
            "cannot sum non-numeric value {other}"
        ))),
    }
}

fn to_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int32(i) => Ok(*i as f64),
        Value::Int64(i) | Value::Timestamp(i) => Ok(*i as f64),
        Value::Float64(f) => Ok(*f),
        other => Err(HtapError::InvalidArgument(format!(
            "cannot average non-numeric value {other}"
        ))),
    }
}

#[cfg(test)]
mod memory_regression_tests {
    use super::*;

    #[test]
    fn value_byte_estimate_accounts_for_vector_and_values() {
        let values = vec![Value::Int64(1), Value::Int64(2)];
        let expected = std::mem::size_of::<Vec<Value>>()
            + values
                .iter()
                .map(MemoryBudget::estimate_value_bytes)
                .sum::<usize>();

        assert_eq!(estimate_values_bytes(&values), expected);
    }

    #[test]
    fn keyed_byte_estimate_accounts_for_output_and_sort_keys() {
        let row = Keyed {
            output: vec![Value::Int64(1)],
            keys: vec![Value::Int64(2), Value::Int64(3)],
        };
        let expected = std::mem::size_of::<Keyed>()
            + estimate_values_bytes(&row.output)
            + estimate_values_bytes(&row.keys);

        assert_eq!(estimate_keyed_bytes(&row), expected);
    }

    #[test]
    fn join_build_estimate_accounts_for_hash_table_row_metadata() {
        let rows = vec![vec![Value::Int64(1)], vec![Value::Int64(2)]];
        let expected = rows
            .iter()
            .map(|values| {
                MemoryBudget::estimate_row_bytes(&Row::new(values.clone()))
                    .saturating_add(std::mem::size_of::<Vec<Value>>())
                    .saturating_add(std::mem::size_of::<usize>())
            })
            .sum::<usize>();

        assert_eq!(estimate_join_build_bytes(&rows), expected);
    }

    #[test]
    fn distinct_aggregate_state_estimate_grows_with_values() {
        let mut state = AggState::Distinct(BTreeSet::new());
        let empty_bytes = state.estimated_bytes();

        state
            .accumulate(
                &AggregateSpec {
                    func: AggFn::Count,
                    distinct: true,
                    arg: None,
                    data_type: DataType::Int64,
                    nullable: false,
                    name: "COUNT(DISTINCT ...)".into(),
                },
                Value::Int64(42),
            )
            .unwrap();

        assert!(
            state.estimated_bytes() > empty_bytes,
            "distinct aggregate memory estimate must include retained values"
        );
    }

    #[test]
    fn growing_distinct_group_charges_only_incremental_state_bytes() {
        let spec = AggregateSpec {
            func: AggFn::Count,
            distinct: true,
            arg: None,
            data_type: DataType::Int64,
            nullable: false,
            name: "COUNT(DISTINCT value)".into(),
        };
        let key = vec![Value::Int64(1)];
        let representative = vec![Value::Int64(1)];

        let initial_states = vec![AggState::new(&spec)];
        let initial_state_bytes = initial_states
            .iter()
            .map(AggState::estimated_bytes)
            .sum::<usize>();
        let initial_bytes = estimate_group_entry_bytes(&key, &representative, &initial_states);

        // Compute the exact final group charge before creating the budget. The successful case
        // has one byte spare, so charging each resize with the full new state must exceed it.
        let mut final_states = initial_states;
        for value in [
            Value::String("first".into()),
            Value::String("second".into()),
        ] {
            final_states[0].accumulate(&spec, value).unwrap();
        }
        let final_state_bytes = final_states
            .iter()
            .map(AggState::estimated_bytes)
            .sum::<usize>();
        let final_group_bytes = initial_bytes + (final_state_bytes - initial_state_bytes);

        let grow_group = |budget_bytes: usize| -> Result<AggregateGroup> {
            let memory_budget = Arc::new(MemoryBudget::new(budget_bytes));
            let states = vec![AggState::new(&spec)];
            let reservation = memory_budget.try_reserve(initial_bytes)?;
            let mut group = AggregateGroup {
                representative: representative.clone(),
                representative_index: 0,
                states,
                reserved_bytes: initial_bytes,
                reservations: vec![reservation],
            };

            for value in [
                Value::String("first".into()),
                Value::String("second".into()),
            ] {
                let old_state_bytes = group
                    .states
                    .iter()
                    .map(AggState::estimated_bytes)
                    .sum::<usize>();
                group.states[0].accumulate(&spec, value)?;
                resize_group_reservation(&mut group, old_state_bytes, &memory_budget)?;
            }
            Ok(group)
        };

        let group = grow_group(final_group_bytes + 1)
            .expect("a group whose final retained state fits the budget must be accepted");
        assert_eq!(
            group.reserved_bytes, final_group_bytes,
            "the group reservation must equal its entry plus final state size, not the sum of \
             every intermediate state size"
        );

        let error = grow_group(final_group_bytes - 1)
            .expect_err("a group whose final retained state exceeds the budget must be refused");
        assert!(
            memory_budget_exceeded(&error),
            "expected a memory-budget error, got {error}"
        );
    }
}
