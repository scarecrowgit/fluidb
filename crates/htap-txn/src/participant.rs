//! Transaction participant trait, identifiers, and deterministic participant ordering.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use htap_common::{HtapError, Result, Version};
use serde::{Deserialize, Serialize};

/// Maximum payload size per participant or transaction request (16 MiB).
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Strongly typed transaction identifier newtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(pub u64);

impl TransactionId {
    /// Creates a new `TransactionId`.
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the inner transaction identifier value.
    #[inline]
    pub const fn get(&self) -> u64 {
        self.0
    }

    /// Returns the inner transaction identifier as `u64`.
    #[inline]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for TransactionId {
    #[inline]
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<TransactionId> for u64 {
    #[inline]
    fn from(id: TransactionId) -> Self {
        id.0
    }
}

/// Backwards-compatible type alias for `TransactionId`.
pub type TxnId = TransactionId;

/// Strongly typed transaction participant identifier newtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParticipantId(pub u64);

impl ParticipantId {
    /// Creates a new `ParticipantId`.
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the inner participant identifier value.
    #[inline]
    pub const fn get(&self) -> u64 {
        self.0
    }

    /// Returns the inner participant identifier as `u64`.
    #[inline]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for ParticipantId {
    #[inline]
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<ParticipantId> for u64 {
    #[inline]
    fn from(id: ParticipantId) -> Self {
        id.0
    }
}

/// Storage-agnostic opaque participant work unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantWork {
    /// ID of the target participant.
    pub participant_id: ParticipantId,
    /// Storage-agnostic opaque mutation payload.
    pub payload: Vec<u8>,
}

impl ParticipantWork {
    /// Creates a new participant work unit.
    pub fn new(participant_id: impl Into<ParticipantId>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            participant_id: participant_id.into(),
            payload: payload.into(),
        }
    }
}

/// Request to commit participant work units within a single transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRequest {
    /// Participant work units to prepare and apply.
    pub participants: Vec<ParticipantWork>,
}

impl TransactionRequest {
    /// Creates and validates a new `TransactionRequest`.
    pub fn new(participants: Vec<ParticipantWork>) -> Result<Self> {
        let req = Self { participants };
        req.validate()?;
        Ok(req)
    }

    /// Validates the request constraints:
    /// - Non-empty participants
    /// - Unique participant IDs
    /// - Maximum 16 MiB payload size per participant and in total
    pub fn validate(&self) -> Result<()> {
        if self.participants.is_empty() {
            return Err(HtapError::InvalidArgument(
                "transaction request must contain at least one participant".into(),
            ));
        }

        let mut seen = BTreeSet::new();
        let mut total_payload: usize = 0;

        for work in &self.participants {
            if !seen.insert(work.participant_id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate participant id {} in transaction request",
                    work.participant_id
                )));
            }

            if work.payload.len() > MAX_PAYLOAD_SIZE {
                return Err(HtapError::InvalidArgument(format!(
                    "participant {} payload size {} exceeds maximum 16 MiB",
                    work.participant_id,
                    work.payload.len()
                )));
            }

            total_payload = total_payload.saturating_add(work.payload.len());
            if total_payload > MAX_PAYLOAD_SIZE {
                return Err(HtapError::InvalidArgument(format!(
                    "total transaction request payload size {} exceeds maximum 16 MiB",
                    total_payload
                )));
            }
        }

        Ok(())
    }
}

/// Metadata describing a successfully committed transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedTransaction {
    /// Unique identifier of the committed transaction.
    pub transaction_id: TransactionId,
    /// Monotonically allocated commit version.
    pub version: Version,
    /// Read snapshot version at transaction begin.
    pub snapshot: Version,
    /// List of participant IDs that committed the transaction.
    pub participant_ids: Vec<ParticipantId>,
}

impl CommittedTransaction {
    /// Creates a new `CommittedTransaction`.
    pub fn new(
        transaction_id: TransactionId,
        version: Version,
        snapshot: Version,
        participant_ids: Vec<ParticipantId>,
    ) -> Self {
        Self {
            transaction_id,
            version,
            snapshot,
            participant_ids,
        }
    }
}

/// Trait implemented by any storage partition, tablet, or engine resource participating in 2PC.
pub trait TxnParticipant: Send + Sync {
    /// Returns the unique identifier of this participant.
    fn id(&self) -> ParticipantId;

    /// Phase 1: Prepare.
    ///
    /// The participant checks constraints, detects conflicts against `snapshot`,
    /// and stages mutations specified in `payload`.
    /// Returning an error causes the transaction manager to abort the transaction.
    fn prepare(&self, snapshot: Version, payload: &[u8]) -> Result<()>;

    /// Phase 2: Apply (Commit).
    ///
    /// Called once the transaction's commit is durable in the journal.
    /// The participant applies/linearizes the exact `payload` at `version`.
    fn apply(&self, txn_id: TransactionId, version: Version, payload: &[u8]) -> Result<()>;

    /// Rollback/Abort.
    ///
    /// Discards staged mutations and releases held resources for `txn_id`.
    fn abort(&self, txn_id: TransactionId) -> Result<()> {
        let _ = txn_id;
        Ok(())
    }

    /// Phase 3 (Visibility): Publish.
    ///
    /// Idempotent notification to advance visible versions and make committed data readable.
    fn publish(&self, txn_id: TransactionId, version: Version) -> Result<()> {
        let _ = (txn_id, version);
        Ok(())
    }

    /// This participant's own highest durably applied commit version, if it tracks one.
    ///
    /// [`crate::TransactionManager::recover`] uses this to cross-check that a participant never
    /// silently drifts ahead of (or stays behind) the transaction journal it is registered with:
    /// in one shared MVCC version domain, a participant's own durable state must land exactly on
    /// the journal's replayed high-water mark once recovery has applied every committed
    /// transaction. Defaults to `None` for participants that do not track a comparable version
    /// (e.g. test mocks), which excludes them from that check.
    ///
    /// # Contract for participants that override this (return `Some`)
    ///
    /// The exact-match check above is only sound if **every** [`crate::TransactionManager`]
    /// commit registered against this journal touches **every** participant that overrides
    /// [`Self::committed_version`] to return `Some` — i.e. every such participant's
    /// [`Self::apply`] is called for every committed transaction, so its own tracked version
    /// advances in lockstep with the journal's replayed high-water mark. A participant that
    /// overrides this but is only sometimes included in a transaction's participant set (so some
    /// commits bump the journal's `max_version` without ever calling that participant's `apply`)
    /// will fall behind and be reported as corruption by [`crate::TransactionManager::recover`]
    /// even though nothing is actually wrong. Return `None` instead for a participant that does
    /// not (or cannot) meet this "touched by every commit" contract.
    fn committed_version(&self) -> Option<Version> {
        None
    }
}

/// Sorts a slice of references to participants in deterministic ascending order of their IDs.
///
/// Deterministic ordering avoids lock-order inversion and deadlocks across concurrent
/// transactions touching multiple participants.
pub fn order_participants<P: ?Sized + TxnParticipant>(participants: &mut [&P]) {
    participants.sort_by_key(|p| p.id());
}

/// Sorts a slice of `Arc<dyn TxnParticipant>` in deterministic ascending order of their IDs.
pub fn order_arc_participants(participants: &mut [Arc<dyn TxnParticipant>]) {
    participants.sort_by_key(|p| p.id());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestParticipant {
        id: ParticipantId,
        aborts: AtomicUsize,
    }

    impl TxnParticipant for TestParticipant {
        fn id(&self) -> ParticipantId {
            self.id
        }
        fn prepare(&self, _snapshot: Version, _payload: &[u8]) -> Result<()> {
            Ok(())
        }
        fn apply(&self, _txn_id: TransactionId, _version: Version, _payload: &[u8]) -> Result<()> {
            Ok(())
        }
        fn abort(&self, _txn_id: TransactionId) -> Result<()> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_newtypes() {
        let tid = TransactionId::new(42);
        assert_eq!(tid.get(), 42);
        assert_eq!(tid.as_u64(), 42);
        assert_eq!(format!("{tid}"), "42");
        assert_eq!(TransactionId::from(42u64), tid);
        assert_eq!(u64::from(tid), 42);

        let pid = ParticipantId::new(7);
        assert_eq!(pid.get(), 7);
        assert_eq!(pid.as_u64(), 7);
        assert_eq!(format!("{pid}"), "7");
        assert_eq!(ParticipantId::from(7u64), pid);
        assert_eq!(u64::from(pid), 7);
    }

    #[test]
    fn test_transaction_request_validation() {
        // Empty participants
        let empty_req = TransactionRequest {
            participants: vec![],
        };
        assert!(matches!(
            empty_req.validate().unwrap_err(),
            HtapError::InvalidArgument(_)
        ));

        // Duplicate participant IDs
        let dup_req = TransactionRequest {
            participants: vec![
                ParticipantWork::new(1, vec![1]),
                ParticipantWork::new(1, vec![2]),
            ],
        };
        assert!(matches!(
            dup_req.validate().unwrap_err(),
            HtapError::InvalidArgument(_)
        ));

        // Valid request
        let valid_req = TransactionRequest {
            participants: vec![
                ParticipantWork::new(1, vec![1]),
                ParticipantWork::new(2, vec![2]),
            ],
        };
        assert!(valid_req.validate().is_ok());
    }

    #[test]
    fn test_order_participants() {
        let p3 = TestParticipant {
            id: ParticipantId::new(30),
            aborts: AtomicUsize::new(0),
        };
        let p1 = TestParticipant {
            id: ParticipantId::new(10),
            aborts: AtomicUsize::new(0),
        };
        let p2 = TestParticipant {
            id: ParticipantId::new(20),
            aborts: AtomicUsize::new(0),
        };

        let mut list: Vec<&dyn TxnParticipant> = vec![&p3, &p1, &p2];
        order_participants(&mut list);
        assert_eq!(list[0].id(), ParticipantId::new(10));
        assert_eq!(list[1].id(), ParticipantId::new(20));
        assert_eq!(list[2].id(), ParticipantId::new(30));
    }

    #[test]
    fn test_order_arc_participants() {
        let p3: Arc<dyn TxnParticipant> = Arc::new(TestParticipant {
            id: ParticipantId::new(100),
            aborts: AtomicUsize::new(0),
        });
        let p1: Arc<dyn TxnParticipant> = Arc::new(TestParticipant {
            id: ParticipantId::new(15),
            aborts: AtomicUsize::new(0),
        });
        let p2: Arc<dyn TxnParticipant> = Arc::new(TestParticipant {
            id: ParticipantId::new(42),
            aborts: AtomicUsize::new(0),
        });

        let mut list = vec![p3, p1, p2];
        order_arc_participants(&mut list);
        assert_eq!(list[0].id(), ParticipantId::new(15));
        assert_eq!(list[1].id(), ParticipantId::new(42));
        assert_eq!(list[2].id(), ParticipantId::new(100));
    }
}
