//! Deterministic replica placement planning and coordinator-mediated local activation flow.
//!
//! # Scope and Concurrency Guarantees
//! - **Pure Deterministic Placement**: Computes balanced, colocation-free replica placement plans
//!   over an immutable [`CatalogSnapshot`], candidate [`NodeId`] set, and target replication factor.
//!   Canonicalizes inputs (sorts candidates and tablet IDs in ascending order), preserves existing
//!   healthy placements, and greedily assigns additions to candidates with the least current load,
//!   breaking ties deterministically by smallest [`NodeId`].
//! - **Colocation & Ref Safety**: Strictly rejects duplicate/empty candidate lists, insufficient
//!   distinct nodes, unknown tablet references, or duplicate replica placement on the same node.
//! - **Monotonic Replica ID Allocation**: Allocates replica IDs starting from `(max_existing_id + 1)`
//!   with explicit arithmetic overflow validation.
//! - **Coordinator-Mediated Local Activation**:
//!   1. Validates coordinator leadership fencing token for the scope.
//!   2. Stages new target [`ReplicaDescriptor`] as non-leader (`is_leader = false`) and unhealthy
//!      (`healthy = false`) via coordinator-fenced atomic CAS.
//!   3. Performs logical snapshot clone and integrity verification using [`htap_movement::LocalDataMover`]
//!      and [`htap_rowstore::Engine`].
//!   4. Atomically transitions target replica to healthy (`healthy = true`) via coordinator-fenced CAS.
//! - **Truthful Local Scope**: Operates strictly within local engine, data mover, and coordinator
//!   storage. Distributed network transports, remote consensus handoffs, and physical engine copying
//!   are deferred; cluster topology and source leadership are strictly preserved.

use std::collections::{BTreeMap, BTreeSet};

use htap_catalog::store::CatalogStore;
use htap_catalog::{CatalogSnapshot, NodeId, ReplicaDescriptor, ReplicaId, TabletId};
use htap_common::{FencingToken, HtapError, Result};
use htap_movement::{clone_tablet, verify_package, LocalDataMover, TabletCloneOptions};
use htap_rowstore::Engine;
use serde::{Deserialize, Serialize};

use crate::Coordinator;

/// Descriptor of a single planned replica addition to a tablet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementAddition {
    /// Identifier of the tablet receiving the replica.
    pub tablet_id: TabletId,
    /// Candidate node assigned to host this replica.
    pub node_id: NodeId,
    /// Monotonically allocated replica identifier.
    pub replica_id: ReplicaId,
}

impl PlacementAddition {
    /// Create a new [`PlacementAddition`].
    pub fn new(tablet_id: TabletId, node_id: NodeId, replica_id: ReplicaId) -> Self {
        Self {
            tablet_id,
            node_id,
            replica_id,
        }
    }
}

/// Target placement specification for a single tablet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletPlacement {
    /// Unique identifier of the tablet.
    pub tablet_id: TabletId,
    /// All target node IDs hosting replicas for this tablet (preserved healthy + additions),
    /// sorted in deterministic ascending order.
    pub target_nodes: Vec<NodeId>,
    /// New replica additions planned for this tablet.
    pub additions: Vec<PlacementAddition>,
}

impl TabletPlacement {
    /// Create a new [`TabletPlacement`].
    pub fn new(
        tablet_id: TabletId,
        target_nodes: Vec<NodeId>,
        additions: Vec<PlacementAddition>,
    ) -> Self {
        Self {
            tablet_id,
            target_nodes,
            additions,
        }
    }
}

/// Complete deterministic placement plan across all catalog tablets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPlan {
    /// Target replication factor planned.
    pub replication_factor: usize,
    /// Per-tablet placement plans sorted by ascending [`TabletId`].
    pub tablets: Vec<TabletPlacement>,
    /// All replica additions across all tablets in planned execution order.
    pub additions: Vec<PlacementAddition>,
}

impl PlacementPlan {
    /// Create a new [`PlacementPlan`].
    pub fn new(
        replication_factor: usize,
        tablets: Vec<TabletPlacement>,
        additions: Vec<PlacementAddition>,
    ) -> Self {
        Self {
            replication_factor,
            tablets,
            additions,
        }
    }

    /// Return slice of all additions in the plan.
    pub fn additions(&self) -> &[PlacementAddition] {
        &self.additions
    }

    /// Return tablet placement for a given tablet ID, if present.
    pub fn tablet_placement(&self, tablet_id: TabletId) -> Option<&TabletPlacement> {
        self.tablets.iter().find(|tp| tp.tablet_id == tablet_id)
    }

    /// Whether this plan has zero additions.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty()
    }

    /// Total number of replica additions across all tablets.
    pub fn total_additions(&self) -> usize {
        self.additions.len()
    }
}

/// Compute a pure deterministic replica placement plan over a catalog snapshot.
///
/// # Invariants and Determinism
/// 1. Canonicalizes inputs: candidate node list and snapshot tablets are processed in strictly
///    ascending ID order.
/// 2. Validates inputs:
///    - `replication_factor` must be > 0.
///    - `candidates` must be non-empty and contain no duplicate IDs.
///    - `candidates.len()` must be >= `replication_factor`.
///    - `snapshot` must pass full semantic catalog validation.
///    - Snapshot cannot have colocation (multiple replicas of the same tablet on the same node).
/// 3. Preserves existing healthy replicas; unhealthy replicas do not count toward target replication
///    factor, but prevent colocation on their assigned nodes.
/// 4. Additions are assigned greedily to eligible candidate nodes with the least current load,
///    breaking ties deterministically by smallest [`NodeId`].
/// 5. Replica IDs are assigned sequentially starting from `max_existing_id + 1` with checked
///    overflow arithmetic.
pub fn plan_placement(
    snapshot: &CatalogSnapshot,
    candidates: &[NodeId],
    replication_factor: usize,
) -> Result<PlacementPlan> {
    if replication_factor == 0 {
        return Err(HtapError::InvalidArgument(
            "replication factor must be greater than 0".into(),
        ));
    }
    if candidates.is_empty() {
        return Err(HtapError::InvalidArgument(
            "candidate nodes list cannot be empty".into(),
        ));
    }

    // Check duplicate candidates
    let mut seen_candidates = BTreeSet::new();
    for &node_id in candidates {
        if !seen_candidates.insert(node_id) {
            return Err(HtapError::InvalidArgument(format!(
                "duplicate candidate node ID: {node_id}"
            )));
        }
    }

    if candidates.len() < replication_factor {
        return Err(HtapError::InvalidArgument(format!(
            "insufficient candidate nodes: have {}, replication factor requires {}",
            candidates.len(),
            replication_factor
        )));
    }

    // Canonical sorted candidates
    let sorted_candidates: Vec<NodeId> = seen_candidates.into_iter().collect();

    // Semantic catalog validation
    snapshot.validate()?;

    // Validate foreign keys and unknown tablet refs
    let known_tablet_ids: BTreeSet<TabletId> = snapshot.tablets.iter().map(|t| t.id).collect();
    for rep in &snapshot.replicas {
        if !known_tablet_ids.contains(&rep.tablet_id) {
            return Err(HtapError::InvalidArgument(format!(
                "replica {} references unknown tablet {}",
                rep.id, rep.tablet_id
            )));
        }
    }

    // Check colocation in existing snapshot
    for tablet in &snapshot.tablets {
        let mut seen_tablet_nodes = BTreeSet::new();
        for rep in snapshot.tablet_replicas(tablet.id) {
            if !seen_tablet_nodes.insert(rep.node_id) {
                return Err(HtapError::InvalidArgument(format!(
                    "colocation detected in catalog: tablet {} has multiple replicas on node {}",
                    tablet.id, rep.node_id
                )));
            }
        }
    }

    // Initial load map tracking healthy replicas on candidate nodes
    let mut load_map: BTreeMap<NodeId, usize> = sorted_candidates.iter().map(|&n| (n, 0)).collect();
    for rep in &snapshot.replicas {
        if rep.healthy {
            if let Some(cnt) = load_map.get_mut(&rep.node_id) {
                *cnt += 1;
            }
        }
    }

    // Allocate replica ids above the persisted high-water mark (never reuse ids of
    // removed replicas, whose movement packages may still exist on disk).
    let mut max_replica_id_u64 = snapshot.id_high_water().replica;

    // Canonicalize tablets: sort by TabletId ascending
    let mut sorted_tablets = snapshot.tablets.clone();
    sorted_tablets.sort_by_key(|t| t.id);

    let mut tablet_placements = Vec::with_capacity(sorted_tablets.len());
    let mut all_additions = Vec::new();

    for tablet in &sorted_tablets {
        let existing_reps = snapshot.tablet_replicas(tablet.id);

        // Nodes currently hosting any replica for this tablet (healthy or unhealthy) to avoid colocation
        let mut tablet_nodes: BTreeSet<NodeId> = existing_reps.iter().map(|r| r.node_id).collect();

        // Nodes hosting healthy replicas for this tablet
        let mut healthy_nodes: BTreeSet<NodeId> = existing_reps
            .iter()
            .filter(|r| r.healthy)
            .map(|r| r.node_id)
            .collect();

        let healthy_count = healthy_nodes.len();
        let needed = replication_factor.saturating_sub(healthy_count);

        let mut additions_for_tablet = Vec::with_capacity(needed);
        for _ in 0..needed {
            let eligible: Vec<NodeId> = sorted_candidates
                .iter()
                .copied()
                .filter(|n| !tablet_nodes.contains(n))
                .collect();

            if eligible.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "insufficient distinct nodes: cannot place {} replicas for tablet {} without colocation",
                    replication_factor, tablet.id
                )));
            }

            // Pick candidate with minimum current load, tie-breaking by smallest NodeId
            let chosen_node = *eligible
                .iter()
                .min_by_key(|&&n| load_map[&n])
                .expect("eligible must not be empty");

            *load_map.get_mut(&chosen_node).unwrap() += 1;
            tablet_nodes.insert(chosen_node);
            healthy_nodes.insert(chosen_node);

            max_replica_id_u64 =
                max_replica_id_u64
                    .checked_add(1)
                    .ok_or(HtapError::CounterOverflow {
                        counter: "replica_id",
                    })?;

            let addition =
                PlacementAddition::new(tablet.id, chosen_node, ReplicaId::new(max_replica_id_u64));
            additions_for_tablet.push(addition.clone());
            all_additions.push(addition);
        }

        tablet_placements.push(TabletPlacement::new(
            tablet.id,
            healthy_nodes.into_iter().collect(),
            additions_for_tablet,
        ));
    }

    Ok(PlacementPlan::new(
        replication_factor,
        tablet_placements,
        all_additions,
    ))
}

/// Stage a new replica addition into the catalog as non-leader and unhealthy under coordinator fence.
///
/// # Concurrency & Fencing
/// - Validates `token` against coordinator active leadership for `scope`.
/// - Inserts [`ReplicaDescriptor`] with `is_leader = false` and `healthy = false`.
/// - Updates catalog snapshot atomically via [`Coordinator::fenced_catalog_compare_and_set`].
/// - Idempotent retry: if the replica is already staged with identical tablet and node, returns
///   the existing descriptor.
pub fn stage_placement_addition(
    coordinator: &dyn Coordinator,
    scope: &str,
    token: FencingToken,
    catalog: &dyn CatalogStore,
    addition: &PlacementAddition,
) -> Result<ReplicaDescriptor> {
    coordinator.validate_fence(scope, token)?;

    let mut attempts = 0;
    loop {
        attempts += 1;
        let snapshot = catalog
            .load()?
            .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

        let _tablet = snapshot.tablet(addition.tablet_id).ok_or_else(|| {
            HtapError::NotFound(format!(
                "tablet {} not found in catalog",
                addition.tablet_id
            ))
        })?;

        if let Some(existing) = snapshot.replica(addition.replica_id) {
            if existing.tablet_id == addition.tablet_id && existing.node_id == addition.node_id {
                return Ok(existing.clone());
            } else {
                return Err(HtapError::Conflict(format!(
                    "replica {} already exists with different tablet or node",
                    addition.replica_id
                )));
            }
        }

        for rep in snapshot.tablet_replicas(addition.tablet_id) {
            if rep.node_id == addition.node_id {
                return Err(HtapError::Conflict(format!(
                    "tablet {} already has replica {} on node {}",
                    addition.tablet_id, rep.id, addition.node_id
                )));
            }
        }

        let staged = ReplicaDescriptor::new(
            addition.replica_id,
            addition.tablet_id,
            addition.node_id,
            false, // is_leader: non-leader
            false, // healthy: staged unready
            1,     // generation
        );

        let mut next_snapshot = snapshot.clone();
        for tab in &mut next_snapshot.tablets {
            if tab.id == addition.tablet_id {
                tab.replicas.push(addition.replica_id);
                break;
            }
        }
        next_snapshot.replicas.push(staged.clone());
        let mut high_water = next_snapshot.id_high_water();
        high_water.replica = high_water.replica.max(staged.id.get());
        next_snapshot.id_high_water = high_water;
        next_snapshot.generation =
            snapshot
                .generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;

        match coordinator.fenced_catalog_compare_and_set(
            scope,
            token,
            catalog,
            snapshot.generation,
            next_snapshot,
        ) {
            Ok(()) => return Ok(staged),
            Err(HtapError::Conflict(_)) if attempts < 5 => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Activate a replica addition locally under coordinator fencing.
///
/// # Workflow
/// 1. Validates coordinator fence token for `scope`.
/// 2. Stages target replica if not yet staged (`healthy = false`, `is_leader = false`).
/// 3. Clones source tablet logical snapshot into a durable package via [`clone_tablet`].
/// 4. Verifies package integrity and catalog consistency via [`verify_package`].
/// 5. Marks target replica healthy (`healthy = true`) via fenced CAS.
///
/// # Failure Semantics
/// If package cloning, verification, or fenced CAS fails, the target replica remains in
/// the catalog as unhealthy (`healthy = false`) and is not activated.
#[allow(clippy::too_many_arguments)]
pub fn activate_placement_addition(
    coordinator: &dyn Coordinator,
    scope: &str,
    token: FencingToken,
    catalog: &dyn CatalogStore,
    engine: &Engine,
    mover: &LocalDataMover,
    addition: &PlacementAddition,
    job_id: &str,
) -> Result<ReplicaDescriptor> {
    coordinator.validate_fence(scope, token)?;

    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    // Check if target replica already exists; stage if missing
    let target_rep = match snapshot.replica(addition.replica_id) {
        Some(rep) => {
            if rep.healthy {
                return Ok(rep.clone());
            }
            rep.clone()
        }
        None => stage_placement_addition(coordinator, scope, token, catalog, addition)?,
    };

    let clone_options = TabletCloneOptions::new(job_id, addition.tablet_id, addition.replica_id);

    // 1. Logical snapshot clone
    clone_tablet(mover, &clone_options, catalog, engine)?;

    // 2. Package integrity verification
    verify_package(mover, &clone_options, catalog)?;

    // 3. Fenced CAS to mark target replica healthy
    let mut attempts = 0;
    loop {
        attempts += 1;
        let snap = catalog
            .load()?
            .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

        let current_rep = snap.replica(target_rep.id).ok_or_else(|| {
            HtapError::NotFound(format!(
                "target replica {} not found in catalog",
                target_rep.id
            ))
        })?;

        if current_rep.healthy {
            return Ok(current_rep.clone());
        }

        let mut next_snap = snap.clone();
        let mut updated = None;
        for rep in &mut next_snap.replicas {
            if rep.id == target_rep.id {
                rep.healthy = true;
                rep.generation =
                    rep.generation
                        .checked_add(1)
                        .ok_or(HtapError::CounterOverflow {
                            counter: "replica_generation",
                        })?;
                updated = Some(rep.clone());
                break;
            }
        }
        let updated = updated.expect("target replica must exist");
        next_snap.generation =
            snap.generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;

        match coordinator.fenced_catalog_compare_and_set(
            scope,
            token,
            catalog,
            snap.generation,
            next_snap,
        ) {
            Ok(()) => return Ok(updated),
            Err(HtapError::Conflict(_)) if attempts < 5 => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Activate all replica additions in a placement plan sequentially under coordinator fencing.
#[allow(clippy::too_many_arguments)]
pub fn activate_placement_plan(
    coordinator: &dyn Coordinator,
    scope: &str,
    token: FencingToken,
    catalog: &dyn CatalogStore,
    engine: &Engine,
    mover: &LocalDataMover,
    plan: &PlacementPlan,
    job_prefix: &str,
) -> Result<Vec<ReplicaDescriptor>> {
    let mut activated = Vec::with_capacity(plan.additions.len());
    for (idx, addition) in plan.additions.iter().enumerate() {
        let job_id = format!(
            "{job_prefix}-{}-{}-{}",
            addition.tablet_id, addition.replica_id, idx
        );
        let rep = activate_placement_addition(
            coordinator,
            scope,
            token,
            catalog,
            engine,
            mover,
            addition,
            &job_id,
        )?;
        activated.push(rep);
    }
    Ok(activated)
}
