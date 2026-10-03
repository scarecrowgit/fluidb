use std::fs;
use std::path::Path;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::fs::dur::rename;
use htap_common::fs::{create_dir_all_durable, write_new_tmp_file};
use htap_common::{encode_key, ColumnDef, DataType, Mutation, Row, Schema, Value};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};
use htap_movement::{decode_job, encode_job, LocalDataMover, MovementJobPhase, TabletCloneOptions};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const SOURCE_TABLET_ID: TabletId = TabletId::new(2);
const LEADER_REPLICA_ID: ReplicaId = ReplicaId::new(1);
const TARGET_REPLICA_ID: ReplicaId = ReplicaId::new(2);

fn acked(info: &CrashInfo, label: &str) -> bool {
    info.acked_labels.iter().any(|value| value == label)
}

fn schema() -> Schema {
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

fn setup_clone_workload(root: &Path) -> (Arc<LocalCatalogStore>, Arc<Engine>, LocalDataMover) {
    let catalog_root = root.join("catalog");
    let rowstore_root = root.join("rowstore");
    let movement_root = root.join("movement");

    create_dir_all_durable(&catalog_root).unwrap();
    create_dir_all_durable(&rowstore_root).unwrap();
    create_dir_all_durable(&movement_root).unwrap();

    let catalog = Arc::new(LocalCatalogStore::open(catalog_root).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_root)).unwrap());
    let txn_manager = TransactionManager::open(root.join("txn.journal")).unwrap();
    txn_manager.register_participant(Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    )));

    let table = TableDescriptor::new(
        TABLE_ID,
        "accounts",
        schema(),
        vec![0],
        vec![PARTITION_ID],
        1,
    );
    let partition = PartitionDescriptor::new(
        PARTITION_ID,
        TABLE_ID,
        "p0",
        StorageDescriptor::Row,
        vec![SOURCE_TABLET_ID],
        1,
    );
    let tablet = TabletDescriptor::new(
        SOURCE_TABLET_ID,
        PARTITION_ID,
        0,
        vec![LEADER_REPLICA_ID, TARGET_REPLICA_ID],
        1,
    );
    let leader = ReplicaDescriptor::new(
        LEADER_REPLICA_ID,
        SOURCE_TABLET_ID,
        NodeId::new(1),
        true,
        true,
        1,
    );
    let target = ReplicaDescriptor::new(
        TARGET_REPLICA_ID,
        SOURCE_TABLET_ID,
        NodeId::new(2),
        false,
        false,
        1,
    );
    catalog
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

    let mutations = [("alpha", 1), ("beta", 2), ("gamma", 3)]
        .into_iter()
        .map(|(name, id)| Mutation::Put {
            partition_id: PARTITION_ID.as_u64(),
            key: encode_key(&[Value::Int32(id)]).unwrap(),
            row: Row::new(vec![Value::Int32(id), Value::String(name.into())]),
        })
        .collect::<Vec<Mutation>>();
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    let request =
        TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
            .unwrap();
    txn_manager.commit_request(request).unwrap();

    (catalog, engine, LocalDataMover::new(movement_root).unwrap())
}

fn write_published_file_without_dir_sync(path: &Path, bytes: &[u8]) {
    let tmp_path = path.with_extension("tmp");
    write_new_tmp_file(&tmp_path, bytes, None).unwrap();
    rename(&tmp_path, path).unwrap();
}

fn install_unsynced_clone_lifetime(
    _root: &Path,
    catalog: &LocalCatalogStore,
    mover: &LocalDataMover,
    job_id: &str,
) {
    let scratch = tempfile::tempdir().unwrap();
    let (scratch_catalog, scratch_engine, scratch_mover) = setup_clone_workload(scratch.path());
    let options = TabletCloneOptions::new(job_id, SOURCE_TABLET_ID, TARGET_REPLICA_ID);

    let scratch_manifest = scratch_mover
        .clone_tablet(&options, scratch_catalog.as_ref(), &scratch_engine)
        .unwrap();
    assert_eq!(scratch_manifest.source_tablet_id, SOURCE_TABLET_ID);
    assert_eq!(scratch_manifest.target_replica_id, TARGET_REPLICA_ID);
    assert_eq!(
        scratch_catalog.load().unwrap(),
        catalog.load().unwrap(),
        "scratch clone catalog must match the recorded workload catalog"
    );

    let scratch_data_path = scratch_mover
        .tablet_data_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap();
    let scratch_manifest_path = scratch_mover
        .tablet_manifest_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap();
    let scratch_job_path = scratch_mover.job_file_path(job_id).unwrap();

    let data_bytes = fs::read(scratch_data_path).unwrap();
    let manifest_bytes = fs::read(scratch_manifest_path).unwrap();
    let mut job = decode_job(&fs::read(scratch_job_path).unwrap()).unwrap();
    assert!(job.is_complete());
    job.phase = MovementJobPhase::Running;
    job.report = None;
    job.error = None;
    job.request.path = mover
        .tablet_manifest_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap();
    let job_bytes = encode_job(&job).unwrap();

    let package_dir = mover
        .tablet_package_dir(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap();
    let job_dir = mover.job_dir(job_id).unwrap();
    create_dir_all_durable(&package_dir).unwrap();
    create_dir_all_durable(&job_dir).unwrap();

    write_published_file_without_dir_sync(
        &mover
            .tablet_data_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
            .unwrap(),
        &data_bytes,
    );
    write_published_file_without_dir_sync(
        &mover
            .tablet_manifest_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
            .unwrap(),
        &manifest_bytes,
    );
    write_published_file_without_dir_sync(&mover.job_file_path(job_id).unwrap(), &job_bytes);

    mover.verify_package(&options, catalog).unwrap();
    assert!(mover.load_job(job_id).unwrap().unwrap().is_running());
}

fn package_presence(root: &Path, job_id: &str) -> (bool, bool) {
    let mover = LocalDataMover::new(root.join("movement")).unwrap();
    let manifest = mover
        .tablet_manifest_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap()
        .exists();
    let data = mover
        .tablet_data_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, job_id)
        .unwrap()
        .exists();

    (manifest, data)
}

fn target_replica_is_healthy(catalog: &LocalCatalogStore) -> bool {
    catalog
        .load()
        .unwrap()
        .unwrap()
        .replicas
        .iter()
        .find(|replica| replica.id == TARGET_REPLICA_ID)
        .map(|replica| replica.healthy)
        .unwrap_or(false)
}

#[test]
fn repair_after_unsynced_publish_requires_durable_package() {
    let harness =
        CrashHarness::new("repair_after_unsynced_publish_requires_durable_package").unwrap();
    let job_id = "repair-clone-job";

    harness
        .run_workload(|workload| {
            let (catalog, engine, mover) = setup_clone_workload(workload.root());
            let options = TabletCloneOptions::new(job_id, SOURCE_TABLET_ID, TARGET_REPLICA_ID);

            let _ = engine;
            install_unsynced_clone_lifetime(workload.root(), catalog.as_ref(), &mover, job_id);
            mover.repair_tablet(&options, catalog.as_ref()).unwrap();
            workload.ack("repaired");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;

            let catalog = LocalCatalogStore::open(root.join("catalog")).unwrap();
            let mover = LocalDataMover::new(root.join("movement")).unwrap();
            let options = TabletCloneOptions::new(job_id, SOURCE_TABLET_ID, TARGET_REPLICA_ID);
            let has_catalog_snapshot = catalog.load().unwrap().is_some();

            if has_catalog_snapshot && target_replica_is_healthy(&catalog) {
                assert_eq!(
                    package_presence(root, job_id),
                    (true, true),
                    "a healthy replica requires a durable clone package"
                );
                mover.verify_package(&options, &catalog).unwrap();
            }

            if acked(info, "repaired") {
                checked_with_ack = true;
                assert!(
                    has_catalog_snapshot && target_replica_is_healthy(&catalog),
                    "acknowledged repair must publish target replica health"
                );
                assert_eq!(
                    package_presence(root, job_id),
                    (true, true),
                    "an acknowledged repair requires a durable clone package"
                );
                mover.verify_package(&options, &catalog).unwrap();
            }
        })
        .unwrap();

    assert!(checked_count > 1, "expected multiple crash images");
    assert!(checked_with_ack, "expected an acknowledged repaired image");
}
