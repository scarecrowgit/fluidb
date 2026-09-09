//! Mock tests, corruption checks, bounded frames, torn-final repair,
//! deterministic ordering, and crash recovery for `htap-txn`.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use htap_common::{HtapError, Result, Version};
use htap_txn::{
    Journal, JournalOptions, JournalRecord, ParticipantId, ParticipantWork, TransactionId,
    TransactionManager, TransactionRequest, TxnParticipant, TxnState, MAX_PAYLOAD_SIZE,
};
use parking_lot::Mutex;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParticipantEvent {
    Prepare(Version, Vec<u8>),
    Apply(TransactionId, Version, Vec<u8>),
    Abort(TransactionId),
    Publish(TransactionId, Version),
}

struct MockParticipant {
    id: ParticipantId,
    fail_prepare: AtomicBool,
    fail_apply: AtomicBool,
    events: Mutex<Vec<ParticipantEvent>>,
    prepares: AtomicUsize,
    applies: AtomicUsize,
    aborts: AtomicUsize,
    publishes: AtomicUsize,
}

impl MockParticipant {
    fn new(id: impl Into<ParticipantId>) -> Self {
        Self {
            id: id.into(),
            fail_prepare: AtomicBool::new(false),
            fail_apply: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            prepares: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
        }
    }

    fn set_fail_prepare(&self, fail: bool) {
        self.fail_prepare.store(fail, Ordering::SeqCst);
    }

    fn set_fail_apply(&self, fail: bool) {
        self.fail_apply.store(fail, Ordering::SeqCst);
    }

    fn recorded_events(&self) -> Vec<ParticipantEvent> {
        self.events.lock().clone()
    }
}

impl TxnParticipant for MockParticipant {
    fn id(&self) -> ParticipantId {
        self.id
    }

    fn prepare(&self, snapshot: Version, payload: &[u8]) -> Result<()> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        if self.fail_prepare.load(Ordering::SeqCst) {
            return Err(HtapError::Conflict(format!(
                "mock participant {} intentionally rejected prepare",
                self.id
            )));
        }
        self.events
            .lock()
            .push(ParticipantEvent::Prepare(snapshot, payload.to_vec()));
        Ok(())
    }

    fn apply(&self, txn_id: TransactionId, version: Version, payload: &[u8]) -> Result<()> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        if self.fail_apply.load(Ordering::SeqCst) {
            return Err(HtapError::Internal(format!(
                "mock participant {} intentionally failed apply for txn {txn_id}",
                self.id
            )));
        }
        self.events
            .lock()
            .push(ParticipantEvent::Apply(txn_id, version, payload.to_vec()));
        Ok(())
    }

    fn abort(&self, txn_id: TransactionId) -> Result<()> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        self.events.lock().push(ParticipantEvent::Abort(txn_id));
        Ok(())
    }

    fn publish(&self, txn_id: TransactionId, version: Version) -> Result<()> {
        self.publishes.fetch_add(1, Ordering::SeqCst);
        self.events
            .lock()
            .push(ParticipantEvent::Publish(txn_id, version));
        Ok(())
    }
}

#[test]
fn test_journal_basic_append_and_read() {
    let temp = NamedTempFile::new().unwrap();
    let mut journal = Journal::open(temp.path()).unwrap();

    let rec_intent = JournalRecord::Intent {
        txn_id: TransactionId::new(1),
        snapshot: Version::new(2),
        participants: vec![
            ParticipantWork::new(101, b"intent payload 101".to_vec()),
            ParticipantWork::new(202, b"intent payload 202".to_vec()),
        ],
    };
    let rec_commit = JournalRecord::Commit {
        txn_id: TransactionId::new(1),
        version: Version::new(2),
    };

    journal.append(&rec_intent).unwrap();
    journal.append(&rec_commit).unwrap();

    let records = journal.read_all().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], rec_intent);
    assert_eq!(records[1], rec_commit);
    journal.check_integrity().unwrap();
}

#[test]
fn test_journal_bounded_frames() {
    let temp = NamedTempFile::new().unwrap();
    let opts = JournalOptions::new(temp.path()).with_max_frame_size(128);
    let mut journal = Journal::open_with_options(opts).unwrap();

    let large_rec = JournalRecord::Intent {
        txn_id: TransactionId::new(1),
        snapshot: Version::new(2),
        participants: vec![ParticipantWork::new(1, vec![7u8; 500])],
    };

    // Rejects append that exceeds bounded max_frame_size
    let err = journal.append(&large_rec).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
}

#[test]
fn test_journal_torn_final_incomplete_header() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let mut journal = Journal::open(&path).unwrap();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(1),
            version: Version::new(2),
        })
        .unwrap();
    let valid_end = journal.valid_bytes();

    // Inject 5 bytes (less than the 8-byte header) at EOF
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0x01, 0x02, 0x03, 0x04, 0x05]).unwrap();
        file.sync_all().unwrap();
    }

    // Reopen with auto-repair
    let mut reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.valid_bytes(), valid_end);

    let recs = reopened.read_all().unwrap();
    assert_eq!(recs.len(), 1);

    // Can append new record after repair cleanly
    reopened
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(2),
            version: Version::new(3),
        })
        .unwrap();

    let recs2 = reopened.read_all().unwrap();
    assert_eq!(recs2.len(), 2);
}

#[test]
fn test_journal_torn_final_incomplete_payload() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let mut journal = Journal::open(&path).unwrap();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(1),
            version: Version::new(2),
        })
        .unwrap();
    let valid_end = journal.valid_bytes();

    // Inject header claiming 100 bytes payload, but only append 10 bytes payload
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        let payload_len: u32 = 100;
        let crc: u32 = 0xdeadbeef;
        file.write_all(&payload_len.to_le_bytes()).unwrap();
        file.write_all(&crc.to_le_bytes()).unwrap();
        file.write_all(&[0xaa; 10]).unwrap();
        file.sync_all().unwrap();
    }

    let mut reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.valid_bytes(), valid_end);
    let recs = reopened.read_all().unwrap();
    assert_eq!(recs.len(), 1);
}

#[test]
fn test_journal_torn_final_corrupted_crc_at_eof() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let mut journal = Journal::open(&path).unwrap();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(1),
            version: Version::new(2),
        })
        .unwrap();
    let valid_end = journal.valid_bytes();

    // Append second record
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(2),
            version: Version::new(3),
        })
        .unwrap();

    // Corrupt the CRC of the final record
    {
        let mut bytes = std::fs::read(&path).unwrap();
        let crc_offset = (valid_end + 4) as usize;
        bytes[crc_offset] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
    }

    // Torn final repair truncates the damaged final record
    let mut reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.valid_bytes(), valid_end);

    let recs = reopened.read_all().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].txn_id(), TransactionId::new(1));
}

#[test]
fn test_journal_corruption_checks_middle() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let mut journal = Journal::open(&path).unwrap();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(1),
            version: Version::new(2),
        })
        .unwrap();
    let offset_rec2 = journal.valid_bytes();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(2),
            version: Version::new(3),
        })
        .unwrap();
    journal
        .append(&JournalRecord::Commit {
            txn_id: TransactionId::new(3),
            version: Version::new(4),
        })
        .unwrap();

    // Corrupt payload of record 2 (which is in the middle, since record 3 follows it)
    {
        let mut bytes = std::fs::read(&path).unwrap();
        let payload_byte = (offset_rec2 + 10) as usize;
        bytes[payload_byte] ^= 0x55;
        std::fs::write(&path, &bytes).unwrap();
    }

    // Opening with auto-repair MUST error rather than truncating middle records
    let res = Journal::open(&path);
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
}

#[test]
fn test_deterministic_ordering_and_mock_execution() {
    let temp = NamedTempFile::new().unwrap();
    let tm = TransactionManager::open(temp.path()).unwrap();

    // Create participants with arbitrary IDs
    let ids = [70, 15, 95, 3];
    let mut mocks = Vec::new();

    for &id in &ids {
        let mock = Arc::new(MockParticipant::new(id));
        mocks.push(mock.clone());
        tm.register_participant(mock);
    }

    let mut txn = tm.begin().unwrap();
    // Register in reverse/random order with specific opaque payloads
    txn.add_participant(95, b"payload_95");
    txn.add_participant(3, b"payload_3");
    txn.add_participant(70, b"payload_70");
    txn.add_participant(15, b"payload_15");

    let committed = tm.commit(&mut txn).unwrap();
    assert_eq!(committed.version, Version::new(2));
    assert_eq!(committed.snapshot, Version::INITIAL);
    assert_eq!(
        committed.participant_ids,
        vec![
            ParticipantId::new(3),
            ParticipantId::new(15),
            ParticipantId::new(70),
            ParticipantId::new(95)
        ]
    );
    assert_eq!(txn.state(), TxnState::Committed);

    // Verify each participant received prepare, apply, and publish with exact payloads
    for mock in &mocks {
        let expected_payload = format!("payload_{}", mock.id.get()).into_bytes();
        assert_eq!(mock.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(mock.applies.load(Ordering::SeqCst), 1);
        assert_eq!(mock.publishes.load(Ordering::SeqCst), 1);
        assert_eq!(mock.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(
            mock.recorded_events(),
            vec![
                ParticipantEvent::Prepare(Version::INITIAL, expected_payload.clone()),
                ParticipantEvent::Apply(TransactionId::new(1), Version::new(2), expected_payload),
                ParticipantEvent::Publish(TransactionId::new(1), Version::new(2)),
            ]
        );
    }
}

#[test]
fn test_prepare_failure_aborts_prior_participants() {
    let temp = NamedTempFile::new().unwrap();
    let tm = TransactionManager::open(temp.path()).unwrap();

    let p1 = Arc::new(MockParticipant::new(10));
    let p2 = Arc::new(MockParticipant::new(20));
    let p3 = Arc::new(MockParticipant::new(30));

    // Configure p2 to fail prepare
    p2.set_fail_prepare(true);

    tm.register_participant(p1.clone());
    tm.register_participant(p2.clone());
    tm.register_participant(p3.clone());

    let mut txn = tm.begin().unwrap();
    // Added in reverse order, but executed in sorted ID order (10, 20, 30)
    txn.add_participant(30, b"p30_work");
    txn.add_participant(20, b"p20_work");
    txn.add_participant(10, b"p10_work");

    let res = tm.commit(&mut txn);
    assert!(res.is_err());
    assert_eq!(txn.state(), TxnState::Aborted);

    // p1 (ID 10) was prepared first, then when p2 failed, p1 was aborted
    assert_eq!(p1.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(p1.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(p1.applies.load(Ordering::SeqCst), 0);

    // p2 (ID 20) failed prepare; not applied
    assert_eq!(p2.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(p2.applies.load(Ordering::SeqCst), 0);

    // p3 (ID 30) was never prepared or applied
    assert_eq!(p3.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(p3.applies.load(Ordering::SeqCst), 0);
}

#[test]
fn test_missing_participants() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    // 1. Commit with unregistered participant should fail
    {
        let tm = TransactionManager::open(&path).unwrap();
        let p1 = Arc::new(MockParticipant::new(10));
        tm.register_participant(p1);

        let mut txn = tm.begin().unwrap();
        txn.add_participant(10, b"p10");
        txn.add_participant(99, b"unregistered_p99");

        let err = tm.commit(&mut txn).unwrap_err();
        assert!(matches!(err, HtapError::NotFound(_)));
    }

    // 2. Recovery with missing registered participant should fail
    {
        // Write durable Intent + Commit records for participant 500
        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(&JournalRecord::Intent {
                txn_id: TransactionId::new(1),
                snapshot: Version::new(1),
                participants: vec![ParticipantWork::new(500, b"data500".to_vec())],
            })
            .unwrap();
        journal
            .append(&JournalRecord::Commit {
                txn_id: TransactionId::new(1),
                version: Version::new(2),
            })
            .unwrap();
    }

    {
        let tm = TransactionManager::open(&path).unwrap();
        // Do not register participant 500
        let err = tm.recover().unwrap_err();
        assert!(matches!(err, HtapError::NotFound(_)));
    }
}

#[test]
fn test_recovery_committed_transaction_exact_payload_replay() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    // 1. Run commit on first transaction manager instance
    {
        let tm = TransactionManager::open(&path).unwrap();
        let p1 = Arc::new(MockParticipant::new(10));
        let p2 = Arc::new(MockParticipant::new(20));
        tm.register_participant(p1);
        tm.register_participant(p2);

        let mut txn = tm.begin().unwrap();
        txn.add_participant(10, b"payload_for_p10_exact");
        txn.add_participant(20, b"payload_for_p20_exact");
        let committed = tm.commit(&mut txn).unwrap();
        assert_eq!(committed.version, Version::new(2));
    }

    // 2. Reopen and recover with fresh participant instances
    let tm2 = TransactionManager::open(&path).unwrap();
    let p1_recovered = Arc::new(MockParticipant::new(10));
    let p2_recovered = Arc::new(MockParticipant::new(20));
    tm2.register_participant(p1_recovered.clone());
    tm2.register_participant(p2_recovered.clone());

    let report = tm2.recover().unwrap();
    assert_eq!(report.committed_txns, vec![TransactionId::new(1)]);
    assert!(report.aborted_txns.is_empty());
    assert_eq!(report.max_version, Version::new(2));
    assert_eq!(report.visible_version, Version::new(2));

    // Both participants should have received apply with exact payloads and publish
    assert_eq!(p1_recovered.applies.load(Ordering::SeqCst), 1);
    assert_eq!(p1_recovered.publishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        p1_recovered.recorded_events(),
        vec![
            ParticipantEvent::Apply(
                TransactionId::new(1),
                Version::new(2),
                b"payload_for_p10_exact".to_vec()
            ),
            ParticipantEvent::Publish(TransactionId::new(1), Version::new(2)),
        ]
    );

    assert_eq!(p2_recovered.applies.load(Ordering::SeqCst), 1);
    assert_eq!(p2_recovered.publishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        p2_recovered.recorded_events(),
        vec![
            ParticipantEvent::Apply(
                TransactionId::new(1),
                Version::new(2),
                b"payload_for_p20_exact".to_vec()
            ),
            ParticipantEvent::Publish(TransactionId::new(1), Version::new(2)),
        ]
    );

    // Subsequent transaction starts at monotonic version 3
    let mut next_txn = tm2.begin().unwrap();
    next_txn.add_participant(10, b"next_txn_work");
    let committed_v3 = tm2.commit(&mut next_txn).unwrap();
    assert_eq!(committed_v3.version, Version::new(3));
}

#[test]
fn test_recovery_intent_only_is_ignored() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    // Simulate crash after Intent is written but BEFORE Commit is written
    {
        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(&JournalRecord::Intent {
                txn_id: TransactionId::new(42),
                snapshot: Version::new(2),
                participants: vec![
                    ParticipantWork::new(100, vec![1, 2, 3]),
                    ParticipantWork::new(200, vec![4, 5, 6]),
                ],
            })
            .unwrap();
    }

    // Reopen and recover
    let tm = TransactionManager::open(&path).unwrap();
    let p1 = Arc::new(MockParticipant::new(100));
    let p2 = Arc::new(MockParticipant::new(200));
    tm.register_participant(p1.clone());
    tm.register_participant(p2.clone());

    let report = tm.recover().unwrap();
    // Intent-only is ignored!
    assert!(report.committed_txns.is_empty());
    assert!(report.aborted_txns.is_empty());

    // Neither participant was applied or aborted
    assert_eq!(p1.applies.load(Ordering::SeqCst), 0);
    assert_eq!(p1.aborts.load(Ordering::SeqCst), 0);
    assert_eq!(p2.applies.load(Ordering::SeqCst), 0);
    assert_eq!(p2.aborts.load(Ordering::SeqCst), 0);
}

#[test]
fn test_post_commit_failure_retryable_via_recover() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // Inject apply failure
    p.set_fail_apply(true);

    let mut txn = tm.begin().unwrap();
    txn.add_participant(10, b"resilient_work");

    // Commit fails during post-commit apply phase
    let res = tm.commit(&mut txn);
    assert!(res.is_err());
    // Visible version was not advanced
    assert_eq!(tm.visible_version(), Version::INITIAL);

    // Now resolve participant failure condition
    p.set_fail_apply(false);

    // Recover replays the durable commit and succeeds
    let report = tm.recover().unwrap();
    assert_eq!(report.committed_txns, vec![TransactionId::new(1)]);
    assert_eq!(report.visible_version, Version::new(2));
    assert_eq!(tm.visible_version(), Version::new(2));

    // Participant received apply and publish during recovery
    assert_eq!(p.applies.load(Ordering::SeqCst), 2); // 1 failed + 1 successful during recover
    assert_eq!(p.publishes.load(Ordering::SeqCst), 1);
}

#[test]
fn test_transaction_request_validation_rules() {
    // 1. Nonempty validation
    let empty_req = TransactionRequest {
        participants: vec![],
    };
    assert!(matches!(
        empty_req.validate().unwrap_err(),
        HtapError::InvalidArgument(_)
    ));

    // 2. Unique IDs validation
    let dup_req = TransactionRequest {
        participants: vec![
            ParticipantWork::new(1, vec![1, 2]),
            ParticipantWork::new(1, vec![3, 4]),
        ],
    };
    assert!(matches!(
        dup_req.validate().unwrap_err(),
        HtapError::InvalidArgument(_)
    ));

    // 3. Max payload size validation (single participant)
    let oversize_work = ParticipantWork::new(1, vec![0u8; MAX_PAYLOAD_SIZE + 1]);
    let oversize_req = TransactionRequest {
        participants: vec![oversize_work],
    };
    assert!(matches!(
        oversize_req.validate().unwrap_err(),
        HtapError::InvalidArgument(_)
    ));
}
