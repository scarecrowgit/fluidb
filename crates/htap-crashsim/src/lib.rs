//! Crash-consistency test harness for HTAP filesystem durability protocols.
//!
//! This dev-only crate materializes filesystem images from recorded operation
//! prefixes and provides policies for exploring crash points.

mod harness;
mod materialize;
mod model;
mod policy;

pub use harness::{
    assert_skip_kills, assert_skip_survives, clear_powerloss_failure, powerloss_failure,
    set_powerloss_failure, CrashHarness, PowerLossFailure, PowerLossFailurePhase, WorkloadContext,
};
#[doc(hidden)]
pub use htap_common::fs::SkipSync as __SkipSync;
pub use htap_common::fs::{parse_skip_sync_env, SyncFault};

/// Defines a test that must fail when one sync site is skipped.
#[macro_export]
macro_rules! crashsim_witness {
    ($fn_name:ident, site = $site:literal, body = $body:path) => {
        #[test]
        fn $fn_name() {
            $crate::assert_skip_kills(
                stringify!($fn_name),
                $crate::__SkipSync::Site($site),
                || $body(),
            );
        }
    };
}

/// Defines a test that must fail when a class of sync operations is skipped.
#[macro_export]
macro_rules! crashsim_control {
    ($fn_name:ident, skip = File, body = $body:path) => {
        #[test]
        fn $fn_name() {
            $crate::assert_skip_kills(stringify!($fn_name), $crate::__SkipSync::File, || $body());
        }
    };
    ($fn_name:ident, skip = Directory, body = $body:path) => {
        #[test]
        fn $fn_name() {
            $crate::assert_skip_kills(stringify!($fn_name), $crate::__SkipSync::Directory, || {
                $body()
            });
        }
    };
    ($fn_name:ident, skip = All, body = $body:path) => {
        #[test]
        fn $fn_name() {
            $crate::assert_skip_kills(stringify!($fn_name), $crate::__SkipSync::All, || $body());
        }
    };
}

/// Defines a test that must survive when one sync site is skipped.
#[macro_export]
macro_rules! crashsim_survivor {
    ($fn_name:ident, site = $site:literal, body = $body:path) => {
        #[test]
        fn $fn_name() {
            $crate::assert_skip_survives(
                stringify!($fn_name),
                $crate::__SkipSync::Site($site),
                || $body(),
            );
        }
    };
}
pub use policy::{
    parse_recovery_repro_string, parse_repro_string, CrashInfo, CrashPolicy, RecoveryReproPoint,
    RecoveryStage,
};
