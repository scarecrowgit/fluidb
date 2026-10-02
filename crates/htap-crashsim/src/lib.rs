//! Crash-consistency test harness for HTAP filesystem durability protocols.
//!
//! This dev-only crate materializes filesystem images from recorded operation
//! prefixes and provides policies for exploring crash points.

mod harness;
mod materialize;
mod model;
mod policy;

pub use harness::{CrashHarness, WorkloadContext};
pub use policy::{
    parse_recovery_repro_string, parse_repro_string, CrashInfo, CrashPolicy, RecoveryReproPoint,
    RecoveryStage,
};
