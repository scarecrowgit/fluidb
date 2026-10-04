use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{encode_key, ColumnDef, DataType, Mutation, Row, Schema, Value};
use htap_crashsim::{CrashHarness, SyncFault};
use htap_movement::{LocalDataMover, TabletCloneOptions};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};

const TABLE_ID: TableId = TableId::new(1);
const PARTITION_ID: PartitionId = PartitionId::new(10);
const SOURCE_TABLET_ID: TabletId = TabletId::new(2);
const LEADER_REPLICA_ID: ReplicaId = ReplicaId::new(1);
const TARGET_REPLICA_ID: ReplicaId = ReplicaId::new(2);
const JOB_ID: &str = "repair-sync-fault";
const COMPLETE_ACK: &str = "repair-complete";
const AMBIGUOUS_AFTER_RENAME: &[(u64, &str, u64, &str)] = &[(
    8,
    "atomic_publish:dir_sync",
    1,
    "F12: the final catalog publish rename is visible before its directory sync fails",
)];

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

fn setup(root: &std::path::Path) -> (Arc<LocalCatalogStore>, Arc<Engine>, LocalDataMover) {
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

    let transaction_manager = TransactionManager::open(root.join("txn.journal")).unwrap();
    transaction_manager.register_participant(Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    )));
    let mutation = Mutation::Put {
        partition_id: PARTITION_ID.as_u64(),
        key: encode_key(&[Value::Int32(1)]).unwrap(),
        row: Row::new(vec![Value::Int32(1), Value::String("alpha".into())]),
    };
    let payload = RowstoreParticipant::encode_payload(&[mutation]).unwrap();
    transaction_manager
        .commit_request(
            TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
                .unwrap(),
        )
        .unwrap();

    (catalog, engine, mover)
}

fn target_is_healthy(catalog: &LocalCatalogStore) -> bool {
    catalog
        .load()
        .unwrap()
        .unwrap()
        .replica(TARGET_REPLICA_ID)
        .unwrap()
        .healthy
}

fn run_repair(sync_fault: SyncFault) -> (bool, bool, bool, u64, u64) {
    let harness = CrashHarness::new("repair_sync_failure_is_never_swallowed").unwrap();
    let result = RefCell::new(None);

    harness
        .run_workload(|workload| {
            let (catalog, engine, mover) = setup(workload.root());
            let options = TabletCloneOptions::new(JOB_ID, SOURCE_TABLET_ID, TARGET_REPLICA_ID);

            mover
                .clone_tablet(&options, catalog.as_ref(), engine.as_ref())
                .unwrap();
            assert!(!target_is_healthy(catalog.as_ref()));

            harness.set_sync_fault(sync_fault);
            let repair_result = mover.repair_tablet(&options, catalog.as_ref());
            let succeeded = repair_result.is_ok();
            let mut acknowledged = false;
            if succeeded {
                workload.ack(COMPLETE_ACK);
                acknowledged = true;
            }
            *result.borrow_mut() =
                Some((succeeded, acknowledged, target_is_healthy(catalog.as_ref())));
        })
        .unwrap();

    let (succeeded, acknowledged, healthy) = result.into_inner().unwrap();
    (
        succeeded,
        acknowledged,
        healthy,
        harness.sync_attempts(),
        harness.sync_faults_fired(),
    )
}

#[test]
fn repair_sync_failure_is_never_swallowed() {
    let (succeeded, acknowledged, healthy, attempts, faults) = run_repair(SyncFault::None);
    assert!(succeeded, "baseline repair must succeed");
    assert!(
        acknowledged,
        "baseline repair must produce its completion acknowledgement"
    );
    assert!(
        healthy,
        "baseline repair must make the target replica healthy"
    );
    assert_eq!(faults, 0);
    assert!(
        attempts >= 1,
        "repair region must attempt at least one sync"
    );
    eprintln!("repair_sync_failure_is_never_swallowed: N={attempts}");

    let mut visible_after_err = Vec::new();
    let mut sweep_attempts = BTreeMap::new();
    for nth in 1..=attempts {
        let (succeeded, acknowledged, healthy, observed_attempts, faults) =
            run_repair(SyncFault::Nth(nth));

        assert!(
            !succeeded,
            "repair unexpectedly succeeded with sync fault at ordinal {nth}"
        );
        assert!(
            !acknowledged,
            "repair produced a completion acknowledgement after sync fault at ordinal {nth}"
        );
        assert_eq!(
            faults, 1,
            "sync fault at ordinal {nth} did not fire exactly once"
        );
        if healthy {
            visible_after_err.push(nth);
        }
        assert!(
            observed_attempts >= nth,
            "repair stopped before injected sync ordinal {nth}"
        );
        sweep_attempts.insert(nth, observed_attempts);
    }

    let expected_visible_after_err: Vec<u64> = AMBIGUOUS_AFTER_RENAME
        .iter()
        .map(|(ordinal, _, _, _)| *ordinal)
        .collect();
    assert_eq!(
        visible_after_err, expected_visible_after_err,
        "repair exposed a healthy target after Err at unexpected sync ordinals"
    );

    for &(ordinal, site, occurrence, reason) in AMBIGUOUS_AFTER_RENAME {
        let (succeeded, acknowledged, healthy, observed_attempts, faults) =
            run_repair(SyncFault::Site { site, occurrence });

        assert!(
            !succeeded,
            "repair unexpectedly succeeded with sync fault at {site}: {reason}"
        );
        assert!(
            !acknowledged,
            "repair produced a completion acknowledgement after sync fault at {site}: {reason}"
        );
        assert_eq!(
            faults, 1,
            "sync fault at {site} did not fire exactly once: {reason}"
        );
        assert_eq!(
            observed_attempts,
            sweep_attempts[&ordinal],
            "repair sync attempts for {site} occurrence {occurrence} did not match Nth ordinal {ordinal}"
        );
        assert!(
            healthy,
            "repair did not expose the F12 ambiguous outcome at ordinal {ordinal}, site {site}: {reason}"
        );
    }
}
