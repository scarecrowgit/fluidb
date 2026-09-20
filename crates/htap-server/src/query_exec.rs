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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use htap_catalog::{CatalogSnapshot, TableDescriptor};
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_rowstore::Snapshot;
use htap_sql::ast::{AnalyticFilter, ComparisonOp};
use htap_sql::expr::{
    cast_value, compare, AggFn, AggregateSpec, BinOp, EvalContext, Expr, SubqueryBudget,
    SubqueryRunner, VariableLookup,
};
use htap_sql::query::{
    BoundQuery, JoinKind, JoinSpec, JoinTree, OrderItem, PeerFrameBound, QueryBody, RowFrameBound,
    SelectBody, SetOpKind, TableSlot, ValueFrameBound, WindowFrame, WindowFrameDirection,
    WindowFunctionKind,
};
use htap_sql::result::StatementResult;

use crate::session::WriteSet;
use crate::{olap, scan_partition_compact, LocalServer};

/// Per-statement execution context.
pub(crate) struct ExecContext<'a> {
    pub server: &'a LocalServer,
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
}

/// Executes a bound query and returns its result set.
pub(crate) fn execute_query(
    server: &LocalServer,
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
    snapshot: Snapshot,
    write_set: Option<&WriteSet>,
    variables: Option<&dyn VariableLookup>,
) -> Result<StatementResult> {
    let ctx = ExecContext {
        server,
        catalog,
        snapshot,
        write_set,
        variables,
        working_rows: None,
        working_width: 0,
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
    right_slot_start: usize,
    right_slot_end: usize,
    join: &'a JoinSpec,
    subqueries: &'a [Vec<Row>],
    current_outer_row: Option<&'a [Value]>,
    variables: Option<&'a dyn VariableLookup>,
    subquery_runner: Option<&'a dyn SubqueryRunner>,
    subquery_budget: Option<&'a SubqueryBudget>,
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

struct CorrelatedRunner<'a> {
    ctx: &'a ExecContext<'a>,
    subqueries: &'a [BoundQuery],
    budget: &'a SubqueryBudget,
}

impl SubqueryRunner for CorrelatedRunner<'_> {
    fn run(&self, index: usize, outer_row: &[Value]) -> Result<Vec<Row>> {
        let query = self
            .subqueries
            .get(index)
            .ok_or_else(|| HtapError::Internal(format!("subquery {index} not available")))?;
        if query.correlated {
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
    let runner = CorrelatedRunner {
        ctx,
        subqueries: &query.subqueries,
        budget,
    };

    let mut keyed = match &query.body {
        QueryBody::Select(sel) => run_select(
            ctx,
            sel,
            &query.subqueries,
            &query.order_by,
            &subqueries,
            current_outer_row,
            Some(&runner),
            Some(budget),
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
            let rows = match *kind {
                SetOpKind::UnionAll => {
                    let mut rows = left_rows;
                    rows.extend(right_rows);
                    rows
                }
                SetOpKind::UnionDistinct => {
                    let mut rows = left_rows;
                    rows.extend(right_rows);
                    dedup_rows(rows)
                }
                SetOpKind::ExceptAll
                | SetOpKind::ExceptDistinct
                | SetOpKind::IntersectAll
                | SetOpKind::IntersectDistinct => {
                    let mut left_counts: BTreeMap<Vec<Value>, usize> = BTreeMap::new();
                    let mut left_order = Vec::new();
                    for row in left_rows {
                        let count = left_counts.entry(row.clone()).or_insert(0);
                        if *count == 0 {
                            left_order.push(row.clone());
                        }
                        *count += 1;
                    }
                    let mut right_counts: BTreeMap<Vec<Value>, usize> = BTreeMap::new();
                    for row in right_rows {
                        *right_counts.entry(row).or_insert(0) += 1;
                    }

                    let mut rows = Vec::new();
                    for row in left_order {
                        let left_count = left_counts[&row];
                        let right_count = right_counts.get(&row).copied().unwrap_or(0);
                        let copies = match *kind {
                            SetOpKind::ExceptDistinct => usize::from(right_count == 0),
                            SetOpKind::ExceptAll => left_count.saturating_sub(right_count),
                            SetOpKind::IntersectDistinct => usize::from(right_count > 0),
                            SetOpKind::IntersectAll => left_count.min(right_count),
                            SetOpKind::UnionAll | SetOpKind::UnionDistinct => unreachable!(),
                        };
                        for _ in 0..copies {
                            rows.push(row.clone());
                        }
                    }
                    rows
                }
            };
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
        sort_keyed(&mut keyed, &query.order_by);
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
    let ctx = EvalContext {
        row: &[],
        current_outer_row: None,
        aggregates: &[],
        output: Some(output),
        subqueries,
        variables,
        subquery_runner: None,
        subquery_budget: None,
    };
    order_by.iter().map(|o| o.expr.eval(&ctx)).collect()
}

fn dedup_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut seen: BTreeSet<Vec<Value>> = BTreeSet::new();
    rows.into_iter()
        .filter(|r| seen.insert(r.clone()))
        .collect()
}

fn sort_keyed(rows: &mut [Keyed], order_by: &[OrderItem]) {
    rows.sort_by(|a, b| {
        for (i, item) in order_by.iter().enumerate() {
            let (ka, kb) = (&a.keys[i], &b.keys[i]);
            let ord = match (ka.is_null(), kb.is_null()) {
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
                    let o = compare(ka, kb).ok().flatten().unwrap_or_else(|| ka.cmp(kb));
                    if item.asc {
                        o
                    } else {
                        o.reverse()
                    }
                }
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        a.output.cmp(&b.output)
    });
}

/// Executes one select block, returning projected rows with their sort keys.
#[allow(clippy::too_many_arguments)]
fn run_select(
    ctx: &ExecContext<'_>,
    sel: &SelectBody,
    bound_subqueries: &[BoundQuery],
    order_by: &[OrderItem],
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<Vec<Keyed>> {
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

    for j in &sel.joins {
        if let Some(on) = &j.on {
            note(on);
        }
    }
    if let Some(tree) = &sel.join_tree {
        note_join_tree_on(tree, &mut note);
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
    if sel.tree_only {
        if let Some(tree) = &sel.join_tree {
            mark_tree_null_supplying(tree, false, &mut null_supplying);
        }
    } else {
        for j in &sel.joins {
            match j.kind {
                JoinKind::Left => null_supplying[j.right_slot] = true,
                JoinKind::Right => null_supplying[..j.right_slot]
                    .iter_mut()
                    .for_each(|n| *n = true),
                JoinKind::Full => {
                    null_supplying[..j.right_slot]
                        .iter_mut()
                        .for_each(|n| *n = true);
                    null_supplying[j.right_slot] = true;
                }
                JoinKind::Inner | JoinKind::Cross => {}
            }
        }
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

    // 3. Joins. Tree-only bodies retain their recursive structure; lowerable bodies keep the
    // existing left-deep path unchanged.
    let mut current: Vec<Vec<Value>> = if sel.tree_only {
        match &sel.join_tree {
            Some(tree) => evaluate_join_tree(
                sel,
                tree,
                &mut slot_rows,
                subqueries,
                current_outer_row,
                ctx.variables,
                subquery_runner,
                subquery_budget,
            )?,
            None => vec![Vec::new()],
        }
    } else {
        let mut current = match slot_rows.first() {
            Some(_) => slot_rows.remove(0),
            None => vec![Vec::new()], // FROM-less select: one empty row.
        };
        for (j, join) in sel.joins.iter().enumerate() {
            let right = std::mem::take(&mut slot_rows[0]);
            slot_rows.remove(0);
            let right_offset = sel.slot_offset(join.right_slot);
            let right_width = sel.slots[join.right_slot].width();
            debug_assert_eq!(j + 1, join.right_slot);
            current = join_rows(JoinRowsInput {
                left: current,
                left_width: right_offset,
                right,
                right_width,
                right_slot_start: join.right_slot,
                right_slot_end: join.right_slot + 1,
                join,
                subqueries,
                current_outer_row,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
            })?;
        }
        current
    };

    // 4. WHERE.
    if let Some(filter) = &sel.filter {
        let mut kept = Vec::with_capacity(current.len());
        for row in current {
            let c = EvalContext {
                row: &row,
                current_outer_row,
                aggregates: &[],
                output: None,
                subqueries,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
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
        let mut groups: BTreeMap<Vec<Value>, (Vec<Value>, Vec<AggState>)> = BTreeMap::new();
        for row in current {
            let c = EvalContext {
                row: &row,
                current_outer_row,
                aggregates: &[],
                output: None,
                subqueries,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
            };
            let key: Vec<Value> = sel
                .group_by
                .iter()
                .map(|g| g.eval(&c))
                .collect::<Result<_>>()?;
            let entry = groups.entry(key).or_insert_with(|| {
                (
                    row.clone(),
                    sel.aggregates.iter().map(AggState::new).collect(),
                )
            });
            for (state, spec) in entry.1.iter_mut().zip(sel.aggregates.iter()) {
                let value = match &spec.arg {
                    Some(arg) => arg.eval(&c)?,
                    None => Value::Int64(1),
                };
                state.accumulate(spec, value)?;
            }
        }

        if groups.is_empty() && sel.group_by.is_empty() {
            let empty_row = vec![Value::Null; sel.row_width()];
            groups.insert(
                Vec::new(),
                (
                    empty_row,
                    sel.aggregates.iter().map(AggState::new).collect(),
                ),
            );
        }

        for (_, (row, states)) in groups {
            let aggregates = states
                .iter()
                .zip(sel.aggregates.iter())
                .map(|(state, spec)| state.finish(spec))
                .collect::<Result<Vec<_>>>()?;
            window_inputs.push((row, aggregates));
        }
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
            let projection_ctx = EvalContext {
                row: &row,
                current_outer_row,
                aggregates: &aggregates,
                output: None,
                subqueries,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
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

            let having_ctx = EvalContext {
                row: &row,
                current_outer_row,
                aggregates: &aggregates,
                output: Some(&output),
                subqueries,
                variables: ctx.variables,
                subquery_runner,
                subquery_budget,
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
        subqueries,
        current_outer_row,
        ctx.variables,
        subquery_runner,
        subquery_budget,
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
            subqueries,
            current_outer_row,
            ctx.variables,
            subquery_runner,
            subquery_budget,
        )?);
    }

    // 10. DISTINCT.
    if sel.distinct {
        let mut seen: BTreeSet<Vec<Value>> = BTreeSet::new();
        produced.retain(|k| seen.insert(k.output.clone()));
    }
    Ok(produced)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_windows(
    sel: &SelectBody,
    inputs: &[(Vec<Value>, Vec<Value>)],
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<Vec<Vec<Value>>> {
    let mut results = vec![vec![Value::Null; sel.windows.len()]; inputs.len()];
    for (window_index, window) in sel.windows.iter().enumerate() {
        let mut partitions: BTreeMap<Vec<Value>, Vec<usize>> = BTreeMap::new();
        let mut order_keys = vec![Vec::new(); inputs.len()];

        for (input_index, (row, aggregates)) in inputs.iter().enumerate() {
            let eval = EvalContext {
                row,
                current_outer_row,
                aggregates,
                output: None,
                subqueries,
                variables,
                subquery_runner,
                subquery_budget,
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
                        let buckets = window_arg_u64(
                            window,
                            0,
                            inputs,
                            input_index,
                            subqueries,
                            current_outer_row,
                            variables,
                            subquery_runner,
                            subquery_budget,
                        )?;
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
                            window_arg_u64(
                                window,
                                1,
                                inputs,
                                input_index,
                                subqueries,
                                current_outer_row,
                                variables,
                                subquery_runner,
                                subquery_budget,
                            )?
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
                                eval_window_arg(
                                    window,
                                    0,
                                    inputs,
                                    target_index,
                                    subqueries,
                                    current_outer_row,
                                    variables,
                                    subquery_runner,
                                    subquery_budget,
                                )?
                            }
                            None if window.args.len() >= 3 => eval_window_arg(
                                window,
                                2,
                                inputs,
                                input_index,
                                subqueries,
                                current_outer_row,
                                variables,
                                subquery_runner,
                                subquery_budget,
                            )?,
                            None => Value::Null,
                        }
                    }
                    WindowFunctionKind::FirstValue | WindowFunctionKind::LastValue => {
                        let frame = window_frame_range(
                            window,
                            partition,
                            position,
                            &order_keys,
                            inputs,
                            input_index,
                            subqueries,
                            current_outer_row,
                            variables,
                            subquery_runner,
                            subquery_budget,
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
                                    inputs,
                                    partition[frame_position],
                                    subqueries,
                                    current_outer_row,
                                    variables,
                                    subquery_runner,
                                    subquery_budget,
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
                            inputs,
                            input_index,
                            subqueries,
                            current_outer_row,
                            variables,
                            subquery_runner,
                            subquery_budget,
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
                                    eval_window_arg(
                                        window,
                                        0,
                                        inputs,
                                        frame_input_index,
                                        subqueries,
                                        current_outer_row,
                                        variables,
                                        subquery_runner,
                                        subquery_budget,
                                    )?
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

#[allow(clippy::too_many_arguments)]
fn window_frame_range(
    window: &htap_sql::query::WindowSpec,
    partition: &[usize],
    position: usize,
    order_keys: &[Vec<Value>],
    inputs: &[(Vec<Value>, Vec<Value>)],
    input_index: usize,
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
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
                        let (row, aggregates) = &inputs[input_index];
                        let offset = value.eval(&EvalContext {
                            row,
                            current_outer_row,
                            aggregates,
                            output: None,
                            subqueries,
                            variables,
                            subquery_runner,
                            subquery_budget,
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

#[allow(clippy::too_many_arguments)]
fn eval_window_arg(
    window: &htap_sql::query::WindowSpec,
    argument: usize,
    inputs: &[(Vec<Value>, Vec<Value>)],
    input_index: usize,
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<Value> {
    let (row, aggregates) = &inputs[input_index];
    window.args[argument].eval(&EvalContext {
        row,
        current_outer_row,
        aggregates,
        output: None,
        subqueries,
        variables,
        subquery_runner,
        subquery_budget,
    })
}

#[allow(clippy::too_many_arguments)]
fn window_arg_u64(
    window: &htap_sql::query::WindowSpec,
    argument: usize,
    inputs: &[(Vec<Value>, Vec<Value>)],
    input_index: usize,
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<u64> {
    match eval_window_arg(
        window,
        argument,
        inputs,
        input_index,
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
    )? {
        Value::Int32(value) if value >= 0 => Ok(value as u64),
        Value::Int64(value) if value >= 0 => Ok(value as u64),
        value => Err(HtapError::InvalidArgument(format!(
            "window offset or bucket count must be a non-negative integer, got {value}"
        ))),
    }
}

/// Projects one input row (or group) and computes top-level sort keys.
#[allow(clippy::too_many_arguments)]
fn project_row(
    sel: &SelectBody,
    order_by: &[OrderItem],
    row: &[Value],
    aggregates: &[Value],
    windows: &[Value],
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<Keyed> {
    let c = EvalContext {
        row,
        current_outer_row,
        aggregates,
        output: Some(windows),
        subqueries,
        variables,
        subquery_runner,
        subquery_budget,
    };
    let output: Vec<Value> = sel
        .projection
        .iter()
        .map(|p| p.expr.eval(&c))
        .collect::<Result<_>>()?;
    let c = EvalContext {
        row,
        current_outer_row,
        aggregates,
        output: Some(&output),
        subqueries,
        variables,
        subquery_runner,
        subquery_budget,
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
fn tree_slot_range(tree: &JoinTree) -> (usize, usize) {
    match tree {
        JoinTree::Leaf(slot) => (*slot, *slot),
        JoinTree::Join { left, right, .. } => {
            let (first, _) = tree_slot_range(left);
            let (_, last) = tree_slot_range(right);
            (first, last)
        }
    }
}

/// Evaluates a nested join subtree bottom-up. Each result row is the concatenation of its
/// leaves' full-width slot rows, which is also the global slot order at the root.
#[allow(clippy::too_many_arguments)]
fn evaluate_join_tree(
    sel: &SelectBody,
    tree: &JoinTree,
    slot_rows: &mut [Vec<Vec<Value>>],
    subqueries: &[Vec<Row>],
    current_outer_row: Option<&[Value]>,
    variables: Option<&dyn VariableLookup>,
    subquery_runner: Option<&dyn SubqueryRunner>,
    subquery_budget: Option<&SubqueryBudget>,
) -> Result<Vec<Vec<Value>>> {
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
                subqueries,
                current_outer_row,
                variables,
                subquery_runner,
                subquery_budget,
            )?;
            let right_rows = evaluate_join_tree(
                sel,
                right,
                slot_rows,
                subqueries,
                current_outer_row,
                variables,
                subquery_runner,
                subquery_budget,
            )?;
            let (left_first, left_last) = tree_slot_range(left);
            let (right_first, right_last) = tree_slot_range(right);
            debug_assert_eq!(left_last + 1, right_first);

            let left_width = sel.slots[left_first..=left_last]
                .iter()
                .map(TableSlot::width)
                .sum();
            let right_width = sel.slots[right_first..=right_last]
                .iter()
                .map(TableSlot::width)
                .sum();
            let equi_key_types = join_equi_key_types(on.as_ref(), right_first, right_last + 1);
            let join = JoinSpec {
                kind: *kind,
                right_slot: right_first,
                on: on.clone(),
                equi_key_types,
            };
            join_rows(JoinRowsInput {
                left: left_rows,
                left_width,
                right: right_rows,
                right_width,
                right_slot_start: right_first,
                right_slot_end: right_last + 1,
                join: &join,
                subqueries,
                current_outer_row,
                variables,
                subquery_runner,
                subquery_budget,
            })
        }
    }
}

fn join_equi_key_types(
    on: Option<&Expr>,
    right_slot_start: usize,
    right_slot_end: usize,
) -> Vec<Option<DataType>> {
    let Some(on) = on else {
        return Vec::new();
    };
    let is_left =
        |slots: &[usize]| !slots.is_empty() && slots.iter().all(|slot| *slot < right_slot_start);
    let is_right = |slots: &[usize]| {
        !slots.is_empty()
            && slots
                .iter()
                .all(|slot| *slot >= right_slot_start && *slot < right_slot_end)
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

/// Joins the accumulated left rows with the right relation's rows.
fn join_rows(input: JoinRowsInput<'_>) -> Result<Vec<Vec<Value>>> {
    let JoinRowsInput {
        left,
        left_width,
        right,
        right_width,
        right_slot_start,
        right_slot_end,
        join,
        subqueries,
        current_outer_row,
        variables,
        subquery_runner,
        subquery_budget,
    } = input;
    debug_assert!(left.iter().all(|row| row.len() == left_width));
    debug_assert!(right.iter().all(|row| row.len() == right_width));

    let null_right = vec![Value::Null; right_width];
    let mut out = Vec::new();
    let combine = |l: &[Value], r: &[Value]| {
        let mut v = Vec::with_capacity(l.len() + r.len());
        v.extend_from_slice(l);
        v.extend_from_slice(r);
        v
    };

    let on = match (&join.on, join.kind) {
        (None, _) | (_, JoinKind::Cross) => {
            for l in &left {
                for r in &right {
                    out.push(combine(l, r));
                }
            }
            return Ok(out);
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
            let is_left = |s: &[usize]| !s.is_empty() && s.iter().all(|&x| x < right_slot_start);
            let is_right = |s: &[usize]| {
                !s.is_empty()
                    && s.iter()
                        .all(|&x| x >= right_slot_start && x < right_slot_end)
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
        on.eval_predicate(&EvalContext {
            row,
            current_outer_row,
            aggregates: &[],
            output: None,
            subqueries,
            variables,
            subquery_runner,
            subquery_budget,
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
        for (li, l) in left.iter().enumerate() {
            for (ri, r) in right.iter().enumerate() {
                let row = combine(l, r);
                if eval_on(&row)? {
                    left_matched[li] = true;
                    right_matched[ri] = true;
                    out.push(row);
                }
            }
        }
    } else {
        let canonical_types = canonical_types.expect("canonical key types checked above");
        let mut table: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
        let pad = vec![Value::Null; left_width];
        for (ri, r) in right.iter().enumerate() {
            let padded = combine(&pad, r);
            let c = EvalContext::row_only(&padded);
            let mut key = Vec::with_capacity(right_keys.len());
            let mut has_null = false;
            for (k, canonical_type) in right_keys.iter().zip(&canonical_types) {
                let v = k.eval(&c)?;
                if v.is_null() {
                    has_null = true;
                    break;
                }
                key.push(normalize_key(v, *canonical_type)?);
            }
            if !has_null {
                table.entry(key).or_default().push(ri);
            }
        }
        for (li, l) in left.iter().enumerate() {
            let c = EvalContext::row_only(l);
            let mut key = Vec::with_capacity(left_keys.len());
            let mut has_null = false;
            for (k, canonical_type) in left_keys.iter().zip(&canonical_types) {
                let v = k.eval(&c)?;
                if v.is_null() {
                    has_null = true;
                    break;
                }
                key.push(normalize_key(v, *canonical_type)?);
            }
            if has_null {
                continue;
            }

            if let Some(candidates) = table.get(&key) {
                for &ri in candidates {
                    let row = combine(l, &right[ri]);
                    if eval_on(&row)? {
                        left_matched[li] = true;
                        right_matched[ri] = true;
                        out.push(row);
                    }
                }
            }
        }
    }

    match join.kind {
        JoinKind::Left => {
            for (li, l) in left.iter().enumerate() {
                if !left_matched[li] {
                    out.push(combine(l, &null_right));
                }
            }
        }
        JoinKind::Right => {
            let null_left = vec![Value::Null; left_width];
            for (ri, r) in right.iter().enumerate() {
                if !right_matched[ri] {
                    out.push(combine(&null_left, r));
                }
            }
        }
        JoinKind::Full => {
            for (li, l) in left.iter().enumerate() {
                if !left_matched[li] {
                    out.push(combine(l, &null_right));
                }
            }
            let null_left = vec![Value::Null; left_width];
            for (ri, r) in right.iter().enumerate() {
                if !right_matched[ri] {
                    out.push(combine(&null_left, r));
                }
            }
        }
        JoinKind::Inner | JoinKind::Cross => {}
    }
    Ok(out)
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
                        "SUM produced a non-finite value".into(),
                    ));
                }
                *acc = Some(next);
            }
            AggState::Avg { sum, count } => {
                *sum += to_f64(&v)?;
                if !sum.is_finite() {
                    return Err(HtapError::InvalidArgument(
                        "AVG produced a non-finite value".into(),
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
