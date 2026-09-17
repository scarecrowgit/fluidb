//! Synchronous local transaction manager coordinating two-phase commit
//! with durable journaling, deterministic participant ordering, and crash recovery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use htap_common::{HtapError, Result, Version};
use parking_lot::{Mutex, RwLock};

use crate::journal::{Journal, JournalOptions, JournalRecord};
use crate::participant::{
    CommittedTransaction, ParticipantId, ParticipantWork, TransactionId, TransactionRequest,
    TxnParticipant,
};

/// Lifecycle state of a local transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// Transaction is currently active and can register participants or mutate data.
    Active,
    /// Transaction prepared and staged across participants.
    Prepared,
    /// Transaction commit record is durable in journal; apply/publish pending completion.
    DurablyCommitted,
    /// Transaction has fully committed, applied, and published.
    Committed,
    /// Transaction aborted; staged participant state has been rolled back.
    Aborted,
}

impl TxnState {
    /// Alternate alias for [`TxnState::DurablyCommitted`].
    #[allow(non_upper_case_globals)]
    pub const CompletionPending: Self = Self::DurablyCommitted;

    /// Upper-case constant alias for [`TxnState::DurablyCommitted`].
    pub const COMPLETION_PENDING: Self = Self::DurablyCommitted;

    /// Returns true if the transaction has reached durable commit.
    pub fn is_durably_committed(&self) -> bool {
        matches!(self, Self::DurablyCommitted | Self::Committed)
    }
}

/// A transaction handle managed by [`TransactionManager`].
#[derive(Debug)]
pub struct Transaction {
    id: TransactionId,
    read_version: Version,
    commit_version: Option<Version>,
    participants: Vec<ParticipantWork>,
    state: TxnState,
}

impl Transaction {
    /// Creates a new active transaction handle.
    pub fn new(id: TransactionId, read_version: Version) -> Self {
        Self {
            id,
            read_version,
            commit_version: None,
            participants: Vec::new(),
            state: TxnState::Active,
        }
    }

    /// Return the unique transaction identifier.
    pub fn id(&self) -> TransactionId {
        self.id
    }

    /// Return the read/snapshot version assigned at transaction begin.
    pub fn read_version(&self) -> Version {
        self.read_version
    }

    /// Return the commit version assigned at transaction commit, if committed.
    pub fn commit_version(&self) -> Option<Version> {
        self.commit_version
    }

    /// Return the current lifecycle state of this transaction.
    pub fn state(&self) -> TxnState {
        self.state
    }

    /// Return the participant works registered with this transaction.
    pub fn participants(&self) -> &[ParticipantWork] {
        &self.participants
    }

    /// Register a participant with its opaque mutation payload.
    pub fn add_participant(
        &mut self,
        participant_id: impl Into<ParticipantId>,
        payload: impl Into<Vec<u8>>,
    ) {
        self.participants
            .push(ParticipantWork::new(participant_id, payload));
    }

    /// Register a participant work unit directly.
    pub fn add_work(&mut self, work: ParticipantWork) {
        self.participants.push(work);
    }

    /// Replace participant work units from a [`TransactionRequest`].
    pub fn set_request(&mut self, request: TransactionRequest) {
        self.participants = request.participants;
    }

    /// Export the transaction's work units as a [`TransactionRequest`].
    pub fn to_request(&self) -> TransactionRequest {
        TransactionRequest {
            participants: self.participants.clone(),
        }
    }
}

/// Report returned following journal crash recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Transactions successfully committed and published during recovery.
    pub committed_txns: Vec<TransactionId>,
    /// Explicitly aborted transactions observed in journal.
    pub aborted_txns: Vec<TransactionId>,
    /// Transactions left unresolved during recovery (e.g. intents missing commit/abort or safely discarded torn tails).
    pub unresolved_txns: Vec<TransactionId>,
    /// Detailed reasons for transactions left unresolved.
    pub unresolved_reasons: BTreeMap<TransactionId, String>,
    /// Highest assigned MVCC version found during journal replay.
    pub max_version: Version,
    /// Re-established visible MVCC version.
    pub visible_version: Version,
}

/// Type alias for deterministic journal decision boundary test hooks.
pub type JournalHook = Box<dyn FnOnce(&mut Journal) -> Result<()> + Send>;

/// What kind of failure latched the manager, and therefore whether [`TransactionManager::recover`]
/// is allowed to clear it in-process or must wait for a fresh reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryCause {
    /// The journal append or sync itself failed (or a test hook stood in for one failing) at the
    /// commit decision boundary. An in-process resync afterward proves nothing: fsync succeeding
    /// after an earlier fsync failed does not establish that the earlier write is durable (the
    /// kernel may already consider the page clean). Only a fresh reopen — a brand new file open
    /// and scan, exactly what a real process restart would do — can be trusted here.
    JournalIo,
    /// The commit record itself is proven durable (successfully appended and fsynced); the
    /// failure was in a participant's own `apply`/`publish` step, which `recover()` retries with
    /// the exact same durable payload and can safely resolve in-process.
    ParticipantIo,
}

/// Details of the earliest unresolved `DurablePending` commit, latched manager-wide.
///
/// Once any [`TransactionManager::commit`] call returns `DurablePending` (the commit record is
/// durable but apply/publish for it did not complete), a *later* transaction cannot safely
/// allocate the next commit version: the pending transaction's own commit version is not yet
/// applied, so a later commit would apply out of order and `apply_external`/`publish` would
/// reject it, in turn journaling a second `Intent`/failed attempt and leaving `recover()` unable
/// to make sense of the journal on the next reopen (storage-reviewer finding F3). Latching here
/// rejects every later `commit` before it does any work, with a non-retryable
/// [`HtapError::RecoveryRequired`] (never `Conflict`), until [`TransactionManager::recover`]
/// resolves the latched transaction in-process (only possible when `cause` is
/// [`RecoveryCause::ParticipantIo`]; see [`RecoveryCause`]) or the manager is reopened from a
/// fresh journal replay.
#[derive(Debug, Clone)]
struct RecoveryLatch {
    txn_id: u64,
    version: Version,
    reason: String,
    cause: RecoveryCause,
}

/// Synchronous local transaction manager.
///
/// Orchestrates 2PC across local participants using deterministic ID ordering
/// to prevent deadlocks and guarantees durability via CRC32C journal logging.
pub struct TransactionManager {
    decision_lock: Mutex<()>,
    intent_append_hook: Mutex<Option<JournalHook>>,
    commit_append_hook: Mutex<Option<JournalHook>>,
    commit_sync_hook: Mutex<Option<JournalHook>>,
    abort_append_hook: Mutex<Option<JournalHook>>,
    journal: Mutex<Journal>,
    participants: RwLock<BTreeMap<ParticipantId, Arc<dyn TxnParticipant>>>,
    next_txn_id: AtomicU64,
    next_version: Mutex<Version>,
    visible_version: Mutex<Version>,
    /// Set by [`Self::commit`] whenever it returns `DurablePending`; see [`RecoveryLatch`].
    recovery_required: Mutex<Option<RecoveryLatch>>,
}

impl TransactionManager {
    /// Create a new transaction manager using the provided [`Journal`].
    pub fn new(journal: Journal) -> Self {
        Self {
            decision_lock: Mutex::new(()),
            intent_append_hook: Mutex::new(None),
            commit_append_hook: Mutex::new(None),
            commit_sync_hook: Mutex::new(None),
            abort_append_hook: Mutex::new(None),
            journal: Mutex::new(journal),
            participants: RwLock::new(BTreeMap::new()),
            next_txn_id: AtomicU64::new(1),
            next_version: Mutex::new(Version::new(2)),
            visible_version: Mutex::new(Version::INITIAL),
            recovery_required: Mutex::new(None),
        }
    }

    /// Returns `true` if an earlier `DurablePending` commit has latched the manager: every
    /// [`Self::commit`] call is rejected until [`Self::recover`] resolves it or the manager is
    /// reopened from a fresh journal replay.
    pub fn recovery_required(&self) -> bool {
        self.recovery_required.lock().is_some()
    }

    /// Latches the manager on a `DurablePending` outcome, keeping the earliest one if called more
    /// than once (later `commit` calls are rejected before they can produce a second).
    fn latch_recovery_required(
        &self,
        txn_id: u64,
        version: Version,
        reason: &str,
        cause: RecoveryCause,
    ) {
        let mut latch = self.recovery_required.lock();
        if latch.is_none() {
            *latch = Some(RecoveryLatch {
                txn_id,
                version,
                reason: reason.to_string(),
                cause,
            });
        }
    }

    /// Builds a `DurablePending` error for `txn_id`/`version`/`reason` and latches the manager
    /// (F3) so every later `commit` is rejected until recovery resolves it. `cause` records
    /// whether [`TransactionManager::recover`] may clear this latch in-process (see
    /// [`RecoveryCause`]).
    fn durable_pending(
        &self,
        txn_id: u64,
        version: Version,
        reason: String,
        cause: RecoveryCause,
    ) -> HtapError {
        self.latch_recovery_required(txn_id, version, &reason, cause);
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        }
    }

    /// Builds the non-retryable error returned for every `commit` call while the manager is
    /// latched: [`HtapError::RecoveryRequired`] naming the latched transaction, so it never maps
    /// to a retryable wire error (e.g. MySQL 1213/`40001`) and is never confused with the latched
    /// transaction's own `DurablePending` (this transaction itself did no work and definitely did
    /// not commit).
    fn recovery_required_error(latch: &RecoveryLatch) -> HtapError {
        HtapError::RecoveryRequired {
            blocking_txn: latch.txn_id,
            reason: format!(
                "manager is latched pending recovery of an earlier ambiguous commit (txn {}, \
                 version {}): {}; no further commits are accepted until `recover()` resolves it \
                 in-process or the manager is reopened",
                latch.txn_id, latch.version, latch.reason
            ),
        }
    }

    /// Open or create a transaction manager with default journal options at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let journal = Journal::open(path)?;
        Ok(Self::new(journal))
    }

    /// Open or create a transaction manager using explicit journal options.
    pub fn open_with_options(opts: JournalOptions) -> Result<Self> {
        let journal = Journal::open_with_options(opts)?;
        Ok(Self::new(journal))
    }

    /// Register a participant with the manager.
    pub fn register_participant(&self, participant: Arc<dyn TxnParticipant>) {
        self.participants
            .write()
            .insert(participant.id(), participant);
    }

    /// Unregister a participant by ID, returning it if present.
    pub fn unregister_participant(&self, id: ParticipantId) -> Option<Arc<dyn TxnParticipant>> {
        self.participants.write().remove(&id)
    }

    /// Retrieve a registered participant by ID.
    pub fn get_participant(&self, id: ParticipantId) -> Option<Arc<dyn TxnParticipant>> {
        self.participants.read().get(&id).cloned()
    }

    /// Sets a one-shot deterministic test hook invoked right before the Intent record append
    /// (fix-pass round 3, item 1: lets tests exercise a real journal-I/O failure at the Intent
    /// decision boundary, distinct from the existing commit-boundary hooks below).
    #[doc(hidden)]
    pub fn set_intent_append_hook<F>(&self, hook: F)
    where
        F: FnOnce(&mut Journal) -> Result<()> + Send + 'static,
    {
        *self.intent_append_hook.lock() = Some(Box::new(hook));
    }

    /// Sets a one-shot deterministic test hook invoked right before commit record append.
    pub fn set_commit_append_hook<F>(&self, hook: F)
    where
        F: FnOnce(&mut Journal) -> Result<()> + Send + 'static,
    {
        *self.commit_append_hook.lock() = Some(Box::new(hook));
    }

    /// Sets a one-shot deterministic test hook invoked right before commit record sync.
    pub fn set_commit_sync_hook<F>(&self, hook: F)
    where
        F: FnOnce(&mut Journal) -> Result<()> + Send + 'static,
    {
        *self.commit_sync_hook.lock() = Some(Box::new(hook));
    }

    /// Sets a one-shot deterministic test hook invoked right before the Abort record append
    /// (fix-pass round 3, item 1: lets tests exercise a real journal-I/O failure while aborting).
    #[doc(hidden)]
    pub fn set_abort_append_hook<F>(&self, hook: F)
    where
        F: FnOnce(&mut Journal) -> Result<()> + Send + 'static,
    {
        *self.abort_append_hook.lock() = Some(Box::new(hook));
    }

    /// Allocate a new monotonic transaction ID.
    pub fn next_txn_id(&self) -> Result<TransactionId> {
        let mut current = self.next_txn_id.load(Ordering::SeqCst);
        loop {
            let next = current
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow { counter: "txn_id" })?;
            match self.next_txn_id.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(val) => return Ok(TransactionId::new(val)),
                Err(actual) => current = actual,
            }
        }
    }

    /// Allocate a new monotonic commit version.
    pub fn allocate_version(&self) -> Result<Version> {
        let mut v = self.next_version.lock();
        let assigned = *v;
        let next = assigned.checked_next()?;
        *v = next;
        Ok(assigned)
    }

    /// Return the current visible MVCC version.
    pub fn visible_version(&self) -> Version {
        *self.visible_version.lock()
    }

    /// Return the next version that will be allocated.
    pub fn next_version(&self) -> Version {
        *self.next_version.lock()
    }

    /// Return this manager's journal's configured maximum frame payload size.
    ///
    /// Fix-pass round 3, item 3(d): a cheap (single uncontended lock, no I/O) way for callers
    /// that need to bound an estimated durable `Intent` frame size (e.g.
    /// [`crate::journal::intent_frame_size_bound`]'s callers) against the actual configured limit
    /// of the journal this manager will really append to, instead of assuming
    /// [`crate::journal::DEFAULT_MAX_FRAME_SIZE`].
    pub fn max_frame_size(&self) -> usize {
        self.journal.lock().options().max_frame_size
    }

    /// Begin a new synchronous transaction.
    pub fn begin(&self) -> Result<Transaction> {
        let id = self.next_txn_id()?;
        let read_version = self.visible_version();
        Ok(Transaction::new(id, read_version))
    }

    /// Commit a transaction request directly, beginning a new transaction internally.
    pub fn commit_request(&self, request: TransactionRequest) -> Result<CommittedTransaction> {
        let mut txn = self.begin()?;
        txn.set_request(request);
        self.commit(&mut txn)
    }

    /// Commit a transaction using two-phase commit with deterministic participant ordering.
    ///
    /// 1. Validates transaction state and request constraints (nonempty, unique, max 16 MiB).
    /// 2. Deterministically prepares participants in ascending ID order.
    /// 3. Writes and fsyncs durable `Intent` record with exact payloads and snapshot.
    /// 4. Assigns monotonic commit version.
    /// 5. Writes and fsyncs durable `Commit` record (linearization point).
    /// 6. Applies mutations and publishes visibility in deterministic ascending ID order.
    /// 7. Advances visible version only after all participants succeed.
    pub fn commit(&self, txn: &mut Transaction) -> Result<CommittedTransaction> {
        let _decision_guard = self.decision_lock.lock();

        // Reject before touching anything else (F3): an earlier commit's outcome is still
        // ambiguous, so this transaction's own commit version must not be allocated ahead of it.
        if let Some(latch) = self.recovery_required.lock().as_ref() {
            return Err(Self::recovery_required_error(latch));
        }

        if txn.state != TxnState::Active {
            return Err(HtapError::Conflict(format!(
                "transaction {} cannot commit in state {:?}",
                txn.id, txn.state
            )));
        }

        // 1. Validate request
        let request = txn.to_request();
        request.validate()?;

        // Sort participants deterministically in ascending ID order
        let mut sorted_works = request.participants;
        sorted_works.sort_by_key(|w| w.participant_id);

        // Fix-pass item 3c: reject an oversize durable `Intent` frame here, before any prepare or
        // journal work. `TransactionRequest::validate` above only bounds the raw mutation JSON
        // bytes (16 MiB); `serde_json` then re-encodes each participant's `payload: Vec<u8>` as a
        // JSON number array (~3-4 bytes per original byte) inside the `Intent` frame, so a request
        // that passes that check can still produce a frame bigger than this journal's own
        // configured `max_frame_size` — which would otherwise only be discovered by `encode_frame`
        // itself, after prepare has already run. `intent_frame_size_bound` is a conservative
        // (never-underestimating) closed-form bound from just the total payload byte count and the
        // participant count (fix-pass round 3, item 3(c): many small-payload participants each add
        // their own JSON object overhead).
        {
            let total_payload_bytes: usize = sorted_works.iter().map(|w| w.payload.len()).sum();
            let max_frame_size = self.max_frame_size();
            let bound =
                crate::journal::intent_frame_size_bound(total_payload_bytes, sorted_works.len());
            if bound > max_frame_size {
                return Err(HtapError::InvalidArgument(format!(
                    "transaction request's estimated durable Intent frame size ({bound} bytes, \
                     from {total_payload_bytes} bytes of mutation payload) exceeds the journal's \
                     maximum frame size ({max_frame_size} bytes); reduce the size of this \
                     transaction"
                )));
            }
        }

        // Resolve participants from registry; keep registry lock out of participant method calls
        let resolved: Vec<Arc<dyn TxnParticipant>> = {
            let registry = self.participants.read();
            let mut list = Vec::with_capacity(sorted_works.len());
            for work in &sorted_works {
                let p = registry.get(&work.participant_id).ok_or_else(|| {
                    HtapError::NotFound(format!(
                        "participant {} is not registered",
                        work.participant_id
                    ))
                })?;
                list.push(Arc::clone(p));
            }
            list
        };

        // 2. Prepare Phase in deterministic sorted order
        let mut prepared: Vec<Arc<dyn TxnParticipant>> = Vec::with_capacity(resolved.len());
        for (work, p) in sorted_works.iter().zip(&resolved) {
            if let Err(err) = p.prepare(txn.read_version, &work.payload) {
                // Prepare rejected: rollback all previously prepared participants
                for prep in prepared.iter().rev() {
                    let _ = prep.abort(txn.id);
                }
                txn.state = TxnState::Aborted;
                return Err(err);
            }
            prepared.push(Arc::clone(p));
        }

        txn.state = TxnState::Prepared;

        // Check that commit version successor can be allocated before durable decision / mutations
        {
            let next_v = self.next_version.lock();
            if let Err(err) = next_v.checked_next() {
                for prep in prepared.iter().rev() {
                    let _ = prep.abort(txn.id);
                }
                txn.state = TxnState::Aborted;
                return Err(err);
            }
        }

        // 3. Fsync Intent record
        let intent_record = JournalRecord::Intent {
            txn_id: txn.id,
            snapshot: txn.read_version,
            participants: sorted_works.clone(),
        };
        {
            let mut journal = self.journal.lock();
            let intent_result = {
                // Hook for deterministic fault injection at Intent append, matching the existing
                // commit-boundary hooks' shape: fires first as a pre-check, and if it returns
                // `Ok`, the real append/sync below still runs normally.
                let hook_result = self
                    .intent_append_hook
                    .lock()
                    .take()
                    .map(|hook| hook(&mut journal));
                match hook_result {
                    Some(Err(err)) => Err(err),
                    Some(Ok(())) | None => {
                        journal.append(&intent_record).and_then(|_| journal.sync())
                    }
                }
            };
            if let Err(err) = intent_result {
                for prep in prepared.iter().rev() {
                    let _ = prep.abort(txn.id);
                }
                txn.state = TxnState::Aborted;
                // Fix-pass round 3, item 1: a real journal I/O failure while writing the Intent
                // record means this journal file handle's own state may no longer be
                // trustworthy (the underlying `Journal` may have poisoned itself; see
                // `Journal`'s `poisoned` field), even though this specific transaction's own
                // outcome here is a clean, definite abort. Latch the manager so a later
                // `commit`/`abort` reports the same well-known `RecoveryRequired` instead of a
                // raw journal I/O error, until a fresh reopen — never for a pure validation
                // failure (e.g. `encode_frame` rejecting an oversize frame), which is not a
                // journal-I/O problem: only this one oversize transaction was rejected, and a
                // smaller retry must still work normally.
                if matches!(err, HtapError::Io(_)) {
                    self.latch_recovery_required(
                        txn.id.as_u64(),
                        Version::INITIAL,
                        &format!("intent journal write failed: {err}"),
                        RecoveryCause::JournalIo,
                    );
                }
                return Err(err);
            }
        }

        // 4. Assign monotonic commit version
        let version = self.allocate_version()?;
        txn.commit_version = Some(version);

        // Linearization point boundary entered: once Commit frame append begins,
        // transaction state transitions to DurablyCommitted / CompletionPending.
        // It cannot be rolled back or aborted.
        txn.state = TxnState::DurablyCommitted;

        let commit_record = JournalRecord::Commit {
            txn_id: txn.id,
            version,
        };

        // 5. Append and Sync Commit record at the decision boundary
        {
            let mut journal = self.journal.lock();

            // Hook for deterministic fault injection at commit append
            if let Some(hook) = self.commit_append_hook.lock().take() {
                if let Err(err) = hook(&mut journal) {
                    return Err(self.durable_pending(
                        txn.id.as_u64(),
                        version,
                        format!("commit append failed at decision boundary: {err}"),
                        RecoveryCause::JournalIo,
                    ));
                }
            }

            // 5a. Append Commit frame (without sync)
            if let Err(err) = journal.append_nosync(&commit_record) {
                return Err(self.durable_pending(
                    txn.id.as_u64(),
                    version,
                    format!(
                        "commit append failed: {err}; commit status ambiguous, recovery required"
                    ),
                    RecoveryCause::JournalIo,
                ));
            }

            // Hook for deterministic fault injection at commit sync
            if let Some(hook) = self.commit_sync_hook.lock().take() {
                if let Err(err) = hook(&mut journal) {
                    return Err(self.durable_pending(
                        txn.id.as_u64(),
                        version,
                        format!("commit sync failed at decision boundary: {err}"),
                        RecoveryCause::JournalIo,
                    ));
                }
            }

            // 5b. Sync Commit frame
            if let Err(err) = journal.sync() {
                return Err(self.durable_pending(
                    txn.id.as_u64(),
                    version,
                    format!(
                        "commit sync failed: {err}; commit status ambiguous, recovery required"
                    ),
                    RecoveryCause::JournalIo,
                ));
            }
        }

        // 6. Apply participants in deterministic sorted order
        for (work, p) in sorted_works.iter().zip(&resolved) {
            if let Err(err) = p.apply(txn.id, version, &work.payload) {
                return Err(self.durable_pending(
                    txn.id.as_u64(),
                    version,
                    format!("participant {} apply failed: {err}", work.participant_id),
                    RecoveryCause::ParticipantIo,
                ));
            }
        }

        // 7. Publish visibility in deterministic sorted order
        for p in &resolved {
            if let Err(err) = p.publish(txn.id, version) {
                return Err(self.durable_pending(
                    txn.id.as_u64(),
                    version,
                    format!("participant {} publish failed: {err}", p.id()),
                    RecoveryCause::ParticipantIo,
                ));
            }
        }

        // 8. Advance visible version only after all succeed
        {
            let mut vis = self.visible_version.lock();
            if let Ok(next_vis) = vis.checked_next() {
                if version == next_vis {
                    *vis = version;
                }
            }
        }

        txn.state = TxnState::Committed;
        let participant_ids = sorted_works.into_iter().map(|w| w.participant_id).collect();
        Ok(CommittedTransaction {
            transaction_id: txn.id,
            version,
            snapshot: txn.read_version,
            participant_ids,
        })
    }

    /// Abort an active transaction, rolling back participants and writing an Abort journal entry.
    ///
    /// A transaction that has already durably committed (from its own state, regardless of the
    /// manager-wide latch) is always rejected with `Conflict`, exactly as before. Otherwise,
    /// rejected while the manager is latched on a [`RecoveryCause::JournalIo`] cause (fix-pass
    /// item 6c): an `Abort` record is itself a real journal append, and a journal-I/O latch means
    /// this journal's own state may not be trustworthy until a fresh reopen resyncs it. A latch
    /// caused by a participant apply/publish failure ([`RecoveryCause::ParticipantIo`]) leaves the
    /// journal itself sound, so an unrelated transaction's `abort` is still permitted.
    pub fn abort(&self, txn: &mut Transaction) -> Result<()> {
        let _decision_guard = self.decision_lock.lock();

        if txn.state == TxnState::Committed || txn.state == TxnState::DurablyCommitted {
            return Err(HtapError::Conflict(format!(
                "transaction {} has already durably committed and cannot be aborted",
                txn.id
            )));
        }

        if txn.state == TxnState::Aborted {
            return Ok(());
        }

        if let Some(latch) = self.recovery_required.lock().as_ref() {
            if latch.cause == RecoveryCause::JournalIo {
                return Err(Self::recovery_required_error(latch));
            }
        }

        // Keep participant registry lock out of participant method calls by cloning sorted Arcs first
        let participants_to_abort: Vec<Arc<dyn TxnParticipant>> = {
            let registry = self.participants.read();
            txn.participants
                .iter()
                .filter_map(|work| registry.get(&work.participant_id).cloned())
                .collect()
        };
        let mut sorted_participants = participants_to_abort;
        sorted_participants.sort_by_key(|p| p.id());
        sorted_participants.dedup_by_key(|p| p.id());

        for p in &sorted_participants {
            let _ = p.abort(txn.id);
        }

        {
            let mut journal = self.journal.lock();
            let abort_result = {
                // Hook for deterministic fault injection at Abort append, matching the Intent/
                // commit-boundary hooks' shape: fires first as a pre-check, and if it returns
                // `Ok`, the real append/sync below still runs normally.
                let hook_result = self
                    .abort_append_hook
                    .lock()
                    .take()
                    .map(|hook| hook(&mut journal));
                match hook_result {
                    Some(Err(err)) => Err(err),
                    Some(Ok(())) | None => journal
                        .append(&JournalRecord::Abort { txn_id: txn.id })
                        .and_then(|_| journal.sync()),
                }
            };
            if let Err(err) = abort_result {
                // Fix-pass round 3, item 1: same reasoning as the Intent-append case above — a
                // real journal I/O failure here means this journal handle's own state may no
                // longer be trustworthy, so latch the manager (JournalIo cause) rather than
                // leaving future callers to see a raw, inconsistent journal error.
                if matches!(err, HtapError::Io(_)) {
                    self.latch_recovery_required(
                        txn.id.as_u64(),
                        Version::INITIAL,
                        &format!("abort journal write failed: {err}"),
                        RecoveryCause::JournalIo,
                    );
                }
                return Err(err);
            }
        }

        txn.state = TxnState::Aborted;
        Ok(())
    }

    /// Recover state from the journal.
    ///
    /// Replays logged records, completes apply/publish for transactions with a durable
    /// commit marker using exact stored payloads, ignores uncommitted intents,
    /// requires registered participants, and restores monotonic counters.
    pub fn recover(&self) -> Result<RecoveryReport> {
        let _decision_guard = self.decision_lock.lock();

        // Fix-pass round 3, item 2: a `JournalIo`-caused latch means an earlier append or sync on
        // this same journal file handle failed in a way whose durability is unknowable; an
        // in-process `sync()` succeeding afterward (even the one a few lines below) proves
        // nothing about whatever failed earlier on that same fd, so replaying anything here could
        // durably apply a `Commit` whose journal record the next real reopen finds missing or
        // corrupt. Refuse outright and apply nothing until a fresh reopen (a brand new `Journal`
        // handle, whose own `scan`/repair is trustworthy). Also treat the underlying `Journal`
        // being separately marked poisoned (see `Journal::is_poisoned`) as the same condition,
        // latching the manager defensively if nothing already had, so this invariant holds even
        // if a caller poisoned the journal through some path that did not itself latch.
        {
            let already_latched_journal_io = self
                .recovery_required
                .lock()
                .as_ref()
                .is_some_and(|latch| latch.cause == RecoveryCause::JournalIo);
            let journal_poisoned = self.journal.lock().is_poisoned();
            if already_latched_journal_io || journal_poisoned {
                if journal_poisoned {
                    let reason = self
                        .journal
                        .lock()
                        .poison_reason()
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "journal is poisoned".to_string());
                    self.latch_recovery_required(
                        0,
                        Version::INITIAL,
                        &reason,
                        RecoveryCause::JournalIo,
                    );
                }
                let latch = self.recovery_required.lock();
                return Err(Self::recovery_required_error(latch.as_ref().expect(
                    "either already latched above, or just latched by the poisoned-journal branch",
                )));
            }
        }

        let (records, torn_detail) = self.journal.lock().recover_records()?;

        // Fix-pass item 1a: force a real fsync of the journal now, before any participant apply
        // below. A `Commit` record we are about to treat as durable and replay into a participant
        // may still only be sitting in the OS page cache — reading it back via `recover_records`'s
        // ordinary file read proves nothing about durability by itself (see the crash sequence in
        // this module's fix-pass notes: `append_nosync` succeeds, the process crashes or a sync
        // fails, and a naive `recover()` would apply a record that a real crash then loses). No
        // `sync_dir` is needed alongside this: the journal file already existed and was neither
        // created nor renamed here, only its already-open handle is being fsynced.
        //
        // Fix-pass round 3, item 2: if this very fsync fails, latch the manager (the underlying
        // `Journal` poisons itself automatically; see `Journal::sync`) and apply nothing —
        // consistent with the top-of-function check above rejecting every subsequent `recover()`
        // call until a fresh reopen.
        if let Err(err) = self.journal.lock().sync() {
            self.latch_recovery_required(
                0,
                Version::INITIAL,
                &format!("recover()'s own fsync failed: {err}"),
                RecoveryCause::JournalIo,
            );
            return Err(err);
        }

        let mut intents: BTreeMap<TransactionId, (Version, Vec<ParticipantWork>)> = BTreeMap::new();
        let mut commits: BTreeMap<TransactionId, Version> = BTreeMap::new();
        let mut aborts: BTreeSet<TransactionId> = BTreeSet::new();

        let mut max_txn_id = 0u64;
        let mut max_version = Version::INITIAL;

        for rec in records {
            max_txn_id = max_txn_id.max(rec.txn_id().as_u64());
            if let Some(v) = rec.version() {
                max_version = max_version.max(v);
            }

            match rec {
                JournalRecord::Intent {
                    txn_id,
                    snapshot,
                    participants,
                } => {
                    intents.insert(txn_id, (snapshot, participants));
                }
                JournalRecord::Commit { txn_id, version } => {
                    commits.insert(txn_id, version);
                }
                JournalRecord::Abort { txn_id } => {
                    aborts.insert(txn_id);
                }
            }
        }

        // Recovery must reject Commit+Abort for the same txn as corruption; Abort only valid before durable Commit.
        for txn_id in commits.keys() {
            if aborts.contains(txn_id) {
                return Err(HtapError::Corruption(format!(
                    "malformed journal: transaction {txn_id} contains both commit and abort records"
                )));
            }
        }

        // Keep participant registry lock out of participant method calls by cloning first
        let registry_snapshot = self.participants.read().clone();
        let mut committed_txns = Vec::new();

        // Sort commits by assigned version to replay in exact linear order
        let mut commit_list: Vec<(TransactionId, Version)> = commits.into_iter().collect();
        commit_list.sort_by_key(|(_, v)| *v);

        for (txn_id, commit_version) in commit_list {
            let (_snapshot, mut works) = intents.remove(&txn_id).ok_or_else(|| {
                HtapError::Corruption(format!(
                    "committed transaction {txn_id} missing corresponding intent in journal"
                ))
            })?;

            // Sort participants deterministically by ID
            works.sort_by_key(|w| w.participant_id);

            // Recovery requires registered participants; resolve sorted Arcs first
            let mut resolved: Vec<Arc<dyn TxnParticipant>> = Vec::with_capacity(works.len());
            for work in &works {
                let p = registry_snapshot.get(&work.participant_id).ok_or_else(|| {
                    HtapError::NotFound(format!(
                        "participant {} required for recovery of txn {txn_id} is not registered",
                        work.participant_id
                    ))
                })?;
                resolved.push(Arc::clone(p));
            }

            // Apply exact participant payloads in sorted order (no registry lock held)
            for (work, p) in works.iter().zip(&resolved) {
                p.apply(txn_id, commit_version, &work.payload)
                    .map_err(|err| match err {
                        HtapError::InvalidArgument(msg) => HtapError::Corruption(format!(
                            "corrupted participant {} payload during journal recovery for txn {txn_id}: {msg}",
                            work.participant_id
                        )),
                        other => other,
                    })?;
            }

            // Publish in sorted order (no registry lock held)
            for p in &resolved {
                p.publish(txn_id, commit_version)?;
            }

            // Advance visible version under manager state lock
            {
                let mut vis = self.visible_version.lock();
                if commit_version > *vis {
                    *vis = commit_version;
                }
            }

            committed_txns.push(txn_id);
        }

        // Remove aborted transactions from intents map
        for aborted_id in &aborts {
            intents.remove(aborted_id);
        }
        let aborted_txns: Vec<TransactionId> = aborts.into_iter().collect();

        // Any remaining intents had neither commit nor abort: left unresolved
        let mut unresolved_txns = Vec::new();
        let mut unresolved_reasons = BTreeMap::new();

        for (unresolved_id, (snap, _)) in intents {
            unresolved_txns.push(unresolved_id);
            let reason = if let Some(ref torn) = torn_detail {
                format!(
                    "transaction {unresolved_id} left unresolved: intent logged at snapshot {snap}, but commit record was torn and safely discarded ({torn})"
                )
            } else {
                format!(
                    "transaction {unresolved_id} left unresolved: intent logged at snapshot {snap}, but no commit or abort record found in journal"
                )
            };
            tracing::warn!(txn_id = %unresolved_id, %reason, "unresolved transaction during recovery");
            unresolved_reasons.insert(unresolved_id, reason);
        }

        // Fix-pass item 1b: cross-check each registered participant's own durable state against
        // what this replay just established. In one shared MVCC version domain, a participant
        // that tracks a comparable version (see `TxnParticipant::committed_version`) must land
        // exactly on `max_version` once every committed transaction above has been applied and
        // published. A participant strictly ahead means it already durably applied a commit whose
        // record has since gone missing from this very journal (exactly the corruption this fix
        // pass exists to catch); a participant still behind after replay means replay itself
        // failed to fully apply the journal's committed transactions. Either is corruption, not
        // something a later commit can safely paper over — fail loudly here, at open, before any
        // new commit can be journaled.
        for participant in registry_snapshot.values() {
            if let Some(engine_version) = participant.committed_version() {
                if engine_version > max_version {
                    return Err(HtapError::Corruption(format!(
                        "participant {} is ahead of the transaction journal after recovery: \
                         engine committed_version {engine_version} > journal max_version \
                         {max_version}; a durable commit record was lost from the journal",
                        participant.id()
                    )));
                }
                if engine_version < max_version {
                    return Err(HtapError::Corruption(format!(
                        "participant {} is behind the transaction journal after recovery: \
                         engine committed_version {engine_version} < journal max_version \
                         {max_version}; replay did not fully apply the journal's committed \
                         transactions",
                        participant.id()
                    )));
                }
            }
        }

        let next_txn = max_txn_id
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow { counter: "txn_id" })?;
        let next_v = max_version.checked_next()?;

        // Fix-pass item 2: `next_txn_id()`/`begin()` allocate transaction ids via a lock-free CAS
        // loop that does not take `decision_lock`, so a live `begin()` can race concurrently with
        // this `recover()` (unlike `next_version`/`visible_version`, which are only ever mutated
        // while `decision_lock` is held, so `recover()` cannot race a concurrent `commit()`
        // touching them). A plain `store` here could move the counter backwards below an id
        // already handed out to a live in-flight transaction, producing duplicate txn ids; use
        // `fetch_max` so recovery only ever raises the counter, never lowers it.
        self.next_txn_id.fetch_max(next_txn, Ordering::SeqCst);
        *self.next_version.lock() = next_v;
        *self.visible_version.lock() = max_version;

        // F3 / fix-pass item 1c: clear the manager-wide recovery latch, if any, once this replay
        // has conclusively resolved the latched transaction one way or the other — but only when
        // `cause` is `RecoveryCause::ParticipantIo`. A `ParticipantIo` latch's commit record was
        // already proven durable (appended and fsynced) before the failure; `recover()` replaying
        // it here either completes it (it now appears in `committed_txns`, exactly what an
        // in-process retry of its own apply/publish does) or conclusively shows it never became
        // durable (it appears in `unresolved_txns`), and either conclusion is trustworthy
        // in-process. A `RecoveryCause::JournalIo` latch is never cleared here, regardless of what
        // this replay concludes: the journal append or sync itself failed, and an in-process
        // resync afterward (even the one this same `recover()` call just performed at item 1a)
        // cannot prove the original write was durable — fsync succeeding after an earlier fsync
        // failed proves nothing about that earlier write. Only a fresh reopen (a brand new
        // `TransactionManager::new`, whose `recovery_required` always starts `None`) clears it.
        {
            let mut latch = self.recovery_required.lock();
            if let Some(pending) = latch.as_ref() {
                if pending.cause == RecoveryCause::ParticipantIo {
                    let txn_id = TransactionId::new(pending.txn_id);
                    if committed_txns.contains(&txn_id) || unresolved_txns.contains(&txn_id) {
                        *latch = None;
                    }
                }
            }
        }

        Ok(RecoveryReport {
            committed_txns,
            aborted_txns,
            unresolved_txns,
            unresolved_reasons,
            max_version,
            visible_version: max_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as SyncMutex;
    use tempfile::NamedTempFile;

    struct MockStore {
        id: ParticipantId,
        events: SyncMutex<Vec<String>>,
    }

    impl MockStore {
        fn new(id: ParticipantId) -> Self {
            Self {
                id,
                events: SyncMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().clone()
        }
    }

    impl TxnParticipant for MockStore {
        fn id(&self) -> ParticipantId {
            self.id
        }

        fn prepare(&self, snapshot: Version, payload: &[u8]) -> Result<()> {
            self.events
                .lock()
                .push(format!("{}:prepare:{snapshot}:{:?}", self.id, payload));
            Ok(())
        }

        fn apply(&self, txn_id: TransactionId, version: Version, payload: &[u8]) -> Result<()> {
            self.events.lock().push(format!(
                "{}:apply:{txn_id}:{version}:{:?}",
                self.id, payload
            ));
            Ok(())
        }

        fn abort(&self, txn_id: TransactionId) -> Result<()> {
            self.events
                .lock()
                .push(format!("{}:abort:{txn_id}", self.id));
            Ok(())
        }

        fn publish(&self, txn_id: TransactionId, version: Version) -> Result<()> {
            self.events
                .lock()
                .push(format!("{}:publish:{txn_id}:{version}", self.id));
            Ok(())
        }
    }

    #[test]
    fn test_sync_commit_and_deterministic_order() {
        let temp = NamedTempFile::new().unwrap();
        let tm = TransactionManager::open(temp.path()).unwrap();

        let s1 = Arc::new(MockStore::new(ParticipantId::new(100)));
        let s2 = Arc::new(MockStore::new(ParticipantId::new(10)));
        let s3 = Arc::new(MockStore::new(ParticipantId::new(50)));

        tm.register_participant(s1.clone());
        tm.register_participant(s2.clone());
        tm.register_participant(s3.clone());

        let mut txn = tm.begin().unwrap();
        // Insert participants in unordered order with payloads
        txn.add_participant(100, b"work100");
        txn.add_participant(10, b"work10");
        txn.add_participant(50, b"work50");

        let committed = tm.commit(&mut txn).unwrap();
        assert_eq!(committed.version, Version::new(2));
        assert_eq!(committed.snapshot, Version::INITIAL);
        assert_eq!(
            committed.participant_ids,
            vec![
                ParticipantId::new(10),
                ParticipantId::new(50),
                ParticipantId::new(100)
            ]
        );
        assert_eq!(txn.state(), TxnState::Committed);

        // Verification of execution on s2 (id 10)
        assert_eq!(
            s2.events(),
            vec![
                format!("10:prepare:v1:{:?}", b"work10"),
                format!("10:apply:1:v2:{:?}", b"work10"),
                "10:publish:1:v2".to_string()
            ]
        );
        // Verification of execution on s3 (id 50)
        assert_eq!(
            s3.events(),
            vec![
                format!("50:prepare:v1:{:?}", b"work50"),
                format!("50:apply:1:v2:{:?}", b"work50"),
                "50:publish:1:v2".to_string()
            ]
        );
        // Verification of execution on s1 (id 100)
        assert_eq!(
            s1.events(),
            vec![
                format!("100:prepare:v1:{:?}", b"work100"),
                format!("100:apply:1:v2:{:?}", b"work100"),
                "100:publish:1:v2".to_string()
            ]
        );
    }

    #[test]
    fn test_txn_id_and_version_allocation_overflow_at_u64_max() {
        let temp = NamedTempFile::new().unwrap();
        let tm = TransactionManager::open(temp.path()).unwrap();

        // 1. next_txn_id overflow
        tm.next_txn_id.store(u64::MAX, Ordering::SeqCst);
        let err = tm.next_txn_id().unwrap_err();
        assert!(matches!(
            err,
            HtapError::CounterOverflow { counter: "txn_id" }
        ));
        // Verify state is not wrapped
        assert_eq!(tm.next_txn_id.load(Ordering::SeqCst), u64::MAX);

        // begin() also fails with the same error
        let begin_err = tm.begin().unwrap_err();
        assert!(matches!(
            begin_err,
            HtapError::CounterOverflow { counter: "txn_id" }
        ));

        // 2. allocate_version overflow
        *tm.next_version.lock() = Version::new(u64::MAX);
        let ver_err = tm.allocate_version().unwrap_err();
        assert!(matches!(
            ver_err,
            HtapError::CounterOverflow { counter: "version" }
        ));
        // Verify state is not wrapped
        assert_eq!(tm.next_version(), Version::new(u64::MAX));
    }

    #[test]
    fn test_commit_version_overflow_no_journal_mutation() {
        let temp = NamedTempFile::new().unwrap();
        let tm = TransactionManager::open(temp.path()).unwrap();
        let s1 = Arc::new(MockStore::new(ParticipantId::new(1)));
        tm.register_participant(s1.clone());

        // Normal begin
        let mut txn = tm.begin().unwrap();
        txn.add_participant(1, b"payload");

        // Set next_version to u64::MAX before commit
        *tm.next_version.lock() = Version::new(u64::MAX);

        let initial_journal_len = std::fs::metadata(temp.path()).unwrap().len();

        let commit_err = tm.commit(&mut txn).unwrap_err();
        assert!(matches!(
            commit_err,
            HtapError::CounterOverflow { counter: "version" }
        ));

        // Participant was aborted
        assert_eq!(
            s1.events(),
            vec![
                format!("1:prepare:v1:{:?}", b"payload"),
                format!("1:abort:{}", txn.id())
            ]
        );
        assert_eq!(txn.state(), TxnState::Aborted);

        // Journal must not have mutated (no Intent or Commit appended)
        let final_journal_len = std::fs::metadata(temp.path()).unwrap().len();
        assert_eq!(
            initial_journal_len, final_journal_len,
            "journal must not be mutated on version overflow"
        );
    }

    #[test]
    fn test_recovery_overflow_at_u64_max() {
        let temp = NamedTempFile::new().unwrap();
        // Write an Intent and Commit record with version u64::MAX
        {
            let mut j = Journal::open(temp.path()).unwrap();
            let intent = JournalRecord::Intent {
                txn_id: TransactionId::new(10),
                snapshot: Version::INITIAL,
                participants: vec![],
            };
            let commit = JournalRecord::Commit {
                txn_id: TransactionId::new(10),
                version: Version::new(u64::MAX),
            };
            j.append(&intent).unwrap();
            j.append(&commit).unwrap();
            j.sync().unwrap();
        }

        let tm = TransactionManager::open(temp.path()).unwrap();
        let err = tm.recover().unwrap_err();
        assert!(matches!(
            err,
            HtapError::CounterOverflow { counter: "version" }
        ));

        // Now test max_txn_id at u64::MAX
        let temp2 = NamedTempFile::new().unwrap();
        {
            let mut j = Journal::open(temp2.path()).unwrap();
            let intent = JournalRecord::Intent {
                txn_id: TransactionId::new(u64::MAX),
                snapshot: Version::INITIAL,
                participants: vec![],
            };
            let commit = JournalRecord::Commit {
                txn_id: TransactionId::new(u64::MAX),
                version: Version::new(2),
            };
            j.append(&intent).unwrap();
            j.append(&commit).unwrap();
            j.sync().unwrap();
        }
        let tm2 = TransactionManager::open(temp2.path()).unwrap();
        let err2 = tm2.recover().unwrap_err();
        assert!(matches!(
            err2,
            HtapError::CounterOverflow { counter: "txn_id" }
        ));
    }
}
