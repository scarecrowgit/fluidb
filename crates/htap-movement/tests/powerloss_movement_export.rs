use std::path::Path;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::fs::create_dir_all_durable;
use htap_common::{encode_key, ColumnDef, DataType, Mutation, Row, Schema, Value};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};
use htap_movement::{export, CopyOptions, DataFormat, LocalDataMover};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const TABLET_ID: TabletId = TabletId::new(2);
const REPLICA_ID: ReplicaId = ReplicaId::new(1);
const EXPECTED_CSV: &[u8] = b"id,name\n1,alpha\n2,beta\n";

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

fn setup_export_workload(root: &Path) -> (Arc<LocalCatalogStore>, Arc<Engine>, LocalDataMover) {
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

    let mutations = [("alpha", 1), ("beta", 2)]
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

fn options(root: &Path, job_id: &str, output: &Path) -> CopyOptions {
    CopyOptions::new(
        job_id,
        TABLE_ID,
        TABLET_ID,
        DataFormat::Csv,
        root.join(output),
    )
}

fn read_export_twice(path: &Path) -> Option<Vec<u8>> {
    let first = path.exists().then(|| std::fs::read(path).unwrap());
    let second = path.exists().then(|| std::fs::read(path).unwrap());

    assert_eq!(
        second, first,
        "a second export reopen changed file contents"
    );
    first
}

fn run_export_workload(
    workload: &htap_crashsim::WorkloadContext,
    job_id: &str,
    output: &Path,
    durable_output_dir: bool,
) {
    let (catalog, engine, mover) = setup_export_workload(workload.root());
    let output_dir = workload.root().join(output).parent().unwrap().to_path_buf();

    if durable_output_dir {
        create_dir_all_durable(&output_dir).unwrap();
    }

    let options = options(workload.root(), job_id, output);
    export(&mover, &options, catalog.as_ref(), &engine).unwrap();
    workload.ack("export-complete");
}

#[test]
fn export_file_complete_or_absent() {
    let harness = CrashHarness::new("export_file_complete_or_absent").unwrap();
    let output = Path::new("exports/result.csv");

    harness
        .run_workload(|workload| run_export_workload(workload, "export-file-job", output, true))
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let file = read_export_twice(&root.join(output));

            assert!(
                file.as_deref().is_none_or(|bytes| bytes == EXPECTED_CSV),
                "final export name contains a partial file"
            );

            if acked(info, "export-complete") {
                checked_with_ack = true;
                assert_eq!(file.as_deref(), Some(EXPECTED_CSV));
            }
        })
        .unwrap();

    assert!(checked_count > 1, "expected multiple crash images");
    assert!(checked_with_ack, "expected an acknowledged export image");
}

#[test]
fn export_preexisting_volatile_destination() {
    let harness = CrashHarness::new("export_preexisting_volatile_destination").unwrap();
    let output = Path::new("out/new/file.csv");

    harness
        .run_workload(|workload| {
            let (catalog, engine, mover) = setup_export_workload(workload.root());

            // Simulate a prior attempt that created these directories without directory syncs.
            htap_common::fs::dur::create_dir_all(workload.root().join("out/new")).unwrap();

            let options = options(workload.root(), "volatile-destination-job", output);
            export(&mover, &options, catalog.as_ref(), &engine).unwrap();
            workload.ack("export-complete");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let file = read_export_twice(&root.join(output));

            if acked(info, "export-complete") {
                checked_with_ack = true;
                assert_eq!(
                    file.as_deref(),
                    Some(EXPECTED_CSV),
                    "acknowledged export to volatile destination disappeared"
                );
            }
        })
        .unwrap();

    assert!(checked_count > 1, "expected multiple crash images");
    assert!(checked_with_ack, "expected an acknowledged export image");
}

#[test]
fn export_to_fresh_nested_destination() {
    let harness = CrashHarness::new("export_to_fresh_nested_destination").unwrap();
    let output = Path::new("out/new/file.csv");

    harness
        .run_workload(|workload| run_export_workload(workload, "nested-export-job", output, false))
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
                let file = read_export_twice(&root.join(output));

                assert!(
                    file.as_deref().is_none_or(|bytes| bytes == EXPECTED_CSV),
                    "final nested export name contains a partial file"
                );

                if acked(info, "export-complete") {
                    checked_with_ack = true;
                    assert_eq!(
                        file.as_deref(),
                        Some(EXPECTED_CSV),
                        "acknowledged nested export disappeared"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(checked_with_ack, "expected an acknowledged export image");
    }
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_movement_export_write_sync,
    site = "movement:export_write_sync",
    body = export_file_complete_or_absent
);
htap_crashsim::crashsim_witness!(
    witness_sync_ancestors_sync,
    site = "sync_ancestors:sync",
    body = export_preexisting_volatile_destination
);
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = export_file_complete_or_absent
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = export_file_complete_or_absent
);
