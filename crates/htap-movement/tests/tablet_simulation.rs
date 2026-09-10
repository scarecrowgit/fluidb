//! Integration tests for Phase 5 Task 5: Truthful logical tablet snapshot clone and repair simulation.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{
    encode_key, ColumnDef, DataType, HtapError, Mutation, Row, Schema, Value, Version,
};
use htap_movement::{
    clone_tablet, encode_manifest, repair_tablet, verify_package, LocalDataMover,
    TabletCloneOptions,
};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};
use tempfile::TempDir;

struct TestContext {
    _temp: TempDir,
    cat_dir: PathBuf,
    _row_dir: PathBuf,
    mover_dir: PathBuf,
    cat_store: Arc<LocalCatalogStore>,
    engine: Arc<Engine>,
    txn_manager: Arc<TransactionManager>,
    mover: LocalDataMover,
    table_id: TableId,
    part_id: PartitionId,
    tablet_id: TabletId,
    leader_rep_id: ReplicaId,
    target_rep_id: ReplicaId,
    schema: Schema,
}

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
            name: "balance".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn create_test_context() -> TestContext {
    let temp = tempfile::tempdir().unwrap();
    let cat_dir = temp.path().join("catalog");
    let row_dir = temp.path().join("rowstore");
    let journal_path = temp.path().join("txn.journal");
    let mover_dir = temp.path().join("movement");

    let cat_store = Arc::new(LocalCatalogStore::open(&cat_dir).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(&row_dir)).unwrap());
    let txn_manager = Arc::new(TransactionManager::open(journal_path).unwrap());

    // Register rowstore participant with participant ID 1
    let participant = Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    ));
    txn_manager.register_participant(participant);

    let mover = LocalDataMover::new(&mover_dir).unwrap();

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let leader_rep_id = ReplicaId::new(1000);
    let target_rep_id = ReplicaId::new(1001);

    let schema = create_schema();

    // Setup valid multi-replica catalog fixture:
    // Tablet has 2 replicas:
    // - Leader replica (1000): is_leader = true, healthy = true, generation = 1
    // - Follower target replica (1001): is_leader = false, healthy = false, generation = 1
    let table = TableDescriptor::new(
        table_id,
        "accounts",
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
    let tablet =
        TabletDescriptor::new(tablet_id, part_id, 0, vec![leader_rep_id, target_rep_id], 1);
    let leader_replica = ReplicaDescriptor::new(
        leader_rep_id,
        tablet_id,
        NodeId::new(1),
        true, // is_leader
        true, // healthy
        1,    // generation
    );
    let target_replica = ReplicaDescriptor::new(
        target_rep_id,
        tablet_id,
        NodeId::new(2),
        false, // is_leader
        false, // healthy: false (unhealthy target)
        1,     // generation
    );

    let snap = CatalogSnapshot::new(
        1,
        vec![table],
        vec![partition],
        vec![tablet],
        vec![leader_replica, target_replica],
    );
    cat_store.compare_and_set(0, snap).unwrap();

    TestContext {
        _temp: temp,
        cat_dir,
        _row_dir: row_dir,
        mover_dir,
        cat_store,
        engine,
        txn_manager,
        mover,
        table_id,
        part_id,
        tablet_id,
        leader_rep_id,
        target_rep_id,
        schema,
    }
}

/// Helper to commit mutations into rowstore via TransactionManager.
fn commit_mutations(ctx: &TestContext, mutations: Vec<Mutation>) {
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    let req = TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
        .unwrap();
    ctx.txn_manager.commit_request(req).unwrap();
}

#[test]
fn test_valid_multi_replica_fixture_and_snapshot_consistency() {
    let ctx = create_test_context();

    // Verify initial catalog state: target replica is unhealthy
    let snap = ctx.cat_store.load().unwrap().unwrap();
    let initial_target = snap.replica(ctx.target_rep_id).unwrap();
    assert!(
        !initial_target.healthy,
        "target replica must initially be unhealthy"
    );
    assert_eq!(initial_target.generation, 1);
    assert_eq!(snap.generation, 1);

    // 1. Commit initial data: 5 accounts
    // rows: (1, "User_1", 100) .. (5, "User_5", 500)
    let mut muts = Vec::new();
    for id in 1..=5 {
        let row = Row::new(vec![
            Value::Int32(id),
            Value::String(format!("User_{id}")),
            Value::Int32(id * 100),
        ]);
        let user_key = encode_key(&[Value::Int32(id)]).unwrap();
        muts.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: user_key,
            row,
        });
    }
    commit_mutations(&ctx, muts);

    // 2. Perform updates and tombstones before clone:
    // Update id=2 ("Bob_Updated", 250)
    // Delete id=4
    let row_2 = Row::new(vec![
        Value::Int32(2),
        Value::String("Bob_Updated".into()),
        Value::Int32(250),
    ]);
    let key_2 = encode_key(&[Value::Int32(2)]).unwrap();
    let key_4 = encode_key(&[Value::Int32(4)]).unwrap();
    commit_mutations(
        &ctx,
        vec![
            Mutation::Put {
                partition_id: ctx.part_id.as_u64(),
                key: key_2,
                row: row_2.clone(),
            },
            Mutation::Delete {
                partition_id: ctx.part_id.as_u64(),
                key: key_4,
            },
        ],
    );

    // 3. Clone tablet
    let options = TabletCloneOptions::new("job-clone-1", ctx.tablet_id, ctx.target_rep_id);
    let manifest = ctx
        .mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Verify manifest contents:
    assert_eq!(manifest.job_id, "job-clone-1");
    assert_eq!(manifest.table_id, ctx.table_id);
    assert_eq!(manifest.partition_id, ctx.part_id);
    assert_eq!(manifest.source_tablet_id, ctx.tablet_id);
    assert_eq!(manifest.target_replica_id, ctx.target_rep_id);
    assert_eq!(manifest.schema, ctx.schema);
    assert!(manifest.base_version.get() > 0);
    // Rows collapsed: 1, 2 (updated), 3, 5 (total 4 rows; 4 is tombstoned)
    assert_eq!(manifest.row_count, 4);
    assert!(manifest.payload_checksum > 0);
    assert!(manifest.payload_bytes > 0);

    // Verify package files exist on disk under movement root
    let manifest_path =
        ctx.mover
            .tablet_manifest_path(ctx.tablet_id, ctx.target_rep_id, "job-clone-1");
    let data_path = ctx
        .mover
        .tablet_data_path(ctx.tablet_id, ctx.target_rep_id, "job-clone-1");
    assert!(manifest_path.exists());
    assert!(data_path.exists());

    // 4. Snapshot consistency: Commit NEW rows (id 6..10) AFTER the clone
    let mut post_muts = Vec::new();
    for id in 6..=10 {
        let row = Row::new(vec![
            Value::Int32(id),
            Value::String(format!("User_{id}")),
            Value::Int32(id * 100),
        ]);
        let user_key = encode_key(&[Value::Int32(id)]).unwrap();
        post_muts.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: user_key,
            row,
        });
    }
    commit_mutations(&ctx, post_muts);

    // Verify the clone package still reflects strictly the snapshot rows (4 rows)
    let verified_manifest = ctx
        .mover
        .verify_package(&options, ctx.cat_store.as_ref())
        .unwrap();
    assert_eq!(verified_manifest.row_count, 4);

    let raw_data = fs::read(&data_path).unwrap();
    let rows: Vec<Row> = serde_json::from_slice(&raw_data).unwrap();
    assert_eq!(rows.len(), 4);
    let ids: Vec<i32> = rows
        .iter()
        .map(|r| match r.get(0).unwrap() {
            Value::Int32(v) => *v,
            _ => panic!("expected int32"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 5]);

    // Check id=2 has the updated value
    assert_eq!(
        rows[1].get(1).unwrap(),
        &Value::String("Bob_Updated".into())
    );
    assert_eq!(rows[1].get(2).unwrap(), &Value::Int32(250));

    // 5. Repair tablet
    let repaired_replica = ctx
        .mover
        .repair_tablet(&options, ctx.cat_store.as_ref())
        .unwrap();

    assert_eq!(repaired_replica.id, ctx.target_rep_id);
    assert!(repaired_replica.healthy);
    assert_eq!(repaired_replica.generation, 2);

    // Verify catalog snapshot after CAS:
    let final_snap = ctx.cat_store.load().unwrap().unwrap();
    assert_eq!(final_snap.generation, 2);

    let leader = final_snap.replica(ctx.leader_rep_id).unwrap();
    assert!(leader.is_leader);
    assert!(leader.healthy);
    assert_eq!(leader.generation, 1, "leader generation must not change");

    let follower = final_snap.replica(ctx.target_rep_id).unwrap();
    assert!(!follower.is_leader);
    assert!(follower.healthy, "target replica must now be healthy");
    assert_eq!(follower.generation, 2);

    // Topology is fully preserved:
    assert_eq!(final_snap.tables.len(), 1);
    assert_eq!(final_snap.partitions.len(), 1);
    assert_eq!(final_snap.tablets.len(), 1);
    assert_eq!(final_snap.replicas.len(), 2);
}

#[test]
fn test_corrupt_package_blocks_repair_then_regenerate_and_healthy_cas() {
    let ctx = create_test_context();

    // Insert 2 rows
    let row1 = Row::new(vec![
        Value::Int32(1),
        Value::String("A".into()),
        Value::Int32(10),
    ]);
    let row2 = Row::new(vec![
        Value::Int32(2),
        Value::String("B".into()),
        Value::Int32(20),
    ]);
    commit_mutations(
        &ctx,
        vec![
            Mutation::Put {
                partition_id: ctx.part_id.as_u64(),
                key: encode_key(&[Value::Int32(1)]).unwrap(),
                row: row1,
            },
            Mutation::Put {
                partition_id: ctx.part_id.as_u64(),
                key: encode_key(&[Value::Int32(2)]).unwrap(),
                row: row2,
            },
        ],
    );

    let options = TabletCloneOptions::new("job-corrupt-1", ctx.tablet_id, ctx.target_rep_id);
    ctx.mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Corrupt the DATA artifact file by flipping bytes
    let data_path = ctx
        .mover
        .tablet_data_path(ctx.tablet_id, ctx.target_rep_id, "job-corrupt-1");
    let mut data_bytes = fs::read(&data_path).unwrap();
    data_bytes[10] ^= 0xFF; // flip bits
    fs::write(&data_path, &data_bytes).unwrap();

    // verify_package must detect corruption
    let verify_err = ctx
        .mover
        .verify_package(&options, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(verify_err, HtapError::Corruption(_)),
        "expected corruption error, got {verify_err:?}"
    );

    // repair_tablet must refuse health restoration
    let repair_err = ctx
        .mover
        .repair_tablet(&options, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(repair_err, HtapError::Corruption(_)),
        "expected corruption error, got {repair_err:?}"
    );

    // Target replica remains unhealthy
    let snap_after_failed_repair = ctx.cat_store.load().unwrap().unwrap();
    let rep = snap_after_failed_repair.replica(ctx.target_rep_id).unwrap();
    assert!(!rep.healthy);
    assert_eq!(rep.generation, 1);
    assert_eq!(snap_after_failed_repair.generation, 1);

    // Regenerate clone with a new job
    let regen_options = TabletCloneOptions::new("job-regen-2", ctx.tablet_id, ctx.target_rep_id);
    let regen_manifest = ctx
        .mover
        .clone_tablet(&regen_options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(regen_manifest.row_count, 2);

    // Repair with the regenerated package: CAS succeeds and sets healthy = true
    let repaired_rep = ctx
        .mover
        .repair_tablet(&regen_options, ctx.cat_store.as_ref())
        .unwrap();
    assert!(repaired_rep.healthy);
    assert_eq!(repaired_rep.generation, 2);

    let final_snap = ctx.cat_store.load().unwrap().unwrap();
    assert_eq!(final_snap.generation, 2);
    assert!(final_snap.replica(ctx.target_rep_id).unwrap().healthy);
}

#[test]
fn test_corrupt_manifest_blocks_repair() {
    let ctx = create_test_context();

    let options = TabletCloneOptions::new("job-corrupt-mnf", ctx.tablet_id, ctx.target_rep_id);
    ctx.mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let manifest_path =
        ctx.mover
            .tablet_manifest_path(ctx.tablet_id, ctx.target_rep_id, "job-corrupt-mnf");
    let mut manifest_bytes = fs::read(&manifest_path).unwrap();
    // Corrupt the header magic
    manifest_bytes[0] ^= 0xFF;
    fs::write(&manifest_path, &manifest_bytes).unwrap();

    let err = ctx
        .mover
        .repair_tablet(&options, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    let snap = ctx.cat_store.load().unwrap().unwrap();
    assert!(!snap.replica(ctx.target_rep_id).unwrap().healthy);
}

#[test]
fn test_restart_and_idempotent_retry() {
    let ctx = create_test_context();

    // Insert 1 row
    let row = Row::new(vec![
        Value::Int32(1),
        Value::String("Z".into()),
        Value::Int32(99),
    ]);
    commit_mutations(
        &ctx,
        vec![Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: encode_key(&[Value::Int32(1)]).unwrap(),
            row,
        }],
    );

    let options = TabletCloneOptions::new("job-restart-1", ctx.tablet_id, ctx.target_rep_id);
    let original_manifest = ctx
        .mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let rep1 = ctx
        .mover
        .repair_tablet(&options, ctx.cat_store.as_ref())
        .unwrap();
    assert!(rep1.healthy);
    assert_eq!(rep1.generation, 2);

    // Simulate complete node / process restart:
    // Drop catalog store and mover, then reopen from disk
    drop(ctx.cat_store);
    drop(ctx.mover);

    let reopened_cat_store = Arc::new(LocalCatalogStore::open(&ctx.cat_dir).unwrap());
    let reopened_mover = LocalDataMover::new(&ctx.mover_dir).unwrap();

    // Re-running clone_tablet is idempotent: returns verified manifest
    let retry_manifest = reopened_mover
        .clone_tablet(&options, reopened_cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(retry_manifest, original_manifest);

    // Re-running repair_tablet is idempotent: detects target replica is already healthy
    let retry_rep = reopened_mover
        .repair_tablet(&options, reopened_cat_store.as_ref())
        .unwrap();
    assert!(retry_rep.healthy);
    assert_eq!(retry_rep.generation, 2);

    // Catalog snapshot generation remains 2 (no spurious CAS bump)
    let snap = reopened_cat_store.load().unwrap().unwrap();
    assert_eq!(snap.generation, 2);
}

#[test]
fn test_mismatch_rejections() {
    let ctx = create_test_context();

    // 1. Target ID mismatch
    let options = TabletCloneOptions::new("job-mismatch-1", ctx.tablet_id, ctx.target_rep_id);
    ctx.mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let wrong_target_opts = TabletCloneOptions::new(
        "job-mismatch-1",
        ctx.tablet_id,
        ReplicaId::new(9999), // wrong target replica ID
    );
    let err = ctx
        .mover
        .verify_package(&wrong_target_opts, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(matches!(
        err,
        HtapError::NotFound(_) | HtapError::Corruption(_)
    ));

    // 2. Source tablet ID mismatch
    let wrong_tablet_opts = TabletCloneOptions::new(
        "job-mismatch-1",
        TabletId::new(999), // wrong source tablet ID
        ctx.target_rep_id,
    );
    let err = ctx
        .mover
        .verify_package(&wrong_tablet_opts, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(matches!(
        err,
        HtapError::NotFound(_) | HtapError::Corruption(_)
    ));

    // 3. Base version mismatch
    let wrong_version_opts = options.clone().with_pinned_version(Version::new(999999));
    let err = ctx
        .mover
        .verify_package(&wrong_version_opts, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected corruption for base version mismatch, got {err:?}"
    );

    // 4. Schema mismatch
    // Clone with a valid job
    let options_schema =
        TabletCloneOptions::new("job-schema-check", ctx.tablet_id, ctx.target_rep_id);
    ctx.mover
        .clone_tablet(&options_schema, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Modify table schema in catalog
    let mut snap = ctx.cat_store.load().unwrap().unwrap();
    let old_gen = snap.generation;
    snap.generation += 1;
    let mut table = snap.tables[0].clone();
    let new_schema = Schema::new(vec![
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
            name: "balance".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "extra_col".into(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();
    table.schema = new_schema;
    snap.tables = vec![table];
    ctx.cat_store.compare_and_set(old_gen, snap).unwrap();

    // Now verify_package and repair_tablet must reject due to schema mismatch
    let err = ctx
        .mover
        .verify_package(&options_schema, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected corruption for schema mismatch, got {err:?}"
    );

    let err = ctx
        .mover
        .repair_tablet(&options_schema, ctx.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected corruption for schema mismatch, got {err:?}"
    );

    // 5. Row-count mismatch
    // Restore schema for another test
    let ctx2 = create_test_context();
    let options_row_count =
        TabletCloneOptions::new("job-rowcount-mismatch", ctx2.tablet_id, ctx2.target_rep_id);
    let mut manifest = ctx2
        .mover
        .clone_tablet(&options_row_count, ctx2.cat_store.as_ref(), &ctx2.engine)
        .unwrap();

    // Tamper with manifest row count in the file and re-encode with valid envelope
    let manifest_path = ctx2.mover.tablet_manifest_path(
        ctx2.tablet_id,
        ctx2.target_rep_id,
        "job-rowcount-mismatch",
    );
    manifest.row_count = 9999;
    let tampered_bytes = encode_manifest(&manifest).unwrap();
    fs::write(&manifest_path, &tampered_bytes).unwrap();

    let err = ctx2
        .mover
        .verify_package(&options_row_count, ctx2.cat_store.as_ref())
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected corruption for row-count mismatch, got {err:?}"
    );
}

#[test]
fn test_topology_validation_enforcement() {
    let temp = tempfile::tempdir().unwrap();
    let cat_dir = temp.path().join("catalog");
    let row_dir = temp.path().join("rowstore");
    let mover_dir = temp.path().join("movement");

    let cat_store = Arc::new(LocalCatalogStore::open(&cat_dir).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(&row_dir)).unwrap());
    let mover = LocalDataMover::new(&mover_dir).unwrap();

    let table_id = TableId::new(1);
    let p1 = PartitionId::new(10);
    let p2 = PartitionId::new(11);
    let t1 = TabletId::new(100);
    let r1 = ReplicaId::new(1000);
    let r2 = ReplicaId::new(1001);

    let schema = create_schema();

    // Multi-partition table (2 partitions) violates single-tablet local topology requirement
    let table = TableDescriptor::new(
        table_id,
        "multi_part",
        schema.clone(),
        vec![0],
        vec![p1, p2],
        1,
    );
    let part1 = PartitionDescriptor::new(p1, table_id, "p0", StorageDescriptor::Row, vec![t1], 1);
    let part2 = PartitionDescriptor::new(p2, table_id, "p1", StorageDescriptor::Row, vec![], 1);
    let tablet1 = TabletDescriptor::new(t1, p1, 0, vec![r1, r2], 1);
    let rep1 = ReplicaDescriptor::new(r1, t1, NodeId::new(1), true, true, 1);
    let rep2 = ReplicaDescriptor::new(r2, t1, NodeId::new(2), false, false, 1);

    let snap = CatalogSnapshot::new(
        1,
        vec![table],
        vec![part1, part2],
        vec![tablet1],
        vec![rep1, rep2],
    );
    cat_store.compare_and_set(0, snap).unwrap();

    let options = TabletCloneOptions::new("job-fail-topo", t1, r2);
    let err = mover
        .clone_tablet(&options, cat_store.as_ref(), &engine)
        .unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for multi-partition topology, got {err:?}"
    );
}

#[test]
fn test_free_functions_api() {
    let ctx = create_test_context();

    let options = TabletCloneOptions::new("job-free-fn", ctx.tablet_id, ctx.target_rep_id);

    // Call free functions directly
    let manifest = clone_tablet(&ctx.mover, &options, ctx.cat_store.as_ref(), &ctx.engine).unwrap();
    assert_eq!(manifest.job_id, "job-free-fn");

    let verified = verify_package(&ctx.mover, &options, ctx.cat_store.as_ref()).unwrap();
    assert_eq!(verified.job_id, "job-free-fn");

    let rep = repair_tablet(&ctx.mover, &options, ctx.cat_store.as_ref()).unwrap();
    assert!(rep.healthy);
    assert_eq!(rep.generation, 2);
}
