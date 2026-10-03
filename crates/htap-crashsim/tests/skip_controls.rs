use std::any::Any;
use std::fs;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use htap_common::fs::{
    self as htap_fs, current_skip_sync, scope_skip_hits, with_skip_sync, DurOpenOptions, Op,
    SkipSync,
};
use htap_crashsim::{assert_skip_kills, CrashHarness, CrashPolicy};

const DATA: &[u8] = b"durable contents";
const DATA_SYNC_SITE: &str = "skip_controls:data_sync";
const ROOT_SYNC_SITE: &str = "skip_controls:root_sync";

fn run_durable_workload(harness: &CrashHarness) {
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(&path).unwrap();
            file.write_all(DATA).unwrap();
            file.sync_all_site(DATA_SYNC_SITE).unwrap();

            htap_fs::dur::sync_dir_site(workload.root(), ROOT_SYNC_SITE).unwrap();
            workload.ack("published");
        })
        .unwrap();
}

#[test]
fn durable_publish_survives() {
    let harness = CrashHarness::new("skip_controls_durable_publish").unwrap();
    run_durable_workload(&harness);

    let mut checked = 0;
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if !info.acked_labels.iter().any(|label| label == "published") {
                return;
            }

            checked += 1;
            assert_eq!(fs::read(root.join("data")).unwrap(), DATA);
        })
        .unwrap();

    assert!(checked > 0, "no acknowledged crash image was checked");
}

#[test]
fn assert_skip_kills_accepts_a_real_kill() {
    assert_skip_kills(
        "real skipped file sync",
        SkipSync::Site(DATA_SYNC_SITE),
        durable_publish_survives,
    );
}

htap_crashsim::crashsim_witness!(
    crashsim_witness_accepts_a_real_kill,
    site = "skip_controls:data_sync",
    body = durable_publish_survives
);

#[test]
fn assert_skip_kills_rejects_untagged_panic() {
    let panic = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills("untagged panic", SkipSync::Site(DATA_SYNC_SITE), || {
            let harness = CrashHarness::new("skip_controls_untagged_panic").unwrap();
            harness
                .run_workload(|workload| {
                    let path = workload.root().join("data");
                    let mut options = DurOpenOptions::new();
                    options.write(true).create(true).truncate(true);

                    let mut file = options.open(path).unwrap();
                    file.write_all(DATA).unwrap();
                    file.sync_all_site(DATA_SYNC_SITE).unwrap();

                    panic!("plain untagged panic");
                })
                .unwrap();
        });
    }))
    .unwrap_err();

    assert!(
        panic_message(panic.as_ref()).contains("died for another reason"),
        "unexpected panic: {}",
        panic_message(panic.as_ref())
    );
}

#[test]
fn assert_skip_kills_rejects_zero_hits() {
    let panic = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills(
            "unexercised skip site",
            SkipSync::Site("skip_controls:never"),
            || {
                let harness = CrashHarness::new("skip_controls_zero_hits").unwrap();
                run_durable_workload(&harness);

                harness
                    .enumerate(&CrashPolicy::Strict, |_root, _info| {
                        panic!("deliberate checker failure");
                    })
                    .unwrap();
            },
        );
    }))
    .unwrap_err();

    assert!(
        panic_message(panic.as_ref()).contains("site never exercised"),
        "unexpected panic: {}",
        panic_message(panic.as_ref())
    );
}

#[test]
fn assert_skip_kills_rejects_survivor() {
    let panic = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills(
            "surviving skipped directory sync",
            SkipSync::Site(ROOT_SYNC_SITE),
            || {
                let harness = CrashHarness::new("skip_controls_survivor").unwrap();
                run_durable_workload(&harness);

                harness
                    .enumerate(&CrashPolicy::Strict, |root, _info| {
                        let path = root.join("data");
                        if path.exists() {
                            assert_eq!(fs::read(path).unwrap(), DATA);
                        }
                    })
                    .unwrap();
            },
        );
    }))
    .unwrap_err();

    assert!(
        panic_message(panic.as_ref()).contains("mutation survived"),
        "unexpected panic: {}",
        panic_message(panic.as_ref())
    );
}

#[test]
fn denylisted_recorder_panic_is_not_a_kill() {
    let panic = panic::catch_unwind(AssertUnwindSafe(|| {
        assert_skip_kills(
            "denylisted recorder panic",
            SkipSync::Site(DATA_SYNC_SITE),
            || {
                let harness = CrashHarness::new("skip_controls_denylisted_panic").unwrap();
                harness
                    .run_workload(|workload| {
                        let path = workload.root().join("data");
                        let mut options = DurOpenOptions::new();
                        options.write(true).create(true).truncate(true);

                        let mut file = options.open(&path).unwrap();
                        file.write_all(DATA).unwrap();
                        file.sync_all_site(DATA_SYNC_SITE).unwrap();

                        fs::write(&path, b"raw mutation").unwrap();
                        htap_fs::fsync_file(&path).unwrap();
                    })
                    .unwrap();
            },
        );
    }))
    .unwrap_err();

    assert!(
        panic_message(panic.as_ref()).contains("died for another reason"),
        "unexpected panic: {}",
        panic_message(panic.as_ref())
    );
}

#[test]
fn skip_site_removes_only_that_site_from_the_log() {
    let harness = CrashHarness::new("skip_controls_site_log").unwrap();

    with_skip_sync(SkipSync::Site(DATA_SYNC_SITE), || {
        run_durable_workload(&harness);
    });

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile { site, .. } if *site == Some(DATA_SYNC_SITE)
        )
    }));
    assert!(snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncDir { site, .. } if *site == Some(ROOT_SYNC_SITE)
        )
    }));
}

#[test]
fn skip_scope_restores_on_panic_and_nests() {
    let outer = SkipSync::Site(ROOT_SYNC_SITE);
    let inner = SkipSync::Site(DATA_SYNC_SITE);

    with_skip_sync(outer.clone(), || {
        let panic = panic::catch_unwind(AssertUnwindSafe(|| {
            with_skip_sync(inner.clone(), || {
                assert_eq!(current_skip_sync(), inner);
                panic!("deliberate nested-scope panic");
            });
        }));
        assert!(panic.is_err());
        assert_eq!(current_skip_sync(), outer);
    });

    assert_eq!(current_skip_sync(), SkipSync::None);
}

#[test]
fn recovery_recorders_inherit_skip() {
    const RECOVERY_SYNC_SITE: &str = "skip_controls:recovery_sync";

    let harness = CrashHarness::new("skip_controls_recovery_inherits_skip").unwrap();
    run_durable_workload(&harness);

    with_skip_sync(SkipSync::Site(RECOVERY_SYNC_SITE), || {
        harness
            .enumerate_recovery(
                &CrashPolicy::Strict,
                |root| {
                    let path = root.join("data");
                    if path.exists() {
                        let mut options = DurOpenOptions::new();
                        options.read(true);
                        let file = options.open(path).unwrap();
                        file.sync_all_site(RECOVERY_SYNC_SITE).unwrap();
                    }
                },
                |_root, info| {
                    let recovery = info.recovery.as_ref().unwrap();
                    assert!(!recovery.ops.iter().any(|op| {
                        matches!(
                            op,
                            Op::FsyncFile { site, .. }
                                if *site == Some(RECOVERY_SYNC_SITE)
                        )
                    }));
                },
            )
            .unwrap();
    });

    assert!(scope_skip_hits() > 0);
}

#[test]
fn skip_hits_counts_suppressed_syncs() {
    let harness = CrashHarness::new("skip_controls_hit_count").unwrap();

    with_skip_sync(SkipSync::Site(DATA_SYNC_SITE), || {
        harness
            .run_workload(|workload| {
                let path = workload.root().join("data");
                let mut options = DurOpenOptions::new();
                options.write(true).create(true).truncate(true);

                let mut file = options.open(path).unwrap();
                file.write_all(DATA).unwrap();
                for _ in 0..3 {
                    file.sync_all_site(DATA_SYNC_SITE).unwrap();
                }
            })
            .unwrap();
    });

    assert_eq!(scope_skip_hits(), 3);
}

#[test]
fn witness_scope_forces_exhaustive_no_dedupe() {
    let harness = CrashHarness::new("skip_controls_exhaustive_scope").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(path).unwrap();
            for byte in 0_u8..12 {
                file.write_all(&[byte]).unwrap();
            }
        })
        .unwrap();

    let log_len = harness.snapshot().unwrap().log.len();

    let mut n_default = 0;
    harness
        .enumerate(&CrashPolicy::Strict, |_root, _info| {
            n_default += 1;
        })
        .unwrap();

    let mut n_scoped = 0;
    with_skip_sync(SkipSync::Site("skip_controls:never"), || {
        harness
            .enumerate(&CrashPolicy::Strict, |_root, _info| {
                n_scoped += 1;
            })
            .unwrap();
    });

    assert_eq!(n_scoped, log_len + 1);
    assert!(n_scoped > n_default);
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

fn _assert_path_type(_: &Path) {}
