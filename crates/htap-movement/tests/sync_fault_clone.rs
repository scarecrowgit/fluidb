use std::cell::RefCell;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{ColumnDef, DataType, Schema};
use htap_crashsim::{CrashHarness, SyncFault};
use htap_movement::{LocalDataMover, TabletCloneOptions};
use htap_rowstore::{Engine, EngineOptions};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const SOURCE_TABLET_ID: TabletId = TabletId::new(2);
const LEADER_REPLICA_ID: ReplicaId = ReplicaId::new(1);
const TARGET_REPLICA_ID: ReplicaId = ReplicaId::new(2);
const JOB_ID: &str = "sync-fault-clone-job";
const COMPLETE_ACK: &str = "clone-complete";

// These faults occur after the clone's manifest and completed job state become
// visible, but before clone_tablet returns to its caller.
const AMBIGUOUS_AFTER_RENAME: &[(u64, &str, u64, &str)] = &[
    (
        18,
        "atomic_publish:dir_sync",
        2,
        "F12: the publish rename already made the completed clone manifest and job visible when the post-rename sync fails",
    ),
    (
        19,
        "sync_dir:sync",
        8,
        "F12: the publish rename already made the completed clone manifest and job visible when the post-rename sync fails",
    ),
];

struct CloneRun {
    harness: CrashHarness,
    returned_ok: bool,
    acknowledged: bool,
    job_complete: bool,
    manifest_exists: bool,
}

fn schema() -> Schema {
    Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap()
}

fn setup_clone_workload(
    root: &std::path::Path,
) -> (Arc<LocalCatalogStore>, Arc<Engine>, LocalDataMover) {
    let catalog = Arc::new(LocalCatalogStore::open(root.join("catalog")).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(root.join("rowstore"))).unwrap());
    let mover = LocalDataMover::new(root.join("movement")).unwrap();

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

    (catalog, engine, mover)
}

fn run_clone(sync_fault: SyncFault) -> CloneRun {
    let harness = CrashHarness::new("clone_sync_failure_is_never_swallowed").unwrap();
    let returned_ok = RefCell::new(None);
    let acknowledged = RefCell::new(false);
    let job_complete = RefCell::new(None);
    let manifest_exists = RefCell::new(None);

    harness
        .run_workload(|workload| {
            let (catalog, engine, mover) = setup_clone_workload(workload.root());
            let options = TabletCloneOptions::new(JOB_ID, SOURCE_TABLET_ID, TARGET_REPLICA_ID);
            let manifest_path = mover
                .tablet_manifest_path(SOURCE_TABLET_ID, TARGET_REPLICA_ID, JOB_ID)
                .unwrap();

            // Setup syncs are deliberately outside the fault-counted operation region.
            harness.set_sync_fault(sync_fault);
            let result = mover.clone_tablet(&options, catalog.as_ref(), &engine);
            let succeeded = result.is_ok();
            *returned_ok.borrow_mut() = Some(succeeded);

            if succeeded {
                workload.ack(COMPLETE_ACK);
                *acknowledged.borrow_mut() = true;
            }

            *job_complete.borrow_mut() = Some(
                mover
                    .load_job(JOB_ID)
                    .unwrap()
                    .is_some_and(|job| job.is_complete()),
            );
            *manifest_exists.borrow_mut() = Some(manifest_path.exists());
        })
        .unwrap();

    CloneRun {
        harness,
        returned_ok: returned_ok
            .into_inner()
            .expect("clone operation did not run"),
        acknowledged: acknowledged.into_inner(),
        job_complete: job_complete
            .into_inner()
            .expect("clone job state was not observed"),
        manifest_exists: manifest_exists
            .into_inner()
            .expect("clone manifest state was not observed"),
    }
}

#[test]
fn clone_sync_failure_is_never_swallowed() {
    let baseline = run_clone(SyncFault::None);
    assert!(baseline.returned_ok, "baseline clone must succeed");
    assert!(
        baseline.acknowledged,
        "baseline clone must produce its completion acknowledgement"
    );

    let attempts = baseline.harness.sync_attempts();
    eprintln!("clone_sync_failure_is_never_swallowed: N={attempts}");
    assert!(
        attempts >= 1,
        "clone operation must attempt at least one durable sync"
    );

    let mut ordinal_attempts = Vec::new();
    let mut visible_after_err = Vec::new();

    for ordinal in 1..=attempts {
        let run = run_clone(SyncFault::Nth(ordinal));
        ordinal_attempts.push((ordinal, run.harness.sync_attempts()));

        assert!(
            !run.returned_ok,
            "clone silently succeeded when sync fault ordinal {ordinal} fired"
        );
        assert_eq!(
            run.harness.sync_faults_fired(),
            1,
            "sync fault ordinal {ordinal} did not fire exactly once"
        );
        assert!(
            !run.acknowledged,
            "clone produced a completion acknowledgement after sync fault ordinal {ordinal}"
        );

        if run.job_complete && run.manifest_exists {
            visible_after_err.push(ordinal);
        }
    }

    eprintln!("clone_sync_failure_is_never_swallowed: ordinal_sync_attempts={ordinal_attempts:?}");

    let expected_visible: Vec<_> = AMBIGUOUS_AFTER_RENAME
        .iter()
        .map(|(ordinal, _, _, _)| *ordinal)
        .collect();
    assert_eq!(
        visible_after_err, expected_visible,
        "clone visible success after Err must be limited to documented F12 post-rename publishes; \
         ordinal_sync_attempts={ordinal_attempts:?}"
    );

    for &(ordinal, site, occurrence, reason) in AMBIGUOUS_AFTER_RENAME {
        let nth_attempts = ordinal_attempts
            .iter()
            .find_map(|(observed_ordinal, observed_attempts)| {
                (*observed_ordinal == ordinal).then_some(*observed_attempts)
            })
            .expect("ambiguous ordinal must be inside the Nth sweep");
        let run = run_clone(SyncFault::Site { site, occurrence });

        assert!(
            !run.returned_ok,
            "F12 companion fault at {site} occurrence {occurrence} returned Ok: {reason}"
        );
        assert_eq!(
            run.harness.sync_faults_fired(),
            1,
            "F12 companion fault at {site} occurrence {occurrence} did not fire exactly once"
        );
        assert!(
            !run.acknowledged,
            "F12 companion fault at {site} occurrence {occurrence} produced a completion acknowledgement"
        );
        assert!(
            run.job_complete && run.manifest_exists,
            "F12 companion fault at {site} occurrence {occurrence} did not produce the documented visible state: {reason}"
        );
        assert_eq!(
            run.harness.sync_attempts(),
            nth_attempts,
            "F12 companion fault at {site} occurrence {occurrence} did not match Nth ordinal {ordinal}; \
             ordinal_sync_attempts={ordinal_attempts:?}"
        );
    }
}
