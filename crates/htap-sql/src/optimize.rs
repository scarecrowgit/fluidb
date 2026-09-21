//! Cost-estimation primitives for future query optimization.
//!
//! This module is intentionally independent of binding, routing, and execution.

use std::collections::HashMap;

use htap_catalog::TableStats;
use htap_common::{DataType, HtapError, Result};

use crate::expr::{BinOp, Expr};
use crate::query::{BoundQuery, JoinKind, JoinTree, QueryBody, SelectBody};

/// Provides table statistics without coupling the optimizer to catalog storage.
pub trait StatsLookup {
    /// Returns statistics for `table`, if available.
    fn table_stats(&self, table: &str) -> Option<&TableStats>;
}

/// Identifies how an estimate was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimateSource {
    /// Estimate derived from real table statistics.
    Stats,
    /// Estimate using a hard-coded default heuristic.
    Default,
}

/// Default selectivity for an equality predicate without usable statistics.
pub const DEFAULT_EQUALITY_SELECTIVITY: f64 = 0.1;

/// Default selectivity for a range predicate without usable statistics.
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 0.3;

/// An estimated row count and the provenance of that estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeEstimate {
    /// Estimated number of rows produced by the node.
    pub row_count: u64,
    /// Whether statistics or a default heuristic produced the estimate.
    pub source: EstimateSource,
}

/// Unique identifier for a leaf considered for predicate pushdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeafCandidateId(pub usize);

/// Unique identifier for a predicate within a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PredicateAtomId(pub u32);

/// Where a predicate originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateOrigin {
    /// From a join's `ON` clause.
    On {
        /// Post-order index of the join in the query's join tree.
        join_index: usize,
    },
    /// From the `WHERE` clause.
    Where,
    /// Generated from a `USING` join.
    Using,
    /// Generated from a `NATURAL` join.
    Natural,
}

/// How freely a predicate can move in the join tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateMobility {
    /// Freely movable within its connected component of `INNER`/`CROSS` joins.
    FreelyMovableWithinComponent,
    /// Pinned to a specific join's `ON` clause; cannot move.
    PinnedToJoin {
        /// Post-order index of the join.
        join_id: usize,
    },
    /// Must stay after all joins; cannot move before any join.
    PostJoinOnly,
}

/// One conjunct extracted from a query predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct PredicateAtom {
    /// Unique identifier for this predicate within a query.
    pub id: PredicateAtomId,
    /// The predicate expression.
    pub expr: Expr,
    /// Slot indices referenced by this predicate.
    pub referenced_slots: Vec<usize>,
    /// Where this predicate originated.
    pub origin: PredicateOrigin,
    /// How freely this predicate can move in the join tree.
    pub mobility: PredicateMobility,
}

/// Unique identifier for a join node in a reordered join tree.
pub type JoinNodeId = usize;

/// The input selected as the build side of a physical join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSide {
    /// Build a hash table from the left input.
    Left,
    /// Build a hash table from the right input.
    Right,
}

/// A predicate's assigned evaluation point in a reordered join tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicatePlacement {
    /// Evaluate the predicate while scanning the referenced input.
    Scan(usize),
    /// Evaluate the predicate at the specified join node.
    Join(JoinNodeId),
    /// Evaluate the predicate after all joins have completed.
    PostJoin,
}

/// The physical choices made for a bound select query.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalQuery {
    /// Select body with its optimized join tree.
    ///
    /// Non-select query bodies currently have no select body to optimize.
    pub select: Option<SelectBody>,
    /// Build-side choice for every join node, keyed by post-order node ID.
    pub join_build_sides: HashMap<JoinNodeId, BuildSide>,
    /// Selected pushdown leaf for each slot, or `None` when no leaf was selected.
    pub pushdown_selections: HashMap<usize, Option<LeafCandidateId>>,
    /// Cardinality estimates in join-tree post-order, including leaves.
    pub node_estimates: Vec<NodeEstimate>,
    /// Whether optimization fell back to the unchanged select body.
    pub fallback: bool,
}

/// A SELECT body with a reordered join tree and predicate placements.
#[derive(Debug, Clone, PartialEq)]
pub struct ReorderedSelect {
    /// The reordered join tree.
    pub join_tree: JoinTree,
    /// Placement selected for every extracted predicate.
    pub predicate_placements: Vec<(PredicateAtomId, PredicatePlacement)>,
    /// Metadata for each join node in post-order.
    pub join_metadata: Vec<JoinNodeMetadata>,
}

/// Planning metadata associated with one join node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinNodeMetadata {
    /// Post-order identifier of this join node.
    pub id: JoinNodeId,
    /// Join kind preserved from the logical join tree.
    pub kind: JoinKind,
    /// Build side selected for the join.
    pub build_side: BuildSide,
}

/// Extracts individual conjuncts from the `WHERE` and join `ON` clauses and classifies
/// how they may move during join reordering.
pub fn extract_predicate_atoms(select: &SelectBody) -> Vec<PredicateAtom> {
    let mut atoms = Vec::new();
    let mut next_id = 0_u32;
    let mut null_supplying_slots = Vec::new();
    collect_null_supplying_slots(&select.join_tree, &mut null_supplying_slots);

    if let Some(filter) = &select.filter {
        for expr in conjuncts(filter) {
            let referenced_slots = expr.referenced_slots();
            let mobility = if contains_correlated_subquery(expr)
                || referenced_slots
                    .iter()
                    .any(|slot| null_supplying_slots.contains(slot))
                || spans_outer_join_boundary(&select.join_tree, &referenced_slots)
            {
                PredicateMobility::PostJoinOnly
            } else {
                PredicateMobility::FreelyMovableWithinComponent
            };
            atoms.push(PredicateAtom {
                id: PredicateAtomId(next_id),
                expr: expr.clone(),
                referenced_slots,
                origin: PredicateOrigin::Where,
                mobility,
            });
            next_id += 1;
        }
    }

    let mut join_index = 0;
    extract_join_atoms(
        &select.join_tree,
        false,
        &mut join_index,
        &mut next_id,
        &mut atoms,
    );
    atoms
}

fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    fn collect<'a>(expr: &'a Expr, output: &mut Vec<&'a Expr>) {
        if let Expr::BinaryOp {
            op: BinOp::And,
            left,
            right,
        } = expr
        {
            collect(left, output);
            collect(right, output);
        } else {
            output.push(expr);
        }
    }

    let mut output = Vec::new();
    collect(expr, &mut output);
    output
}

fn extract_join_atoms(
    tree: &JoinTree,
    under_null_supplying_side: bool,
    next_join_index: &mut usize,
    next_id: &mut u32,
    atoms: &mut Vec<PredicateAtom>,
) {
    let JoinTree::Join {
        kind,
        left,
        right,
        on,
    } = tree
    else {
        return;
    };

    let left_is_null_supplying = matches!(kind, JoinKind::Right | JoinKind::Full);
    let right_is_null_supplying = matches!(kind, JoinKind::Left | JoinKind::Full);

    extract_join_atoms(
        left,
        under_null_supplying_side || left_is_null_supplying,
        next_join_index,
        next_id,
        atoms,
    );
    extract_join_atoms(
        right,
        under_null_supplying_side || right_is_null_supplying,
        next_join_index,
        next_id,
        atoms,
    );

    let join_index = *next_join_index;
    *next_join_index += 1;

    if let Some(on) = on {
        for expr in conjuncts(on) {
            let mobility = match kind {
                JoinKind::Inner | JoinKind::Cross if !under_null_supplying_side => {
                    PredicateMobility::FreelyMovableWithinComponent
                }
                JoinKind::Inner
                | JoinKind::Cross
                | JoinKind::Left
                | JoinKind::Right
                | JoinKind::Full => PredicateMobility::PinnedToJoin {
                    join_id: join_index,
                },
            };
            atoms.push(PredicateAtom {
                id: PredicateAtomId(*next_id),
                expr: expr.clone(),
                referenced_slots: expr.referenced_slots(),
                origin: PredicateOrigin::On { join_index },
                mobility,
            });
            *next_id += 1;
        }
    }
}

fn contains_correlated_subquery(expr: &Expr) -> bool {
    let mut correlated = false;
    expr.walk(&mut |node| {
        if matches!(
            node,
            Expr::CorrelatedColumnRef { .. }
                | Expr::ScalarSubquery {
                    correlated: true,
                    ..
                }
                | Expr::InSubquery {
                    correlated: true,
                    ..
                }
                | Expr::Exists {
                    correlated: true,
                    ..
                }
        ) {
            correlated = true;
        }
    });
    correlated
}

fn subtree_slots(tree: &JoinTree, output: &mut Vec<usize>) {
    match tree {
        JoinTree::Leaf(slot) => output.push(*slot),
        JoinTree::Join { left, right, .. } => {
            subtree_slots(left, output);
            subtree_slots(right, output);
        }
    }
}

fn collect_null_supplying_slots(tree: &JoinTree, output: &mut Vec<usize>) {
    let JoinTree::Join {
        kind, left, right, ..
    } = tree
    else {
        return;
    };

    collect_null_supplying_slots(left, output);
    collect_null_supplying_slots(right, output);

    match kind {
        JoinKind::Left => subtree_slots(right, output),
        JoinKind::Right => subtree_slots(left, output),
        JoinKind::Full => {
            subtree_slots(left, output);
            subtree_slots(right, output);
        }
        JoinKind::Inner | JoinKind::Cross => {}
    }

    output.sort_unstable();
    output.dedup();
}

fn spans_outer_join_boundary(tree: &JoinTree, referenced_slots: &[usize]) -> bool {
    let JoinTree::Join {
        kind, left, right, ..
    } = tree
    else {
        return false;
    };

    if spans_outer_join_boundary(left, referenced_slots)
        || spans_outer_join_boundary(right, referenced_slots)
    {
        return true;
    }

    if matches!(kind, JoinKind::Inner | JoinKind::Cross) {
        return false;
    }

    let mut left_slots = Vec::new();
    let mut right_slots = Vec::new();
    subtree_slots(left, &mut left_slots);
    subtree_slots(right, &mut right_slots);

    referenced_slots
        .iter()
        .any(|slot| left_slots.contains(slot))
        && referenced_slots
            .iter()
            .any(|slot| right_slots.contains(slot))
}

/// Estimates the total number of rows in a table.
///
/// Returns an error when the lookup has no statistics for `table`.
pub fn estimate_row_count(
    stats_lookup: &dyn StatsLookup,
    table: &str,
) -> Result<(u64, EstimateSource)> {
    stats_lookup
        .table_stats(table)
        .map(|stats| (stats.row_count, EstimateSource::Stats))
        .ok_or_else(|| HtapError::NotFound(format!("table statistics for '{table}' not found")))
}

/// Estimates the selectivity of an equality predicate.
///
/// When the column has a distinct-value count, the estimate is
/// `1 / max(distinct_count, 1)`, clamped to `[0.0, 1.0]`. Missing table
/// statistics, an out-of-bounds column index, or a missing distinct-value
/// count uses the documented default of `0.1`.
pub fn estimate_equality_selectivity(
    stats_lookup: &dyn StatsLookup,
    table: &str,
    column_index: usize,
) -> (f64, EstimateSource) {
    let Some(distinct_count) = stats_lookup
        .table_stats(table)
        .and_then(|stats| stats.columns.get(column_index))
        .and_then(|column| column.distinct_count)
    else {
        return (DEFAULT_EQUALITY_SELECTIVITY, EstimateSource::Default);
    };

    let selectivity = (1.0_f64 / distinct_count.max(1) as f64).clamp(0.0, 1.0);
    (selectivity, EstimateSource::Stats)
}

/// Estimates the selectivity of a range predicate.
///
/// A column with known minimum and maximum values uses the statistics-based
/// estimate `0.3` when both values have the same ordered numeric or date-like
/// type. Missing or unsupported statistics use the default `0.3`.
pub fn estimate_range_selectivity(
    stats_lookup: &dyn StatsLookup,
    table: &str,
    column_index: usize,
) -> (f64, EstimateSource) {
    let Some(column) = stats_lookup
        .table_stats(table)
        .and_then(|stats| stats.columns.get(column_index))
    else {
        return (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default);
    };

    let supported = match (&column.min, &column.max) {
        (Some(min), Some(max)) if min.data_type() == max.data_type() => matches!(
            min.data_type(),
            Some(DataType::Int64 | DataType::Float64 | DataType::Timestamp)
        ),
        _ => false,
    };

    if supported {
        (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Stats)
    } else {
        (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default)
    }
}

/// Estimates the output cardinality of a join.
///
/// The product is computed in floating point to avoid integer multiplication
/// overflow, converted to `u64` using Rust's saturating float-to-integer cast,
/// and constrained to at least one row.
pub fn estimate_join_cardinality(
    left_rows: u64,
    right_rows: u64,
    left_selectivity: f64,
    right_selectivity: f64,
) -> u64 {
    ((left_rows as f64 * right_rows as f64 * left_selectivity * right_selectivity) as u64).max(1)
}

/// Optimizes a bound query without changing its result set.
pub fn optimize(query: &BoundQuery, stats_lookup: &dyn StatsLookup) -> PhysicalQuery {
    let QueryBody::Select(select) = &query.body else {
        return PhysicalQuery {
            select: None,
            join_build_sides: HashMap::new(),
            pushdown_selections: HashMap::new(),
            node_estimates: Vec::new(),
            fallback: false,
        };
    };

    optimize_select(select, stats_lookup)
}

fn optimize_select(select: &SelectBody, stats_lookup: &dyn StatsLookup) -> PhysicalQuery {
    let atoms = extract_predicate_atoms(select);
    let reordered = reorder_joins_unchecked(select, stats_lookup, &atoms);

    if validate_predicate_conservation(select, &atoms, &reordered).is_err() {
        return identity_physical_query(select, stats_lookup, true);
    }

    let mut optimized = select.clone();
    if materialize_predicate_placements(&mut optimized, &atoms, &reordered).is_err() {
        return identity_physical_query(select, stats_lookup, true);
    }

    let join_build_sides = reordered
        .join_metadata
        .iter()
        .map(|metadata| (metadata.id, metadata.build_side))
        .collect();

    PhysicalQuery {
        pushdown_selections: select_pushdown_candidates(&optimized, &atoms, stats_lookup),
        node_estimates: estimate_nodes(&optimized.join_tree, &optimized, stats_lookup, &atoms),
        select: Some(optimized),
        join_build_sides,
        fallback: false,
    }
}

fn and_exprs(mut expressions: Vec<Expr>) -> Option<Expr> {
    let first = expressions.first()?.clone();
    expressions.remove(0);
    Some(
        expressions
            .into_iter()
            .fold(first, |left, right| Expr::BinaryOp {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            }),
    )
}

fn attach_join_predicates(
    tree: &mut JoinTree,
    predicates: &HashMap<JoinNodeId, Vec<Expr>>,
    next_join_id: &mut JoinNodeId,
) {
    let JoinTree::Join {
        kind,
        left,
        right,
        on,
    } = tree
    else {
        return;
    };

    attach_join_predicates(left, predicates, next_join_id);
    attach_join_predicates(right, predicates, next_join_id);

    let join_id = *next_join_id;
    *next_join_id += 1;

    // All original ON conjuncts were extracted into predicate atoms. Rebuild the
    // condition exclusively from their validated, layout-rebased copies.
    let mut expressions = predicates.get(&join_id).cloned().unwrap_or_default();

    // A cross-product node cannot carry a condition: the executor intentionally
    // dispatches it without evaluating `on`. Promote condition-bearing rebuilt nodes.
    if !expressions.is_empty() && matches!(*kind, JoinKind::Cross) {
        *kind = JoinKind::Inner;
    }

    // Every reordered inner join gets an executable ON expression. TRUE is represented
    // by a non-zero integer predicate for a predicate-free cross-product edge.
    if expressions.is_empty() && matches!(*kind, JoinKind::Inner) {
        expressions.push(Expr::Literal(htap_common::Value::Int64(1)));
    }
    *on = and_exprs(expressions);
}

fn remap_predicate_columns(expr: &mut Expr, select: &SelectBody, layout: &[usize]) -> Result<()> {
    match expr {
        Expr::ColumnRef {
            slot,
            column,
            offset,
            ..
        } => {
            let position = layout.iter().position(|candidate| candidate == slot).ok_or_else(|| {
                HtapError::Internal(format!(
                    "predicate column references slot {slot}, which is unavailable at its attachment point"
                ))
            })?;
            let table_slot = select.slots.get(*slot).ok_or_else(|| {
                HtapError::Internal(format!(
                    "predicate column references missing table slot {slot}"
                ))
            })?;
            if *column >= table_slot.width() {
                return Err(HtapError::Internal(format!(
                    "predicate column {column} is outside slot {slot} width {}",
                    table_slot.width()
                )));
            }

            let mut remapped = 0usize;
            for preceding_slot in &layout[..position] {
                let width = select
                    .slots
                    .get(*preceding_slot)
                    .ok_or_else(|| {
                        HtapError::Internal(format!(
                            "predicate attachment layout references missing table slot {preceding_slot}"
                        ))
                    })?
                    .width();
                remapped = remapped.checked_add(width).ok_or_else(|| {
                    HtapError::Internal("predicate column offset overflow".into())
                })?;
            }
            *offset = remapped
                .checked_add(*column)
                .ok_or_else(|| HtapError::Internal("predicate column offset overflow".into()))?;
        }
        Expr::BinaryOp { left, right, .. } => {
            remap_predicate_columns(left, select, layout)?;
            remap_predicate_columns(right, select, layout)?;
        }
        Expr::Not(inner)
        | Expr::Negate(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Cast { expr: inner, .. } => {
            remap_predicate_columns(inner, select, layout)?;
        }
        Expr::Like { expr, pattern, .. } => {
            remap_predicate_columns(expr, select, layout)?;
            remap_predicate_columns(pattern, select, layout)?;
        }
        Expr::In { expr, list, .. } => {
            remap_predicate_columns(expr, select, layout)?;
            for item in list {
                remap_predicate_columns(item, select, layout)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            remap_predicate_columns(expr, select, layout)?;
            remap_predicate_columns(low, select, layout)?;
            remap_predicate_columns(high, select, layout)?;
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                remap_predicate_columns(operand, select, layout)?;
            }
            for (condition, result) in branches {
                remap_predicate_columns(condition, select, layout)?;
                remap_predicate_columns(result, select, layout)?;
            }
            if let Some(result) = else_result {
                remap_predicate_columns(result, select, layout)?;
            }
        }
        Expr::ScalarFunction { args, .. } => {
            for argument in args {
                remap_predicate_columns(argument, select, layout)?;
            }
        }
        Expr::InSubquery { expr, .. } => {
            remap_predicate_columns(expr, select, layout)?;
        }
        Expr::CorrelatedColumnRef { .. }
        | Expr::OutputColumn { .. }
        | Expr::Literal(_)
        | Expr::AggregateRef { .. }
        | Expr::WindowRef { .. }
        | Expr::ScalarSubquery { .. }
        | Expr::Exists { .. }
        | Expr::Variable { .. } => {}
    }
    Ok(())
}

fn materialize_predicate_placements(
    select: &mut SelectBody,
    atoms: &[PredicateAtom],
    reordered: &ReorderedSelect,
) -> Result<()> {
    let atoms_by_id = atoms
        .iter()
        .map(|atom| (atom.id, atom))
        .collect::<HashMap<_, _>>();
    let mut join_layouts = Vec::new();
    collect_join_layouts(&reordered.join_tree, &mut join_layouts);
    let logical_layout = (0..select.slots.len()).collect::<Vec<_>>();
    let mut join_predicates = HashMap::<JoinNodeId, Vec<Expr>>::new();
    let mut residual = Vec::new();

    for (id, placement) in &reordered.predicate_placements {
        let atom = atoms_by_id.get(id).ok_or_else(|| {
            HtapError::Internal(format!("predicate {id:?} has no extracted atom"))
        })?;
        let layout = match placement {
            PredicatePlacement::Join(join_id) => join_layouts.get(*join_id).ok_or_else(|| {
                HtapError::Internal(format!(
                    "predicate {id:?} references missing join node {join_id}"
                ))
            })?,
            // Scan predicates are currently retained in the residual filter, which runs
            // after the executor restores the binder's logical slot order.
            PredicatePlacement::Scan(_) | PredicatePlacement::PostJoin => &logical_layout,
        };

        let mut expr = atom.expr.clone();
        remap_predicate_columns(&mut expr, select, layout)?;

        match placement {
            PredicatePlacement::Join(join_id) => {
                join_predicates.entry(*join_id).or_default().push(expr);
            }
            PredicatePlacement::Scan(_) | PredicatePlacement::PostJoin => {
                residual.push(expr);
            }
        }
    }

    select.join_tree = reordered.join_tree.clone();
    attach_join_predicates(&mut select.join_tree, &join_predicates, &mut 0);
    select.filter = and_exprs(residual);
    Ok(())
}

fn identity_physical_query(
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    fallback: bool,
) -> PhysicalQuery {
    let atoms = extract_predicate_atoms(select);
    let mut metadata = Vec::new();
    collect_join_metadata(&select.join_tree, select, stats_lookup, &mut metadata);

    PhysicalQuery {
        select: Some(select.clone()),
        join_build_sides: metadata
            .into_iter()
            .map(|item| (item.id, item.build_side))
            .collect(),
        pushdown_selections: select_pushdown_candidates(select, &atoms, stats_lookup),
        node_estimates: estimate_nodes(&select.join_tree, select, stats_lookup, &atoms),
        fallback,
    }
}

fn select_pushdown_candidates(
    select: &SelectBody,
    atoms: &[PredicateAtom],
    stats_lookup: &dyn StatsLookup,
) -> HashMap<usize, Option<LeafCandidateId>> {
    let mut selections = HashMap::new();

    for slot in 0..select.slots.len() {
        let candidates: Vec<_> = atoms
            .iter()
            .enumerate()
            .filter(|(_, atom)| {
                atom.mobility == PredicateMobility::FreelyMovableWithinComponent
                    && atom.referenced_slots == [slot]
            })
            .collect();

        let selected = candidates
            .iter()
            .map(|(index, atom)| {
                (
                    LeafCandidateId(*index),
                    predicate_selectivity(atom, select, stats_lookup),
                )
            })
            .min_by(|left, right| {
                left.1
                     .0
                    .total_cmp(&right.1 .0)
                    // Defaults carry no cost information, so retain first-match order.
                    .then_with(|| left.0.cmp(&right.0))
            })
            .map(|(id, _)| id);

        selections.insert(slot, selected);
    }

    selections
}

fn predicate_selectivity(
    atom: &PredicateAtom,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
) -> (f64, EstimateSource) {
    match &atom.expr {
        Expr::BinaryOp {
            op: BinOp::Eq,
            left,
            right,
        } => {
            let location = column_location(left).or_else(|| column_location(right));
            location
                .and_then(|(slot, column)| {
                    base_table_name(select, slot)
                        .map(|table| estimate_equality_selectivity(stats_lookup, table, column))
                })
                .unwrap_or((DEFAULT_EQUALITY_SELECTIVITY, EstimateSource::Default))
        }
        Expr::BinaryOp {
            op: BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte,
            left,
            right,
        } => {
            let location = column_location(left).or_else(|| column_location(right));
            location
                .and_then(|(slot, column)| {
                    base_table_name(select, slot)
                        .map(|table| estimate_range_selectivity(stats_lookup, table, column))
                })
                .unwrap_or((DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default))
        }
        _ => (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default),
    }
}

fn estimate_nodes(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> Vec<NodeEstimate> {
    fn collect(
        tree: &JoinTree,
        select: &SelectBody,
        stats_lookup: &dyn StatsLookup,
        atoms: &[PredicateAtom],
        output: &mut Vec<NodeEstimate>,
    ) -> (u64, EstimateSource) {
        let estimate = match tree {
            JoinTree::Leaf(slot) => base_table_name(select, *slot)
                .and_then(|table| estimate_row_count(stats_lookup, table).ok())
                .unwrap_or((1_000, EstimateSource::Default)),
            JoinTree::Join { left, right, .. } => {
                let (left_rows, left_source) = collect(left, select, stats_lookup, atoms, output);
                let (right_rows, right_source) =
                    collect(right, select, stats_lookup, atoms, output);
                let mut left_slots = Vec::new();
                let mut right_slots = Vec::new();
                subtree_slots(left, &mut left_slots);
                subtree_slots(right, &mut right_slots);
                let selectivity =
                    join_selectivity(atoms, &left_slots, &right_slots, select, stats_lookup);
                let source = if left_source == EstimateSource::Stats
                    && right_source == EstimateSource::Stats
                {
                    EstimateSource::Stats
                } else {
                    EstimateSource::Default
                };
                (
                    estimate_join_cardinality(left_rows, right_rows, selectivity, 1.0),
                    source,
                )
            }
        };

        output.push(NodeEstimate {
            row_count: estimate.0,
            source: estimate.1,
        });
        estimate
    }

    let mut estimates = Vec::new();
    collect(tree, select, stats_lookup, atoms, &mut estimates);
    estimates
}

/// Reorders maximal `INNER`/`CROSS` join components and assigns every
/// extracted predicate to its earliest valid evaluation point.
///
/// Components containing a recursive CTE working-table slot are left in their
/// original order because the recursive term is a reordering barrier.
pub fn reorder_joins(
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
) -> Result<ReorderedSelect> {
    let atoms = extract_predicate_atoms(select);
    let reordered = reorder_joins_unchecked(select, stats_lookup, &atoms);

    validate_predicate_conservation(select, &atoms, &reordered)?;
    Ok(reordered)
}

fn reorder_joins_unchecked(
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> ReorderedSelect {
    let join_tree = reorder_tree(&select.join_tree, select, stats_lookup, atoms);

    let mut join_metadata = Vec::new();
    collect_join_metadata(&join_tree, select, stats_lookup, &mut join_metadata);

    let mut original_join_slots = Vec::new();
    collect_join_slot_sets(&select.join_tree, &mut original_join_slots);

    let mut reordered_join_slots = Vec::new();
    collect_join_slot_sets(&join_tree, &mut reordered_join_slots);

    let mut outer_boundaries = Vec::new();
    collect_outer_join_boundaries(&select.join_tree, &mut outer_boundaries);

    let predicate_placements = atoms
        .iter()
        .map(|atom| {
            let placement = match atom.mobility {
                PredicateMobility::PostJoinOnly => PredicatePlacement::PostJoin,
                PredicateMobility::PinnedToJoin { join_id } => {
                    let target_slots = original_join_slots
                        .get(join_id)
                        .cloned()
                        .unwrap_or_default();
                    reordered_join_slots
                        .iter()
                        .position(|slots| *slots == target_slots)
                        .map(PredicatePlacement::Join)
                        .unwrap_or(PredicatePlacement::PostJoin)
                }
                PredicateMobility::FreelyMovableWithinComponent => {
                    let candidate = place_movable_predicate(&join_tree, &atom.referenced_slots);
                    let PredicateOrigin::On { join_index } = atom.origin else {
                        return (atom.id, candidate);
                    };
                    let Some(origin_slots) = original_join_slots.get(join_index) else {
                        return (atom.id, PredicatePlacement::PostJoin);
                    };

                    if placement_crosses_outer_boundary(
                        origin_slots,
                        candidate,
                        &reordered_join_slots,
                        &outer_boundaries,
                    ) {
                        reordered_join_slots
                            .iter()
                            .position(|slots| slots == origin_slots)
                            .map(PredicatePlacement::Join)
                            .unwrap_or(PredicatePlacement::PostJoin)
                    } else {
                        candidate
                    }
                }
            };
            (atom.id, placement)
        })
        .collect();

    ReorderedSelect {
        join_tree,
        predicate_placements,
        join_metadata,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OuterBoundaryLocation {
    Left,
    Right,
    At,
    Outside,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OuterJoinBoundary {
    slots: Vec<usize>,
    left_slots: Vec<usize>,
    right_slots: Vec<usize>,
}

fn collect_outer_join_boundaries(tree: &JoinTree, output: &mut Vec<OuterJoinBoundary>) {
    let JoinTree::Join {
        kind, left, right, ..
    } = tree
    else {
        return;
    };

    collect_outer_join_boundaries(left, output);
    collect_outer_join_boundaries(right, output);

    if matches!(kind, JoinKind::Inner | JoinKind::Cross) {
        return;
    }

    let mut left_slots = Vec::new();
    let mut right_slots = Vec::new();
    subtree_slots(left, &mut left_slots);
    subtree_slots(right, &mut right_slots);
    left_slots.sort_unstable();
    left_slots.dedup();
    right_slots.sort_unstable();
    right_slots.dedup();

    let mut slots = left_slots.clone();
    slots.extend(right_slots.iter().copied());
    slots.sort_unstable();
    slots.dedup();

    output.push(OuterJoinBoundary {
        slots,
        left_slots,
        right_slots,
    });
}

fn outer_boundary_location(slots: &[usize], boundary: &OuterJoinBoundary) -> OuterBoundaryLocation {
    if slots == boundary.slots {
        OuterBoundaryLocation::At
    } else if slots.iter().all(|slot| boundary.left_slots.contains(slot)) {
        OuterBoundaryLocation::Left
    } else if slots.iter().all(|slot| boundary.right_slots.contains(slot)) {
        OuterBoundaryLocation::Right
    } else {
        OuterBoundaryLocation::Outside
    }
}

fn placement_crosses_outer_boundary(
    origin_slots: &[usize],
    placement: PredicatePlacement,
    reordered_join_slots: &[Vec<usize>],
    boundaries: &[OuterJoinBoundary],
) -> bool {
    let attachment_slots = match placement {
        PredicatePlacement::Scan(slot) => vec![slot],
        PredicatePlacement::Join(join_id) => {
            let Some(slots) = reordered_join_slots.get(join_id) else {
                return true;
            };
            slots.clone()
        }
        PredicatePlacement::PostJoin => Vec::new(),
    };

    boundaries.iter().any(|boundary| {
        let origin_location = outer_boundary_location(origin_slots, boundary);
        let attachment_location = if placement == PredicatePlacement::PostJoin {
            OuterBoundaryLocation::Outside
        } else {
            outer_boundary_location(&attachment_slots, boundary)
        };
        origin_location != attachment_location
    })
}

/// Validates that join reordering preserved every predicate exactly once and
/// placed it at an evaluation point permitted by its references and mobility.
pub fn validate_predicate_conservation(
    original: &SelectBody,
    atoms: &[PredicateAtom],
    reordered: &ReorderedSelect,
) -> Result<()> {
    use std::collections::HashMap;

    let mut atom_counts = HashMap::new();
    for atom in atoms {
        *atom_counts.entry(atom.id).or_insert(0_usize) += 1;
    }
    if atom_counts.values().any(|count| *count != 1) {
        return Err(HtapError::Internal(
            "predicate atoms contain duplicate ids".into(),
        ));
    }

    let mut placement_counts = HashMap::new();
    for (id, _) in &reordered.predicate_placements {
        *placement_counts.entry(*id).or_insert(0_usize) += 1;
    }
    if atom_counts != placement_counts {
        return Err(HtapError::Internal(
            "predicate ids were not conserved during join reordering".into(),
        ));
    }

    let mut reordered_join_slots = Vec::new();
    collect_join_slot_sets(&reordered.join_tree, &mut reordered_join_slots);

    let mut original_join_slots = Vec::new();
    collect_join_slot_sets(&original.join_tree, &mut original_join_slots);

    let mut outer_boundaries = Vec::new();
    collect_outer_join_boundaries(&original.join_tree, &mut outer_boundaries);

    for atom in atoms {
        let placement = reordered
            .predicate_placements
            .iter()
            .find_map(|(id, placement)| (*id == atom.id).then_some(*placement))
            .ok_or_else(|| {
                HtapError::Internal(format!("predicate {:?} has no placement", atom.id))
            })?;

        let attachment_slots = match placement {
            PredicatePlacement::Scan(slot) => vec![slot],
            PredicatePlacement::Join(join_id) => {
                reordered_join_slots.get(join_id).cloned().ok_or_else(|| {
                    HtapError::Internal(format!(
                        "predicate {:?} references missing join node {join_id}",
                        atom.id
                    ))
                })?
            }
            PredicatePlacement::PostJoin => {
                let mut slots = Vec::new();
                subtree_slots(&reordered.join_tree, &mut slots);
                slots
            }
        };

        if !atom
            .referenced_slots
            .iter()
            .all(|slot| attachment_slots.contains(slot))
        {
            return Err(HtapError::Internal(format!(
                "predicate {:?} is attached outside its referenced subtree",
                atom.id
            )));
        }

        if let PredicateOrigin::On { join_index } = atom.origin {
            let origin_slots = original_join_slots.get(join_index).ok_or_else(|| {
                HtapError::Internal(format!(
                    "predicate {:?} originated from missing join {join_index}",
                    atom.id
                ))
            })?;
            if placement_crosses_outer_boundary(
                origin_slots,
                placement,
                &reordered_join_slots,
                &outer_boundaries,
            ) {
                return Err(HtapError::Internal(format!(
                    "predicate {:?} crossed an outer-join boundary",
                    atom.id
                )));
            }
        }

        match atom.mobility {
            PredicateMobility::PostJoinOnly if placement != PredicatePlacement::PostJoin => {
                return Err(HtapError::Internal(format!(
                    "post-join predicate {:?} was moved before the final join",
                    atom.id
                )));
            }
            PredicateMobility::PinnedToJoin { join_id } => {
                let expected_slots = original_join_slots.get(join_id).ok_or_else(|| {
                    HtapError::Internal(format!(
                        "predicate {:?} is pinned to missing original join {join_id}",
                        atom.id
                    ))
                })?;
                let pinned = match placement {
                    PredicatePlacement::Join(reordered_id) => reordered_join_slots
                        .get(reordered_id)
                        .is_some_and(|slots| slots == expected_slots),
                    _ => false,
                };
                if !pinned {
                    return Err(HtapError::Internal(format!(
                        "predicate {:?} moved away from its pinned join",
                        atom.id
                    )));
                }
            }
            PredicateMobility::FreelyMovableWithinComponent | PredicateMobility::PostJoinOnly => {}
        }
    }

    Ok(())
}

fn reorder_tree(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> JoinTree {
    match tree {
        JoinTree::Leaf(_) => tree.clone(),
        JoinTree::Join {
            kind: JoinKind::Inner | JoinKind::Cross,
            ..
        } => {
            let mut relations = Vec::new();
            flatten_inner_component(tree, select, stats_lookup, atoms, &mut relations);

            let contains_working_table = relations.iter().any(|relation| {
                let mut slots = Vec::new();
                subtree_slots(relation, &mut slots);
                slots.iter().any(|slot| {
                    matches!(
                        select.slots.get(*slot),
                        Some(crate::query::TableSlot::WorkingTableSlot { .. })
                    )
                })
            });

            if contains_working_table || relations.len() < 2 {
                rebuild_without_reordering(tree, select, stats_lookup, atoms)
            } else if relations.len() <= 8 {
                reorder_component_dp(relations, select, stats_lookup, atoms).unwrap_or_else(|| {
                    rebuild_without_reordering(tree, select, stats_lookup, atoms)
                })
            } else {
                reorder_component_greedy(relations, select, stats_lookup, atoms).unwrap_or_else(
                    || rebuild_without_reordering(tree, select, stats_lookup, atoms),
                )
            }
        }
        JoinTree::Join {
            kind,
            left,
            right,
            on,
        } => JoinTree::Join {
            kind: *kind,
            left: Box::new(reorder_tree(left, select, stats_lookup, atoms)),
            right: Box::new(reorder_tree(right, select, stats_lookup, atoms)),
            on: on.clone(),
        },
    }
}

fn rebuild_without_reordering(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> JoinTree {
    match tree {
        JoinTree::Leaf(_) => tree.clone(),
        JoinTree::Join {
            kind,
            left,
            right,
            on,
        } => JoinTree::Join {
            kind: *kind,
            left: Box::new(reorder_tree(left, select, stats_lookup, atoms)),
            right: Box::new(reorder_tree(right, select, stats_lookup, atoms)),
            on: on.clone(),
        },
    }
}

fn flatten_inner_component(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
    output: &mut Vec<JoinTree>,
) {
    match tree {
        JoinTree::Join {
            kind: JoinKind::Inner | JoinKind::Cross,
            left,
            right,
            ..
        } => {
            flatten_inner_component(left, select, stats_lookup, atoms, output);
            flatten_inner_component(right, select, stats_lookup, atoms, output);
        }
        _ => output.push(reorder_tree(tree, select, stats_lookup, atoms)),
    }
}

fn reorder_component_dp(
    relations: Vec<JoinTree>,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> Option<JoinTree> {
    let relation_count = relations.len();
    let full_mask = (1_usize << relation_count) - 1;
    type JoinPlan = (JoinTree, u64, f64, Vec<usize>);

    let mut plans: Vec<Option<JoinPlan>> = vec![None; full_mask + 1];

    for (index, relation) in relations.into_iter().enumerate() {
        let mut slots = Vec::new();
        subtree_slots(&relation, &mut slots);
        slots.sort_unstable();
        slots.dedup();
        let rows = estimate_tree_rows(&relation, select, stats_lookup, atoms);
        plans[1 << index] = Some((relation, rows, rows as f64, slots));
    }

    for mask in 1..=full_mask {
        if mask.count_ones() < 2 {
            continue;
        }

        let first_bit = 1_usize << mask.trailing_zeros();
        let mut left_mask = (mask - 1) & mask;
        while left_mask != 0 {
            if left_mask & first_bit == 0 {
                left_mask = (left_mask - 1) & mask;
                continue;
            }

            let right_mask = mask ^ left_mask;
            if right_mask == 0 {
                left_mask = (left_mask - 1) & mask;
                continue;
            }

            let Some((left_tree, left_rows, left_cost, left_slots)) = plans[left_mask].clone()
            else {
                left_mask = (left_mask - 1) & mask;
                continue;
            };
            let Some((right_tree, right_rows, right_cost, right_slots)) = plans[right_mask].clone()
            else {
                left_mask = (left_mask - 1) & mask;
                continue;
            };

            let selectivity =
                join_selectivity(atoms, &left_slots, &right_slots, select, stats_lookup);
            let output_rows = estimate_join_cardinality(left_rows, right_rows, selectivity, 1.0);
            let cost = left_cost + right_cost + output_rows as f64;
            let kind = if has_join_predicate(atoms, &left_slots, &right_slots, select) {
                JoinKind::Inner
            } else {
                JoinKind::Cross
            };

            let mut slots = left_slots;
            slots.extend(right_slots);
            slots.sort_unstable();
            slots.dedup();

            let candidate = (
                JoinTree::Join {
                    kind,
                    left: Box::new(left_tree),
                    right: Box::new(right_tree),
                    on: None,
                },
                output_rows,
                cost,
                slots,
            );

            if plans[mask]
                .as_ref()
                .is_none_or(|existing| cost < existing.2)
            {
                plans[mask] = Some(candidate);
            }

            left_mask = (left_mask - 1) & mask;
        }
    }

    plans[full_mask].take().map(|plan| plan.0)
}

fn reorder_component_greedy(
    mut relations: Vec<JoinTree>,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> Option<JoinTree> {
    let mut candidates = relations.iter().enumerate();
    let (mut first_index, first_relation) = candidates.next()?;
    let mut first_rows = estimate_tree_rows(first_relation, select, stats_lookup, atoms);

    for (index, relation) in candidates {
        let rows = estimate_tree_rows(relation, select, stats_lookup, atoms);
        if rows < first_rows {
            first_index = index;
            first_rows = rows;
        }
    }

    let mut plan = relations.remove(first_index);
    let mut plan_slots = Vec::new();
    subtree_slots(&plan, &mut plan_slots);
    plan_slots.sort_unstable();
    plan_slots.dedup();
    let mut plan_rows = estimate_tree_rows(&plan, select, stats_lookup, atoms);

    while !relations.is_empty() {
        let mut candidates = relations.iter().enumerate().map(|(index, relation)| {
            let mut relation_slots = Vec::new();
            subtree_slots(relation, &mut relation_slots);
            relation_slots.sort_unstable();
            relation_slots.dedup();

            let relation_rows = estimate_tree_rows(relation, select, stats_lookup, atoms);
            let selectivity =
                join_selectivity(atoms, &plan_slots, &relation_slots, select, stats_lookup);
            let output_rows = estimate_join_cardinality(plan_rows, relation_rows, selectivity, 1.0);
            let kind = if has_join_predicate(atoms, &plan_slots, &relation_slots, select) {
                JoinKind::Inner
            } else {
                JoinKind::Cross
            };
            (index, output_rows, kind)
        });

        let Some((mut best_index, mut best_rows, mut best_kind)) = candidates.next() else {
            return Some(plan);
        };

        for (index, rows, kind) in candidates {
            if rows < best_rows {
                best_index = index;
                best_rows = rows;
                best_kind = kind;
            }
        }

        let relation = relations.remove(best_index);
        let mut relation_slots = Vec::new();
        subtree_slots(&relation, &mut relation_slots);

        plan = JoinTree::Join {
            kind: best_kind,
            left: Box::new(plan),
            right: Box::new(relation),
            on: None,
        };
        plan_rows = best_rows;
        plan_slots.extend(relation_slots);
        plan_slots.sort_unstable();
        plan_slots.dedup();
    }

    Some(plan)
}

fn predicate_evaluates_at_join(
    atom: &PredicateAtom,
    left_slots: &[usize],
    right_slots: &[usize],
    select: &SelectBody,
) -> bool {
    match atom.mobility {
        PredicateMobility::FreelyMovableWithinComponent => {
            atom.referenced_slots
                .iter()
                .any(|slot| left_slots.contains(slot))
                && atom
                    .referenced_slots
                    .iter()
                    .any(|slot| right_slots.contains(slot))
        }
        PredicateMobility::PinnedToJoin { join_id } => {
            let mut candidate_slots = left_slots.to_vec();
            candidate_slots.extend(right_slots.iter().copied());
            candidate_slots.sort_unstable();
            candidate_slots.dedup();

            let mut original_join_slots = Vec::new();
            collect_join_slot_sets(&select.join_tree, &mut original_join_slots);
            original_join_slots
                .get(join_id)
                .is_some_and(|slots| *slots == candidate_slots)
        }
        PredicateMobility::PostJoinOnly => false,
    }
}

fn has_join_predicate(
    atoms: &[PredicateAtom],
    left_slots: &[usize],
    right_slots: &[usize],
    select: &SelectBody,
) -> bool {
    atoms
        .iter()
        .any(|atom| predicate_evaluates_at_join(atom, left_slots, right_slots, select))
}

fn join_selectivity(
    atoms: &[PredicateAtom],
    left_slots: &[usize],
    right_slots: &[usize],
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
) -> f64 {
    let mut selectivity = 1.0;

    for atom in atoms {
        if !predicate_evaluates_at_join(atom, left_slots, right_slots, select) {
            continue;
        }

        let atom_selectivity = match &atom.expr {
            Expr::BinaryOp {
                op: BinOp::Eq,
                left,
                right,
            } => equality_expr_selectivity(left, right, select, stats_lookup),
            Expr::BinaryOp {
                op: BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte,
                left,
                right,
            } => range_expr_selectivity(left, right, select, stats_lookup),
            _ => DEFAULT_RANGE_SELECTIVITY,
        };
        selectivity *= atom_selectivity;
    }

    selectivity.clamp(0.0, 1.0)
}

fn equality_expr_selectivity(
    left: &Expr,
    right: &Expr,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
) -> f64 {
    let left_selectivity = column_location(left)
        .and_then(|(slot, column)| base_table_name(select, slot).map(|table| (table, column)))
        .map(|(table, column)| estimate_equality_selectivity(stats_lookup, table, column).0);
    let right_selectivity = column_location(right)
        .and_then(|(slot, column)| base_table_name(select, slot).map(|table| (table, column)))
        .map(|(table, column)| estimate_equality_selectivity(stats_lookup, table, column).0);

    match (left_selectivity, right_selectivity) {
        (Some(left), Some(right)) => left.min(right),
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => DEFAULT_EQUALITY_SELECTIVITY,
    }
}

fn range_expr_selectivity(
    left: &Expr,
    right: &Expr,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
) -> f64 {
    column_location(left)
        .or_else(|| column_location(right))
        .and_then(|(slot, column)| base_table_name(select, slot).map(|table| (table, column)))
        .map(|(table, column)| estimate_range_selectivity(stats_lookup, table, column).0)
        .unwrap_or(DEFAULT_RANGE_SELECTIVITY)
}

fn column_location(expr: &Expr) -> Option<(usize, usize)> {
    match expr {
        Expr::ColumnRef { slot, column, .. } => Some((*slot, *column)),
        _ => None,
    }
}

fn base_table_name(select: &SelectBody, slot: usize) -> Option<&str> {
    match select.slots.get(slot) {
        Some(crate::query::TableSlot::Base { table, .. }) => Some(table),
        _ => None,
    }
}

fn estimate_tree_rows(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    atoms: &[PredicateAtom],
) -> u64 {
    match tree {
        JoinTree::Leaf(slot) => base_table_name(select, *slot)
            .and_then(|table| estimate_row_count(stats_lookup, table).ok())
            .map(|(rows, _)| rows)
            .unwrap_or(1_000),
        JoinTree::Join { left, right, .. } => {
            let left_rows = estimate_tree_rows(left, select, stats_lookup, atoms);
            let right_rows = estimate_tree_rows(right, select, stats_lookup, atoms);
            let mut left_slots = Vec::new();
            let mut right_slots = Vec::new();
            subtree_slots(left, &mut left_slots);
            subtree_slots(right, &mut right_slots);
            let selectivity =
                join_selectivity(atoms, &left_slots, &right_slots, select, stats_lookup);
            estimate_join_cardinality(left_rows, right_rows, selectivity, 1.0)
        }
    }
}

fn collect_join_slot_sets(tree: &JoinTree, output: &mut Vec<Vec<usize>>) {
    let JoinTree::Join { left, right, .. } = tree else {
        return;
    };

    collect_join_slot_sets(left, output);
    collect_join_slot_sets(right, output);

    let mut slots = Vec::new();
    subtree_slots(tree, &mut slots);
    slots.sort_unstable();
    slots.dedup();
    output.push(slots);
}

fn collect_join_layouts(tree: &JoinTree, output: &mut Vec<Vec<usize>>) {
    let JoinTree::Join { left, right, .. } = tree else {
        return;
    };

    collect_join_layouts(left, output);
    collect_join_layouts(right, output);

    let mut slots = Vec::new();
    subtree_slots(tree, &mut slots);
    output.push(slots);
}

fn collect_join_metadata(
    tree: &JoinTree,
    select: &SelectBody,
    stats_lookup: &dyn StatsLookup,
    output: &mut Vec<JoinNodeMetadata>,
) -> u64 {
    match tree {
        JoinTree::Leaf(slot) => base_table_name(select, *slot)
            .and_then(|table| estimate_row_count(stats_lookup, table).ok())
            .map(|(rows, _)| rows)
            .unwrap_or(1_000),
        JoinTree::Join {
            kind, left, right, ..
        } => {
            let left_rows = collect_join_metadata(left, select, stats_lookup, output);
            let right_rows = collect_join_metadata(right, select, stats_lookup, output);
            let id = output.len();
            output.push(JoinNodeMetadata {
                id,
                kind: *kind,
                build_side: if left_rows <= right_rows {
                    BuildSide::Left
                } else {
                    BuildSide::Right
                },
            });
            estimate_join_cardinality(left_rows, right_rows, 1.0, 1.0)
        }
    }
}

fn place_movable_predicate(tree: &JoinTree, referenced_slots: &[usize]) -> PredicatePlacement {
    let mut slots = referenced_slots.to_vec();
    slots.sort_unstable();
    slots.dedup();

    if slots.len() == 1 {
        return PredicatePlacement::Scan(slots[0]);
    }
    if slots.is_empty() {
        return PredicatePlacement::PostJoin;
    }

    let mut join_slots = Vec::new();
    collect_join_slot_sets(tree, &mut join_slots);
    join_slots
        .iter()
        .enumerate()
        .filter(|(_, candidate)| slots.iter().all(|slot| candidate.contains(slot)))
        .min_by_key(|(_, candidate)| candidate.len())
        .map(|(id, _)| PredicatePlacement::Join(id))
        .unwrap_or(PredicatePlacement::PostJoin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_catalog::ColumnStats;
    use htap_common::Value;
    use std::collections::HashMap;

    struct FakeStats {
        tables: HashMap<String, TableStats>,
    }

    impl StatsLookup for FakeStats {
        fn table_stats(&self, table: &str) -> Option<&TableStats> {
            self.tables.get(table)
        }
    }

    struct MissingStats;

    impl StatsLookup for MissingStats {
        fn table_stats(&self, _table: &str) -> Option<&TableStats> {
            None
        }
    }

    fn column(slot: usize, column: usize, name: &str) -> Expr {
        Expr::ColumnRef {
            slot,
            column,
            offset: slot,
            name: name.into(),
            data_type: DataType::Int64,
            nullable: false,
        }
    }

    fn int(value: i64) -> Expr {
        Expr::Literal(Value::Int64(value))
    }

    fn eq(left: Expr, right: Expr) -> Expr {
        Expr::BinaryOp {
            op: BinOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn and(left: Expr, right: Expr) -> Expr {
        Expr::BinaryOp {
            op: BinOp::And,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn leaf(slot: usize) -> JoinTree {
        JoinTree::Leaf(slot)
    }

    fn join(kind: JoinKind, left: JoinTree, right: JoinTree, on: Option<Expr>) -> JoinTree {
        JoinTree::Join {
            kind,
            left: Box::new(left),
            right: Box::new(right),
            on,
        }
    }

    fn select(join_tree: JoinTree, filter: Option<Expr>) -> SelectBody {
        SelectBody {
            slots: Vec::new(),
            join_tree,
            visible_schemas: Vec::new(),
            filter,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            windows: Vec::new(),
            having: None,
            projection: Vec::new(),
            distinct: false,
            correlated_outer_refs: Vec::new(),
        }
    }

    fn stats_lookup() -> FakeStats {
        let stats = TableStats {
            analyzed_at_version: 1,
            row_count: 1_000,
            columns: vec![
                ColumnStats {
                    null_count: 0,
                    distinct_count: Some(100),
                    min: Some(Value::Int64(1)),
                    max: Some(Value::Int64(1_000)),
                },
                ColumnStats {
                    null_count: 0,
                    distinct_count: Some(0),
                    min: None,
                    max: None,
                },
                ColumnStats {
                    null_count: 0,
                    distinct_count: None,
                    min: Some(Value::String("a".into())),
                    max: Some(Value::String("z".into())),
                },
            ],
        };

        FakeStats {
            tables: HashMap::from([("orders".to_string(), stats)]),
        }
    }

    #[test]
    fn extracts_simple_where_conjuncts() {
        let filter = and(eq(column(0, 0, "a"), int(1)), eq(column(1, 0, "b"), int(2)));
        let atoms = extract_predicate_atoms(&select(
            join(JoinKind::Inner, leaf(0), leaf(1), None),
            Some(filter),
        ));

        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].id, PredicateAtomId(0));
        assert_eq!(atoms[0].referenced_slots, vec![0]);
        assert_eq!(atoms[0].origin, PredicateOrigin::Where);
        assert_eq!(
            atoms[0].mobility,
            PredicateMobility::FreelyMovableWithinComponent
        );
        assert_eq!(atoms[1].id, PredicateAtomId(1));
        assert_eq!(atoms[1].referenced_slots, vec![1]);
        assert_eq!(atoms[1].origin, PredicateOrigin::Where);
        assert_eq!(
            atoms[1].mobility,
            PredicateMobility::FreelyMovableWithinComponent
        );
    }

    #[test]
    fn outer_join_on_conjuncts_are_pinned() {
        let on = and(
            eq(column(0, 0, "a.id"), column(1, 0, "b.id")),
            eq(column(1, 1, "b.active"), int(1)),
        );
        let atoms = extract_predicate_atoms(&select(
            join(JoinKind::Left, leaf(0), leaf(1), Some(on)),
            None,
        ));

        assert_eq!(atoms.len(), 2);
        for (id, atom) in atoms.iter().enumerate() {
            assert_eq!(atom.id, PredicateAtomId(id as u32));
            assert_eq!(atom.origin, PredicateOrigin::On { join_index: 0 });
            assert_eq!(
                atom.mobility,
                PredicateMobility::PinnedToJoin { join_id: 0 }
            );
        }
        assert_eq!(atoms[0].referenced_slots, vec![0, 1]);
        assert_eq!(atoms[1].referenced_slots, vec![1]);
    }

    #[test]
    fn inner_on_predicate_under_null_supplying_side_is_pinned() {
        let inner = join(
            JoinKind::Inner,
            leaf(1),
            leaf(2),
            Some(and(
                eq(column(1, 0, "b.id"), column(2, 0, "c.id")),
                eq(column(1, 1, "b.enabled"), int(1)),
            )),
        );
        let tree = join(
            JoinKind::Left,
            leaf(0),
            inner,
            Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
        );

        let atoms = extract_predicate_atoms(&select(tree, None));

        assert_eq!(atoms.len(), 3);
        assert_eq!(atoms[0].origin, PredicateOrigin::On { join_index: 0 });
        assert_eq!(
            atoms[0].mobility,
            PredicateMobility::PinnedToJoin { join_id: 0 }
        );
        assert_eq!(atoms[1].origin, PredicateOrigin::On { join_index: 0 });
        assert_eq!(
            atoms[1].mobility,
            PredicateMobility::PinnedToJoin { join_id: 0 }
        );
    }

    #[test]
    fn where_on_left_join_null_supplying_side_is_post_join_only() {
        let tree = join(
            JoinKind::Left,
            leaf(0),
            leaf(1),
            Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
        );
        let atoms = extract_predicate_atoms(&select(tree, Some(eq(column(1, 1, "b.x"), int(5)))));

        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].origin, PredicateOrigin::Where);
        assert_eq!(atoms[0].referenced_slots, vec![1]);
        assert_eq!(atoms[0].mobility, PredicateMobility::PostJoinOnly);
        assert_eq!(atoms[1].origin, PredicateOrigin::On { join_index: 0 });
        assert_eq!(
            atoms[1].mobility,
            PredicateMobility::PinnedToJoin { join_id: 0 }
        );
    }

    #[test]
    fn left_side_left_join_on_predicate_remains_pinned() {
        let on = and(
            eq(column(0, 0, "a.id"), column(1, 0, "b.id")),
            eq(column(0, 1, "a.enabled"), int(1)),
        );
        let atoms = extract_predicate_atoms(&select(
            join(JoinKind::Left, leaf(0), leaf(1), Some(on)),
            None,
        ));

        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[1].referenced_slots, vec![0]);
        assert_eq!(atoms[1].origin, PredicateOrigin::On { join_index: 0 });
        assert_eq!(
            atoms[1].mobility,
            PredicateMobility::PinnedToJoin { join_id: 0 }
        );
    }

    #[test]
    fn correlated_where_predicate_is_post_join_only() {
        let predicate = eq(
            column(0, 0, "a.id"),
            Expr::ScalarSubquery {
                index: 0,
                data_type: DataType::Int64,
                correlated: true,
            },
        );
        let atoms = extract_predicate_atoms(&select(leaf(0), Some(predicate)));

        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].id, PredicateAtomId(0));
        assert_eq!(atoms[0].origin, PredicateOrigin::Where);
        assert_eq!(atoms[0].referenced_slots, vec![0]);
        assert_eq!(atoms[0].mobility, PredicateMobility::PostJoinOnly);
    }

    #[test]
    fn predicate_spanning_outer_join_boundary_is_post_join_only() {
        let tree = join(
            JoinKind::Inner,
            join(
                JoinKind::Left,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
            ),
            leaf(2),
            None,
        );
        let predicate = eq(column(0, 1, "a.x"), column(1, 1, "b.x"));
        let atoms = extract_predicate_atoms(&select(tree, Some(predicate)));

        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].origin, PredicateOrigin::Where);
        assert_eq!(atoms[0].referenced_slots, vec![0, 1]);
        assert_eq!(atoms[0].mobility, PredicateMobility::PostJoinOnly);
        assert_eq!(atoms[1].origin, PredicateOrigin::On { join_index: 0 });
        assert_eq!(
            atoms[1].mobility,
            PredicateMobility::PinnedToJoin { join_id: 0 }
        );
    }

    #[test]
    fn estimates_row_count_from_statistics() {
        assert_eq!(
            estimate_row_count(&stats_lookup(), "orders").unwrap(),
            (1_000, EstimateSource::Stats)
        );
    }

    #[test]
    fn missing_row_count_is_an_error() {
        let error = estimate_row_count(&MissingStats, "missing").unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn estimates_equality_selectivity_from_distinct_count() {
        let (selectivity, source) = estimate_equality_selectivity(&stats_lookup(), "orders", 0);
        assert_eq!(selectivity, 0.01);
        assert_eq!(source, EstimateSource::Stats);
    }

    #[test]
    fn equality_selectivity_uses_defaults_for_unknown_statistics() {
        assert_eq!(
            estimate_equality_selectivity(&MissingStats, "orders", 0),
            (DEFAULT_EQUALITY_SELECTIVITY, EstimateSource::Default)
        );
        assert_eq!(
            estimate_equality_selectivity(&stats_lookup(), "orders", 99),
            (DEFAULT_EQUALITY_SELECTIVITY, EstimateSource::Default)
        );
        assert_eq!(
            estimate_equality_selectivity(&stats_lookup(), "orders", 2),
            (DEFAULT_EQUALITY_SELECTIVITY, EstimateSource::Default)
        );
    }

    #[test]
    fn equality_selectivity_is_clamped_to_one() {
        assert_eq!(
            estimate_equality_selectivity(&stats_lookup(), "orders", 1),
            (1.0, EstimateSource::Stats)
        );
    }

    #[test]
    fn estimates_supported_range_from_statistics() {
        assert_eq!(
            estimate_range_selectivity(&stats_lookup(), "orders", 0),
            (0.3, EstimateSource::Stats)
        );
    }

    #[test]
    fn range_selectivity_uses_defaults_for_unknown_or_unsupported_statistics() {
        assert_eq!(
            estimate_range_selectivity(&MissingStats, "orders", 0),
            (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default)
        );
        assert_eq!(
            estimate_range_selectivity(&stats_lookup(), "orders", 99),
            (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default)
        );
        assert_eq!(
            estimate_range_selectivity(&stats_lookup(), "orders", 2),
            (DEFAULT_RANGE_SELECTIVITY, EstimateSource::Default)
        );
    }

    #[test]
    fn estimates_join_cardinality() {
        assert_eq!(estimate_join_cardinality(1_000, 500, 0.1, 0.2), 10_000);
        assert_eq!(estimate_join_cardinality(10, 20, 1.0, 1.0), 200);
        assert_eq!(estimate_join_cardinality(0, 20, 1.0, 1.0), 1);
        assert_eq!(estimate_join_cardinality(10, 20, 0.0, 1.0), 1);
    }

    fn tree_slots(tree: &JoinTree) -> Vec<usize> {
        let mut slots = Vec::new();
        subtree_slots(tree, &mut slots);
        slots.sort_unstable();
        slots
    }

    #[test]
    fn test_predicate_conservation_catches_dropped_predicate() {
        let body = select(
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
            ),
            None,
        );
        let atoms = extract_predicate_atoms(&body);
        let reordered = ReorderedSelect {
            join_tree: body.join_tree.clone(),
            predicate_placements: Vec::new(),
            join_metadata: Vec::new(),
        };

        assert!(validate_predicate_conservation(&body, &atoms, &reordered).is_err());
    }

    #[test]
    fn predicate_conservation_rejects_hoisting_from_outer_join_null_side() {
        let body = select(
            join(
                JoinKind::Left,
                leaf(0),
                join(
                    JoinKind::Inner,
                    leaf(1),
                    leaf(2),
                    Some(eq(column(1, 1, "b.enabled"), int(1))),
                ),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
            ),
            None,
        );
        let atoms = extract_predicate_atoms(&body);
        let reordered = ReorderedSelect {
            join_tree: body.join_tree.clone(),
            predicate_placements: vec![
                (atoms[0].id, PredicatePlacement::Join(1)),
                (atoms[1].id, PredicatePlacement::Join(1)),
            ],
            join_metadata: Vec::new(),
        };

        assert!(validate_predicate_conservation(&body, &atoms, &reordered).is_err());
    }

    #[test]
    fn test_predicate_conservation_catches_duplicated_predicate() {
        let body = select(
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
            ),
            None,
        );
        let atoms = extract_predicate_atoms(&body);
        let placement = (atoms[0].id, PredicatePlacement::Join(0));
        let reordered = ReorderedSelect {
            join_tree: body.join_tree.clone(),
            predicate_placements: vec![placement, placement],
            join_metadata: Vec::new(),
        };

        assert!(validate_predicate_conservation(&body, &atoms, &reordered).is_err());
    }

    fn join_with_slots<'a>(tree: &'a JoinTree, target_slots: &[usize]) -> Option<&'a JoinTree> {
        let JoinTree::Join { left, right, .. } = tree else {
            return None;
        };

        let mut slots = Vec::new();
        subtree_slots(tree, &mut slots);
        slots.sort_unstable();
        slots.dedup();
        if slots == target_slots {
            return Some(tree);
        }

        join_with_slots(left, target_slots).or_else(|| join_with_slots(right, target_slots))
    }

    #[test]
    fn optimize_keeps_pinned_inner_on_predicate_on_an_inner_join() {
        let mut body = select(
            join(
                JoinKind::Left,
                leaf(0),
                join(
                    JoinKind::Inner,
                    leaf(1),
                    leaf(2),
                    Some(eq(column(1, 0, "c.id"), column(2, 0, "k.id"))),
                ),
                Some(eq(column(0, 0, "r.id"), column(1, 0, "c.id"))),
            ),
            None,
        );
        body.slots = vec![
            base_slot_with_columns("r", 1),
            base_slot_with_columns("c", 1),
            base_slot_with_columns("k", 1),
        ];

        let physical = optimize(&bound_query(body), &MissingStats);
        let optimized = physical.select.expect("select query should be optimized");
        let inner_pair =
            join_with_slots(&optimized.join_tree, &[1, 2]).expect("inner pair should remain");

        let JoinTree::Join { kind, on, .. } = inner_pair else {
            panic!("inner pair should be a join");
        };
        assert_eq!(*kind, JoinKind::Inner);
        let condition = on
            .as_ref()
            .expect("inner pair should retain its ON predicate");
        assert!(matches!(
            condition,
            Expr::BinaryOp {
                op: BinOp::Eq,
                left,
                right,
            } if matches!(
                left.as_ref(),
                Expr::ColumnRef {
                    slot: 1,
                    column: 0,
                    ..
                }
            ) && matches!(
                right.as_ref(),
                Expr::ColumnRef {
                    slot: 2,
                    column: 0,
                    ..
                }
            )
        ));
    }

    #[test]
    fn optimize_remaps_columns_when_join_predicate_moves() {
        let mut body = select(
            join(
                JoinKind::Inner,
                join(JoinKind::Cross, leaf(0), leaf(1), None),
                leaf(2),
                Some(eq(column(1, 0, "t1.id"), column(2, 0, "t2.id"))),
            ),
            None,
        );
        body.slots = vec![
            base_slot_with_columns("t0", 2),
            base_slot_with_columns("t1", 2),
            base_slot_with_columns("t2", 2),
        ];

        let original_tree = body.join_tree.clone();
        let physical = optimize(&bound_query(body), &MissingStats);
        let optimized = physical.select.expect("select query should be optimized");

        assert!(!physical.fallback);
        assert_ne!(optimized.join_tree, original_tree);
        assert_eq!(tree_slots(&optimized.join_tree), vec![0, 1, 2]);
    }

    #[test]
    fn test_existing_predicate_conservation_mutation_tests_still_pass() {
        let body = select(
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
            ),
            None,
        );
        let atoms = extract_predicate_atoms(&body);

        let dropped = ReorderedSelect {
            join_tree: body.join_tree.clone(),
            predicate_placements: Vec::new(),
            join_metadata: Vec::new(),
        };
        assert!(validate_predicate_conservation(&body, &atoms, &dropped).is_err());

        let placement = (atoms[0].id, PredicatePlacement::Join(0));
        let duplicated = ReorderedSelect {
            join_tree: body.join_tree.clone(),
            predicate_placements: vec![placement, placement],
            join_metadata: Vec::new(),
        };
        assert!(validate_predicate_conservation(&body, &atoms, &duplicated).is_err());
    }

    #[test]
    fn outer_join_hoisting_shape_passes_conservation_validation() {
        let body = select(
            join(
                JoinKind::Left,
                leaf(0),
                join(
                    JoinKind::Inner,
                    leaf(1),
                    leaf(2),
                    Some(and(
                        eq(column(1, 0, "b.id"), column(2, 0, "c.id")),
                        eq(column(1, 1, "b.enabled"), int(1)),
                    )),
                ),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
            ),
            None,
        );

        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert!(validate_predicate_conservation(
            &body,
            &extract_predicate_atoms(&body),
            &reordered
        )
        .is_ok());
        assert!(reordered
            .predicate_placements
            .iter()
            .take(2)
            .all(|(_, placement)| *placement == PredicatePlacement::Join(0)));
    }

    #[test]
    fn outer_join_sinking_shape_passes_conservation_validation() {
        let body = select(
            join(
                JoinKind::Inner,
                join(
                    JoinKind::Left,
                    leaf(0),
                    leaf(1),
                    Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
                ),
                leaf(2),
                Some(and(
                    eq(column(0, 0, "a.id"), column(2, 0, "c.id")),
                    eq(column(0, 1, "a.x"), column(1, 1, "b.x")),
                )),
            ),
            None,
        );

        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert!(validate_predicate_conservation(
            &body,
            &extract_predicate_atoms(&body),
            &reordered
        )
        .is_ok());
        assert!(reordered
            .predicate_placements
            .iter()
            .filter(|(id, _)| *id != PredicateAtomId(0))
            .all(|(_, placement)| *placement == PredicatePlacement::Join(1)));
    }

    #[test]
    fn reorders_four_table_star_without_losing_slots() {
        let tree = join(
            JoinKind::Inner,
            join(
                JoinKind::Inner,
                join(
                    JoinKind::Inner,
                    leaf(0),
                    leaf(1),
                    Some(eq(column(0, 0, "c.id"), column(1, 0, "a.c_id"))),
                ),
                leaf(2),
                Some(eq(column(0, 0, "c.id"), column(2, 0, "b.c_id"))),
            ),
            leaf(3),
            Some(eq(column(0, 0, "c.id"), column(3, 0, "d.c_id"))),
        );
        let body = select(tree.clone(), None);
        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert_eq!(tree_slots(&reordered.join_tree), vec![0, 1, 2, 3]);
        assert_eq!(reordered.join_metadata.len(), 3);
        assert_eq!(extract_predicate_atoms(&body).len(), 3);
        assert_eq!(reordered.predicate_placements.len(), 3);
    }

    #[test]
    fn reorders_four_table_chain_and_places_each_join_predicate() {
        let tree = join(
            JoinKind::Inner,
            join(
                JoinKind::Inner,
                join(
                    JoinKind::Inner,
                    leaf(0),
                    leaf(1),
                    Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
                ),
                leaf(2),
                Some(eq(column(1, 0, "b.id"), column(2, 0, "c.b_id"))),
            ),
            leaf(3),
            Some(eq(column(2, 0, "c.id"), column(3, 0, "d.c_id"))),
        );
        let body = select(tree, None);
        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert_eq!(tree_slots(&reordered.join_tree), vec![0, 1, 2, 3]);
        assert_eq!(reordered.predicate_placements.len(), 3);
        assert!(reordered
            .predicate_placements
            .iter()
            .all(|(_, placement)| matches!(placement, PredicatePlacement::Join(_))));
    }

    #[test]
    fn disconnected_equi_join_subgraphs_require_a_cross_join() {
        let tree = join(
            JoinKind::Inner,
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
            ),
            join(
                JoinKind::Inner,
                leaf(2),
                leaf(3),
                Some(eq(column(2, 0, "c.id"), column(3, 0, "d.c_id"))),
            ),
            None,
        );
        let body = select(tree, None);
        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert_eq!(tree_slots(&reordered.join_tree), vec![0, 1, 2, 3]);
        assert!(reordered
            .join_metadata
            .iter()
            .any(|metadata| metadata.kind == JoinKind::Cross));
    }

    #[test]
    fn mixed_cross_and_equi_joins_preserve_all_relations() {
        let tree = join(
            JoinKind::Cross,
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(column(0, 0, "a.id"), column(1, 0, "b.a_id"))),
            ),
            leaf(2),
            None,
        );
        let body = select(tree, None);
        let reordered = reorder_joins(&body, &MissingStats).expect("reordering should succeed");

        assert_eq!(tree_slots(&reordered.join_tree), vec![0, 1, 2]);
        assert_eq!(reordered.join_metadata.len(), 2);
        assert_eq!(reordered.predicate_placements.len(), 1);
    }

    fn bound_query(body: SelectBody) -> BoundQuery {
        BoundQuery {
            body: QueryBody::Select(body),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            subqueries: Vec::new(),
            correlated: false,
            correlated_outer_refs: Vec::new(),
            output_columns: Vec::new(),
        }
    }

    fn base_slot(table: &str) -> crate::query::TableSlot {
        crate::query::TableSlot::Base {
            table: table.into(),
            alias: table.into(),
            columns: Vec::new(),
        }
    }

    fn base_slot_with_columns(table: &str, width: usize) -> crate::query::TableSlot {
        crate::query::TableSlot::Base {
            table: table.into(),
            alias: table.into(),
            columns: (0..width)
                .map(|index| htap_common::ColumnDef {
                    name: format!("c{index}"),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: false,
                })
                .collect(),
        }
    }

    #[test]
    fn optimize_produces_valid_output_for_existing_fixture_shapes() {
        let fixtures = [
            select(leaf(0), Some(eq(column(0, 0, "a"), int(1)))),
            select(
                join(
                    JoinKind::Inner,
                    leaf(0),
                    leaf(1),
                    Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
                ),
                None,
            ),
            select(
                join(
                    JoinKind::Left,
                    leaf(0),
                    leaf(1),
                    Some(eq(column(0, 0, "a.id"), column(1, 0, "b.id"))),
                ),
                Some(eq(column(1, 1, "b.x"), int(5))),
            ),
        ];

        for body in fixtures {
            let atoms = extract_predicate_atoms(&body);
            let reordered = reorder_joins_unchecked(&body, &MissingStats, &atoms);
            assert!(validate_predicate_conservation(&body, &atoms, &reordered).is_ok());
        }
    }

    #[test]
    fn optimize_falls_back_when_conservation_validation_fails() {
        let body = select(
            join(
                JoinKind::Inner,
                leaf(0),
                leaf(1),
                Some(eq(
                    column(2, 0, "missing.id"),
                    column(3, 0, "also_missing.id"),
                )),
            ),
            None,
        );

        let physical = optimize(&bound_query(body.clone()), &MissingStats);

        assert!(physical.fallback);
        assert_eq!(physical.select, Some(body));
    }

    #[test]
    fn select_with_empty_slots_is_not_confused_with_empty_plan() {
        let body = select(leaf(0), None);

        let physical = optimize(&bound_query(body.clone()), &MissingStats);

        assert_eq!(physical.select, Some(body));
        assert!(!physical.fallback);
        assert_eq!(physical.node_estimates.len(), 1);
    }

    #[test]
    fn random_small_select_shapes_pass_conservation_validation() {
        let mut state = 0x5eed_u64;
        for relation_count in 1..=6 {
            for _ in 0..64 {
                let mut tree = leaf(0);
                let mut predicates = Vec::new();

                for slot in 1..relation_count {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let connected_to = (state as usize) % slot;
                    let predicate = eq(
                        column(connected_to, 0, "left.id"),
                        column(slot, 0, "right.id"),
                    );
                    predicates.push(predicate.clone());
                    tree = join(
                        if state & 1 == 0 {
                            JoinKind::Inner
                        } else {
                            JoinKind::Cross
                        },
                        tree,
                        leaf(slot),
                        (state & 1 == 0).then_some(predicate),
                    );
                }

                let filter = predicates.into_iter().reduce(and);
                let body = select(tree, filter);
                let atoms = extract_predicate_atoms(&body);
                let reordered = reorder_joins_unchecked(&body, &MissingStats, &atoms);
                assert!(validate_predicate_conservation(&body, &atoms, &reordered).is_ok());
            }
        }
    }

    #[test]
    fn pushdown_uses_cost_with_stats_and_first_match_without_stats() {
        let mut body = select(
            leaf(0),
            Some(and(
                eq(column(0, 2, "orders.unknown"), int(1)),
                eq(column(0, 0, "orders.id"), int(1)),
            )),
        );
        body.slots.push(base_slot("orders"));
        let query = bound_query(body);

        let with_stats = optimize(&query, &stats_lookup());
        assert_eq!(
            with_stats.pushdown_selections.get(&0),
            Some(&Some(LeafCandidateId(1)))
        );

        let without_stats = optimize(&query, &MissingStats);
        assert_eq!(
            without_stats.pushdown_selections.get(&0),
            Some(&Some(LeafCandidateId(0)))
        );
    }
}
