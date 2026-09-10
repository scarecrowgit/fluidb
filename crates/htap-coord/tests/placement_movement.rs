//! Integration tests for Phase 6 Task 2: Local deterministic placement and activation.

use std::fs;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{
    encode_key, ColumnDef, DataType, FencingToken, HtapError, Mutation, Row, Schema, Value,
};
use htap_coord::placement::{
    activate_placement_addition, activate_placement_plan, plan_placement, stage_placement_addition,
};
use htap_coord::{Coordinator, LocalCoordinator};
use htap_movement::LocalDataMover;
use htap_rowstore::{Engine, EngineOptions};
use tempfile::TempDir;

fn create_schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "name".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn create_single_tablet_fixture(
    table_id: TableId,
    part_id: PartitionId,
    tablet_id: TabletId,
    leader_rep_id: ReplicaId,
    leader_node_id: NodeId,
) -> (CatalogSnapshot, Schema) {
    let schema = create_schema();
    let table = TableDescriptor::new(
        table_id,
        "test_table",
        schema.clone(),
        vec![0],
        vec![part_id],
        1,
    );
    let partition = PartitionDescriptor::new(
        part_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, part_id, 0, vec![leader_rep_id], 1);
    let leader_replica = ReplicaDescriptor::new(
        leader_rep_id,
        tablet_id,
        leader_node_id,
        true, // is_leader
        true, // healthy
        1,
    );

    let snapshot = CatalogSnapshot::new(
        1,
        vec![table],
        vec![partition],
        vec![tablet],
        vec![leader_replica],
    );

    (snapshot, schema)
}

struct TestContext {
    _temp: TempDir,
    coord: LocalCoordinator,
    cat_store: Arc<LocalCatalogStore>,
    engine: Arc<Engine>,
    mover: LocalDataMover,
    tablet_id: TabletId,
    part_id: PartitionId,
    leader_rep_id: ReplicaId,
    _schema: Schema,
}

fn create_test_context() -> TestContext {
    let temp = tempfile::tempdir().unwrap();
    let coord_dir = temp.path().join("coord");
    let cat_dir = temp.path().join("catalog");
    let row_dir = temp.path().join("rowstore");
    let mover_dir = temp.path().join("movement");

    let coord = LocalCoordinator::open(coord_dir).unwrap();
    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(row_dir)).unwrap());
    let mover = LocalDataMover::new(mover_dir).unwrap();

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let leader_rep_id = ReplicaId::new(1000);
    let leader_node_id = NodeId::new(1);

    let (snap, schema) =
        create_single_tablet_fixture(table_id, part_id, tablet_id, leader_rep_id, leader_node_id);
    cat_store.compare_and_set(0, snap).unwrap();

    TestContext {
        _temp: temp,
        coord,
        cat_store,
        engine,
        mover,
        tablet_id,
        part_id,
        leader_rep_id,
        _schema: schema,
    }
}

#[test]
fn test_deterministic_equality_under_permuted_candidates_and_catalog() {
    let schema = create_schema();

    // Table 1: 2 partitions with 1 tablet each
    let t1_id = TableId::new(1);
    let p1_id = PartitionId::new(11);
    let p2_id = PartitionId::new(12);
    let tab1_id = TabletId::new(101);
    let tab2_id = TabletId::new(102);
    let r1_id = ReplicaId::new(1001);
    let r2_id = ReplicaId::new(1002);

    let t1 = TableDescriptor::new(
        t1_id,
        "users",
        schema.clone(),
        vec![0],
        vec![p1_id, p2_id],
        1,
    );
    let part1 =
        PartitionDescriptor::new(p1_id, t1_id, "p0", StorageDescriptor::Row, vec![tab1_id], 1);
    let part2 =
        PartitionDescriptor::new(p2_id, t1_id, "p1", StorageDescriptor::Row, vec![tab2_id], 1);
    let tab1 = TabletDescriptor::new(tab1_id, p1_id, 0, vec![r1_id], 1);
    let tab2 = TabletDescriptor::new(tab2_id, p2_id, 1, vec![r2_id], 1);
    let rep1 = ReplicaDescriptor::new(r1_id, tab1_id, NodeId::new(10), true, true, 1);
    let rep2 = ReplicaDescriptor::new(r2_id, tab2_id, NodeId::new(20), true, true, 1);

    // Table 2: 1 partition with 1 tablet
    let t2_id = TableId::new(2);
    let p3_id = PartitionId::new(21);
    let tab3_id = TabletId::new(201);
    let r3_id = ReplicaId::new(2001);

    let t2 = TableDescriptor::new(t2_id, "orders", schema, vec![0], vec![p3_id], 1);
    let part3 =
        PartitionDescriptor::new(p3_id, t2_id, "p0", StorageDescriptor::Row, vec![tab3_id], 1);
    let tab3 = TabletDescriptor::new(tab3_id, p3_id, 0, vec![r3_id], 1);
    let rep3 = ReplicaDescriptor::new(r3_id, tab3_id, NodeId::new(10), true, true, 1);

    let snapshot = CatalogSnapshot::new(
        1,
        vec![t1.clone(), t2.clone()],
        vec![part1.clone(), part2.clone(), part3.clone()],
        vec![tab1.clone(), tab2.clone(), tab3.clone()],
        vec![rep1.clone(), rep2.clone(), rep3.clone()],
    );

    let candidates = vec![
        NodeId::new(10),
        NodeId::new(20),
        NodeId::new(30),
        NodeId::new(40),
    ];
    let permuted_candidates = vec![
        NodeId::new(40),
        NodeId::new(10),
        NodeId::new(30),
        NodeId::new(20),
    ];

    let plan1 = plan_placement(&snapshot, &candidates, 3).unwrap();
    let plan2 = plan_placement(&snapshot, &permuted_candidates, 3).unwrap();
    assert_eq!(
        plan1, plan2,
        "placement plan must be strictly identical under permuted candidates"
    );

    // Permute snapshot order: reverse tables, partitions, tablets, replicas
    let permuted_snapshot = CatalogSnapshot::new(
        1,
        vec![t2, t1],
        vec![part3, part2, part1],
        vec![tab3, tab2, tab1],
        vec![rep3, rep2, rep1],
    );

    let plan3 = plan_placement(&permuted_snapshot, &candidates, 3).unwrap();
    assert_eq!(
        plan1, plan3,
        "placement plan must be strictly identical under permuted catalog snapshot vectors"
    );
}

#[test]
fn test_balanced_non_colocation_multi_tablet_fixture() {
    let schema = create_schema();

    // 4 tablets across 4 partitions
    let t_id = TableId::new(1);
    let mut parts = Vec::new();
    let mut tabs = Vec::new();
    let mut reps = Vec::new();
    let mut part_ids = Vec::new();

    // Initial placement:
    // Tab 1 -> Node 1
    // Tab 2 -> Node 2
    // Tab 3 -> Node 1
    // Tab 4 -> Node 2
    let initial_nodes = [
        NodeId::new(1),
        NodeId::new(2),
        NodeId::new(1),
        NodeId::new(2),
    ];

    for (i, &init_node) in initial_nodes.iter().enumerate() {
        let pid = PartitionId::new(10 + i as u64);
        let tid = TabletId::new(100 + i as u64);
        let rid = ReplicaId::new(1000 + i as u64);
        part_ids.push(pid);

        parts.push(PartitionDescriptor::new(
            pid,
            t_id,
            format!("p{i}"),
            StorageDescriptor::Row,
            vec![tid],
            1,
        ));
        tabs.push(TabletDescriptor::new(tid, pid, i as u32, vec![rid], 1));
        reps.push(ReplicaDescriptor::new(rid, tid, init_node, true, true, 1));
    }

    let table = TableDescriptor::new(t_id, "multi_tab", schema, vec![0], part_ids, 1);
    let snapshot = CatalogSnapshot::new(1, vec![table], parts, tabs, reps);

    let candidates = [
        NodeId::new(1),
        NodeId::new(2),
        NodeId::new(3),
        NodeId::new(4),
    ];
    let replication_factor = 3;

    let plan = plan_placement(&snapshot, &candidates, replication_factor).unwrap();

    assert_eq!(plan.replication_factor, 3);
    assert_eq!(plan.tablets.len(), 4);

    let mut total_replicas_per_node = std::collections::BTreeMap::new();
    for &node in &candidates {
        total_replicas_per_node.insert(node, 0);
    }

    for tp in &plan.tablets {
        // Must meet target replication factor
        assert_eq!(
            tp.target_nodes.len(),
            3,
            "tablet {} must have target replication factor 3",
            tp.tablet_id
        );

        // No colocation: target nodes must be strictly distinct
        let mut deduped = tp.target_nodes.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            tp.target_nodes.len(),
            "colocation detected in target nodes for tablet {}",
            tp.tablet_id
        );

        for &n in &tp.target_nodes {
            *total_replicas_per_node.get_mut(&n).unwrap() += 1;
        }

        // Each tablet had 1 healthy replica initially, so 2 additions needed
        assert_eq!(tp.additions.len(), 2);
    }

    // 4 tablets * 3 replicas = 12 total replicas across 4 nodes -> perfectly balanced at 3 replicas each
    for (node, count) in total_replicas_per_node {
        assert_eq!(
            count, 3,
            "node {} has {} replicas, expected exactly 3 for balanced placement",
            node, count
        );
    }
}

#[test]
fn test_invalid_replication_and_input() {
    let (snapshot, _) = create_single_tablet_fixture(
        TableId::new(1),
        PartitionId::new(10),
        TabletId::new(100),
        ReplicaId::new(1000),
        NodeId::new(1),
    );

    let valid_candidates = [NodeId::new(1), NodeId::new(2), NodeId::new(3)];

    // 1. Replication factor 0
    let err = plan_placement(&snapshot, &valid_candidates, 0).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "replication factor 0 must be rejected"
    );

    // 2. Empty candidates
    let err = plan_placement(&snapshot, &[], 2).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "empty candidates must be rejected"
    );

    // 3. Duplicate candidate IDs
    let err = plan_placement(
        &snapshot,
        &[NodeId::new(1), NodeId::new(2), NodeId::new(1)],
        2,
    )
    .unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "duplicate candidate must be rejected"
    );

    // 4. Insufficient candidates (fewer candidates than replication factor)
    let err = plan_placement(&snapshot, &[NodeId::new(1)], 2).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "insufficient candidates must be rejected"
    );

    // 5. Unknown tablet ref in snapshot
    let mut corrupt_snap = snapshot.clone();
    corrupt_snap.replicas.push(ReplicaDescriptor::new(
        ReplicaId::new(9999),
        TabletId::new(999),
        NodeId::new(2),
        false,
        true,
        1,
    ));
    let err = plan_placement(&corrupt_snap, &valid_candidates, 2).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "unknown tablet ref must be rejected"
    );

    // 6. Colocation in catalog snapshot (same tablet has multiple replicas on the same node)
    let mut collocated_snap = snapshot.clone();
    collocated_snap.replicas.push(ReplicaDescriptor::new(
        ReplicaId::new(1001),
        TabletId::new(100),
        NodeId::new(1), // same node as replica 1000
        false,
        true,
        1,
    ));
    collocated_snap.tablets[0]
        .replicas
        .push(ReplicaId::new(1001));
    let err = plan_placement(&collocated_snap, &valid_candidates, 2).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "colocation in snapshot must be rejected"
    );

    // 7. Insufficient distinct nodes to avoid colocation (candidate already hosts replica)
    // Tablet 100 is on Node 1; only candidate is Node 1; rep factor 1 is ok, but rep factor 2 needs addition
    let err = plan_placement(&snapshot, &[NodeId::new(1)], 1);
    assert!(
        err.is_ok(),
        "replication factor 1 with existing replica is ok"
    );
}

#[test]
fn test_target_staged_unhealthy_nonleader() {
    let ctx = create_test_context();
    let scope = "tablet:100";

    // Acquire coordinator leadership
    let leadership = ctx.coord.acquire_leadership(scope, NodeId::new(1)).unwrap();

    let snap = ctx.cat_store.load().unwrap().unwrap();
    let candidates = [NodeId::new(1), NodeId::new(2)];
    let plan = plan_placement(&snap, &candidates, 2).unwrap();

    assert_eq!(plan.additions.len(), 1);
    let addition = &plan.additions[0];
    assert_eq!(addition.node_id, NodeId::new(2));

    // Stage addition via fenced CAS
    let staged = stage_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        addition,
    )
    .unwrap();

    // Staged descriptor must be non-leader and unhealthy
    assert_eq!(staged.id, addition.replica_id);
    assert_eq!(staged.tablet_id, addition.tablet_id);
    assert_eq!(staged.node_id, NodeId::new(2));
    assert!(
        !staged.is_leader,
        "staged replica must be non-leader (is_leader = false)"
    );
    assert!(
        !staged.healthy,
        "staged replica must be unready (healthy = false)"
    );

    // Verify catalog snapshot state
    let updated_snap = ctx.cat_store.load().unwrap().unwrap();
    assert_eq!(updated_snap.generation, snap.generation + 1);

    let rep_in_cat = updated_snap.replica(addition.replica_id).unwrap();
    assert!(!rep_in_cat.is_leader);
    assert!(!rep_in_cat.healthy);

    // Leader replica must remain intact
    let leader_in_cat = updated_snap.replica(ctx.leader_rep_id).unwrap();
    assert!(leader_in_cat.is_leader);
    assert!(leader_in_cat.healthy);

    // Tablet descriptor references both replicas
    let tab = updated_snap.tablet(ctx.tablet_id).unwrap();
    assert_eq!(tab.replicas, vec![ctx.leader_rep_id, addition.replica_id]);

    // Idempotent re-staging succeeds and returns identical descriptor
    let restaged = stage_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        addition,
    )
    .unwrap();
    assert_eq!(restaged, staged);
}

#[test]
fn test_clone_verify_then_healthy_activation() {
    let ctx = create_test_context();
    let scope = "tablet:100";

    // Acquire coordinator leadership
    let leadership = ctx.coord.acquire_leadership(scope, NodeId::new(1)).unwrap();

    // Insert 5 rows into engine
    let mut mutations = Vec::new();
    for id in 1..=5 {
        let key = encode_key(&[Value::Int32(id)]).unwrap();
        let row = Row::new(vec![
            Value::Int32(id),
            Value::String(format!("row_{id}")),
            Value::Int64(id as i64 * 100),
        ]);
        mutations.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key,
            row,
        });
    }
    ctx.engine
        .commit(1, ctx.engine.snapshot(), mutations)
        .unwrap();

    let snap = ctx.cat_store.load().unwrap().unwrap();
    let candidates = [NodeId::new(1), NodeId::new(2)];
    let plan = plan_placement(&snap, &candidates, 2).unwrap();
    let addition = &plan.additions[0];

    // Run full coordinator-mediated local activation flow
    let activated = activate_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-act-1",
    )
    .unwrap();

    // Activated replica is now healthy!
    assert_eq!(activated.id, addition.replica_id);
    assert!(activated.healthy, "target replica must now be healthy");
    assert!(!activated.is_leader, "target replica remains non-leader");
    assert_eq!(activated.generation, 2, "target generation incremented");

    // Catalog state check
    let cat_snap = ctx.cat_store.load().unwrap().unwrap();
    let rep = cat_snap.replica(addition.replica_id).unwrap();
    assert!(rep.healthy);
    assert!(!rep.is_leader);

    // Source leader is preserved
    let leader = cat_snap.replica(ctx.leader_rep_id).unwrap();
    assert!(leader.healthy);
    assert!(leader.is_leader);

    // Idempotent re-activation returns immediately
    let reactivated = activate_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-act-1",
    )
    .unwrap();
    assert_eq!(reactivated, activated);
}

#[test]
fn test_corrupt_or_fence_failure_leaves_target_unready() {
    let ctx = create_test_context();
    let scope = "tablet:100";

    let leadership = ctx.coord.acquire_leadership(scope, NodeId::new(1)).unwrap();

    // Insert 1 row so clone has valid payload
    let key = encode_key(&[Value::Int32(42)]).unwrap();
    let row = Row::new(vec![
        Value::Int32(42),
        Value::String("val".into()),
        Value::Int64(999),
    ]);
    ctx.engine
        .commit(
            1,
            ctx.engine.snapshot(),
            vec![Mutation::Put {
                partition_id: ctx.part_id.as_u64(),
                key,
                row,
            }],
        )
        .unwrap();

    let snap = ctx.cat_store.load().unwrap().unwrap();
    let candidates = [NodeId::new(1), NodeId::new(2)];
    let plan = plan_placement(&snap, &candidates, 2).unwrap();
    let addition = &plan.additions[0];

    // Stage target replica first
    stage_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        addition,
    )
    .unwrap();

    // 1. Test corruption failure: clone package, then corrupt DATA artifact
    let clone_opts = htap_movement::TabletCloneOptions::new(
        "job-corrupt-data",
        addition.tablet_id,
        addition.replica_id,
    );
    htap_movement::clone_tablet(&ctx.mover, &clone_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let data_path = ctx
        .mover
        .tablet_data_path(addition.tablet_id, addition.replica_id, "job-corrupt-data")
        .unwrap();
    // Write corrupt bytes into data file
    fs::write(&data_path, b"corrupt-data-payload").unwrap();

    // Now attempt activation with the corrupted package
    let err = activate_placement_addition(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-corrupt-data",
    )
    .unwrap_err();

    assert!(
        matches!(err, HtapError::Corruption(_)),
        "corrupt package must trigger HtapError::Corruption, got {err:?}"
    );

    // Target replica MUST remain unready (healthy = false)
    let cat_snap = ctx.cat_store.load().unwrap().unwrap();
    let rep = cat_snap.replica(addition.replica_id).unwrap();
    assert!(
        !rep.healthy,
        "target replica must remain unready when package is corrupt"
    );

    // 2. Test fence failure: attempt activation with invalid token
    let stale_token = FencingToken::new(9999);
    let err = activate_placement_addition(
        &ctx.coord,
        scope,
        stale_token,
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-new",
    )
    .unwrap_err();

    assert!(
        matches!(err, HtapError::Fenced { .. }),
        "stale token must trigger HtapError::Fenced, got {err:?}"
    );

    // Target replica STILL remains unready
    let cat_snap2 = ctx.cat_store.load().unwrap().unwrap();
    let rep2 = cat_snap2.replica(addition.replica_id).unwrap();
    assert!(
        !rep2.healthy,
        "target replica must remain unready after fence failure"
    );
}

#[test]
fn test_leadership_turnover_stale_token_rejects() {
    let ctx = create_test_context();
    let scope = "tablet:100";

    // Node 1 acquires leadership -> token 1
    let leader_1 = ctx.coord.acquire_leadership(scope, NodeId::new(1)).unwrap();
    let stale_token = leader_1.token();

    // Leadership turnover: Node 2 replaces Node 1 -> token 2
    let leader_2 = ctx.coord.replace_leadership(scope, NodeId::new(2)).unwrap();
    assert!(leader_2.token().get() > stale_token.get());

    let snap = ctx.cat_store.load().unwrap().unwrap();
    let candidates = [NodeId::new(1), NodeId::new(2)];
    let plan = plan_placement(&snap, &candidates, 2).unwrap();
    let addition = &plan.additions[0];

    // Attempting to stage with stale token rejects with Fenced
    let err = stage_placement_addition(
        &ctx.coord,
        scope,
        stale_token,
        ctx.cat_store.as_ref(),
        addition,
    )
    .unwrap_err();
    assert!(
        matches!(err, HtapError::Fenced { .. }),
        "staging with stale token must reject with HtapError::Fenced, got {err:?}"
    );

    // Catalog remains unmodified
    let current_snap = ctx.cat_store.load().unwrap().unwrap();
    assert_eq!(current_snap.generation, snap.generation);
    assert!(current_snap.replica(addition.replica_id).is_none());

    // Attempting activation with stale token rejects with Fenced
    let err = activate_placement_addition(
        &ctx.coord,
        scope,
        stale_token,
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-turnover-1",
    )
    .unwrap_err();
    assert!(
        matches!(err, HtapError::Fenced { .. }),
        "activation with stale token must reject with HtapError::Fenced, got {err:?}"
    );

    // Staging and activating with the new leader token succeeds
    let activated = activate_placement_addition(
        &ctx.coord,
        scope,
        leader_2.token(),
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        addition,
        "job-turnover-2",
    )
    .unwrap();
    assert!(activated.healthy);
}

#[test]
fn test_activate_placement_plan_helper() {
    let ctx = create_test_context();
    let scope = "tablet:100";

    let leadership = ctx.coord.acquire_leadership(scope, NodeId::new(1)).unwrap();

    let snap = ctx.cat_store.load().unwrap().unwrap();
    let candidates = [NodeId::new(1), NodeId::new(2)];
    let plan = plan_placement(&snap, &candidates, 2).unwrap();

    let activated_replicas = activate_placement_plan(
        &ctx.coord,
        scope,
        leadership.token(),
        ctx.cat_store.as_ref(),
        &ctx.engine,
        &ctx.mover,
        &plan,
        "job-plan-batch",
    )
    .unwrap();

    assert_eq!(activated_replicas.len(), 1);
    assert!(activated_replicas[0].healthy);
}

#[test]
fn test_placement_overflow_boundaries_no_catalog_mutation() {
    use htap_coord::placement::PlacementAddition;

    let tmp_coord = TempDir::new().unwrap();
    let tmp_cat = TempDir::new().unwrap();
    let coord = LocalCoordinator::open(tmp_coord.path()).unwrap();
    let catalog = LocalCatalogStore::open(tmp_cat.path()).unwrap();

    let scope = "cluster";
    let leader = coord.acquire_leadership(scope, NodeId::new(1)).unwrap();

    // 1. plan_placement replica_id overflow
    let (mut snap, _) = create_single_tablet_fixture(
        TableId::new(1),
        PartitionId::new(1),
        TabletId::new(1),
        ReplicaId::new(1),
        NodeId::new(1),
    );
    snap.replicas[0].id = ReplicaId::new(u64::MAX);
    snap.tablets[0].replicas = vec![ReplicaId::new(u64::MAX)];
    catalog.compare_and_set(0, snap.clone()).unwrap();

    let plan_err =
        plan_placement(&snap, &[NodeId::new(1), NodeId::new(2), NodeId::new(3)], 2).unwrap_err();
    assert!(matches!(
        plan_err,
        HtapError::CounterOverflow {
            counter: "replica_id"
        }
    ));

    // 2. stage_placement_addition catalog generation overflow
    let tmp_cat2 = TempDir::new().unwrap();
    let catalog2 = LocalCatalogStore::open(tmp_cat2.path()).unwrap();
    let (mut snap_gen_max, _) = create_single_tablet_fixture(
        TableId::new(1),
        PartitionId::new(1),
        TabletId::new(1),
        ReplicaId::new(1),
        NodeId::new(1),
    );
    snap_gen_max.generation = u64::MAX;
    catalog2.compare_and_set(0, snap_gen_max.clone()).unwrap();

    let addition = PlacementAddition::new(TabletId::new(1), NodeId::new(2), ReplicaId::new(100));
    let stage_err =
        stage_placement_addition(&coord, scope, leader.token, &catalog2, &addition).unwrap_err();
    assert!(matches!(
        stage_err,
        HtapError::CounterOverflow {
            counter: "catalog_generation"
        }
    ));
    // Verify catalog generation was not mutated
    assert_eq!(catalog2.current_generation().unwrap(), u64::MAX);

    // 3. activate_placement_addition replica generation overflow
    let tmp_cat3 = TempDir::new().unwrap();
    let catalog3 = LocalCatalogStore::open(tmp_cat3.path()).unwrap();
    let mover_dir = TempDir::new().unwrap();
    let mover = LocalDataMover::new(mover_dir.path()).unwrap();
    let rowstore_dir = TempDir::new().unwrap();
    let rowstore = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());

    let (mut snap_rep_gen, _) = create_single_tablet_fixture(
        TableId::new(1),
        PartitionId::new(1),
        TabletId::new(1),
        ReplicaId::new(1),
        NodeId::new(1),
    );
    // Add target replica staged unready with generation u64::MAX
    let staged_max = ReplicaDescriptor::new(
        ReplicaId::new(200),
        TabletId::new(1),
        NodeId::new(2),
        false,
        false,
        u64::MAX,
    );
    snap_rep_gen.replicas.push(staged_max);
    snap_rep_gen.tablets[0].replicas.push(ReplicaId::new(200));
    catalog3.compare_and_set(0, snap_rep_gen.clone()).unwrap();

    let addition_rep =
        PlacementAddition::new(TabletId::new(1), NodeId::new(2), ReplicaId::new(200));
    let act_err = activate_placement_addition(
        &coord,
        scope,
        leader.token,
        &catalog3,
        rowstore.as_ref(),
        &mover,
        &addition_rep,
        "job-test",
    )
    .unwrap_err();
    assert!(matches!(
        act_err,
        HtapError::CounterOverflow {
            counter: "replica_generation"
        }
    ));
    // Verify catalog generation was not mutated
    assert_eq!(catalog3.current_generation().unwrap(), 1);
}
