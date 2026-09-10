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
    fail_publish: AtomicBool,
    events: Mutex<Vec<ParticipantEvent>>,
    prepares: AtomicUsize,
    applies: AtomicUsize,
    aborts: AtomicUsize,
    publishes: AtomicUsize,
    prepare_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    apply_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    publish_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl MockParticipant {
    fn new(id: impl Into<ParticipantId>) -> Self {
        Self {
            id: id.into(),
            fail_prepare: AtomicBool::new(false),
            fail_apply: AtomicBool::new(false),
            fail_publish: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            prepares: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            prepare_hook: Mutex::new(None),
            apply_hook: Mutex::new(None),
            publish_hook: Mutex::new(None),
        }
    }

    fn set_fail_prepare(&self, fail: bool) {
        self.fail_prepare.store(fail, Ordering::SeqCst);
    }

    fn set_fail_apply(&self, fail: bool) {
        self.fail_apply.store(fail, Ordering::SeqCst);
    }

    fn set_fail_publish(&self, fail: bool) {
        self.fail_publish.store(fail, Ordering::SeqCst);
    }

    fn set_prepare_hook<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.prepare_hook.lock() = Some(Box::new(hook));
    }

    fn set_apply_hook<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.apply_hook.lock() = Some(Box::new(hook));
    }

    fn set_publish_hook<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.publish_hook.lock() = Some(Box::new(hook));
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
        let hook = self.prepare_hook.lock().take();
        if let Some(hook_fn) = hook {
            hook_fn();
        }
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
        let hook = self.apply_hook.lock().take();
        if let Some(hook_fn) = hook {
            hook_fn();
        }
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
        let hook = self.publish_hook.lock().take();
        if let Some(hook_fn) = hook {
            hook_fn();
        }
        if self.fail_publish.load(Ordering::SeqCst) {
            return Err(HtapError::Internal(format!(
                "mock participant {} intentionally failed publish for txn {txn_id}",
                self.id
            )));
        }
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

#[test]
fn test_injected_failure_after_commit_fsync_semantics() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // Inject apply failure (happens immediately after Commit fsync)
    p.set_fail_apply(true);

    let mut txn = tm.begin().unwrap();
    txn.add_participant(10, b"work_after_fsync");

    let err = tm.commit(&mut txn).unwrap_err();
    match err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(txn_id, 1);
            assert_eq!(version, Version::new(2));
            assert!(
                reason.contains("apply failed"),
                "expected apply failure in reason, got {reason}"
            );
        }
        other => panic!("expected HtapError::DurablePending, got {other:?}"),
    }

    // State is DurablyCommitted, NOT Aborted
    assert_eq!(txn.state(), TxnState::DurablyCommitted);
    assert_eq!(txn.commit_version(), Some(Version::new(2)));
    assert_eq!(tm.visible_version(), Version::INITIAL);

    // Durable Commit is irrevocable: abort MUST reject it without appending Abort or invoking participant abort
    let abort_res = tm.abort(&mut txn);
    assert!(abort_res.is_err());
    assert!(matches!(abort_res.unwrap_err(), HtapError::Conflict(_)));
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);

    // Inspect raw journal to confirm no Abort record was appended
    {
        let mut journal = Journal::open(&path).unwrap();
        let records = journal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0], JournalRecord::Intent { .. }));
        assert!(matches!(records[1], JournalRecord::Commit { .. }));
        assert!(!records
            .iter()
            .any(|r| matches!(r, JournalRecord::Abort { .. })));
    }

    // Now resolve failure condition
    p.set_fail_apply(false);

    // Recovery retries and completes exactly once
    let report = tm.recover().unwrap();
    assert_eq!(report.committed_txns, vec![TransactionId::new(1)]);
    assert_eq!(report.visible_version, Version::new(2));
    assert_eq!(tm.visible_version(), Version::new(2));
    assert_eq!(p.applies.load(Ordering::SeqCst), 2); // 1 failed + 1 successful during recover
    assert_eq!(p.publishes.load(Ordering::SeqCst), 1);

    // Repeated recover is idempotent and completes cleanly
    let report2 = tm.recover().unwrap();
    assert_eq!(report2.committed_txns, vec![TransactionId::new(1)]);
    assert_eq!(report2.visible_version, Version::new(2));
}

#[test]
fn test_injected_publish_failure_after_commit_fsync() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(20));
    tm.register_participant(p.clone());

    // Inject publish failure
    p.set_fail_publish(true);

    let mut txn = tm.begin().unwrap();
    txn.add_participant(20, b"work_publish_fail");

    let err = tm.commit(&mut txn).unwrap_err();
    match err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(txn_id, 1);
            assert_eq!(version, Version::new(2));
            assert!(
                reason.contains("publish failed"),
                "expected publish failure in reason, got {reason}"
            );
        }
        other => panic!("expected HtapError::DurablePending, got {other:?}"),
    }

    assert_eq!(txn.state(), TxnState::DurablyCommitted);

    // Abort is rejected
    assert!(matches!(
        tm.abort(&mut txn).unwrap_err(),
        HtapError::Conflict(_)
    ));
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);

    // Clear failure condition and recover
    p.set_fail_publish(false);
    let report = tm.recover().unwrap();
    assert_eq!(report.committed_txns, vec![TransactionId::new(1)]);
    assert_eq!(report.visible_version, Version::new(2));
    assert_eq!(p.publishes.load(Ordering::SeqCst), 2); // 1 failed during commit + 1 in recovery
}

#[test]
fn test_malformed_commit_and_abort_journal_corruption() {
    // Subcase 1: Commit followed by Abort for the same txn_id
    {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        {
            let mut journal = Journal::open(&path).unwrap();
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(1),
                    snapshot: Version::new(1),
                    participants: vec![ParticipantWork::new(10, b"data".to_vec())],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Commit {
                    txn_id: TransactionId::new(1),
                    version: Version::new(2),
                })
                .unwrap();
            journal
                .append(&JournalRecord::Abort {
                    txn_id: TransactionId::new(1),
                })
                .unwrap();
            journal.sync().unwrap();
        }

        let tm = TransactionManager::open(&path).unwrap();
        let p = Arc::new(MockParticipant::new(10));
        tm.register_participant(p);

        let err = tm.recover().unwrap_err();
        assert!(
            matches!(err, HtapError::Corruption(_)),
            "expected Corruption for Commit+Abort journal, got {err:?}"
        );
    }

    // Subcase 2: Abort followed by Commit for the same txn_id
    {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        {
            let mut journal = Journal::open(&path).unwrap();
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(2),
                    snapshot: Version::new(1),
                    participants: vec![ParticipantWork::new(10, b"data".to_vec())],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Abort {
                    txn_id: TransactionId::new(2),
                })
                .unwrap();
            journal
                .append(&JournalRecord::Commit {
                    txn_id: TransactionId::new(2),
                    version: Version::new(2),
                })
                .unwrap();
            journal.sync().unwrap();
        }

        let tm = TransactionManager::open(&path).unwrap();
        let p = Arc::new(MockParticipant::new(10));
        tm.register_participant(p);

        let err = tm.recover().unwrap_err();
        assert!(
            matches!(err, HtapError::Corruption(_)),
            "expected Corruption for Abort+Commit journal, got {err:?}"
        );
    }

    // Subcase 3: Valid Abort without Commit is accepted
    {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        {
            let mut journal = Journal::open(&path).unwrap();
            // Txn 1: aborted before commit
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(1),
                    snapshot: Version::new(1),
                    participants: vec![ParticipantWork::new(10, b"data1".to_vec())],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Abort {
                    txn_id: TransactionId::new(1),
                })
                .unwrap();

            // Txn 2: successfully committed
            journal
                .append(&JournalRecord::Intent {
                    txn_id: TransactionId::new(2),
                    snapshot: Version::new(1),
                    participants: vec![ParticipantWork::new(10, b"data2".to_vec())],
                })
                .unwrap();
            journal
                .append(&JournalRecord::Commit {
                    txn_id: TransactionId::new(2),
                    version: Version::new(2),
                })
                .unwrap();
            journal.sync().unwrap();
        }

        let tm = TransactionManager::open(&path).unwrap();
        let p = Arc::new(MockParticipant::new(10));
        tm.register_participant(p);

        let report = tm.recover().unwrap();
        assert_eq!(report.aborted_txns, vec![TransactionId::new(1)]);
        assert_eq!(report.committed_txns, vec![TransactionId::new(2)]);
        assert_eq!(report.visible_version, Version::new(2));
    }
}

#[test]
fn test_deterministic_blocking_participant_serializes_commits() {
    use std::sync::mpsc::channel;
    use std::thread;

    let temp = NamedTempFile::new().unwrap();
    let tm = Arc::new(TransactionManager::open(temp.path()).unwrap());

    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    let (enter_tx, enter_rx) = channel();
    let (release_tx, release_rx) = channel();

    // Hook prepare on participant to notify enter and wait for release
    let release_rx = Arc::new(Mutex::new(release_rx));
    let release_rx_clone = release_rx.clone();
    p.set_prepare_hook(move || {
        let _ = enter_tx.send(());
        let _ = release_rx_clone.lock().recv();
    });

    let tm_clone1 = tm.clone();
    let handle1 = thread::spawn(move || {
        let mut txn = tm_clone1.begin().unwrap();
        txn.add_participant(10, b"thread1");
        tm_clone1.commit(&mut txn).unwrap()
    });

    // Wait until Thread 1 enters prepare (holding decision_lock)
    enter_rx.recv().unwrap();

    // Spawn Thread 2 attempting commit while Thread 1 holds decision_lock
    let tm_clone2 = tm.clone();
    let handle2 = thread::spawn(move || {
        let mut txn = tm_clone2.begin().unwrap();
        txn.add_participant(10, b"thread2");
        tm_clone2.commit(&mut txn).unwrap()
    });

    // Thread 2 must still be blocked waiting for decision_lock
    thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(p.prepares.load(Ordering::SeqCst), 1);

    // Release Thread 1
    release_tx.send(()).unwrap();

    let committed1 = handle1.join().unwrap();
    let committed2 = handle2.join().unwrap();

    assert_eq!(committed1.version, Version::new(2));
    assert_eq!(committed2.version, Version::new(3));
    assert_eq!(tm.visible_version(), Version::new(3));
}

#[test]
fn test_deterministic_blocking_apply_and_publish_hooks() {
    use std::sync::mpsc::channel;
    use std::thread;

    let temp = NamedTempFile::new().unwrap();
    let tm = Arc::new(TransactionManager::open(temp.path()).unwrap());

    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // 1. Block in apply hook
    let (enter_tx, enter_rx) = channel();
    let (release_tx, release_rx) = channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let r_clone = release_rx.clone();
    p.set_apply_hook(move || {
        let _ = enter_tx.send(());
        let _ = r_clone.lock().recv();
    });

    let tm1 = tm.clone();
    let h1 = thread::spawn(move || {
        let mut txn = tm1.begin().unwrap();
        txn.add_participant(10, b"apply_test1");
        tm1.commit(&mut txn).unwrap()
    });

    enter_rx.recv().unwrap();

    // Thread 2 tries to commit, must be blocked on decision_lock even though Commit fsync finished for Thread 1
    let tm2 = tm.clone();
    let h2 = thread::spawn(move || {
        let mut txn = tm2.begin().unwrap();
        txn.add_participant(10, b"apply_test2");
        tm2.commit(&mut txn).unwrap()
    });

    thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(p.applies.load(Ordering::SeqCst), 1);

    // Release Thread 1
    release_tx.send(()).unwrap();
    let c1 = h1.join().unwrap();
    let c2 = h2.join().unwrap();

    assert_eq!(c1.version, Version::new(2));
    assert_eq!(c2.version, Version::new(3));
    assert_eq!(tm.visible_version(), Version::new(3));

    // 2. Block in publish hook
    let (enter_pub_tx, enter_pub_rx) = channel();
    let (release_pub_tx, release_pub_rx) = channel();
    let release_pub_rx = Arc::new(Mutex::new(release_pub_rx));
    let rp_clone = release_pub_rx.clone();
    p.set_publish_hook(move || {
        let _ = enter_pub_tx.send(());
        let _ = rp_clone.lock().recv();
    });

    let tm3 = tm.clone();
    let h3 = thread::spawn(move || {
        let mut txn = tm3.begin().unwrap();
        txn.add_participant(10, b"pub_test3");
        tm3.commit(&mut txn).unwrap()
    });

    enter_pub_rx.recv().unwrap();

    let tm4 = tm.clone();
    let h4 = thread::spawn(move || {
        let mut txn = tm4.begin().unwrap();
        txn.add_participant(10, b"pub_test4");
        tm4.commit(&mut txn).unwrap()
    });

    thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(p.publishes.load(Ordering::SeqCst), 3); // 2 previous + 1 current in progress

    release_pub_tx.send(()).unwrap();
    let c3 = h3.join().unwrap();
    let c4 = h4.join().unwrap();

    assert_eq!(c3.version, Version::new(4));
    assert_eq!(c4.version, Version::new(5));
    assert_eq!(tm.visible_version(), Version::new(5));
}

#[test]
fn test_commit_append_failure_at_decision_boundary() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // Injected commit append failure at decision boundary
    tm.set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });

    let mut txn = tm.begin().unwrap();
    txn.add_participant(10, b"work_append_fail");

    let err = tm.commit(&mut txn).unwrap_err();
    match err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(txn_id, 1);
            assert_eq!(version, Version::new(2));
            assert!(
                reason.contains("commit append failed"),
                "expected append failure reason, got {reason}"
            );
        }
        other => panic!("expected HtapError::DurablePending, got {other:?}"),
    }

    // State is DurablyCommitted (CompletionPending), not Aborted
    assert_eq!(txn.state(), TxnState::DurablyCommitted);
    assert_eq!(txn.commit_version(), Some(Version::new(2)));

    // Abort is rejected because commit boundary was entered
    let abort_res = tm.abort(&mut txn);
    assert!(abort_res.is_err());
    assert!(matches!(abort_res.unwrap_err(), HtapError::Conflict(_)));
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);

    // Journal contains Intent, but NO Commit and NO Abort record
    {
        let mut journal = Journal::open(&path).unwrap();
        let records = journal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], JournalRecord::Intent { .. }));
        assert!(!records
            .iter()
            .any(|r| matches!(r, JournalRecord::Abort { .. })));
    }

    // Recovery runs safely: leaves the transaction unresolved with explicit reason
    let report = tm.recover().unwrap();
    assert!(report.committed_txns.is_empty());
    assert!(report.aborted_txns.is_empty());
    assert_eq!(report.unresolved_txns, vec![TransactionId::new(1)]);
    let reason = report
        .unresolved_reasons
        .get(&TransactionId::new(1))
        .unwrap();
    assert!(
        reason.contains("left unresolved"),
        "expected unresolved reason, got {reason}"
    );

    // Participants were not applied or published
    assert_eq!(p.applies.load(Ordering::SeqCst), 0);
    assert_eq!(p.publishes.load(Ordering::SeqCst), 0);
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);
}

#[test]
fn test_commit_append_torn_tail_failure_and_recovery_semantics() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // Injected commit append failure simulating torn partial frame written to file
    tm.set_commit_append_hook(|journal| {
        // Write partial torn bytes directly to the file to simulate torn write at EOF
        let mut file = OpenOptions::new().append(true).open(journal.path())?;
        file.write_all(&[0x10, 0x00, 0x00, 0x00, 0xde, 0xad])?; // torn incomplete header
        file.sync_all()?;
        Err(HtapError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "simulated crash/failure mid-append leaving torn frame",
        )))
    });

    let mut txn = tm.begin().unwrap();
    txn.add_participant(10, b"work_torn_append");

    let err = tm.commit(&mut txn).unwrap_err();
    assert!(matches!(err, HtapError::DurablePending { .. }));
    assert_eq!(txn.state(), TxnState::DurablyCommitted);

    // Durable pending transaction cannot be aborted
    assert!(matches!(
        tm.abort(&mut txn).unwrap_err(),
        HtapError::Conflict(_)
    ));
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);

    // 1. Recovery with auto_repair = false rejects the torn final record with Corruption
    {
        let tm_no_repair = TransactionManager::open_with_options(
            JournalOptions::new(&path).with_auto_repair(false),
        )
        .unwrap();
        tm_no_repair.register_participant(p.clone());
        let rec_err = tm_no_repair.recover().unwrap_err();
        assert!(
            matches!(rec_err, HtapError::Corruption(_)),
            "expected Corruption when auto_repair is disabled, got {rec_err:?}"
        );
    }

    // 2. Recovery with auto_repair = true (default) repairs torn final and leaves transaction unresolved
    let report = tm.recover().unwrap();
    assert!(report.committed_txns.is_empty());
    assert!(report.aborted_txns.is_empty());
    assert_eq!(report.unresolved_txns, vec![TransactionId::new(1)]);
    let reason = report
        .unresolved_reasons
        .get(&TransactionId::new(1))
        .unwrap();
    assert!(
        reason.contains("torn and safely discarded"),
        "expected torn tail discarded in reason, got {reason}"
    );

    // Verify journal file is clean and repaired now
    {
        let mut journal = Journal::open(&path).unwrap();
        journal.check_integrity().unwrap();
        let records = journal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], JournalRecord::Intent { .. }));
        assert!(!records
            .iter()
            .any(|r| matches!(r, JournalRecord::Abort { .. })));
    }
}

#[test]
fn test_commit_sync_failure_at_decision_boundary() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();

    let tm = TransactionManager::open(&path).unwrap();
    let p = Arc::new(MockParticipant::new(10));
    tm.register_participant(p.clone());

    // Injected commit sync failure: append succeeded (commit bytes are in file), but sync fails
    tm.set_commit_sync_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated fsync failure after commit record append",
        )))
    });

    let mut txn = tm.begin().unwrap();
    txn.add_participant(10, b"work_sync_fail");

    let err = tm.commit(&mut txn).unwrap_err();
    match err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(txn_id, 1);
            assert_eq!(version, Version::new(2));
            assert!(
                reason.contains("commit sync failed"),
                "expected sync failure reason, got {reason}"
            );
        }
        other => panic!("expected HtapError::DurablePending, got {other:?}"),
    }

    // State is DurablyCommitted (irrevocable)
    assert_eq!(txn.state(), TxnState::DurablyCommitted);
    assert_eq!(txn.commit_version(), Some(Version::new(2)));
    assert_eq!(tm.visible_version(), Version::INITIAL);

    // Abort is rejected: commit marker was written, cannot abort
    let abort_res = tm.abort(&mut txn);
    assert!(abort_res.is_err());
    assert!(matches!(abort_res.unwrap_err(), HtapError::Conflict(_)));
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);

    // Journal contains both Intent AND Commit record, and NO Abort record
    {
        let mut journal = Journal::open(&path).unwrap();
        let records = journal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0], JournalRecord::Intent { .. }));
        assert!(matches!(records[1], JournalRecord::Commit { .. }));
        assert!(!records
            .iter()
            .any(|r| matches!(r, JournalRecord::Abort { .. })));
    }

    // Recovery replays the durable Commit record and successfully completes the transaction
    let report = tm.recover().unwrap();
    assert_eq!(report.committed_txns, vec![TransactionId::new(1)]);
    assert!(report.aborted_txns.is_empty());
    assert!(report.unresolved_txns.is_empty());
    assert_eq!(report.visible_version, Version::new(2));
    assert_eq!(tm.visible_version(), Version::new(2));

    // Participant was applied and published during recovery
    assert_eq!(p.applies.load(Ordering::SeqCst), 1);
    assert_eq!(p.publishes.load(Ordering::SeqCst), 1);
    assert_eq!(p.aborts.load(Ordering::SeqCst), 0);
}
