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
use htap_common::types::{DataType, Row, Value};
use htap_rowstore::Snapshot;
use htap_sql::ast::{AnalyticFilter, ComparisonOp};
use htap_sql::expr::{cast_value, compare, AggFn, AggregateSpec, BinOp, EvalContext, Expr};
use htap_sql::query::{
    BoundQuery, JoinKind, JoinSpec, OrderItem, QueryBody, SelectBody, SetOpKind, TableSlot,
};
use htap_sql::result::StatementResult;

use crate::{olap, scan_partition_compact, LocalServer};

/// Per-statement execution context.
pub(crate) struct ExecContext<'a> {
    pub server: &'a LocalServer,
    pub catalog: &'a CatalogSnapshot,
    pub snapshot: Snapshot,
}

/// Executes a bound query and returns its result set.
pub(crate) fn execute_query(
    server: &LocalServer,
    query: &BoundQuery,
    catalog: &CatalogSnapshot,
) -> Result<StatementResult> {
    let ctx = ExecContext {
        server,
        catalog,
        snapshot: Snapshot::new(server.txn_manager.visible_version()),
    };
    let rows = run_query(&ctx, query)?;
    Ok(StatementResult::query(query.output_columns.clone(), rows))
}

/// A produced row together with its `ORDER BY` sort keys.
struct Keyed {
    output: Vec<Value>,
    keys: Vec<Value>,
}

/// Runs a query to completion, applying ordering and limits.
pub(crate) fn run_query(ctx: &ExecContext<'_>, query: &BoundQuery) -> Result<Vec<Row>> {
    let subqueries: Vec<Vec<Row>> = query
        .subqueries
        .iter()
        .map(|q| run_query(ctx, q))
        .collect::<Result<_>>()?;

    let mut keyed = match &query.body {
        QueryBody::Select(sel) => run_select(ctx, sel, &query.order_by, &subqueries)?,
        QueryBody::SetOp { kind, left, right } => {
            let mut rows = Vec::new();
            for (side, is_left) in [(left, true), (right, false)] {
                let side_rows = run_query(ctx, side)?;
                let _ = is_left;
                for row in side_rows {
                    let mut values = row.into_values();
                    for (i, target) in query.output_columns.iter().enumerate() {
                        if values[i].data_type().is_some_and(|d| d != target.data_type) {
                            values[i] = cast_value(values[i].clone(), target.data_type)?;
                        }
                    }
                    rows.push(values);
                }
            }
            if *kind == SetOpKind::UnionDistinct {
                rows = dedup_rows(rows);
            }
            let mut out = Vec::with_capacity(rows.len());
            for output in rows {
                let keys = order_keys_from_output(&query.order_by, &output, &subqueries)?;
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
) -> Result<Vec<Value>> {
    let ctx = EvalContext {
        row: &[],
        aggregates: &[],
        output: Some(output),
        subqueries,
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
fn run_select(
    ctx: &ExecContext<'_>,
    sel: &SelectBody,
    order_by: &[OrderItem],
    subqueries: &[Vec<Row>],
) -> Result<Vec<Keyed>> {
    // 1. Which columns each slot must provide.
    let mut needed: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); sel.slots.len()];
    let mut note = |e: &Expr| {
        for (slot, col) in e.referenced_columns() {
            needed[slot].insert(col);
        }
    };
    for j in &sel.joins {
        if let Some(on) = &j.on {
            note(on);
        }
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

    // 2. Materialize slots. WHERE conjuncts that touch exactly one slot are offered to
    //    that slot's scan for pruning/pushdown, except for slots on the null-supplying side
    //    of an outer join: their WHERE predicates apply after null padding (for example
    //    `LEFT JOIN c ... WHERE c.id IS NULL`), so they must stay residual.
    let mut null_supplying = vec![false; sel.slots.len()];
    for j in &sel.joins {
        match j.kind {
            JoinKind::Left => null_supplying[j.right_slot] = true,
            JoinKind::Right => null_supplying[..j.right_slot]
                .iter_mut()
                .for_each(|n| *n = true),
            JoinKind::Inner | JoinKind::Cross => {}
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
            TableSlot::Derived { query, .. } => run_query(ctx, query)?
                .into_iter()
                .map(Row::into_values)
                .collect(),
        };
        slot_rows.push(rows);
    }

    // 3. Joins.
    let mut current: Vec<Vec<Value>> = match slot_rows.first() {
        Some(_) => slot_rows.remove(0),
        None => vec![Vec::new()], // FROM-less select: one empty row.
    };
    for (j, join) in sel.joins.iter().enumerate() {
        let right = std::mem::take(&mut slot_rows[0]);
        slot_rows.remove(0);
        let right_offset = sel.slot_offset(join.right_slot);
        let right_width = sel.slots[join.right_slot].width();
        debug_assert_eq!(j + 1, join.right_slot);
        current = join_rows(current, right, right_offset, right_width, join, subqueries)?;
    }

    // 4. WHERE.
    if let Some(filter) = &sel.filter {
        let mut kept = Vec::with_capacity(current.len());
        for row in current {
            let c = EvalContext {
                row: &row,
                aggregates: &[],
                output: None,
                subqueries,
            };
            if filter.eval_predicate(&c)? {
                kept.push(row);
            }
        }
        current = kept;
    }

    // 5-7. Aggregate / project / having.
    let mut produced: Vec<Keyed> = Vec::new();
    let no_aggs: Vec<Value> = Vec::new();
    if sel.is_aggregate() {
        let mut groups: BTreeMap<Vec<Value>, (Vec<Value>, Vec<AggState>)> = BTreeMap::new();
        for row in current {
            let c = EvalContext {
                row: &row,
                aggregates: &[],
                output: None,
                subqueries,
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
                let v = match &spec.arg {
                    Some(arg) => arg.eval(&c)?,
                    None => Value::Int64(1),
                };
                state.accumulate(spec, v)?;
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
            let aggregates: Vec<Value> = states
                .iter()
                .zip(sel.aggregates.iter())
                .map(|(s, spec)| s.finish(spec))
                .collect::<Result<_>>()?;
            if let Some(k) = project_row(sel, order_by, &row, &aggregates, subqueries)? {
                produced.push(k);
            }
        }
    } else {
        for row in current {
            if let Some(k) = project_row(sel, order_by, &row, &no_aggs, subqueries)? {
                produced.push(k);
            }
        }
    }

    // 8. DISTINCT.
    if sel.distinct {
        let mut seen: BTreeSet<Vec<Value>> = BTreeSet::new();
        produced.retain(|k| seen.insert(k.output.clone()));
    }
    Ok(produced)
}

/// Projects one input row (or group), applies HAVING, and computes sort keys.
fn project_row(
    sel: &SelectBody,
    order_by: &[OrderItem],
    row: &[Value],
    aggregates: &[Value],
    subqueries: &[Vec<Row>],
) -> Result<Option<Keyed>> {
    let c = EvalContext {
        row,
        aggregates,
        output: None,
        subqueries,
    };
    let output: Vec<Value> = sel
        .projection
        .iter()
        .map(|p| p.expr.eval(&c))
        .collect::<Result<_>>()?;
    let c = EvalContext {
        row,
        aggregates,
        output: Some(&output),
        subqueries,
    };
    if let Some(h) = &sel.having {
        if !h.eval_predicate(&c)? {
            return Ok(None);
        }
    }
    let keys: Vec<Value> = order_by
        .iter()
        .map(|o| o.expr.eval(&c))
        .collect::<Result<_>>()?;
    Ok(Some(Keyed { output, keys }))
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

/// Joins the accumulated left rows with the right slot's rows.
fn join_rows(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    right_offset: usize,
    right_width: usize,
    join: &JoinSpec,
    subqueries: &[Vec<Row>],
) -> Result<Vec<Vec<Value>>> {
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
            let is_left = |s: &[usize]| !s.is_empty() && s.iter().all(|&x| x < join.right_slot);
            let is_right = |s: &[usize]| s == [join.right_slot];
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
            aggregates: &[],
            output: None,
            subqueries,
        })
    };

    let mut left_matched = vec![false; left.len()];
    let mut right_matched = vec![false; right.len()];

    if left_keys.is_empty() {
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
        // Build on the right side; rows whose key contains NULL never match.
        let mut table: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
        let pad = vec![Value::Null; right_offset];
        for (ri, r) in right.iter().enumerate() {
            let padded = combine(&pad, r);
            let c = EvalContext::row_only(&padded);
            let mut key = Vec::with_capacity(right_keys.len());
            let mut has_null = false;
            for k in &right_keys {
                let v = k.eval(&c)?;
                if v.is_null() {
                    has_null = true;
                    break;
                }
                key.push(normalize_key(v));
            }
            if !has_null {
                table.entry(key).or_default().push(ri);
            }
        }
        for (li, l) in left.iter().enumerate() {
            let c = EvalContext::row_only(l);
            let mut key = Vec::with_capacity(left_keys.len());
            let mut has_null = false;
            for k in &left_keys {
                let v = k.eval(&c)?;
                if v.is_null() {
                    has_null = true;
                    break;
                }
                key.push(normalize_key(v));
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
            let null_left = vec![Value::Null; right_offset];
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

/// Normalizes numeric join keys so `Int32(1)`, `Int64(1)` and `Float64(1.0)` hash equal.
fn normalize_key(v: Value) -> Value {
    match v {
        Value::Int32(i) => Value::Float64(i as f64),
        Value::Int64(i) | Value::Timestamp(i) => Value::Float64(i as f64),
        other => other,
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
