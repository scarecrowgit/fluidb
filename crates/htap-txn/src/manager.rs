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
    /// Transaction prepared and commit record is durable in journal.
    Prepared,
    /// Transaction has fully committed, applied, and published.
    Committed,
    /// Transaction aborted; staged participant state has been rolled back.
    Aborted,
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
    /// Highest assigned MVCC version found during journal replay.
    pub max_version: Version,
    /// Re-established visible MVCC version.
    pub visible_version: Version,
}

/// Synchronous local transaction manager.
///
/// Orchestrates 2PC across local participants using deterministic ID ordering
/// to prevent deadlocks and guarantees durability via CRC32C journal logging.
pub struct TransactionManager {
    journal: Mutex<Journal>,
    participants: RwLock<BTreeMap<ParticipantId, Arc<dyn TxnParticipant>>>,
    next_txn_id: AtomicU64,
    next_version: Mutex<Version>,
    visible_version: Mutex<Version>,
}

impl TransactionManager {
    /// Create a new transaction manager using the provided [`Journal`].
    pub fn new(journal: Journal) -> Self {
        Self {
            journal: Mutex::new(journal),
            participants: RwLock::new(BTreeMap::new()),
            next_txn_id: AtomicU64::new(1),
            next_version: Mutex::new(Version::new(2)),
            visible_version: Mutex::new(Version::INITIAL),
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

    /// Allocate a new monotonic transaction ID.
    pub fn next_txn_id(&self) -> TransactionId {
        TransactionId::new(self.next_txn_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Allocate a new monotonic commit version.
    fn allocate_version(&self) -> Version {
        let mut v = self.next_version.lock();
        let assigned = *v;
        *v = v.next();
        assigned
    }

    /// Return the current visible MVCC version.
    pub fn visible_version(&self) -> Version {
        *self.visible_version.lock()
    }

    /// Return the next version that will be allocated.
    pub fn next_version(&self) -> Version {
        *self.next_version.lock()
    }

    /// Begin a new synchronous transaction.
    pub fn begin(&self) -> Result<Transaction> {
        let id = self.next_txn_id();
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

        // Resolve participants from registry
        let mut resolved: Vec<Arc<dyn TxnParticipant>> = Vec::with_capacity(sorted_works.len());
        {
            let registry = self.participants.read();
            for work in &sorted_works {
                let p = registry.get(&work.participant_id).ok_or_else(|| {
                    HtapError::NotFound(format!(
                        "participant {} is not registered",
                        work.participant_id
                    ))
                })?;
                resolved.push(Arc::clone(p));
            }
        }

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

        // 3. Fsync Intent record
        let intent_record = JournalRecord::Intent {
            txn_id: txn.id,
            snapshot: txn.read_version,
            participants: sorted_works.clone(),
        };
        {
            let mut journal = self.journal.lock();
            journal.append(&intent_record)?;
            journal.sync()?;
        }

        // 4. Assign monotonic commit version
        let version = self.allocate_version();
        txn.commit_version = Some(version);

        // 5. Fsync Commit record (linearization point)
        let commit_record = JournalRecord::Commit {
            txn_id: txn.id,
            version,
        };
        {
            let mut journal = self.journal.lock();
            journal.append(&commit_record)?;
            journal.sync()?;
        }

        txn.state = TxnState::Prepared;

        // 6. Apply participants in deterministic sorted order
        for (work, p) in sorted_works.iter().zip(&resolved) {
            p.apply(txn.id, version, &work.payload)?;
        }

        // 7. Publish visibility in deterministic sorted order
        for p in &resolved {
            p.publish(txn.id, version)?;
        }

        // 8. Advance visible version only after all succeed
        {
            let mut vis = self.visible_version.lock();
            if version == vis.next() {
                *vis = version;
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
    pub fn abort(&self, txn: &mut Transaction) -> Result<()> {
        if txn.state == TxnState::Committed {
            return Err(HtapError::Conflict(format!(
                "transaction {} has already committed and cannot be aborted",
                txn.id
            )));
        }

        if txn.state == TxnState::Aborted {
            return Ok(());
        }

        {
            let registry = self.participants.read();
            for work in &txn.participants {
                if let Some(p) = registry.get(&work.participant_id) {
                    let _ = p.abort(txn.id);
                }
            }
        }

        {
            let mut journal = self.journal.lock();
            journal.append(&JournalRecord::Abort { txn_id: txn.id })?;
            journal.sync()?;
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
        let records = self.journal.lock().read_all()?;

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

        let registry = self.participants.read();
        let mut committed_txns = Vec::new();

        // Sort commits by assigned version to replay in exact linear order
        let mut commit_list: Vec<(TransactionId, Version)> = commits.into_iter().collect();
        commit_list.sort_by_key(|(_, v)| *v);

        for (txn_id, commit_version) in commit_list {
            if aborts.contains(&txn_id) {
                continue;
            }

            let (_snapshot, mut works) = intents.remove(&txn_id).ok_or_else(|| {
                HtapError::Corruption(format!(
                    "committed transaction {txn_id} missing corresponding intent in journal"
                ))
            })?;

            // Sort participants deterministically by ID
            works.sort_by_key(|w| w.participant_id);

            // Recovery requires registered participants
            for work in &works {
                if !registry.contains_key(&work.participant_id) {
                    return Err(HtapError::NotFound(format!(
                        "participant {} required for recovery of txn {txn_id} is not registered",
                        work.participant_id
                    )));
                }
            }

            // Apply exact participant payloads in sorted order
            for work in &works {
                let p = registry.get(&work.participant_id).unwrap();
                p.apply(txn_id, commit_version, &work.payload)?;
            }

            // Publish in sorted order
            for work in &works {
                let p = registry.get(&work.participant_id).unwrap();
                p.publish(txn_id, commit_version)?;
            }

            committed_txns.push(txn_id);
        }

        // Intent-only transactions are ignored completely (no apply/abort, not committed).
        let aborted_txns: Vec<TransactionId> = aborts.into_iter().collect();

        self.next_txn_id.store(max_txn_id + 1, Ordering::SeqCst);
        *self.next_version.lock() = max_version.next();
        *self.visible_version.lock() = max_version;

        Ok(RecoveryReport {
            committed_txns,
            aborted_txns,
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
}
