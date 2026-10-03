use std::any::Any;
use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};

use htap_common::fs::{self as htap_fs, DurOpenOptions, Op, SkipSync};
use htap_crashsim::{assert_skip_kills, CrashHarness, CrashPolicy};

const DATA: &[u8] = b"durable contents";
const WORKLOAD_SYNC_SITE: &str = "review_controls:workload_sync";
const RECOVERY_SYNC_SITE: &str = "review_controls:recovery_sync";

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn run_durable_workload(harness: &CrashHarness) {
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(path).unwrap();
            file.write_all(DATA).unwrap();
            file.sync_all_site(WORKLOAD_SYNC_SITE).unwrap();
            htap_fs::dur::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();
}

fn assert_recovery_model_violation_rejected(
    label: &str,
    harness_name: &str,
    mutate: impl Fn(&std::path::Path),
) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills(label, SkipSync::Site(WORKLOAD_SYNC_SITE), || {
            let harness = CrashHarness::new(harness_name).unwrap();
            run_durable_workload(&harness);

            harness
                .enumerate_recovery(&CrashPolicy::Strict, |root| mutate(root), |_root, _info| {})
                .unwrap();
        });
    }))
    .unwrap_err();

    let message = panic_message(result.as_ref());
    assert!(
        message.contains("died for another reason"),
        "unexpected panic: {message}"
    );
}

#[test]
fn recovery_model_violation_is_not_a_kill() {
    assert_recovery_model_violation_rejected(
        "raw recovery content overwrite",
        "review_controls_recovery_raw_content",
        |root| {
            fs::write(root.join("data"), b"unrecorded overwrite").unwrap();
        },
    );

    assert_recovery_model_violation_rejected(
        "raw recovery file creation",
        "review_controls_recovery_raw_file",
        |root| {
            fs::write(root.join("unrecorded-file"), b"unrecorded").unwrap();
        },
    );

    assert_recovery_model_violation_rejected(
        "raw recovery directory creation",
        "review_controls_recovery_raw_directory",
        |root| {
            fs::create_dir(root.join("unrecorded-directory")).unwrap();
        },
    );
}

#[test]
fn caught_repro_then_unrelated_panic_is_rejected() {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills(
            "caught repro followed by unrelated panic",
            SkipSync::Site(WORKLOAD_SYNC_SITE),
            || {
                let harness =
                    CrashHarness::new("review_controls_caught_repro_unrelated_panic").unwrap();
                run_durable_workload(&harness);

                let enumeration = panic::catch_unwind(AssertUnwindSafe(|| {
                    harness
                        .enumerate(&CrashPolicy::Strict, |_root, _info| {
                            panic!("deliberate checker failure");
                        })
                        .unwrap();
                }));
                assert!(enumeration.is_err());

                panic!("unrelated panic after caught repro");
            },
        );
    }))
    .unwrap_err();

    let message = panic_message(result.as_ref());
    assert!(
        message.contains("died for another reason"),
        "unexpected panic: {message}"
    );
}

#[test]
fn recovery_phase_checker_failure_is_a_kill() {
    assert_skip_kills(
        "skipped recovery sync",
        SkipSync::Site(RECOVERY_SYNC_SITE),
        || {
            let harness =
                CrashHarness::new("review_controls_recovery_phase_checker_failure").unwrap();
            run_durable_workload(&harness);

            harness
                .enumerate_recovery(
                    &CrashPolicy::Strict,
                    |root| {
                        let path = root.join("recovered");
                        let mut options = DurOpenOptions::new();
                        options.write(true).create(true).truncate(true);

                        let mut file = options.open(&path).unwrap();
                        file.write_all(b"recovered").unwrap();
                        file.sync_all_site(RECOVERY_SYNC_SITE).unwrap();
                        htap_fs::dur::sync_dir(root).unwrap();
                    },
                    |root, info| {
                        let recovery = info.recovery.as_ref().unwrap();
                        if recovery
                            .ops
                            .iter()
                            .any(|op| matches!(op, Op::FsyncDir { .. }))
                        {
                            assert_eq!(fs::read(root.join("recovered")).unwrap(), b"recovered");
                        }
                    },
                )
                .unwrap();
        },
    );
}

#[test]
fn torn_repro_seed_outside_default_range_is_replayed() {
    let harness =
        CrashHarness::new("review_controls_torn_repro_seed_outside_default_range").unwrap();
    run_durable_workload(&harness);

    let repro = "review_controls_torn_repro_seed_outside_default_range/torn-s=4096/51/k=0";
    let checker_calls = Cell::new(0);

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        harness
            .enumerate_with_env(
                &CrashPolicy::Torn {
                    seed: 1,
                    sector_size: 4096,
                },
                [("POWERLOSS_REPRO", repro)],
                |_root, _info| {
                    checker_calls.set(checker_calls.get() + 1);
                },
            )
            .unwrap();
    }))
    .unwrap_err();

    let message = panic_message(result.as_ref());
    assert!(
        message.starts_with("POWERLOSS_REPRO=")
            || message.starts_with("POWERLOSS_REPLAY_NOT_REPRODUCED="),
        "unexpected panic: {message}"
    );
    assert_eq!(checker_calls.get(), 1);
}

#[test]
fn recovery_repro_given_to_plain_enumerate_runs_normally() {
    let harness = CrashHarness::new("review_controls_recovery_repro_plain_enumerate").unwrap();
    run_durable_workload(&harness);

    let repro = "review_controls_recovery_repro_plain_enumerate/strict/0/k=0/rk=done";
    let checker_calls = Cell::new(0);

    harness
        .enumerate_with_env(
            &CrashPolicy::Strict,
            [("POWERLOSS_REPRO", repro)],
            |_root, _info| {
                checker_calls.set(checker_calls.get() + 1);
            },
        )
        .unwrap();

    assert!(
        checker_calls.get() > 1,
        "recovery repro incorrectly suppressed normal enumeration"
    );
}

#[test]
fn shim_file_under_ipc_dir_is_ignored() {
    let harness = CrashHarness::new("review_controls_shim_file_under_ipc_dir")
        .unwrap()
        .with_data_root("srv");

    harness
        .run_workload(|workload| {
            let srv = workload.root().join("srv");
            htap_fs::create_dir_all_durable(&srv).unwrap();
            htap_fs::dur::sync_dir(workload.root()).unwrap();

            let ipc = srv.join(".htap-ipc-7");
            fs::create_dir(&ipc).unwrap();

            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);
            let mut file = options.open(ipc.join("payload")).unwrap();
            file.write_all(b"runtime-only").unwrap();
            file.sync_all().unwrap();
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| match op {
        Op::Mkdir { path }
        | Op::Create { path, .. }
        | Op::FsyncDir { path, .. }
        | Op::Unlink { path, .. }
        | Op::Rmdir { path } => path
            .components()
            .any(|component| component.as_os_str() == ".htap-ipc-7"),
        Op::Rename { from, to, .. } => [from, to].iter().any(|path| {
            path.components()
                .any(|component| component.as_os_str() == ".htap-ipc-7")
        }),
        Op::Write { .. } | Op::SetLen { .. } | Op::FsyncFile { .. } | Op::Ack { .. } => false,
    }));
}

#[test]
fn ephemeral_root_after_activity_panics() {
    let harness = CrashHarness::new("review_controls_ephemeral_root_after_activity").unwrap();
    run_durable_workload(&harness);

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _harness = harness.with_data_root("late-data-root");
    }));

    let payload = result.expect_err("adding an ephemeral root after activity must panic");
    assert!(panic_message(payload.as_ref())
        .contains("crashsim ephemeral data roots must be added immediately after registration"));
}
