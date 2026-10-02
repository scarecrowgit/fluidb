use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{CrashHarness, CrashPolicy};

fn write_file(path: &Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn image_signature(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn walk(root: &Path, directory: &Path, output: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                output.push((relative, None));
                walk(root, &path, output);
            } else {
                output.push((relative, Some(fs::read(path).unwrap())));
            }
        }
    }

    let mut output = Vec::new();
    walk(root, root, &mut output);
    output
}

#[test]
fn recovery_crashinfo_carries_outer_acks() {
    let harness = CrashHarness::new("recovery_crashinfo_carries_outer_acks").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("a");
            write_file(&workload.root().join("dirty"), b"unsynced");
        })
        .unwrap();

    let mut images_after_ack = 0;
    harness
        .enumerate_recovery(
            &CrashPolicy::Strict,
            |root| write_file(&root.join("recovered"), b"recovered"),
            |_root, info| {
                if info.acked_labels.iter().any(|label| label == "a") {
                    assert!(info.recovery.is_some());
                    images_after_ack += 1;
                }
            },
        )
        .unwrap();

    assert!(images_after_ack > 0);
}

#[test]
fn recovery_repro_roundtrips_and_replays() {
    fn recover(root: &Path) {
        let path = root.join("recovered");
        write_file(&path, b"recovered");
        htap_fs::fsync_file(&path).unwrap();
        htap_fs::sync_dir(root).unwrap();
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

    let harness = CrashHarness::new("recovery_repro_roundtrips_and_replays").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    let mut captured = None;
    harness
        .enumerate_recovery(&CrashPolicy::Strict, recover, |root, info| {
            let recovery = info.recovery.as_ref().unwrap();
            if !recovery.completed && recovery.crash_point > 0 {
                captured = Some((
                    info.repro_string(),
                    image_signature(root),
                    info.crash_point,
                    info.policy.name(),
                    recovery.crash_point,
                ));
            }
        })
        .unwrap();

    let (repro, signature, outer_point, policy_name, recovery_point) =
        captured.expect("no non-trivial recovery image was generated");

    harness
        .replay_recovery(&repro, recover, |root, info| {
            let recovery = info.recovery.as_ref().unwrap();
            assert_eq!(image_signature(root), signature);
            assert_eq!(info.crash_point, outer_point);
            assert_eq!(info.policy.name(), policy_name);
            assert_eq!(recovery.crash_point, recovery_point);
            assert!(!recovery.completed);
        })
        .unwrap();

    let replay = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness
            .replay_recovery(&repro, recover, |_root, _info| {
                panic!("intentional recovery replay failure");
            })
            .unwrap();
    }));
    let message = panic_message(replay.expect_err("replay did not fail"));
    assert_eq!(message, format!("POWERLOSS_REPRO={repro}"));
}
