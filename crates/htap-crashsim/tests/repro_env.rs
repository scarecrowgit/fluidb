use std::any::Any;
use std::cell::Cell;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{CrashHarness, CrashPolicy};

const FILE_SYNC_SITE: &str = "repro_env:file_sync";
const DIR_SYNC_SITE: &str = "repro_env:dir_sync";
const RECOVERY_SYNC_SITE: &str = "repro_env:recovery_sync";

fn run_durable_workload(harness: &CrashHarness) {
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(&path).unwrap();
            file.write_all(b"durable data").unwrap();
            file.sync_all_site(FILE_SYNC_SITE).unwrap();
            htap_fs::dur::sync_dir_site(workload.root(), DIR_SYNC_SITE).unwrap();
            workload.ack("published");
        })
        .unwrap();
}

fn recover_with_durable_write(root: &std::path::Path) {
    let path = root.join("recovered");
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);

    let mut file = options.open(&path).unwrap();
    file.write_all(b"recovered").unwrap();
    file.sync_all_site(RECOVERY_SYNC_SITE).unwrap();
    htap_fs::dur::sync_dir_site(root, DIR_SYNC_SITE).unwrap();
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[test]
fn replay_env_replays_exactly_one_image() {
    let name = "repro_env_exactly_one";
    let harness = CrashHarness::new(name).unwrap();
    run_durable_workload(&harness);

    let crash_point = harness.snapshot().unwrap().log.len();
    let repro = format!("{name}/strict/0/k={crash_point}");
    let calls = Cell::new(0);

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        harness
            .enumerate_with_env(
                &CrashPolicy::Strict,
                [("POWERLOSS_REPRO", repro.as_str())],
                |_root, info| {
                    calls.set(calls.get() + 1);
                    assert_eq!(info.crash_point, crash_point);
                    panic!("failure at selected crash point");
                },
            )
            .unwrap();
    }));

    let payload = result.expect_err("replay should reproduce the checker failure");
    assert_eq!(
        panic_message(payload.as_ref()),
        format!("POWERLOSS_REPRO={repro}")
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn replay_env_not_reproduced_panics_with_distinct_prefix() {
    let name = "repro_env_not_reproduced";
    let harness = CrashHarness::new(name).unwrap();
    run_durable_workload(&harness);

    let crash_point = harness.snapshot().unwrap().log.len();
    let repro = format!("{name}/strict/0/k={crash_point}");
    let calls = Cell::new(0);

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        harness
            .enumerate_with_env(
                &CrashPolicy::Strict,
                [("POWERLOSS_REPRO", repro.as_str())],
                |_root, info| {
                    calls.set(calls.get() + 1);
                    assert_eq!(info.crash_point, crash_point);
                },
            )
            .unwrap();
    }));

    let payload = result.expect_err("a passing replay must report that it was not reproduced");
    assert!(
        panic_message(payload.as_ref())
            .starts_with(&format!("POWERLOSS_REPLAY_NOT_REPRODUCED={repro}")),
        "unexpected panic: {}",
        panic_message(payload.as_ref())
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn replay_env_recovery_stage_roundtrip() {
    struct EnvGuard {
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("POWERLOSS_REPRO", value),
                None => std::env::remove_var("POWERLOSS_REPRO"),
            }
        }
    }

    let previous = std::env::var("POWERLOSS_REPRO").ok();
    let _guard = EnvGuard { previous };
    std::env::remove_var("POWERLOSS_REPRO");

    let name = "repro_env_recovery_roundtrip";
    let harness = CrashHarness::new(name).unwrap();
    run_durable_workload(&harness);

    let outer_point = harness.snapshot().unwrap().log.len();
    let first_calls = Cell::new(0);
    let first = panic::catch_unwind(AssertUnwindSafe(|| {
        harness
            .enumerate_recovery(
                &CrashPolicy::Strict,
                recover_with_durable_write,
                |_root, info| {
                    let Some(recovery) = info.recovery.as_ref() else {
                        return;
                    };
                    if info.crash_point == outer_point && !recovery.completed {
                        first_calls.set(first_calls.get() + 1);
                        panic!("selected recovery image failed");
                    }
                },
            )
            .unwrap();
    }));

    let first_payload = first.expect_err("initial enumeration should find the selected failure");
    let first_message = panic_message(first_payload.as_ref());
    let repro = first_message
        .strip_prefix("POWERLOSS_REPRO=")
        .expect("failure should contain a reproduction string")
        .to_owned();
    assert!(repro.contains("/rk="));
    assert_eq!(first_calls.get(), 1);

    std::env::set_var("POWERLOSS_REPRO", &repro);

    let replay_calls = Cell::new(0);
    let replay = panic::catch_unwind(AssertUnwindSafe(|| {
        harness
            .enumerate_recovery(
                &CrashPolicy::Strict,
                recover_with_durable_write,
                |_root, info| {
                    replay_calls.set(replay_calls.get() + 1);
                    assert_eq!(info.repro_string(), repro);
                    panic!("selected recovery image failed");
                },
            )
            .unwrap();
    }));

    let replay_payload = replay.expect_err("recovery replay should reproduce the failure");
    assert_eq!(
        panic_message(replay_payload.as_ref()),
        format!("POWERLOSS_REPRO={repro}")
    );
    assert_eq!(replay_calls.get(), 1);
}

#[test]
fn replay_env_rejects_malformed_and_out_of_range() {
    let name = "repro_env_rejects_invalid";
    let harness = CrashHarness::new(name).unwrap();
    run_durable_workload(&harness);

    for repro in [
        "not-a-repro".to_owned(),
        format!(
            "{name}/strict/0/k={}",
            harness.snapshot().unwrap().log.len() + 10_000
        ),
    ] {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            harness
                .enumerate_with_env(
                    &CrashPolicy::Strict,
                    [("POWERLOSS_REPRO", repro.as_str())],
                    |_root, _info| {},
                )
                .unwrap();
        }));

        let payload = result.expect_err("invalid repro should fail before enumeration");
        assert!(
            panic_message(payload.as_ref())
                .starts_with(&format!("POWERLOSS_REPLAY_FAILED={repro}")),
            "unexpected panic: {}",
            panic_message(payload.as_ref())
        );
    }
}

#[test]
fn replay_env_ignores_other_harness() {
    let harness = CrashHarness::new("repro_env_ignores_other").unwrap();
    run_durable_workload(&harness);

    let calls = Cell::new(0);
    harness
        .enumerate_with_env(
            &CrashPolicy::Strict,
            [("POWERLOSS_REPRO", "some_other_harness/strict/0/k=0")],
            |_root, _info| calls.set(calls.get() + 1),
        )
        .unwrap();

    assert!(calls.get() > 1, "expected normal full enumeration");
}

#[test]
fn replay_env_policy_mismatch_enumerates_normally() {
    let name = "repro_env_policy_mismatch";
    let harness = CrashHarness::new(name).unwrap();
    run_durable_workload(&harness);

    let repro = format!("{name}/chaos/0/k=0");
    let calls = Cell::new(0);
    harness
        .enumerate_with_env(
            &CrashPolicy::Strict,
            [("POWERLOSS_REPRO", repro.as_str())],
            |_root, _info| calls.set(calls.get() + 1),
        )
        .unwrap();

    assert!(calls.get() > 1, "expected normal full enumeration");
}
