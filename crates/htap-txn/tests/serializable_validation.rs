#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use htap_common::{HtapError, Result, Version};
use htap_txn::{
    ParticipantId, ParticipantWork, ReadFootprint, SerializableTicket, Transaction, TransactionId,
    TransactionManager, TxnParticipant, WriteKey,
};
use parking_lot::Mutex as ParkingMutex;
use tempfile::NamedTempFile;

struct MockParticipant {
    id: ParticipantId,
    prepared: ParkingMutex<Vec<TransactionId>>,
    aborts: ParkingMutex<Vec<TransactionId>>,
    writes: Option<Vec<WriteKey>>,
}

impl MockParticipant {
    fn new(id: u64, writes: Option<Vec<WriteKey>>) -> Self {
        Self {
            id: ParticipantId::new(id),
            prepared: ParkingMutex::new(Vec::new()),
            aborts: ParkingMutex::new(Vec::new()),
            writes,
        }
    }

    fn prepared_count(&self) -> usize {
        self.prepared.lock().len()
    }

    fn aborts(&self) -> Vec<TransactionId> {
        self.aborts.lock().clone()
    }
}

impl TxnParticipant for MockParticipant {
    fn id(&self) -> ParticipantId {
        self.id
    }

    fn prepare(&self, _snapshot: Version, _payload: &[u8]) -> Result<()> {
        self.prepared.lock().push(TransactionId::new(0));
        Ok(())
    }

    fn apply(&self, _txn_id: TransactionId, _version: Version, _payload: &[u8]) -> Result<()> {
        Ok(())
    }

    fn abort(&self, txn_id: TransactionId) -> Result<()> {
        self.aborts.lock().push(txn_id);
        Ok(())
    }

    fn written_keys(&self, payload: &[u8]) -> Result<Option<Vec<WriteKey>>> {
        if payload.len() >= 2 && payload[0] == b'k' && payload[1] as usize == payload.len() - 2 {
            return Ok(Some(
                payload[2..]
                    .iter()
                    .map(|key| WriteKey::new(1, vec![*key]))
                    .collect(),
            ));
        }

        Ok(self.writes.clone())
    }
}

struct ApplyFailingParticipant {
    inner: MockParticipant,
    fail_apply_once: AtomicBool,
}

impl ApplyFailingParticipant {
    fn new(id: u64, writes: Option<Vec<WriteKey>>, fail_apply_once: bool) -> Self {
        Self {
            inner: MockParticipant::new(id, writes),
            fail_apply_once: AtomicBool::new(fail_apply_once),
        }
    }
}

impl TxnParticipant for ApplyFailingParticipant {
    fn id(&self) -> ParticipantId {
        self.inner.id()
    }

    fn prepare(&self, snapshot: Version, payload: &[u8]) -> Result<()> {
        self.inner.prepare(snapshot, payload)
    }

    fn apply(&self, txn_id: TransactionId, version: Version, payload: &[u8]) -> Result<()> {
        if self.fail_apply_once.swap(false, Ordering::SeqCst) {
            return Err(HtapError::Io(std::io::Error::other(
                "inject participant apply failure",
            )));
        }

        self.inner.apply(txn_id, version, payload)
    }

    fn abort(&self, txn_id: TransactionId) -> Result<()> {
        self.inner.abort(txn_id)
    }

    fn written_keys(&self, payload: &[u8]) -> Result<Option<Vec<WriteKey>>> {
        self.inner.written_keys(payload)
    }
}

fn request(participant_id: ParticipantId) -> ParticipantWork {
    ParticipantWork::new(participant_id, b"write")
}

fn footprint(partition_id: u64, key: &[u8]) -> ReadFootprint {
    let mut reads = ReadFootprint::default();
    reads.record_point(partition_id, key.to_vec());
    reads
}

fn footprint_for_keys(keys: &[u8]) -> ReadFootprint {
    let mut reads = ReadFootprint::default();
    for key in keys {
        reads.record_point(1, vec![*key]);
    }
    reads
}

fn encoded_writes(keys: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(keys.len() + 2);
    payload.push(b'k');
    payload.push(keys.len() as u8);
    payload.extend_from_slice(keys);
    payload
}

fn serializable_txn(
    manager: &TransactionManager,
    ticket: SerializableTicket,
    reads: ReadFootprint,
    participant_id: ParticipantId,
) -> Transaction {
    let mut txn =
        Transaction::new_serializable(manager.next_txn_id().unwrap(), ticket, reads).unwrap();
    txn.add_work(request(participant_id));
    txn
}

#[test]
fn validation_failure_leaves_no_intent_and_aborts_prepared_participants() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(MockParticipant::new(1, Some(vec![WriteKey::new(1, b"k")])));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();
    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    manager.commit(&mut writer).unwrap();

    let journal_len = std::fs::metadata(temp.path()).unwrap().len();
    manager.set_intent_append_hook(|_| {
        panic!("validation failure must happen before the intent append hook")
    });

    let mut reader = serializable_txn(&manager, ticket, footprint(1, b"k"), participant.id());
    let reader_id = reader.id();
    assert!(matches!(
        manager.commit(&mut reader),
        Err(HtapError::Conflict(_))
    ));
    assert_eq!(reader.state(), htap_txn::TxnState::Aborted);
    assert_eq!(std::fs::metadata(temp.path()).unwrap().len(), journal_len);
    assert_eq!(participant.prepared_count(), 2);
    assert_eq!(participant.aborts(), vec![reader_id]);
}

#[test]
fn si_transaction_is_never_validated_but_its_writes_are_recorded() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(MockParticipant::new(1, Some(vec![WriteKey::new(1, b"k")])));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();

    let mut si = manager.begin().unwrap();
    si.add_work(request(participant.id()));
    manager.commit(&mut si).unwrap();

    let mut serializable = serializable_txn(&manager, ticket, footprint(1, b"k"), participant.id());
    assert!(matches!(
        manager.commit(&mut serializable),
        Err(HtapError::Conflict(_))
    ));
    assert_eq!(manager.serializable_stats().validation_aborts, 1);
}

#[test]
fn live_recover_records_writes_for_ticket_pinned_before_failed_apply() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(ApplyFailingParticipant::new(
        1,
        Some(vec![WriteKey::new(1, b"k")]),
        true,
    ));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();

    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    assert!(matches!(
        manager.commit(&mut writer),
        Err(HtapError::DurablePending { .. })
    ));

    manager.recover().unwrap();

    let mut reader = serializable_txn(&manager, ticket, footprint(1, b"k"), participant.id());
    assert!(matches!(
        manager.commit(&mut reader),
        Err(HtapError::Conflict(message)) if message.contains("read-write dependency")
    ));
    assert_eq!(manager.serializable_stats().validation_aborts, 1);
    assert_eq!(manager.serializable_stats().floor_aborts, 0);
}

#[test]
fn live_recover_records_writes_for_ticket_pinned_after_failed_apply() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(ApplyFailingParticipant::new(
        1,
        Some(vec![WriteKey::new(1, b"k")]),
        true,
    ));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    assert!(matches!(
        manager.commit(&mut writer),
        Err(HtapError::DurablePending { .. })
    ));

    // Visibility has not advanced, so this pin still represents the pre-commit snapshot.
    let ticket = manager.pin_serializable().unwrap();
    manager.recover().unwrap();

    let mut reader = serializable_txn(&manager, ticket, footprint(1, b"k"), participant.id());
    assert!(matches!(
        manager.commit(&mut reader),
        Err(HtapError::Conflict(message)) if message.contains("read-write dependency")
    ));
    assert_eq!(manager.serializable_stats().validation_aborts, 1);
    assert_eq!(manager.serializable_stats().floor_aborts, 0);
}

#[test]
fn live_recover_keeps_unrelated_reader_committable() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(ApplyFailingParticipant::new(
        1,
        Some(vec![WriteKey::new(1, b"written")]),
        true,
    ));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();

    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    assert!(matches!(
        manager.commit(&mut writer),
        Err(HtapError::DurablePending { .. })
    ));

    manager.recover().unwrap();

    let mut reader = serializable_txn(
        &manager,
        ticket,
        footprint(1, b"unrelated"),
        participant.id(),
    );
    manager.commit(&mut reader).unwrap();
}

#[test]
fn multi_participant_partial_apply_recovery_records_second_participant() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let first = Arc::new(MockParticipant::new(
        1,
        Some(vec![WriteKey::new(1, b"first")]),
    ));
    let second = Arc::new(ApplyFailingParticipant::new(
        2,
        Some(vec![WriteKey::new(1, b"second")]),
        true,
    ));
    manager.register_participant(first.clone());
    manager.register_participant(second.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();
    let mut writer = manager.begin().unwrap();
    writer.add_work(request(first.id()));
    writer.add_work(request(second.id()));
    assert!(matches!(
        manager.commit(&mut writer),
        Err(HtapError::DurablePending { .. })
    ));

    manager.recover().unwrap();

    let mut reader = serializable_txn(&manager, ticket, footprint(1, b"second"), first.id());
    assert!(matches!(
        manager.commit(&mut reader),
        Err(HtapError::Conflict(message)) if message.contains("read-write dependency")
    ));
    assert_eq!(manager.serializable_stats().validation_aborts, 1);
    assert_eq!(manager.serializable_stats().floor_aborts, 0);
}

#[test]
fn unknown_participant_raises_global_floor() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(MockParticipant::new(1, None));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();
    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    manager.commit(&mut writer).unwrap();

    let mut reader = serializable_txn(
        &manager,
        ticket,
        footprint(99, b"unrelated"),
        participant.id(),
    );
    assert!(matches!(
        manager.commit(&mut reader),
        Err(HtapError::Conflict(message))
            if message.contains("global retained-write floor")
    ));
    assert_eq!(manager.serializable_stats().floor_aborts, 1);
}

#[test]
fn overflow_floor_aborts_old_snapshot_only() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let keys = (0u32..10_001)
        .map(|key| WriteKey::new(1, key.to_be_bytes()))
        .collect();
    let participant = Arc::new(MockParticipant::new(1, Some(keys)));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let old_ticket = manager.pin_serializable().unwrap();
    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    manager.commit(&mut writer).unwrap();

    let new_ticket = manager.pin_serializable().unwrap();

    let mut old_reader =
        serializable_txn(&manager, old_ticket, footprint(1, b"old"), participant.id());
    assert!(matches!(
        manager.commit(&mut old_reader),
        Err(HtapError::Conflict(message)) if message.contains("validation window exceeded")
    ));

    let mut new_reader =
        serializable_txn(&manager, new_ticket, footprint(1, b"new"), participant.id());
    manager.commit(&mut new_reader).unwrap();
}

#[test]
fn prune_respects_oldest_ticket() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(MockParticipant::new(1, Some(vec![WriteKey::new(1, b"k")])));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let oldest_ticket = manager.pin_serializable().unwrap();

    let mut writer = manager.begin().unwrap();
    writer.add_work(request(participant.id()));
    manager.commit(&mut writer).unwrap();

    let newer_ticket = manager.pin_serializable().unwrap();
    drop(newer_ticket);

    let mut oldest_reader = serializable_txn(
        &manager,
        oldest_ticket,
        footprint(1, b"k"),
        participant.id(),
    );
    assert!(matches!(
        manager.commit(&mut oldest_reader),
        Err(HtapError::Conflict(_))
    ));
}

#[derive(Debug)]
struct SerializableCommitOutcome {
    snapshot: Version,
    reads: Vec<u8>,
    writes: Vec<u8>,
    commit_version: Version,
}

struct SeededRng(u64);

impl SeededRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn key_set(&mut self) -> Vec<u8> {
        let mut keys = Vec::new();
        let mask = ((self.next_u64() >> 16) as u8 & 0b1111) | 1;
        for key in 0..4 {
            if mask & (1 << key) != 0 {
                keys.push(key);
            }
        }
        keys
    }
}

#[test]
fn pin_commit_prune_threaded_stress_matches_model() {
    let temp = NamedTempFile::new().unwrap();
    let manager = Arc::new(TransactionManager::open(temp.path()).unwrap());
    let participant = Arc::new(MockParticipant::new(1, Some(Vec::new())));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    // Establish a scheduling-independent validation abort before the random workload.
    let guaranteed_ticket = manager.pin_serializable().unwrap();
    let mut guaranteed_writer = manager.begin().unwrap();
    guaranteed_writer.add_work(ParticipantWork::new(participant.id(), encoded_writes(&[0])));
    manager.commit(&mut guaranteed_writer).unwrap();
    let mut guaranteed_reader = Transaction::new_serializable(
        manager.next_txn_id().unwrap(),
        guaranteed_ticket,
        footprint_for_keys(&[0]),
    )
    .unwrap();
    guaranteed_reader.add_work(request(participant.id()));
    assert!(matches!(
        manager.commit(&mut guaranteed_reader),
        Err(HtapError::Conflict(_))
    ));

    let workers = 10;
    let iterations = 40;
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    for worker in 0..workers {
        let manager = Arc::clone(&manager);
        let outcomes = Arc::clone(&outcomes);
        let participant_id = participant.id();

        handles.push(thread::spawn(move || {
            let mut rng = SeededRng::new(0x5eed_0000_u64 + worker as u64);

            for _ in 0..iterations {
                let ticket = manager.pin_serializable().unwrap();

                // Some pins are deliberately released without validation to exercise pruning.
                if rng.next_u64().is_multiple_of(4) {
                    drop(ticket);
                    thread::yield_now();
                    continue;
                }

                let reads = rng.key_set();
                let writes = rng.key_set();
                let snapshot = ticket.snapshot();
                let mut txn = Transaction::new_serializable(
                    manager.next_txn_id().unwrap(),
                    ticket,
                    footprint_for_keys(&reads),
                )
                .unwrap();
                txn.add_work(ParticipantWork::new(
                    participant_id,
                    encoded_writes(&writes),
                ));

                // Give concurrently pinned transactions an opportunity to commit first.
                thread::yield_now();

                if manager.commit(&mut txn).is_ok() {
                    outcomes.lock().unwrap().push(SerializableCommitOutcome {
                        snapshot,
                        reads,
                        writes,
                        commit_version: txn.commit_version().unwrap(),
                    });
                }

                thread::yield_now();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let outcomes = outcomes.lock().unwrap();
    assert!(
        !outcomes.is_empty(),
        "the stress workload must accept at least one serializable commit"
    );
    assert!(
        manager.serializable_stats().validation_aborts > 0,
        "the stress workload must produce at least one validation abort"
    );

    for accepted in outcomes.iter() {
        for other in outcomes.iter() {
            if other.commit_version > accepted.snapshot
                && other.commit_version < accepted.commit_version
            {
                for read_key in &accepted.reads {
                    assert!(
                        !other.writes.contains(read_key),
                        "accepted serializable commit at {} read key {} from snapshot {}, \
                         but commit at {} wrote that key before its validation",
                        accepted.commit_version,
                        read_key,
                        accepted.snapshot,
                        other.commit_version,
                    );
                }
            }
        }
    }
}

#[test]
fn recovery_required_rejects_before_validation_and_txn_stays_open() {
    let temp = NamedTempFile::new().unwrap();
    let manager = TransactionManager::open(temp.path()).unwrap();
    let participant = Arc::new(MockParticipant::new(1, Some(vec![WriteKey::new(1, b"k")])));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let ticket = manager.pin_serializable().unwrap();
    manager.set_commit_append_hook(|_| {
        Err(HtapError::Io(std::io::Error::other(
            "inject recovery-required latch",
        )))
    });
    let mut pending = manager.begin().unwrap();
    pending.add_work(request(participant.id()));
    assert!(matches!(
        manager.commit(&mut pending),
        Err(HtapError::DurablePending { .. })
    ));

    let mut blocked = serializable_txn(&manager, ticket, footprint(1, b"k"), participant.id());
    assert!(matches!(
        manager.commit(&mut blocked),
        Err(HtapError::RecoveryRequired { .. })
    ));
    assert_eq!(blocked.state(), htap_txn::TxnState::Active);
}

#[test]
fn pin_before_recover_is_rejected_after_checkpoint_reopen() {
    let temp = NamedTempFile::new().unwrap();

    {
        let manager = TransactionManager::open(temp.path()).unwrap();
        let participant = Arc::new(MockParticipant::new(1, Some(Vec::new())));
        manager.register_participant(participant.clone());
        manager.recover().unwrap();

        let mut writer = manager.begin().unwrap();
        writer.add_work(request(participant.id()));
        manager.commit(&mut writer).unwrap();
    }

    let manager = TransactionManager::open(temp.path()).unwrap();
    assert!(matches!(
        manager.pin_serializable(),
        Err(HtapError::Conflict(message))
            if message.contains("requires successful recovery")
    ));

    manager.register_participant(Arc::new(MockParticipant::new(1, Some(Vec::new()))));
    manager.recover().unwrap();
    manager.pin_serializable().unwrap();
}

#[test]
fn lock_order_no_deadlock_with_execution_lock_style_caller() {
    let temp = NamedTempFile::new().unwrap();
    let manager = Arc::new(TransactionManager::open(temp.path()).unwrap());
    let execution_lock = Arc::new(Mutex::new(()));
    let participant = Arc::new(MockParticipant::new(1, Some(Vec::new())));
    manager.register_participant(participant.clone());
    manager.recover().unwrap();

    let (completion_tx, completion_rx) = mpsc::channel();
    let mut handles = Vec::new();

    for caller_id in 0..4 {
        let manager = Arc::clone(&manager);
        let execution_lock = Arc::clone(&execution_lock);
        let participant_id = participant.id();

        let completion_tx = completion_tx.clone();
        handles.push(thread::spawn(move || {
            for iteration in 0..30 {
                let _execution_guard = execution_lock.lock().unwrap();

                if (caller_id + iteration) % 2 == 0 {
                    let ticket = manager.pin_serializable().unwrap();
                    drop(ticket);
                } else {
                    let ticket = manager.pin_serializable().unwrap();
                    let key = ((caller_id + iteration) % 4) as u8;
                    let mut txn = Transaction::new_serializable(
                        manager.next_txn_id().unwrap(),
                        ticket,
                        footprint_for_keys(&[key]),
                    )
                    .unwrap();
                    txn.add_work(ParticipantWork::new(participant_id, encoded_writes(&[key])));
                    let _ = manager.commit(&mut txn);
                }

                thread::yield_now();
            }
            completion_tx.send(()).unwrap();
        }));
    }

    for worker_id in 0..4 {
        let manager = Arc::clone(&manager);
        let completion_tx = completion_tx.clone();

        handles.push(thread::spawn(move || {
            for iteration in 0..60 {
                let ticket = manager.pin_serializable().unwrap();

                if (worker_id + iteration) % 3 == 0 {
                    thread::yield_now();
                }

                drop(ticket);
                thread::yield_now();
            }
            completion_tx.send(()).unwrap();
        }));
    }

    drop(completion_tx);
    for _ in 0..8 {
        completion_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("lock-order test timed out");
    }
    for handle in handles {
        handle.join().unwrap();
    }
}
