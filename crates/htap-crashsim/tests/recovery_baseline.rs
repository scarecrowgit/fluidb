use std::fs;
use std::io::{Seek, SeekFrom, Write};

use htap_common::fs::{self as htap_fs, DurOpenOptions, Op};
use htap_crashsim::{CrashHarness, CrashPolicy};

fn write_file(path: &std::path::Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn write_at(path: &std::path::Path, offset: u64, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true);
    let mut file = options.open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn recovery_unsynced_overwrite_preserves_outer_baseline() {
    let harness =
        CrashHarness::new("recovery_unsynced_overwrite_preserves_outer_baseline").unwrap();

    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"baseline");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    let mut saw_incomplete_recovery = false;
    let mut saw_completed_recovery = false;

    harness
        .enumerate_recovery(
            &CrashPolicy::Strict,
            |root| {
                let path = root.join("data");
                if path.exists() {
                    write_at(&path, 0, b"DIRTY");
                }
            },
            |root, info| {
                let recovery = info
                    .recovery
                    .as_ref()
                    .expect("recovery enumeration did not report a recovery stage");

                if !info.acked_labels.iter().any(|label| label == "published") {
                    return;
                }

                if recovery.completed {
                    saw_completed_recovery = true;
                    assert_eq!(fs::read(root.join("data")).unwrap(), b"DIRTYine");
                } else {
                    saw_incomplete_recovery = true;
                    assert_eq!(fs::read(root.join("data")).unwrap(), b"baseline");
                }
            },
        )
        .unwrap();

    assert!(
        saw_incomplete_recovery,
        "no interrupted recovery image was checked"
    );
    assert!(
        saw_completed_recovery,
        "no completed recovery image was checked"
    );
}

#[test]
fn recovery_atomic_publish_requires_directory_sync() {
    let harness = CrashHarness::new("recovery_atomic_publish_requires_directory_sync").unwrap();

    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"old");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    let mut saw_old_interrupted = false;
    let mut saw_new = false;

    harness
        .enumerate_recovery(
            &CrashPolicy::Strict,
            |root| {
                let destination = root.join("data");
                let temporary = root.join("data.tmp");

                write_file(&temporary, b"new");
                htap_fs::fsync_file(&temporary).unwrap();
                htap_fs::dur::rename(&temporary, &destination).unwrap();
                htap_fs::sync_dir(root).unwrap();
            },
            |root, info| {
                let recovery = info
                    .recovery
                    .as_ref()
                    .expect("recovery enumeration did not report a recovery stage");

                if !info.acked_labels.iter().any(|label| label == "published") {
                    return;
                }

                if recovery.completed {
                    saw_new = true;
                    assert_eq!(fs::read(root.join("data")).unwrap(), b"new");
                } else {
                    let rename_index = recovery
                        .ops
                        .iter()
                        .rposition(|op| matches!(op, Op::Rename { .. }));
                    let directory_synced_after_rename = rename_index.is_some_and(|rename_index| {
                        recovery.ops[rename_index + 1..]
                            .iter()
                            .any(|op| matches!(op, Op::FsyncDir { .. }))
                    });

                    if directory_synced_after_rename {
                        saw_new = true;
                        assert_eq!(fs::read(root.join("data")).unwrap(), b"new");
                    } else {
                        saw_old_interrupted = true;
                        assert_eq!(fs::read(root.join("data")).unwrap(), b"old");
                    }
                }
            },
        )
        .unwrap();

    assert!(
        saw_old_interrupted,
        "no interrupted recovery image with old data was checked"
    );
    assert!(
        saw_new,
        "no recovery image with newly published data was checked"
    );
}
