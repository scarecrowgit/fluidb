use std::fs;
use std::io::{self, Write};

use htap_common::fs::{self as htap_fs, with_skip_sync, DurOpenOptions, Op, SkipSync};
use htap_crashsim::{CrashHarness, SyncFault};

const INJECTED_FAILURE: &str = "crashsim injected sync failure";

fn assert_injected_failure(error: io::Error) {
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), INJECTED_FAILURE);
}

#[cfg(unix)]
#[test]
fn sync_fault_nth_fails_exactly_once_and_logs_nothing() {
    const FILE_SITE: &str = "sync_fault:nth_file";
    const DIR_SITE: &str = "sync_fault:nth_dir";

    let harness = CrashHarness::new("sync_fault_nth_fails_exactly_once").unwrap();
    harness.set_sync_fault(SyncFault::Nth(2));

    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(path).unwrap();
            file.write_all(b"contents").unwrap();

            file.sync_all_site(FILE_SITE).unwrap();
            assert_injected_failure(
                htap_fs::dur::sync_dir_site(workload.root(), DIR_SITE).unwrap_err(),
            );
            file.sync_all_site(FILE_SITE).unwrap();
        })
        .unwrap();

    assert_eq!(harness.sync_attempts(), 3);
    assert_eq!(harness.sync_faults_fired(), 1);

    let snapshot = harness.snapshot().unwrap();
    assert_eq!(
        snapshot
            .log
            .iter()
            .filter(|op| matches!(op, Op::FsyncFile { .. } | Op::FsyncDir { .. }))
            .count(),
        2
    );
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncDir {
                site: Some(DIR_SITE),
                ..
            }
        )
    }));
}

#[test]
fn sync_fault_site_occurrence() {
    const TARGET_SITE: &str = "sync_fault:site_target";
    const OTHER_SITE: &str = "sync_fault:site_other";

    let harness = CrashHarness::new("sync_fault_site_occurrence").unwrap();
    harness.set_sync_fault(SyncFault::Site {
        site: TARGET_SITE,
        occurrence: 2,
    });

    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(path).unwrap();
            file.write_all(b"contents").unwrap();

            file.sync_all_site(TARGET_SITE).unwrap();
            file.sync_all_site(OTHER_SITE).unwrap();
            file.sync_all().unwrap();
            assert_injected_failure(file.sync_all_site(TARGET_SITE).unwrap_err());
            file.sync_all_site(TARGET_SITE).unwrap();
            file.sync_all_site(OTHER_SITE).unwrap();
        })
        .unwrap();

    assert_eq!(harness.sync_attempts(), 6);
    assert_eq!(harness.sync_faults_fired(), 1);

    let snapshot = harness.snapshot().unwrap();
    assert_eq!(
        snapshot
            .log
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    Op::FsyncFile {
                        site: Some(TARGET_SITE),
                        ..
                    }
                )
            })
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .log
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    Op::FsyncFile {
                        site: Some(OTHER_SITE),
                        ..
                    }
                )
            })
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .log
            .iter()
            .filter(|op| matches!(op, Op::FsyncFile { site: None, .. }))
            .count(),
        1
    );
}

#[test]
fn sync_fault_ignored_paths_not_counted() {
    const IGNORED_SITE: &str = "sync_fault:ignored";
    const TRACKED_SITE: &str = "sync_fault:tracked";

    let harness = CrashHarness::new("sync_fault_ignored_paths_not_counted")
        .unwrap()
        .with_data_root("srv");

    harness
        .run_workload(|workload| {
            let srv = workload.root().join("srv");
            htap_fs::create_dir_all_durable(&srv).unwrap();
            harness.set_sync_fault(SyncFault::Nth(1));

            let spill = srv.join("spill");
            fs::create_dir_all(&spill).unwrap();

            let ignored_path = spill.join("data");
            let mut ignored_options = DurOpenOptions::new();
            ignored_options.write(true).create(true).truncate(true);

            let mut ignored = ignored_options.open(ignored_path).unwrap();
            ignored.write_all(b"ephemeral").unwrap();
            ignored.sync_all_site(IGNORED_SITE).unwrap();

            let tracked_path = workload.root().join("tracked");
            let mut tracked_options = DurOpenOptions::new();
            tracked_options.write(true).create(true).truncate(true);

            let mut tracked = tracked_options.open(tracked_path).unwrap();
            tracked.write_all(b"durable").unwrap();
            assert_injected_failure(tracked.sync_all_site(TRACKED_SITE).unwrap_err());
        })
        .unwrap();

    assert_eq!(harness.sync_attempts(), 1);
    assert_eq!(harness.sync_faults_fired(), 1);

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile {
                site: Some(IGNORED_SITE),
                ..
            }
        )
    }));
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile {
                site: Some(TRACKED_SITE),
                ..
            }
        )
    }));
}

#[test]
fn sync_fault_counts_attempts_including_skipped() {
    const SKIPPED_SITE: &str = "sync_fault:skipped";

    let harness = CrashHarness::new("sync_fault_counts_attempts_including_skipped").unwrap();
    harness.set_sync_fault(SyncFault::Nth(2));

    with_skip_sync(SkipSync::Site(SKIPPED_SITE), || {
        harness
            .run_workload(|workload| {
                let path = workload.root().join("data");
                let mut options = DurOpenOptions::new();
                options.write(true).create(true).truncate(true);

                let mut file = options.open(path).unwrap();
                file.write_all(b"contents").unwrap();

                file.sync_all_site(SKIPPED_SITE).unwrap();
                assert_injected_failure(file.sync_all_site(SKIPPED_SITE).unwrap_err());
                file.sync_all_site(SKIPPED_SITE).unwrap();
            })
            .unwrap();
    });

    assert_eq!(harness.sync_attempts(), 3);
    assert_eq!(harness.sync_faults_fired(), 1);

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile {
                site: Some(SKIPPED_SITE),
                ..
            }
        )
    }));
}

#[test]
fn sync_fault_fires_through_sync_data_and_fsync_path() {
    const DATA_SITE: &str = "sync_fault:sync_data";
    const PATH_SITE: &str = "sync_fault:fsync_path";

    let data_harness =
        CrashHarness::new("sync_fault_fires_through_sync_data_and_fsync_path_data").unwrap();
    data_harness.set_sync_fault(SyncFault::Nth(1));

    data_harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(path).unwrap();
            file.write_all(b"contents").unwrap();
            assert_injected_failure(file.sync_data_site(DATA_SITE).unwrap_err());
        })
        .unwrap();

    assert_eq!(data_harness.sync_attempts(), 1);
    assert_eq!(data_harness.sync_faults_fired(), 1);
    let data_snapshot = data_harness.snapshot().unwrap();
    assert!(!data_snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile {
                site: Some(DATA_SITE),
                ..
            }
        )
    }));

    let path_harness =
        CrashHarness::new("sync_fault_fires_through_sync_data_and_fsync_path_path").unwrap();
    path_harness.set_sync_fault(SyncFault::Nth(1));

    path_harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(&path).unwrap();
            file.write_all(b"contents").unwrap();
            assert_injected_failure(htap_fs::dur::fsync_path_site(&path, PATH_SITE).unwrap_err());
        })
        .unwrap();

    assert_eq!(path_harness.sync_attempts(), 1);
    assert_eq!(path_harness.sync_faults_fired(), 1);
    let path_snapshot = path_harness.snapshot().unwrap();
    assert!(!path_snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile {
                site: Some(PATH_SITE),
                ..
            }
        )
    }));
}

#[cfg(unix)]
#[test]
fn sync_fault_ignored_dir_and_unregistered_paths_not_counted() {
    const IGNORED_SITE: &str = "sync_fault:ignored_dir";
    const UNREGISTERED_SITE: &str = "sync_fault:unregistered_dir";
    const TRACKED_SITE: &str = "sync_fault:tracked_dir";

    let harness = CrashHarness::new("sync_fault_ignored_dir_and_unregistered_paths_not_counted")
        .unwrap()
        .with_data_root("srv");

    harness
        .run_workload(|workload| {
            let srv = workload.root().join("srv");
            htap_fs::create_dir_all_durable(&srv).unwrap();

            let spill = srv.join("spill");
            fs::create_dir_all(&spill).unwrap();
            let unregistered = tempfile::tempdir().unwrap();

            harness.set_sync_fault(SyncFault::Nth(1));

            htap_fs::dur::sync_dir_site(&spill, IGNORED_SITE).unwrap();
            htap_fs::dur::sync_dir_site(unregistered.path(), UNREGISTERED_SITE).unwrap();
            assert_eq!(harness.sync_attempts(), 0);
            assert_eq!(harness.sync_faults_fired(), 0);

            assert_injected_failure(
                htap_fs::dur::sync_dir_site(workload.root(), TRACKED_SITE).unwrap_err(),
            );
        })
        .unwrap();

    assert_eq!(harness.sync_attempts(), 1);
    assert_eq!(harness.sync_faults_fired(), 1);

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncDir {
                site: Some(IGNORED_SITE | UNREGISTERED_SITE | TRACKED_SITE),
                ..
            }
        )
    }));
}

#[cfg(unix)]
#[test]
fn sync_fault_missing_path_returns_not_found_uncounted() {
    const MISSING_DIR_SITE: &str = "sync_fault:missing_dir";
    const MISSING_PATH_SITE: &str = "sync_fault:missing_path";

    let harness = CrashHarness::new("sync_fault_missing_path_returns_not_found_uncounted").unwrap();
    harness.set_sync_fault(SyncFault::Nth(1));

    harness
        .run_workload(|workload| {
            let missing = workload.root().join("missing");

            let dir_error = htap_fs::dur::sync_dir_site(&missing, MISSING_DIR_SITE).unwrap_err();
            assert_eq!(dir_error.kind(), io::ErrorKind::NotFound);

            let path_error =
                htap_fs::dur::fsync_path_site(&missing, MISSING_PATH_SITE).unwrap_err();
            assert_eq!(path_error.kind(), io::ErrorKind::NotFound);

            assert_eq!(harness.sync_attempts(), 0);
            assert_eq!(harness.sync_faults_fired(), 0);
        })
        .unwrap();

    assert_eq!(harness.sync_attempts(), 0);
    assert_eq!(harness.sync_faults_fired(), 0);
}
