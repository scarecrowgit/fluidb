//! Integration tests for `RowstoreParticipant` adapter with `TransactionManager`.

use std::sync::Arc;

use htap_common::{HtapError, Mutation, Row, Value, Version};
use htap_rowstore::{Engine, EngineOptions, Snapshot};
use htap_txn::{
    Journal, JournalRecord, ParticipantId, ParticipantWork, RowstoreParticipant, TransactionId,
    TransactionManager, TxnParticipant, TxnState,
};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

#[test]
fn test_one_rowstore_manager_commit() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();

    let journal_path = journal_dir.path().join("txn.journal");
    let manager = TransactionManager::open(&journal_path).unwrap();

    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant_id = ParticipantId::new(10);
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    manager.register_participant(participant);

    // Initial state: nothing committed
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);

    // Commit first transaction (txn 1)
    let mut txn1 = manager.begin().unwrap();
    assert_eq!(txn1.read_version(), Version::INITIAL);

    let mutations1 = vec![Mutation::Put {
        partition_id: 0,
        key: b"k1".to_vec(),
        row: make_row(100),
    }];
    let payload1 = RowstoreParticipant::encode_payload(&mutations1).unwrap();
    txn1.add_participant(participant_id, payload1);

    let committed1 = manager.commit(&mut txn1).unwrap();
    assert_eq!(committed1.version, Version::new(2));
    assert_eq!(committed1.snapshot, Version::INITIAL);
    assert_eq!(txn1.state(), TxnState::Committed);

    // Rowstore is updated and published
    assert_eq!(engine.visible_version(), Version::new(2));
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(100))
    );

    // Commit second transaction (txn 2)
    let mut txn2 = manager.begin().unwrap();
    assert_eq!(txn2.read_version(), Version::new(2));

    let mutations2 = vec![Mutation::Put {
        partition_id: 0,
        key: b"k2".to_vec(),
        row: make_row(200),
    }];
    let payload2 = RowstoreParticipant::encode_payload(&mutations2).unwrap();
    txn2.add_participant(participant_id, payload2);

    let committed2 = manager.commit(&mut txn2).unwrap();
    assert_eq!(committed2.version, Version::new(3));
    assert_eq!(committed2.snapshot, Version::new(2));

    assert_eq!(engine.visible_version(), Version::new(3));
    assert_eq!(engine.committed_version(), Version::new(3));
    assert_eq!(
        engine.get(0, b"k2", engine.snapshot()).unwrap(),
        Some(make_row(200))
    );
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(100))
    );
}

#[test]
fn test_visibility_and_isolation() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();

    let journal_path = journal_dir.path().join("txn.journal");
    let manager = TransactionManager::open(&journal_path).unwrap();

    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant_id = ParticipantId::new(1);
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    manager.register_participant(participant.clone());

    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"secret_key".to_vec(),
        row: make_row(999),
    }];
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();

    // 1. Prepare does not reveal data
    participant.prepare(Version::INITIAL, &payload).unwrap();
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);
    assert_eq!(
        engine.get(0, b"secret_key", engine.snapshot()).unwrap(),
        None
    );

    // 2. Direct apply keeps data hidden before publish
    let txn_id = TransactionId::new(42);
    let version = Version::new(2);
    participant.apply(txn_id, version, &payload).unwrap();

    assert_eq!(engine.committed_version(), version);
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // Hidden from point lookups even with explicit snapshot version
    assert_eq!(
        engine.get(0, b"secret_key", engine.snapshot()).unwrap(),
        None
    );
    assert_eq!(
        engine
            .get(0, b"secret_key", Snapshot::new(version))
            .unwrap(),
        None
    );

    // Hidden from partition scans even with explicit snapshot version
    let scan = engine.scan_partition(0, Snapshot::new(version)).unwrap();
    assert!(scan.is_empty());

    // 3. Publish reveals data
    participant.publish(txn_id, version).unwrap();
    assert_eq!(engine.visible_version(), version);
    assert_eq!(
        engine.get(0, b"secret_key", engine.snapshot()).unwrap(),
        Some(make_row(999))
    );

    let scan_visible = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan_visible.len(), 1);
    assert_eq!(scan_visible[0].key.user_key, b"secret_key");
}

#[test]
fn test_restart_recovery() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();

    let journal_path = journal_dir.path().join("txn.journal");
    let rowstore_opts = EngineOptions::new(rowstore_dir.path());
    let participant_id = ParticipantId::new(1);

    // Step 1: Commit txn 1 normally
    {
        let manager = TransactionManager::open(&journal_path).unwrap();
        let engine = Arc::new(Engine::open(rowstore_opts.clone()).unwrap());
        let participant = Arc::new(RowstoreParticipant::new(
            participant_id,
            Arc::clone(&engine),
        ));
        manager.register_participant(participant);

        let mut txn1 = manager.begin().unwrap();
        let m1 = vec![Mutation::Put {
            partition_id: 0,
            key: b"k1".to_vec(),
            row: make_row(10),
        }];
        txn1.add_participant(
            participant_id,
            RowstoreParticipant::encode_payload(&m1).unwrap(),
        );
        manager.commit(&mut txn1).unwrap();
    }

    // Step 2: Simulate crash after journal fsyncs Intent and Commit for txn 2,
    // but rowstore applied data without publishing before crash.
    {
        let engine = Engine::open(rowstore_opts.clone()).unwrap();
        let m2 = vec![Mutation::Put {
            partition_id: 0,
            key: b"k2".to_vec(),
            row: make_row(20),
        }];
        let payload2 = RowstoreParticipant::encode_payload(&m2).unwrap();

        // Write Intent & Commit to journal directly to simulate commit fsynced in journal
        {
            let mut journal = Journal::open(&journal_path).unwrap();
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(2),
                    snapshot: Version::new(2),
                    participants: vec![ParticipantWork::new(participant_id, payload2.clone())],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Commit {
                    txn_id: TransactionId::new(2),
                    version: Version::new(3),
                })
                .unwrap();
            journal.sync().unwrap();
        }

        // Rowstore applies v3 but does NOT publish before crash
        let participant = RowstoreParticipant::new(participant_id, Arc::new(engine));
        participant
            .apply(TransactionId::new(2), Version::new(3), &payload2)
            .unwrap();
        // Crash occurs: participant.publish(2, Version::new(3)) was NOT called
    }

    // Step 3: Reopen engine. Applied-but-unpublished data MUST be hidden!
    {
        let engine_reopened = Engine::open(rowstore_opts.clone()).unwrap();
        assert_eq!(engine_reopened.visible_version(), Version::new(2));
        assert_eq!(engine_reopened.committed_version(), Version::new(3));

        // k1 is visible (v2), but k2 is hidden (v3 un-published)
        assert_eq!(
            engine_reopened
                .get(0, b"k1", engine_reopened.snapshot())
                .unwrap(),
            Some(make_row(10))
        );
        assert_eq!(
            engine_reopened
                .get(0, b"k2", engine_reopened.snapshot())
                .unwrap(),
            None
        );
    }

    // Step 4: Run transaction manager crash recovery
    {
        let manager = TransactionManager::open(&journal_path).unwrap();
        let engine = Arc::new(Engine::open(rowstore_opts.clone()).unwrap());
        let participant = Arc::new(RowstoreParticipant::new(
            participant_id,
            Arc::clone(&engine),
        ));
        manager.register_participant(participant);

        let report = manager.recover().unwrap();
        assert_eq!(
            report.committed_txns,
            vec![TransactionId::new(1), TransactionId::new(2)]
        );
        assert_eq!(report.visible_version, Version::new(3));

        // Now k2 is published and visible!
        assert_eq!(engine.visible_version(), Version::new(3));
        assert_eq!(
            engine.get(0, b"k2", engine.snapshot()).unwrap(),
            Some(make_row(20))
        );

        // Repeated recovery is idempotent
        let report2 = manager.recover().unwrap();
        assert_eq!(
            report2.committed_txns,
            vec![TransactionId::new(1), TransactionId::new(2)]
        );
        assert_eq!(engine.visible_version(), Version::new(3));
    }
}

#[test]
fn test_malformed_payload_handling() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();

    let journal_path = journal_dir.path().join("txn.journal");
    let rowstore_opts = EngineOptions::new(rowstore_dir.path());
    let participant_id = ParticipantId::new(1);

    // 1. Caller validation: invalid JSON payload rejected with InvalidArgument
    {
        let manager = TransactionManager::open(&journal_path).unwrap();
        let engine = Arc::new(Engine::open(rowstore_opts.clone()).unwrap());
        let participant = Arc::new(RowstoreParticipant::new(
            participant_id,
            Arc::clone(&engine),
        ));
        manager.register_participant(participant);

        let mut txn = manager.begin().unwrap();
        txn.add_participant(participant_id, b"not a valid json payload");

        let err = manager.commit(&mut txn).unwrap_err();
        assert!(
            matches!(err, HtapError::InvalidArgument(_)),
            "expected InvalidArgument for malformed json, got {err:?}"
        );
        assert_eq!(txn.state(), TxnState::Aborted);

        // Empty mutation batch rejected with InvalidArgument
        let empty_payload = serde_json::to_vec(&Vec::<Mutation>::new()).unwrap();
        let mut txn_empty = manager.begin().unwrap();
        txn_empty.add_participant(participant_id, empty_payload);
        let err_empty = manager.commit(&mut txn_empty).unwrap_err();
        assert!(matches!(err_empty, HtapError::InvalidArgument(_)));

        // Duplicate keys in batch rejected with InvalidArgument
        let dup_mutations = vec![
            Mutation::Put {
                partition_id: 0,
                key: b"dup".to_vec(),
                row: make_row(1),
            },
            Mutation::Put {
                partition_id: 0,
                key: b"dup".to_vec(),
                row: make_row(2),
            },
        ];
        let dup_payload = RowstoreParticipant::encode_payload(&dup_mutations).unwrap();
        let mut txn_dup = manager.begin().unwrap();
        txn_dup.add_participant(participant_id, dup_payload);
        let err_dup = manager.commit(&mut txn_dup).unwrap_err();
        assert!(matches!(err_dup, HtapError::InvalidArgument(_)));
    }

    // 2. Recovery path: corrupted payload in journal mapped to Corruption
    {
        let corrupt_journal_dir = tempfile::tempdir().unwrap();
        let corrupt_journal_path = corrupt_journal_dir.path().join("corrupt.journal");

        {
            let mut journal = Journal::open(&corrupt_journal_path).unwrap();
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(1),
                    snapshot: Version::INITIAL,
                    participants: vec![ParticipantWork::new(
                        participant_id,
                        b"corrupted payload data",
                    )],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Commit {
                    txn_id: TransactionId::new(1),
                    version: Version::new(2),
                })
                .unwrap();
            journal.sync().unwrap();
        }

        let manager = TransactionManager::open(&corrupt_journal_path).unwrap();
        let engine = Arc::new(
            Engine::open(EngineOptions::new(tempfile::tempdir().unwrap().path())).unwrap(),
        );
        let participant = Arc::new(RowstoreParticipant::new(
            participant_id,
            Arc::clone(&engine),
        ));
        manager.register_participant(participant);

        let err_recovery = manager.recover().unwrap_err();
        assert!(
            matches!(err_recovery, HtapError::Corruption(_)),
            "expected Corruption error during recovery with malformed journal payload, got {err_recovery:?}"
        );
    }
}
