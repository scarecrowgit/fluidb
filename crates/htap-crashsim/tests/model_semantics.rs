use std::collections::BTreeSet;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use htap_common::fs::{self as htap_fs, DurOpenOptions, Op};
use htap_crashsim::{CrashHarness, CrashPolicy};

fn write_file(path: &Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true);
    let mut file = options.open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
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
fn strict_loses_unsynced_write() {
    let harness = CrashHarness::new("strict_loses_unsynced_write").unwrap();
    harness
        .run_workload(|workload| {
            write_file(&workload.root().join("data"), b"uncommitted");
            workload.ack("write");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "write") {
                assert!(!root.join("data").exists());
            }
        })
        .unwrap();
}

#[test]
fn file_fsync_without_dir_fsync_loses_entry() {
    let harness = CrashHarness::new("file_fsync_without_dir_fsync_loses_entry").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"durable bytes");
            htap_fs::fsync_file(&path).unwrap();
            workload.ack("file-synced");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "file-synced") {
                assert!(!root.join("data").exists());
            }
        })
        .unwrap();
}

#[test]
fn dir_fsync_publishes_entry() {
    let harness = CrashHarness::new("dir_fsync_publishes_entry").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "published") {
                assert_eq!(fs::read(root.join("data")).unwrap(), b"published");
            }
        })
        .unwrap();
}

#[test]
fn rename_without_dir_fsync_keeps_old_name() {
    let harness = CrashHarness::new("rename_without_dir_fsync_keeps_old_name").unwrap();
    harness
        .run_workload(|workload| {
            let old = workload.root().join("old");
            let new = workload.root().join("new");

            write_file(&old, b"contents");
            htap_fs::fsync_file(&old).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            htap_fs::dur::rename(&old, &new).unwrap();
            workload.ack("renamed");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "renamed") {
                assert_eq!(fs::read(root.join("old")).unwrap(), b"contents");
                assert!(!root.join("new").exists());
            }
        })
        .unwrap();
}

#[test]
fn overwrite_after_sync_reverts_to_synced_bytes() {
    let harness = CrashHarness::new("overwrite_after_sync_reverts_to_synced_bytes").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"stable");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 0, b"UNSYNC");
            workload.ack("overwritten");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "overwritten") {
                assert_eq!(fs::read(root.join("data")).unwrap(), b"stable");
            }
        })
        .unwrap();
}

#[test]
fn nested_mkdir_needs_parent_fsync_per_level() {
    let harness = CrashHarness::new("nested_mkdir_needs_parent_fsync_per_level").unwrap();
    let outer = harness.root().join("outer");

    harness
        .run_workload(|workload| {
            let inner = outer.join("inner");

            htap_fs::dur::create_dir(&outer).unwrap();
            htap_fs::dur::create_dir(&inner).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("root-synced");

            htap_fs::sync_dir(&outer).unwrap();
            workload.ack("outer-synced");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            let outer_fsynced = info.ops.iter().any(
                |op| matches!(op, Op::FsyncDir { path, .. } if path == &PathBuf::from("outer")),
            );

            assert_eq!(root.join("outer/inner").is_dir(), outer_fsynced);

            if info.acked_labels.iter().any(|label| label == "root-synced") {
                assert!(root.join("outer").is_dir());
                if !outer_fsynced {
                    assert!(!root.join("outer/inner").exists());
                }
            }
        })
        .unwrap();
}

#[test]
fn dir_fsync_does_not_publish_own_entry() {
    let harness = CrashHarness::new("dir_fsync_does_not_publish_own_entry").unwrap();
    harness
        .run_workload(|workload| {
            let child = workload.root().join("child");
            htap_fs::dur::create_dir(&child).unwrap();
            htap_fs::sync_dir(&child).unwrap();
            workload.ack("child-synced");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info
                .acked_labels
                .iter()
                .any(|label| label == "child-synced")
            {
                assert!(!root.join("child").exists());
            }
        })
        .unwrap();
}

#[test]
fn unlink_without_dir_fsync_resurrects() {
    let harness = CrashHarness::new("unlink_without_dir_fsync_resurrects").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"resurrected");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_file(&path).unwrap();
            workload.ack("unlinked");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.acked_labels.iter().any(|label| label == "unlinked") {
                assert_eq!(fs::read(root.join("data")).unwrap(), b"resurrected");
            }
        })
        .unwrap();
}

#[test]
fn rename_over_unsynced_keeps_old_inode() {
    let harness = CrashHarness::new("rename_over_unsynced_keeps_old_inode").unwrap();
    harness
        .run_workload(|workload| {
            let source = workload.root().join("source");
            let destination = workload.root().join("destination");

            write_file(&destination, b"old inode");
            htap_fs::fsync_file(&destination).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_file(&source, b"new inode");
            htap_fs::fsync_file(&source).unwrap();
            htap_fs::dur::rename(&source, &destination).unwrap();
            workload.ack("renamed-over");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info
                .acked_labels
                .iter()
                .any(|label| label == "renamed-over")
            {
                assert_eq!(fs::read(root.join("destination")).unwrap(), b"old inode");
                assert!(!root.join("source").exists());
            }
        })
        .unwrap();
}

#[test]
fn torn_keeps_sector_aligned_subset() {
    const SECTOR_SIZE: usize = 4;

    let harness = CrashHarness::new("torn_keeps_sector_aligned_subset").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"abcdefghijkl");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 1, b"1234567890");
            workload.ack("dirty");
        })
        .unwrap();

    let mut torn_images = 0;

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 17,
                sector_size: SECTOR_SIZE,
            },
            |root, info| {
                if info.acked_labels.iter().any(|label| label == "dirty") {
                    torn_images += 1;

                    let bytes = fs::read(root.join("data")).unwrap();
                    let old = b"abcdefghijkl";
                    let new = b"a1234567890l";

                    assert_eq!(bytes.len(), old.len());
                    for sector in 0..old.len().div_ceil(SECTOR_SIZE) {
                        let start = sector * SECTOR_SIZE;
                        let end = (start + SECTOR_SIZE).min(old.len());
                        assert!(
                            bytes[start..end] == old[start..end]
                                || bytes[start..end] == new[start..end]
                        );
                    }
                }
            },
        )
        .unwrap();

    assert!(
        torn_images > 0,
        "no torn crash image reached the dirty write"
    );
}

#[test]
fn torn_extension_zero_filled() {
    let harness = CrashHarness::new("torn_extension_zero_filled").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"base");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 8, b"tail");
            workload.ack("extended");
        })
        .unwrap();

    let mut extended_images = 0;

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 9,
                sector_size: 4,
            },
            |root, info| {
                if info.acked_labels.iter().any(|label| label == "extended") {
                    extended_images += 1;

                    let bytes = fs::read(root.join("data")).unwrap();
                    if bytes.len() > 4 {
                        assert!(bytes.len() >= 8);
                        assert_eq!(&bytes[4..8], &[0, 0, 0, 0]);
                    }
                }
            },
        )
        .unwrap();

    assert!(
        extended_images > 0,
        "no torn crash image reached the extending write"
    );
}

#[test]
fn chaos_reaches_rename_before_data_sync() {
    let mut reached = false;
    let harness = CrashHarness::new("chaos_reaches_rename_before_data_sync").unwrap();
    harness
        .run_workload(|workload| {
            let old = workload.root().join("old");
            let new = workload.root().join("new");

            write_file(&old, b"old");
            htap_fs::fsync_file(&old).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_file(&new, b"new unsynced bytes");
            htap_fs::dur::rename(&new, &old).unwrap();
            workload.ack("renamed-before-data-sync");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Chaos { seed: 3 }, |root, info| {
            if info
                .acked_labels
                .iter()
                .any(|label| label == "renamed-before-data-sync")
                && root.join("old").exists()
                && fs::read(root.join("old")).unwrap() != b"old"
            {
                reached = true;
            }
        })
        .unwrap();

    assert!(
        reached,
        "chaos never persisted the rename ahead of its data"
    );
}

#[test]
fn images_deterministic_for_same_seed() {
    fn collect(name: &str) -> Vec<Vec<(PathBuf, Option<Vec<u8>>)>> {
        let harness = CrashHarness::new(name).unwrap();
        harness
            .run_workload(|workload| {
                let path = workload.root().join("data");
                write_file(&path, b"abcdefghijklmnop");
                htap_fs::fsync_file(&path).unwrap();
                htap_fs::sync_dir(workload.root()).unwrap();
                write_at(&path, 2, b"0123456789");
                workload.ack("dirty");
            })
            .unwrap();

        let mut images = Vec::new();
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed: 0x5eed,
                    sector_size: 4,
                },
                |root, _| images.push(image_signature(root)),
            )
            .unwrap();
        images
    }

    assert_eq!(collect("deterministic-a"), collect("deterministic-b"));
}

#[test]
fn dedup_collapses_equal_images() {
    let harness = CrashHarness::new("dedup_collapses_equal_images").unwrap();
    harness
        .run_workload(|workload| {
            workload.ack("first");
            workload.ack("first");
            workload.ack("first");
            workload.ack("second");
        })
        .unwrap();

    let mut generated = 0;
    let mut unique = BTreeSet::new();
    harness
        .enumerate(&CrashPolicy::Strict, |root, _| {
            generated += 1;
            unique.insert(image_signature(root));
        })
        .unwrap();

    assert_eq!(generated, 3);
    assert_eq!(unique.len(), 1);
}

#[test]
fn repro_string_roundtrips() {
    let harness = CrashHarness::new("repro_string_roundtrips").unwrap();
    harness
        .run_workload(|workload| {
            workload.ack("checkpoint");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Chaos { seed: 42 }, |_root, info| {
            let repro = info.repro_string();
            let parsed = htap_crashsim::parse_repro_string(&repro);
            assert_eq!(
                parsed,
                Some((
                    "repro_string_roundtrips".to_owned(),
                    "chaos".to_owned(),
                    42,
                    info.crash_point,
                ))
            );
        })
        .unwrap();
}

#[test]
fn torn_repro_roundtrips_sector_size() {
    const SECTOR_SIZE: usize = 8;
    let old = b"abcdefghijklmnop";
    let new = b"1234567890123456";

    let harness = CrashHarness::new("torn_repro_roundtrips_sector_size").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, old);
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 0, new);
            workload.ack("dirty");
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let crash_point = snapshot
        .log
        .iter()
        .rposition(|op| matches!(op, Op::Write { bytes_written, .. } if bytes_written == new))
        .map(|index| index + 1)
        .unwrap();

    let mut images = Vec::new();
    for seed in 0..32 {
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: SECTOR_SIZE,
                },
                |root, info| {
                    if info.crash_point == crash_point {
                        images.push((info.repro_string(), fs::read(root.join("data")).unwrap()));
                    }
                },
            )
            .unwrap();
    }

    assert!(images.iter().any(|(_, bytes)| {
        (bytes[..SECTOR_SIZE] == old[..SECTOR_SIZE] && bytes[SECTOR_SIZE..] == new[SECTOR_SIZE..])
            || (bytes[..SECTOR_SIZE] == new[..SECTOR_SIZE]
                && bytes[SECTOR_SIZE..] == old[SECTOR_SIZE..])
    }));

    for (repro, expected) in images {
        harness
            .replay(&repro, |root, info| {
                assert!(matches!(
                    info.policy,
                    CrashPolicy::Torn {
                        sector_size: SECTOR_SIZE,
                        ..
                    }
                ));
                assert_eq!(fs::read(root.join("data")).unwrap(), expected);
            })
            .unwrap();
    }
}

#[test]
fn snapshot_retains_recorded_operations() {
    let harness = CrashHarness::new("snapshot_retains_recorded_operations").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"durable");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            write_at(&path, 0, b"dirty");
            workload.ack("checkpoint");
        })
        .unwrap();

    let log = harness.snapshot().unwrap().log;
    assert!(log
        .iter()
        .any(|op| matches!(op, Op::Create { path, .. } if path == Path::new("data"))));
    assert!(log
        .iter()
        .any(|op| matches!(op, Op::Write { bytes_written, .. } if bytes_written == b"durable")));
    assert!(log.iter().any(|op| matches!(op, Op::FsyncFile { .. })));
    assert!(log.iter().any(|op| matches!(op, Op::FsyncDir { .. })));
    assert!(log
        .iter()
        .any(|op| matches!(op, Op::Ack { label } if label == "checkpoint")));
}

#[test]
fn enumerate_recovery_finds_recovery_writes() {
    let harness = CrashHarness::new("enumerate_recovery_finds_recovery_writes").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    let mut published_assertions = 0;
    let mut saw_without_recovered = false;
    let mut saw_with_recovered = false;

    harness
        .enumerate_recovery(
            &CrashPolicy::Strict,
            |root| {
                let recovered = root.join("recovered");
                write_file(&recovered, b"recovered");
                htap_fs::fsync_file(&recovered).unwrap();
                htap_fs::sync_dir(root).unwrap();
            },
            |root, info| {
                if info.acked_labels.iter().any(|label| label == "published") {
                    assert_eq!(fs::read(root.join("data")).unwrap(), b"published");
                    published_assertions += 1;
                }

                if root.join("recovered").exists() {
                    assert_eq!(fs::read(root.join("recovered")).unwrap(), b"recovered");
                    saw_with_recovered = true;
                } else {
                    saw_without_recovered = true;
                }
            },
        )
        .unwrap();

    assert!(
        published_assertions > 0,
        "the published assertion did not run"
    );
    assert!(
        saw_without_recovered,
        "no crash image omitted the recovery write"
    );
    assert!(
        saw_with_recovered,
        "the completed recovery image was not checked"
    );
}

#[test]
fn exhaustive_env_selects_every_index() {
    let harness = CrashHarness::new("exhaustive_env_selects_every_index").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"contents");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            workload.ack("published");
        })
        .unwrap();

    let log_len = harness.snapshot().unwrap().log.len();
    let mut visited = BTreeSet::new();

    harness
        .enumerate_with_env(
            &CrashPolicy::Strict,
            [("POWERLOSS_EXHAUSTIVE", "1")],
            |_root, info| {
                visited.insert(info.crash_point);
            },
        )
        .unwrap();

    assert_eq!(visited, (0..=log_len).collect());
}

#[test]
fn default_selection_skips_plain_write_boundaries() {
    let harness = CrashHarness::new("default_selection_skips_plain_write_boundaries").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, &[0; 24]);
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            for offset in 0..24 {
                write_at(&path, offset, b"1");
            }
            htap_fs::fsync_file(&path).unwrap();
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let all_indices: BTreeSet<_> = (0..=snapshot.log.len()).collect();
    let sync_points: BTreeSet<_> = snapshot
        .log
        .iter()
        .enumerate()
        .filter_map(|(index, op)| {
            matches!(op, Op::FsyncFile { .. } | Op::FsyncDir { .. }).then_some(index + 1)
        })
        .collect();
    let selected: BTreeSet<_> = harness
        .default_crash_points()
        .unwrap()
        .into_iter()
        .collect();

    assert!(selected.is_subset(&all_indices));
    assert!(selected.len() < all_indices.len());
    assert!(sync_points.is_subset(&selected));
    for (index, op) in snapshot.log.iter().enumerate() {
        if matches!(op, Op::FsyncFile { .. } | Op::FsyncDir { .. }) {
            assert!(selected.contains(&index));
            assert!(selected.contains(&(index + 1)));
        }
    }
}

#[test]
fn repro_replay_reproduces_failure() {
    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_owned()
        } else {
            "non-string panic payload".to_owned()
        }
    }

    let harness = CrashHarness::new("repro_replay_reproduces_failure").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"published");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let failing_point = snapshot
        .log
        .iter()
        .position(|op| matches!(op, Op::FsyncDir { .. }))
        .map(|index| index + 1)
        .unwrap();
    let property_message = "image contains published data";

    let enumeration = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness
            .enumerate_with_env(
                &CrashPolicy::Strict,
                [("POWERLOSS_EXHAUSTIVE", "1")],
                |root, _info| {
                    if root.join("data").exists()
                        && fs::read(root.join("data")).unwrap() == b"published"
                    {
                        panic!("{property_message}");
                    }
                },
            )
            .unwrap();
    }));
    let message = panic_message(enumeration.expect_err("enumeration did not fail"));
    let repro = message
        .strip_prefix("POWERLOSS_REPRO=")
        .expect("panic did not contain POWERLOSS_REPRO");
    let parsed = htap_crashsim::parse_repro_string(repro).expect("invalid reproduction string");
    assert_eq!(parsed.3, failing_point);

    let replay = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness
            .replay(repro, |root, _info| {
                if root.join("data").exists()
                    && fs::read(root.join("data")).unwrap() == b"published"
                {
                    panic!("{property_message}");
                }
            })
            .unwrap();
    }));
    let replay_message = panic_message(replay.expect_err("replay did not fail"));
    assert!(replay_message.contains("POWERLOSS_REPRO="));
}

#[test]
fn enumeration_with_env_preserves_crash_images() {
    let harness = CrashHarness::new("enumeration_with_env_preserves_crash_images").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            write_file(&path, b"stable");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            write_at(&path, 0, b"dirty");
            workload.ack("dirty");
        })
        .unwrap();

    harness
        .enumerate_with_env(
            &CrashPolicy::Strict,
            [("HTAP_CRASHSIM_TEST_ENV", "present")],
            |root, info| {
                if info.acked_labels.iter().any(|label| label == "dirty") {
                    assert_eq!(fs::read(root.join("data")).unwrap(), b"stable");
                }
            },
        )
        .unwrap();
}

#[test]
fn dir_fsync_without_parent_fsync_drops_subtree() {
    let harness = CrashHarness::new("dir_fsync_without_parent_fsync_drops_subtree").unwrap();
    harness
        .run_workload(|workload| {
            let child = workload.root().join("child");
            let data = child.join("data");

            htap_fs::dur::create_dir(&child).unwrap();
            write_file(&data, b"durable child data");
            htap_fs::fsync_file(&data).unwrap();
            htap_fs::sync_dir(&child).unwrap();
            workload.ack("child-synced");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info
                .acked_labels
                .iter()
                .any(|label| label == "child-synced")
            {
                assert!(!root.join("child").exists());
            }
        })
        .unwrap();
}

#[test]
fn forward_replay_preserves_unsynced_overwrite_of_baseline_file() {
    let harness =
        CrashHarness::new("forward_replay_preserves_unsynced_overwrite_of_baseline_file").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"baseline");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 0, b"DIRTY");
            workload.ack("overwritten");
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let overwrite_point = snapshot
        .log
        .iter()
        .rposition(|op| matches!(op, Op::Write { bytes_written, .. } if bytes_written == b"DIRTY"))
        .map(|index| index + 1)
        .unwrap();

    harness
        .replay(
            &format!(
                "forward_replay_preserves_unsynced_overwrite_of_baseline_file/strict/0/k={overwrite_point}"
            ),
            |root, info| {
                assert_eq!(info.crash_point, overwrite_point);
                assert_eq!(fs::read(root.join("data")).unwrap(), b"baseline");
            },
        )
        .unwrap();
}

#[test]
fn forward_replay_atomic_publish_over_existing_keeps_old_until_dir_sync() {
    let harness =
        CrashHarness::new("forward_replay_atomic_publish_over_existing_keeps_old_until_dir_sync")
            .unwrap();
    harness
        .run_workload(|workload| {
            let destination = workload.root().join("data");
            let temporary = workload.root().join("data.tmp");

            write_file(&destination, b"old");
            htap_fs::fsync_file(&destination).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_file(&temporary, b"new");
            htap_fs::fsync_file(&temporary).unwrap();
            htap_fs::dur::rename(&temporary, &destination).unwrap();
            workload.ack("renamed");
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let rename_point = snapshot
        .log
        .iter()
        .position(|op| matches!(op, Op::Rename { .. }))
        .map(|index| index + 1)
        .unwrap();

    harness
        .replay(
            &format!(
                "forward_replay_atomic_publish_over_existing_keeps_old_until_dir_sync/strict/0/k={rename_point}"
            ),
            |root, info| {
                assert_eq!(info.crash_point, rename_point);
                assert_eq!(fs::read(root.join("data")).unwrap(), b"old");
                assert!(!root.join("data.tmp").exists());
            },
        )
        .unwrap();
}

#[test]
fn torn_can_keep_later_sector_without_first() {
    const SECTOR_SIZE: usize = 4;

    let harness = CrashHarness::new("torn_can_keep_later_sector_without_first").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"abcdefghijkl");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 0, b"123456789012");
            workload.ack("dirty");
        })
        .unwrap();

    let mut reached = false;
    for seed in 0..128 {
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: SECTOR_SIZE,
                },
                |root, info| {
                    if info.acked_labels.iter().any(|label| label == "dirty") {
                        let bytes = fs::read(root.join("data")).unwrap();
                        if bytes[..SECTOR_SIZE] == *b"abcd"
                            && bytes[SECTOR_SIZE..SECTOR_SIZE * 2] == *b"5678"
                        {
                            reached = true;
                        }
                    }
                },
            )
            .unwrap();

        if reached {
            break;
        }
    }

    assert!(
        reached,
        "torn writes never retained a later sector while losing the first sector"
    );
}

#[test]
fn torn_zero_extension_reachable() {
    let harness = CrashHarness::new("torn_zero_extension_reachable").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"base");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 8, b"tail");
            workload.ack("extended");
        })
        .unwrap();

    let mut reached = false;
    for seed in 0..128 {
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: 4,
                },
                |root, info| {
                    if info.acked_labels.iter().any(|label| label == "extended")
                        && fs::read(root.join("data")).unwrap() == b"base\0\0\0\0tail"
                    {
                        reached = true;
                    }
                },
            )
            .unwrap();

        if reached {
            break;
        }
    }

    assert!(
        reached,
        "torn writes never persisted the zero-filled extension"
    );
}

#[test]
fn chaos_applies_in_log_order() {
    let harness = CrashHarness::new("chaos_applies_in_log_order").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"first");
            write_at(&path, 0, b"second");
            workload.ack("written");
        })
        .unwrap();

    let mut reached = false;
    for seed in 0..128 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, info| {
                if info.acked_labels.iter().any(|label| label == "written")
                    && root.join("data").exists()
                    && fs::read(root.join("data")).unwrap() == b"second"
                {
                    reached = true;
                }
            })
            .unwrap();

        if reached {
            break;
        }
    }

    assert!(
        reached,
        "chaos did not preserve the order of dependent writes when both persisted"
    );
}

#[test]
fn every_crash_image_reachable_from_ancestor_reachability() {
    let harness =
        CrashHarness::new("every_crash_image_reachable_from_ancestor_reachability").unwrap();
    harness
        .run_workload(|workload| {
            let parent = workload.root().join("parent");
            let child = parent.join("child");
            let data = child.join("data");

            htap_fs::dur::create_dir(&parent).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::create_dir(&child).unwrap();
            write_file(&data, b"contents");
            htap_fs::fsync_file(&data).unwrap();
            htap_fs::sync_dir(&child).unwrap();
            htap_fs::sync_dir(&parent).unwrap();
            workload.ack("published");
        })
        .unwrap();

    harness
        .enumerate(&CrashPolicy::Chaos { seed: 23 }, |root, _info| {
            let parent = root.join("parent");
            let child = parent.join("child");
            let data = child.join("data");

            if data.exists() {
                assert!(child.is_dir());
                assert!(parent.is_dir());
            }
            if child.exists() {
                assert!(parent.is_dir());
            }
        })
        .unwrap();
}
