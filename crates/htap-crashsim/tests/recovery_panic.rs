use std::io::Write;
use std::path::Path;

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{parse_recovery_repro_string, CrashHarness, CrashPolicy, RecoveryReproPoint};

const RECOVERY_PANIC: &str = "recovery did not find durable data";

fn write_file(path: &Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn recovery(root: &Path) {
    if !root.join("data").exists() {
        panic!("{RECOVERY_PANIC}");
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[test]
fn recovery_panic_prints_two_stage_repro() {
    let name = "recovery_panic_prints_two_stage_repro";
    let harness = CrashHarness::new(name).unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
        })
        .unwrap();

    let enumeration = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness
            .enumerate_recovery(&CrashPolicy::Strict, recovery, |_root, _info| {})
            .unwrap();
    }));
    let message = panic_message(enumeration.expect_err("enumeration did not panic"));
    let expected_repro = format!("{name}/strict/0/k=0/rk=recover");
    let repro = message
        .strip_prefix("POWERLOSS_REPRO=")
        .expect("enumeration panic did not contain POWERLOSS_REPRO prefix");

    assert_eq!(repro, expected_repro);
    assert!(repro.ends_with("/rk=recover"));
    assert!(matches!(
        parse_recovery_repro_string(repro),
        Some((_, _, _, _, RecoveryReproPoint::Recover))
    ));

    let replay = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness
            .replay_recovery(repro, recovery, |_root, _info| {})
            .unwrap();
    }));
    assert_eq!(
        panic_message(replay.expect_err("recovery replay did not panic")),
        format!("POWERLOSS_REPRO={repro}")
    );
}
