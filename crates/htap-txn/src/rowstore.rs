//! Rowstore transaction participant adapter wrapping an LSM [`Engine`].

use std::sync::Arc;

use htap_common::{HtapError, Mutation, Result, Version};
use htap_rowstore::{Engine, Snapshot};

use crate::participant::{ParticipantId, TransactionId, TxnParticipant, MAX_PAYLOAD_SIZE};

/// Transaction participant adapter wrapping an LSM [`Engine`].
///
/// Implements [`TxnParticipant`] using strict JSON `Vec<Mutation>` serialization,
/// bounded payload limits, idempotent external apply, and explicit MVCC publication.
#[derive(Debug, Clone)]
pub struct RowstoreParticipant {
    id: ParticipantId,
    engine: Arc<Engine>,
}

impl RowstoreParticipant {
    /// Create a new rowstore participant with a stable identifier and engine reference.
    pub fn new(id: impl Into<ParticipantId>, engine: Arc<Engine>) -> Self {
        Self {
            id: id.into(),
            engine,
        }
    }

    /// Access the underlying rowstore [`Engine`].
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Encode a slice of mutations into a strictly typed JSON payload.
    pub fn encode_payload(mutations: &[Mutation]) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(mutations).map_err(|e| {
            HtapError::InvalidArgument(format!("failed to serialize mutation payload: {e}"))
        })?;
        if bytes.len() > MAX_PAYLOAD_SIZE {
            return Err(HtapError::InvalidArgument(format!(
                "mutation payload size {} exceeds maximum 16 MiB",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// Decode and validate a strictly typed JSON mutation payload.
    pub fn decode_payload(payload: &[u8]) -> Result<Vec<Mutation>> {
        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(HtapError::InvalidArgument(format!(
                "mutation payload size {} exceeds maximum 16 MiB",
                payload.len()
            )));
        }
        serde_json::from_slice(payload)
            .map_err(|e| HtapError::InvalidArgument(format!("malformed mutation payload: {e}")))
    }
}

impl TxnParticipant for RowstoreParticipant {
    fn id(&self) -> ParticipantId {
        self.id
    }

    fn prepare(&self, snapshot: Version, payload: &[u8]) -> Result<()> {
        let mutations = Self::decode_payload(payload)?;
        self.engine.prepare(0, Snapshot::new(snapshot), mutations)?;
        Ok(())
    }

    fn apply(&self, txn_id: TransactionId, version: Version, payload: &[u8]) -> Result<()> {
        let mutations = Self::decode_payload(payload)?;
        self.engine
            .apply_external(txn_id.get(), version, mutations)?;
        Ok(())
    }

    fn abort(&self, _txn_id: TransactionId) -> Result<()> {
        Ok(())
    }

    fn publish(&self, _txn_id: TransactionId, version: Version) -> Result<()> {
        self.engine.publish(version)?;
        Ok(())
    }

    fn committed_version(&self) -> Option<Version> {
        Some(self.engine.committed_version())
    }
}
