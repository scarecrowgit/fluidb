//! Synchronous local transaction manager with durable CRC32C journaling.
//!
//! Provides two-phase commit orchestration, bounded journal frames, torn-final repair,
//! deterministic participant ordering, and crash recovery.
#![forbid(unsafe_code)]

pub mod journal;
pub mod manager;
pub mod participant;
pub mod rowstore;

pub use journal::{
    decode_frame_slice, encode_frame, FrameStatus, Journal, JournalOptions, JournalRecord,
    JournalScan, DEFAULT_MAX_FRAME_SIZE, HEADER_SIZE,
};
pub use manager::{RecoveryReport, Transaction, TransactionManager, TxnState};
pub use participant::{
    order_arc_participants, order_participants, CommittedTransaction, ParticipantId,
    ParticipantWork, TransactionId, TransactionRequest, TxnId, TxnParticipant, MAX_PAYLOAD_SIZE,
};
pub use rowstore::RowstoreParticipant;
