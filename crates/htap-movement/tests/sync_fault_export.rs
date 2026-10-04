use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
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
use htap_crashsim::{CrashHarness, SyncFault};
use htap_movement::{export, CopyOptions, DataFormat, LocalDataMover};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const TABLET_ID: TabletId = TabletId::new(2);
const REPLICA_ID: ReplicaId = ReplicaId::new(1);

const AMBIGUOUS_AFTER_RENAME: &[(u64, &str, u64, &str)] = &[
    (
        10,
        "atomic_publish:dir_sync",
        2,
        "F12: the final job-file rename is visible before its post-rename directory sync fails",
    ),
    (
        11,
        "sync_dir:sync",
        3,
        "F12: the final job-file rename is visible before the subsequent jobs-directory sync fails",
    ),
];

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

fn setup_export(root: &Path) -> (Arc<LocalCatalogStore>, Arc<Engine>, LocalDataMover) {
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

    let mutation = Mutation::Put {
        partition_id: PARTITION_ID.as_u64(),
        key: encode_key(&[Value::Int32(1)]).unwrap(),
        row: Row::new(vec![Value::Int32(1), Value::String("alpha".into())]),
    };
    let payload = RowstoreParticipant::encode_payload(&[mutation]).unwrap();
    let request =
        TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
            .unwrap();
    txn_manager.commit_request(request).unwrap();

    (catalog, engine, LocalDataMover::new(movement_root).unwrap())
}

fn options(root: &Path) -> CopyOptions {
    CopyOptions::new(
        "export-sync-fault-job",
        TABLE_ID,
        TABLET_ID,
        DataFormat::Csv,
        root.join("exports/result.csv"),
    )
}

#[test]
fn export_sync_failure_is_never_swallowed() {
    let baseline = CrashHarness::new("export_sync_failure_baseline").unwrap();
    baseline
        .run_workload(|workload| {
            let (catalog, engine, mover) = setup_export(workload.root());
            let options = options(workload.root());

            baseline.set_sync_fault(SyncFault::None);
            export(&mover, &options, catalog.as_ref(), &engine).unwrap();
            workload.ack("export-complete");
        })
        .unwrap();

    let attempts = baseline.sync_attempts();
    eprintln!("export_sync_failure_is_never_swallowed: N={attempts}");
    assert!(attempts >= 1, "export performed no sync attempts");

    let expected_ambiguous: BTreeSet<_> = AMBIGUOUS_AFTER_RENAME
        .iter()
        .map(|(ordinal, _, _, _)| *ordinal)
        .collect();
    let mut observed_ambiguous = BTreeSet::new();
    let mut sweep_attempts = BTreeMap::new();

    for nth in 1..=attempts {
        let harness = CrashHarness::new(format!("export_sync_failure_nth_{nth}")).unwrap();
        let acknowledged = Cell::new(false);
        let complete = Cell::new(false);

        harness
            .run_workload(|workload| {
                let (catalog, engine, mover) = setup_export(workload.root());
                let options = options(workload.root());

                harness.set_sync_fault(SyncFault::Nth(nth));
                let result = export(&mover, &options, catalog.as_ref(), &engine);
                if result.is_ok() {
                    workload.ack("export-complete");
                    acknowledged.set(true);
                }

                complete.set(matches!(
                    mover.load_job(&options.job_id),
                    Ok(Some(job)) if job.is_complete()
                ));
                assert!(
                    result.is_err(),
                    "sync fault ordinal {nth} was swallowed by export"
                );
            })
            .unwrap();

        assert_eq!(
            harness.sync_faults_fired(),
            1,
            "sync fault ordinal {nth} did not fire"
        );
        assert!(
            !acknowledged.get(),
            "sync fault ordinal {nth} produced an export-complete acknowledgement"
        );
        if complete.get() {
            observed_ambiguous.insert(nth);
        }
        sweep_attempts.insert(nth, harness.sync_attempts());
    }

    assert_eq!(
        observed_ambiguous, expected_ambiguous,
        "Complete job visibility after Err must match the documented F12 ambiguous outcomes"
    );

    for &(ordinal, site, occurrence, reason) in AMBIGUOUS_AFTER_RENAME {
        let harness = CrashHarness::new(format!("export_sync_failure_site_{ordinal}")).unwrap();
        let acknowledged = Cell::new(false);
        let complete = Cell::new(false);

        harness
            .run_workload(|workload| {
                let (catalog, engine, mover) = setup_export(workload.root());
                let options = options(workload.root());

                harness.set_sync_fault(SyncFault::Site { site, occurrence });
                let result = export(&mover, &options, catalog.as_ref(), &engine);
                if result.is_ok() {
                    workload.ack("export-complete");
                    acknowledged.set(true);
                }

                complete.set(matches!(
                    mover.load_job(&options.job_id),
                    Ok(Some(job)) if job.is_complete()
                ));
                assert!(
                    result.is_err(),
                    "sync fault at {site} ordinal {ordinal} was swallowed by export"
                );
            })
            .unwrap();

        assert_eq!(
            harness.sync_faults_fired(),
            1,
            "sync fault at {site} ordinal {ordinal} did not fire"
        );
        assert_eq!(
            harness.sync_attempts(),
            sweep_attempts[&ordinal],
            "export sync attempts for {site} occurrence {occurrence} did not match Nth ordinal {ordinal}"
        );
        assert!(
            !acknowledged.get(),
            "sync fault at {site} ordinal {ordinal} produced an export-complete acknowledgement"
        );
        assert!(
            complete.get(),
            "sync fault at {site} ordinal {ordinal} did not produce the expected F12 ambiguous outcome: {reason}"
        );
    }
}
