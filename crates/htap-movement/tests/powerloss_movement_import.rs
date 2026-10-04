use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::fs::create_dir_all_durable;
use htap_common::{ColumnDef, DataType, HtapError, Schema};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};
use htap_movement::{
    copy_from_jsonl_reader, CopyOptions, DataFormat, LocalDataMover, MovementJob, MovementJobPhase,
};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{ParticipantId, RowstoreParticipant, TransactionManager};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const TABLET_ID: TabletId = TabletId::new(1);
const REPLICA_ID: ReplicaId = ReplicaId::new(1);

fn options(root: &Path, job_id: &str) -> CopyOptions {
    CopyOptions::new(
        job_id,
        TABLE_ID,
        TABLET_ID,
        DataFormat::JsonLines,
        root.join("input.jsonl"),
    )
}

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

fn setup_import_workload(
    root: &Path,
) -> (
    Arc<LocalCatalogStore>,
    Arc<Engine>,
    TransactionManager,
    LocalDataMover,
) {
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
        vec![TABLET_ID],
        1,
    );
    let tablet = TabletDescriptor::new(TABLET_ID, PARTITION_ID, 0, vec![REPLICA_ID], 1);
    let replica = ReplicaDescriptor::new(REPLICA_ID, TABLET_ID, NodeId::new(1), true, true, 1);
    catalog
        .compare_and_set(
            0,
            CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]),
        )
        .unwrap();

    (
        catalog,
        engine,
        txn_manager,
        LocalDataMover::new(movement_root).unwrap(),
    )
}

fn reopen_job_twice(root: &Path, job_id: &str) -> Option<MovementJob> {
    let first_mover = LocalDataMover::new(root.join("movement")).unwrap();
    let first = match first_mover.load_job(job_id) {
        Ok(job) => job,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening movement job returned corruption: {error}");
        }
        Err(error) => panic!("reopening movement job failed: {error}"),
    };
    drop(first_mover);

    let second_mover = LocalDataMover::new(root.join("movement")).unwrap();
    let second = second_mover.load_job(job_id).unwrap();
    assert_eq!(
        second, first,
        "a second reopen changed the recovered movement job"
    );
    second
}

#[test]
fn import_only_on_fresh_root_keeps_complete_job() {
    let harness = CrashHarness::new("import_only_on_fresh_root_keeps_complete_job").unwrap();

    harness
        .run_workload(|workload| {
            let (catalog, _engine, txn_manager, mover) = setup_import_workload(workload.root());
            let job_id = "fresh-import-job";

            copy_from_jsonl_reader(
                &mover,
                &options(workload.root(), job_id),
                catalog.as_ref(),
                &txn_manager,
                Cursor::new(b"{\"id\":1,\"name\":\"alpha\"}\n{\"id\":2,\"name\":\"beta\"}\n"),
            )
            .unwrap();

            workload.ack("complete");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 83,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let job = reopen_job_twice(root, "fresh-import-job");

                if acked(info, "complete") {
                    checked_with_ack = true;
                    assert_eq!(
                        job.expect("acknowledged completed import job is absent")
                            .phase,
                        MovementJobPhase::Complete,
                        "acknowledged completed import job regressed after recovery"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(
            checked_with_ack,
            "expected an acknowledged completed import job image"
        );
    }
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = import_only_on_fresh_root_keeps_complete_job
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = import_only_on_fresh_root_keeps_complete_job
);
