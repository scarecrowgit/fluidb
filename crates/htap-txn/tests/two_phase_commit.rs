//! Tests for the timing of first-writer-wins conflict detection.
//!
//! `Engine::prepare` performs the conflict check (see htap-rowstore engine.rs), which
//! `RowstoreParticipant::prepare` invokes during `TransactionManager::commit`'s prepare
//! phase — strictly before any Intent or Commit journal record is durably written.
//! A losing writer must therefore fail cleanly with `HtapError::Conflict`, never
//! `HtapError::DurablePending`, and must leave no trace in the journal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use htap_common::{HtapError, Mutation, Row, Value, Version};
use htap_rowstore::{Engine, EngineIoOp, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, Transaction, TransactionManager,
    TransactionRequest, TxnState,
};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

fn open_manager_and_engine(
    journal_path: &std::path::Path,
    rowstore_dir: &std::path::Path,
    participant_id: ParticipantId,
) -> (TransactionManager, Arc<Engine>) {
    let manager = TransactionManager::open(journal_path).unwrap();
    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir)).unwrap());
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    manager.register_participant(participant);
    (manager, engine)
}

fn single_put_request(participant_id: ParticipantId, key: &[u8], val: i64) -> TransactionRequest {
    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: key.to_vec(),
        row: make_row(val),
    }];
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    TransactionRequest::new(vec![ParticipantWork::new(participant_id, payload)]).unwrap()
}

/// A commits key `k` at snapshot S0. B is built directly against the SAME stale
/// snapshot S0 (as amendment A3 requires for a session commit: `Transaction::new`
/// with the transaction's own pinned snapshot, never `commit_request`/`begin`, which
/// would instead pick up the post-A visible version). B must be rejected with a clean
/// `Conflict` at prepare time, before any Intent/Commit record is journaled for B, and
/// must never be reported as `DurablePending`. After reopening and recovering, B must
/// leave no trace and its value must not be visible.
#[test]
fn test_conflict_detected_before_journal_commit_not_after() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let (manager, engine) =
        open_manager_and_engine(&journal_path, rowstore_dir.path(), participant_id);

    let s0 = manager.visible_version();
    assert_eq!(s0, Version::INITIAL);

    // Txn A commits key "k" at S0.
    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), s0);
    txn_a.set_request(single_put_request(participant_id, b"k", 1));
    manager.commit(&mut txn_a).unwrap();
    assert_eq!(txn_a.state(), TxnState::Committed);

    let journal_len_before_b = std::fs::metadata(&journal_path).unwrap().len();

    // Txn B is built against the SAME stale snapshot S0 and writes the same key.
    let txn_b_id = manager.next_txn_id().unwrap();
    let mut txn_b = Transaction::new(txn_b_id, s0);
    txn_b.set_request(single_put_request(participant_id, b"k", 2));

    let err = manager.commit(&mut txn_b).unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
    assert!(
        !err.is_durable_pending(),
        "conflict caught at prepare time must never surface as DurablePending, got {err:?}"
    );
    assert_eq!(txn_b.state(), TxnState::Aborted);

    // Prepare-time rejection happens before the Intent record is even appended, so the
    // journal must not have grown at all for B's attempt.
    let journal_len_after_b = std::fs::metadata(&journal_path).unwrap().len();
    assert_eq!(
        journal_len_before_b, journal_len_after_b,
        "journal must not be mutated when the conflict is caught at prepare time"
    );

    // A's value stands; B's value never landed.
    assert_eq!(
        engine.get(0, b"k", engine.snapshot()).unwrap(),
        Some(make_row(1))
    );

    drop(manager);
    drop(engine);

    // Reopen and recover: B must have no Intent/Commit record and must not be replayed.
    let engine2 = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant2 = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine2),
    ));
    let manager2 = TransactionManager::open(&journal_path).unwrap();
    manager2.register_participant(participant2);

    let report = manager2.recover().unwrap();
    assert!(
        !report.committed_txns.contains(&txn_b_id),
        "B must not be recovered as committed"
    );
    assert!(
        !report.aborted_txns.contains(&txn_b_id),
        "B was never journaled, so it cannot appear as an explicit abort either"
    );
    assert!(
        !report.unresolved_txns.contains(&txn_b_id),
        "B left no Intent record, so it cannot appear as unresolved"
    );

    assert_eq!(
        engine2.get(0, b"k", engine2.snapshot()).unwrap(),
        Some(make_row(1)),
        "only A's value must be visible after recovery"
    );
}

/// A commits key `k1` at snapshot S0. B is built against the same stale snapshot S0
/// but writes a *different* key `k2`. Since there is no overlap, first-writer-wins
/// must not fire, and B must commit successfully despite its stale snapshot.
#[test]
fn test_stale_snapshot_non_conflicting_key_still_commits() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let (manager, engine) =
        open_manager_and_engine(&journal_path, rowstore_dir.path(), participant_id);

    let s0 = manager.visible_version();

    // Txn A commits key "k1" at S0.
    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), s0);
    txn_a.set_request(single_put_request(participant_id, b"k1", 1));
    manager.commit(&mut txn_a).unwrap();
    assert_eq!(txn_a.state(), TxnState::Committed);

    // Txn B is built against the same stale snapshot S0 but touches a disjoint key.
    let mut txn_b = Transaction::new(manager.next_txn_id().unwrap(), s0);
    txn_b.set_request(single_put_request(participant_id, b"k2", 2));

    let committed_b = manager.commit(&mut txn_b).unwrap();
    assert_eq!(txn_b.state(), TxnState::Committed);
    assert_eq!(committed_b.snapshot, s0);

    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(1))
    );
    assert_eq!(
        engine.get(0, b"k2", engine.snapshot()).unwrap(),
        Some(make_row(2))
    );
}

/// Storage-reviewer finding F3: once `commit` returns `DurablePending` for a transaction (its
/// allocated commit version V was never applied), a later `commit` must not be allowed to
/// allocate V+1 — it would fail `apply_external`'s version-continuity check (since the engine
/// still expects V), get journaled as its own failed/DurablePending attempt, and leave `recover`
/// unable to make sense of the journal on the next reopen. The manager latches on the first
/// `DurablePending` and rejects every later `commit` before it does any work, with a
/// non-retryable `HtapError::RecoveryRequired` (never `Conflict`, so a client cannot mistake it
/// for "rolled back, retry").
///
/// Fix-pass item 1c: A's failure here is injected via `set_commit_append_hook`, which fires
/// before the real journal append even runs (not, as an earlier version of this test's comment
/// incorrectly said, a participant `publish` failure) — a journal-I/O cause. A latch with that
/// cause can only ever be cleared by a fresh reopen, never by an in-process `recover()`, even
/// though that in-process `recover()` still correctly and safely reports A unresolved: an
/// in-process fsync succeeding after an earlier one failed proves nothing about the earlier
/// write's durability, so only a brand new file open and scan (a real process restart) is
/// trusted to lift the latch.
#[test]
fn test_commit_after_durable_pending_is_rejected_until_recovery() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let (manager, engine) =
        open_manager_and_engine(&journal_path, rowstore_dir.path(), participant_id);

    // Txn A's commit record append itself is made to fail at the decision boundary (a
    // journal-I/O cause), so A ends up `DurablePending`: its commit version is durably decided
    // (the manager has committed to it) but the record was never actually written.
    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_a.set_request(single_put_request(participant_id, b"k1", 1));
    manager.set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure at the commit decision boundary",
        )))
    });
    let err_a = manager.commit(&mut txn_a).unwrap_err();
    assert!(
        err_a.is_durable_pending(),
        "expected DurablePending, got {err_a:?}"
    );
    let pending_txn_id = match &err_a {
        HtapError::DurablePending { txn_id, .. } => *txn_id,
        _ => unreachable!(),
    };

    // A later transaction B must be rejected before it does any work: no Intent/Commit record
    // for B, and a non-retryable `RecoveryRequired` naming the still-pending transaction A, never
    // `Conflict` and never A's own `DurablePending` (B itself never attempted anything).
    let journal_len_before_b = std::fs::metadata(&journal_path).unwrap().len();
    let mut txn_b = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_b.set_request(single_put_request(participant_id, b"k2", 2));
    let err_b = manager.commit(&mut txn_b).unwrap_err();
    assert!(
        err_b.is_recovery_required(),
        "expected a non-retryable RecoveryRequired, got {err_b:?}"
    );
    assert!(
        !matches!(err_b, HtapError::Conflict(_)),
        "must never be Conflict: a client could mistake it for rolled-back-retry"
    );
    match &err_b {
        HtapError::RecoveryRequired { blocking_txn, .. } => {
            assert_eq!(
                *blocking_txn, pending_txn_id,
                "names the latched transaction"
            );
        }
        _ => unreachable!(),
    }
    assert_eq!(
        txn_b.state(),
        TxnState::Active,
        "B was rejected before prepare even ran"
    );
    let journal_len_after_b = std::fs::metadata(&journal_path).unwrap().len();
    assert_eq!(
        journal_len_before_b, journal_len_after_b,
        "a latched manager must reject before appending anything for B"
    );

    // Fix-pass round 3, item 2: `recover()` itself must now refuse outright while latched on a
    // journal-I/O cause — an in-process fsync succeeding afterward proves nothing about records
    // "underneath" the earlier failed one, so recovery must not replay anything until a fresh
    // reopen. It must apply nothing and leave the latch exactly as it was.
    let err_recover = manager.recover().unwrap_err();
    assert!(
        err_recover.is_recovery_required(),
        "expected an in-process recover() under a journal-I/O latch to be rejected outright, \
         got {err_recover:?}"
    );
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        None,
        "A never durably committed, so its write must not be visible, and a rejected recover() \
         must not have applied anything"
    );
    assert!(
        manager.recovery_required(),
        "a journal-I/O-caused latch must survive a rejected in-process recover()"
    );

    // A commit attempted right after that rejected in-process recover() is still rejected the
    // same way.
    let mut txn_c = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_c.set_request(single_put_request(participant_id, b"k2", 3));
    let err_c = manager.commit(&mut txn_c).unwrap_err();
    assert!(
        err_c.is_recovery_required(),
        "still latched after a rejected in-process recover(), expected RecoveryRequired, got {err_c:?}"
    );

    // Only a fresh reopen (a brand new `TransactionManager`, whose `recovery_required` always
    // starts `None`) clears the latch. B's own commit version was allocated but never durably
    // consumed (no Commit record was ever written for it), so it is reissued to the next
    // transaction that actually commits.
    drop(manager);
    let engine2 = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant2 = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine2),
    ));
    let manager2 = TransactionManager::open(&journal_path).unwrap();
    manager2.register_participant(participant2);
    let report2 = manager2.recover().unwrap();
    assert!(report2.unresolved_txns.contains(&txn_a.id()));
    assert!(
        !manager2.recovery_required(),
        "a fresh reopen must clear the latch"
    );

    let mut txn_d = Transaction::new(manager2.next_txn_id().unwrap(), manager2.visible_version());
    txn_d.set_request(single_put_request(participant_id, b"k2", 3));
    let committed_d = manager2
        .commit(&mut txn_d)
        .expect("the latch is cleared after reopening from a fresh journal replay");
    assert_eq!(txn_d.state(), TxnState::Committed);
    assert_eq!(
        committed_d.version,
        Version::new(2),
        "the version A's failed attempt would have consumed was never durably written, so it is \
         reissued to the next transaction that actually commits"
    );
    assert_eq!(
        engine2.get(0, b"k2", engine2.snapshot()).unwrap(),
        Some(make_row(3))
    );

    // A second reopen sees a clean journal: A's orphan Intent, plus C's Intent+Commit landing
    // exactly at the reissued version, with no duplication or gaps.
    drop(manager2);
    drop(engine2);
    let engine3 = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant3 = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine3),
    ));
    let manager3 = TransactionManager::open(&journal_path).unwrap();
    manager3.register_participant(participant3);
    let report3 = manager3.recover().unwrap();
    assert!(!manager3.recovery_required());
    assert!(report3.unresolved_txns.contains(&txn_a.id()));
    assert_eq!(report3.committed_txns, vec![txn_d.id()]);
    assert_eq!(
        engine3.get(0, b"k2", engine3.snapshot()).unwrap(),
        Some(make_row(3))
    );
}

/// Fix-pass item 1c / round 3 item 2: a latch caused by a `commit_sync_hook` failure — where
/// `append_nosync` has already really written the commit record's bytes to disk and only the
/// fsync itself is made to fail — must not be cleared by an in-process `recover()`. Per round 3's
/// fix, `recover()` itself now refuses outright and applies nothing in-process here, even though
/// the commit record's bytes really are readable from disk: an in-process fsync succeeding
/// afterward proves nothing about the durability of writes "underneath" the earlier failed one on
/// the same fd. Only a fresh reopen clears the latch and replays the transaction, and since its
/// version really was durably consumed, it is not reissued: the next commit lands at V+1.
#[test]
fn test_latch_via_commit_sync_hook_survives_in_process_recover_until_reopen() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let (manager, engine) =
        open_manager_and_engine(&journal_path, rowstore_dir.path(), participant_id);

    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_a.set_request(single_put_request(participant_id, b"k1", 1));
    manager.set_commit_sync_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated fsync failure after the commit record's bytes are already on disk",
        )))
    });
    let err_a = manager.commit(&mut txn_a).unwrap_err();
    assert!(
        err_a.is_durable_pending(),
        "expected DurablePending, got {err_a:?}"
    );

    // Fix-pass round 3, item 2: even though the commit record's bytes really are on disk
    // (`append_nosync` ran and succeeded before the sync hook fired), `recover()` itself must now
    // refuse outright while latched on a journal-I/O cause — an in-process fsync succeeding
    // afterward proves nothing about durability of writes "underneath" the earlier failed one on
    // the same fd. It must apply nothing here: A is not yet visible.
    let err_recover = manager.recover().unwrap_err();
    assert!(
        err_recover.is_recovery_required(),
        "expected an in-process recover() under a journal-I/O latch to be rejected outright, \
         got {err_recover:?}"
    );
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        None,
        "a rejected recover() must not have applied anything"
    );

    // The latch itself survives that rejected in-process recover() too: the cause is
    // journal-I/O, so nothing short of a fresh reopen can clear it.
    assert!(
        manager.recovery_required(),
        "a journal-I/O-caused latch must survive a rejected in-process recover()"
    );
    let mut txn_b = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_b.set_request(single_put_request(participant_id, b"k2", 2));
    let err_b = manager.commit(&mut txn_b).unwrap_err();
    assert!(
        err_b.is_recovery_required(),
        "still latched after an in-process recover(), got {err_b:?}"
    );

    // A fresh reopen clears the latch. A's commit record really was durable, so it is not
    // reissued: the next commit lands cleanly at V+1.
    drop(manager);
    drop(engine);
    let engine2 = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant2 = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine2),
    ));
    let manager2 = TransactionManager::open(&journal_path).unwrap();
    manager2.register_participant(participant2);
    let report2 = manager2.recover().unwrap();
    assert_eq!(
        report2.committed_txns,
        vec![txn_a.id()],
        "idempotent replay: A is committed exactly once"
    );
    assert!(!manager2.recovery_required());
    assert_eq!(
        engine2.get(0, b"k1", engine2.snapshot()).unwrap(),
        Some(make_row(1))
    );

    let mut txn_c = Transaction::new(manager2.next_txn_id().unwrap(), manager2.visible_version());
    txn_c.set_request(single_put_request(participant_id, b"k2", 2));
    let committed_c = manager2.commit(&mut txn_c).unwrap();
    assert_eq!(
        committed_c.version,
        Version::new(3),
        "V+1: A's version v2 was truly consumed, unlike the append-hook scenario above"
    );
}

/// Fix-pass round 3, item 2: a dedicated, standalone check that an in-process `recover()` under a
/// `JournalIo`-caused latch is rejected outright and applies nothing — independent of which
/// specific journal operation originally caused the latch (here, the Intent-boundary
/// `commit_append_hook`, same as `test_commit_after_durable_pending_is_rejected_until_recovery`).
/// A later fresh reopen still resolves the latched transaction correctly, confirming the rejected
/// `recover()` call left everything exactly as it was for that real replay to act on.
#[test]
fn test_in_process_recover_under_journal_io_latch_is_rejected_and_applies_nothing() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let (manager, engine) =
        open_manager_and_engine(&journal_path, rowstore_dir.path(), participant_id);

    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_a.set_request(single_put_request(participant_id, b"k1", 1));
    manager.set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure at the commit decision boundary",
        )))
    });
    let err_a = manager.commit(&mut txn_a).unwrap_err();
    assert!(
        err_a.is_durable_pending(),
        "expected DurablePending, got {err_a:?}"
    );
    assert!(manager.recovery_required());

    // `recover()` itself must be rejected outright: no `RecoveryReport`, no participant apply or
    // publish, no visible-version advance, and the latch is untouched.
    let err_recover = manager.recover().unwrap_err();
    assert!(
        err_recover.is_recovery_required(),
        "expected RecoveryRequired from an in-process recover() under a JournalIo latch, got \
         {err_recover:?}"
    );
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        None,
        "recover() must apply nothing while latched on a journal-I/O cause"
    );
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert!(
        manager.recovery_required(),
        "the latch must be untouched by the rejected recover()"
    );

    // A fresh reopen still resolves correctly: A's commit record never actually landed (the
    // append hook fired before any real append), so a real replay correctly reports it
    // unresolved, with no participant apply/publish there either.
    drop(manager);
    drop(engine);
    let engine2 = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let participant2 = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine2),
    ));
    let manager2 = TransactionManager::open(&journal_path).unwrap();
    manager2.register_participant(participant2);
    let report2 = manager2.recover().unwrap();
    assert!(report2.unresolved_txns.contains(&txn_a.id()));
    assert!(!manager2.recovery_required());
    assert_eq!(engine2.get(0, b"k1", engine2.snapshot()).unwrap(), None);
}

/// Fix-pass item 1c: a latch caused by a participant apply/publish failure (the commit record
/// itself is proven durable; only the participant's own post-commit step failed) is safely
/// resolved by an in-process `recover()`, which clears the latch — no reopen required. See
/// `htap-txn/tests/rowstore_adapter.rs::test_rowstore_failure_durable_pending_reopen_recover_and_c1_ledger`
/// for the same fault-injection seam used across a reopen instead of in-process.
#[test]
fn test_latch_via_participant_apply_failure_clears_in_process_recover() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let tripped = Arc::new(AtomicBool::new(false));
    let tripped_clone = Arc::clone(&tripped);
    let hook = Arc::new(move |op| {
        if op == EngineIoOp::SstWrite && !tripped_clone.swap(true, Ordering::SeqCst) {
            Err(HtapError::Io(std::io::Error::other(
                "injected sst write failure during manager commit",
            )))
        } else {
            Ok(())
        }
    });

    let manager = TransactionManager::open(&journal_path).unwrap();
    let engine = Arc::new(
        Engine::open(
            EngineOptions::new(rowstore_dir.path())
                .with_memtable_bytes(1)
                .with_io_fault_hook(hook),
        )
        .unwrap(),
    );
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    manager.register_participant(participant);

    let mut txn_a = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_a.set_request(single_put_request(participant_id, b"k1", 42));
    let err_a = manager.commit(&mut txn_a).unwrap_err();
    assert!(
        err_a.is_durable_pending(),
        "expected DurablePending, got {err_a:?}"
    );
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // Same process, same manager and engine: recover() retries apply/publish, which now succeed
    // (the fault hook only trips once), and clears the latch.
    let report = manager.recover().unwrap();
    assert_eq!(report.committed_txns, vec![txn_a.id()]);
    assert!(
        !manager.recovery_required(),
        "a participant-I/O-caused latch must clear once an in-process recover() resolves it"
    );
    assert_eq!(engine.visible_version(), Version::new(2));
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(42))
    );

    // The row is visible exactly once, and a subsequent commit lands cleanly at V+1.
    let mut txn_b = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn_b.set_request(single_put_request(participant_id, b"k2", 2));
    let committed_b = manager.commit(&mut txn_b).unwrap();
    assert_eq!(committed_b.version, Version::new(3));
}

/// Fix-pass item 1b: an engine that is durably ahead of what its transaction journal knows about
/// (here, advanced directly via the legacy non-2PC `Engine::commit` path, which `Engine::commit`'s
/// own doc warns must never be used by session/transaction-manager code) is corruption, detected
/// and rejected loudly at `recover()`, before any new commit can be journaled.
#[test]
fn test_recover_detects_engine_ahead_of_journal_as_corruption() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");

    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"k".to_vec(),
        row: make_row(1),
    }];
    engine.commit(1, engine.snapshot(), mutations).unwrap();
    assert_eq!(engine.committed_version(), Version::new(2));

    let participant_id = ParticipantId::new(1);
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    let manager = TransactionManager::open(&journal_path).unwrap();
    manager.register_participant(participant);

    let err = manager.recover().unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected Corruption for an engine ahead of an empty journal, got {err:?}"
    );
}

/// Fix-pass item 4: the applied-external-transactions ledger cap was previously only enforced at
/// apply time, so a full ledger let a 2PC transaction durably journal its Intent and Commit
/// records and only then discover it could never be applied — a brick. It must instead be
/// rejected at `prepare`, before any journal record, with the transaction left open/aborted
/// cleanly (never `DurablePending`).
#[test]
fn test_ledger_full_commit_rejected_at_prepare_before_journal_growth() {
    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let manager = TransactionManager::open(&journal_path).unwrap();
    let engine = Arc::new(
        Engine::open(
            EngineOptions::new(rowstore_dir.path()).with_max_applied_external_txns_for_test(1),
        )
        .unwrap(),
    );
    let participant = Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    ));
    manager.register_participant(participant);

    // First commit fills the (test-lowered) ledger to its cap.
    let mut txn1 = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn1.set_request(single_put_request(participant_id, b"k1", 1));
    manager.commit(&mut txn1).unwrap();

    // A second, otherwise-valid commit must be rejected at prepare, before any journal record.
    let journal_len_before = std::fs::metadata(&journal_path).unwrap().len();
    let mut txn2 = Transaction::new(manager.next_txn_id().unwrap(), manager.visible_version());
    txn2.set_request(single_put_request(participant_id, b"k2", 2));
    let err = manager.commit(&mut txn2).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for a full ledger, got {err:?}"
    );
    assert!(
        !err.is_durable_pending(),
        "a ledger-full rejection at prepare must never surface as DurablePending"
    );
    assert_eq!(txn2.state(), TxnState::Aborted);

    let journal_len_after = std::fs::metadata(&journal_path).unwrap().len();
    assert_eq!(
        journal_len_before, journal_len_after,
        "prepare-time rejection must leave the journal untouched"
    );
}
