use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{ColumnDef, DataType, HtapError, Schema};
use htap_movement::{CopyOptions, DataFormat, LocalDataMover, TabletCloneOptions};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{ParticipantId, RowstoreParticipant, TransactionManager};
use tempfile::TempDir;

struct TestContext {
    _temp: TempDir,
    movement_root: PathBuf,
    cat_store: Arc<LocalCatalogStore>,
    engine: Arc<Engine>,
    tablet_id: TabletId,
    target_replica_id: ReplicaId,
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
    ])
    .unwrap()
}

fn create_test_context() -> (TestContext, LocalDataMover) {
    let temp = tempfile::tempdir().unwrap();
    let catalog_root = temp.path().join("catalog");
    let rowstore_root = temp.path().join("rowstore");
    let movement_root = temp.path().join("movement");

    let cat_store = Arc::new(LocalCatalogStore::open(catalog_root).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_root)).unwrap());
    let mover = LocalDataMover::new(&movement_root).unwrap();

    let table_id = TableId::new(1);
    let partition_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let leader_replica_id = ReplicaId::new(1000);
    let target_replica_id = ReplicaId::new(1001);
    let schema = create_schema();

    let table = TableDescriptor::new(table_id, "accounts", schema, vec![0], vec![partition_id], 1);
    let partition = PartitionDescriptor::new(
        partition_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(
        tablet_id,
        partition_id,
        0,
        vec![leader_replica_id, target_replica_id],
        1,
    );
    let leader =
        ReplicaDescriptor::new(leader_replica_id, tablet_id, NodeId::new(1), true, true, 1);
    let target = ReplicaDescriptor::new(
        target_replica_id,
        tablet_id,
        NodeId::new(2),
        false,
        false,
        1,
    );

    cat_store
        .compare_and_set(
            0,
            CatalogSnapshot::new(
                1,
                vec![table],
                vec![partition],
                vec![tablet],
                vec![leader, target],
            ),
        )
        .unwrap();

    (
        TestContext {
            _temp: temp,
            movement_root,
            cat_store,
            engine,
            tablet_id,
            target_replica_id,
        },
        mover,
    )
}

#[test]
fn clone_retry_with_same_job_id_succeeds_while_movement_lease_is_held() {
    let (ctx, mover) = create_test_context();
    let options = TabletCloneOptions::new("m1-clone-retry", ctx.tablet_id, ctx.target_replica_id);

    let first_manifest = mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let second_manifest = mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Without M1, this unwrap fails with Conflict because verify_package re-acquires the lease.
    assert_eq!(second_manifest, first_manifest);
}

#[test]
fn csv_import_blocked_by_reclaim_lease_does_not_persist_orphaned_job() {
    let temp = tempfile::tempdir().unwrap();
    let movement_root = temp.path().join("movement");
    let cat_store = Arc::new(LocalCatalogStore::open(temp.path().join("catalog")).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(temp.path().join("rowstore"))).unwrap());
    let txn_manager = Arc::new(TransactionManager::open(temp.path().join("txn.journal")).unwrap());
    txn_manager.register_participant(Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    )));
    let mover = LocalDataMover::new(&movement_root).unwrap();

    let table_id = TableId::new(1);
    let partition_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);
    let schema = create_schema();

    let table = TableDescriptor::new(
        table_id,
        "import_table",
        schema,
        vec![0],
        vec![partition_id],
        1,
    );
    let partition = PartitionDescriptor::new(
        partition_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, partition_id, 0, vec![replica_id], 1);
    let replica = ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, 1);

    cat_store
        .compare_and_set(
            0,
            CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]),
        )
        .unwrap();

    let job_id = "m2-blocked-import";
    let reclaim_lease = mover.try_acquire_reclaim_lease(&[tablet_id]).unwrap();
    let options = CopyOptions::new(
        job_id,
        table_id,
        tablet_id,
        DataFormat::Csv,
        "/unused/import.csv",
    );

    let err = mover
        .copy_from_csv_reader(
            &options,
            cat_store.as_ref(),
            txn_manager.as_ref(),
            Cursor::new(b"id,name\n1,blocked\n".to_vec()),
        )
        .unwrap_err();

    assert!(matches!(err, HtapError::Conflict(_)));

    // Without M2, start_job persists this directory before failing to acquire the lease.
    assert!(!movement_root.join("jobs").join(job_id).exists());

    drop(reclaim_lease);
}

#[test]
fn reclaim_can_delete_clone_artifacts_idempotently() {
    let (ctx, mover) = create_test_context();
    let job_id = "m3-reclaim-artifacts";
    let options = TabletCloneOptions::new(job_id, ctx.tablet_id, ctx.target_replica_id);

    mover
        .clone_tablet(&options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let package_dir = ctx
        .movement_root
        .join("tablets")
        .join(ctx.tablet_id.as_u64().to_string());
    let job_dir = ctx.movement_root.join("jobs").join(job_id);

    assert!(package_dir.exists());
    assert!(job_dir.exists());

    let reclaim_lease = mover.try_acquire_reclaim_lease(&[ctx.tablet_id]).unwrap();
    mover
        .delete_tablet_movement_artifacts(ctx.tablet_id)
        .unwrap();

    // Without M3, the server reclaim callback has no helper to remove these artifacts.
    assert!(!package_dir.exists());
    assert!(!job_dir.exists());

    mover
        .delete_tablet_movement_artifacts(ctx.tablet_id)
        .unwrap();

    drop(reclaim_lease);
}
