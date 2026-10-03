use std::path::Path;

use htap_catalog::{TableId, TabletId};
use htap_common::fs::{
    create_dir_all_durable,
    dur::{self, create_dir_all},
    sync_dir, write_new_tmp_file,
};
use htap_common::HtapError;
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};
use htap_movement::{CopyOptions, DataFormat, LocalDataMover, MovementJobKind, MovementJobPhase};

const TABLE_ID: TableId = TableId::new(1);
const TABLET_ID: TabletId = TabletId::new(1);

fn options(root: &Path, job_id: &str) -> CopyOptions {
    CopyOptions::new(
        job_id,
        TABLE_ID,
        TABLET_ID,
        DataFormat::JsonLines,
        root.join("output.jsonl"),
    )
}

fn acked(info: &CrashInfo, label: &str) -> bool {
    info.acked_labels.iter().any(|value| value == label)
}

fn reopen_job_twice(root: &Path, job_id: &str) -> Option<htap_movement::MovementJob> {
    let first_mover = LocalDataMover::new(root.join("movement")).unwrap();
    let first = match first_mover.load_job(job_id) {
        Ok(job) => job,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening movement job returned corruption: {error}");
        }
        Err(error) => panic!("reopening movement job failed: {error}"),
    };
    drop(first_mover);

    let second_mover = LocalDataMover::new(root.join("movement")).unwrap();
    let second = second_mover.load_job(job_id).unwrap();
    assert_eq!(
        second, first,
        "a second reopen changed the recovered movement job"
    );
    second
}

#[test]
fn job_state_monotonic_no_regression() {
    let harness = CrashHarness::new("job_state_monotonic_no_regression").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("movement");
            create_dir_all_durable(&root).unwrap();
            let jobs_dir = root.join("jobs");
            create_dir_all_durable(&jobs_dir).unwrap();
            let tablets_dir = root.join("tablets");
            create_dir_all_durable(&tablets_dir).unwrap();

            let mover = LocalDataMover::new(&root).unwrap();
            let opts = options(workload.root(), "state-job");
            mover.start_copy(MovementJobKind::Export, &opts).unwrap();
            workload.ack("running");

            let report = htap_movement::CopyReport {
                job_id: "state-job".into(),
                records_read: 3,
                records_committed: 3,
                rows_written: 3,
                records_skipped: 0,
                duration_ms: 0,
            };
            mover.complete_job("state-job", report).unwrap();
            workload.ack("complete");
            drop(mover);
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 73,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let job = reopen_job_twice(root, "state-job");

                if acked(info, "running") {
                    checked_with_ack = true;
                    assert!(
                        job.is_some(),
                        "acknowledged running job disappeared after recovery"
                    );
                }
                if acked(info, "complete") {
                    checked_with_ack = true;
                    assert_eq!(
                        job.expect("acknowledged completed job is absent").phase,
                        MovementJobPhase::Complete,
                        "acknowledged completed job regressed after recovery"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(checked_with_ack, "expected an acknowledged job image");
    }
}

#[test]
fn delete_artifacts_no_resurrection() {
    let harness = CrashHarness::new("delete_artifacts_no_resurrection").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("movement");
            create_dir_all_durable(&root).unwrap();
            let mover = LocalDataMover::new(&root).unwrap();
            let artifact_dir = mover.tablets_dir().join(TABLET_ID.as_u64().to_string());
            create_dir_all_durable(&artifact_dir).unwrap();
            let orphan_tmp = artifact_dir.join("orphan.tmp");
            let orphan = artifact_dir.join("orphan");
            write_new_tmp_file(&orphan_tmp, b"orphan", None).unwrap();
            dur::fsync_path(&orphan_tmp).unwrap();
            dur::rename(&orphan_tmp, &orphan).unwrap();
            sync_dir(&artifact_dir).unwrap();

            mover
                .reclaim_tablet_artifacts(TABLET_ID, || {
                    mover.delete_tablet_movement_artifacts(TABLET_ID)
                })
                .unwrap();
            workload.ack("deleted");
            drop(mover);
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let mover = LocalDataMover::new(root.join("movement")).unwrap();
            let first = mover
                .tablets_dir()
                .join(TABLET_ID.as_u64().to_string())
                .exists();
            drop(mover);
            let mover = LocalDataMover::new(root.join("movement")).unwrap();
            let second = mover
                .tablets_dir()
                .join(TABLET_ID.as_u64().to_string())
                .exists();
            assert_eq!(second, first, "a second reopen changed deletion state");

            if acked(info, "deleted") {
                checked_with_ack = true;
                assert!(!first, "acknowledged artifact deletion resurrected");
            }
        })
        .unwrap();

    assert!(checked_count > 1, "expected multiple crash images");
    assert!(checked_with_ack, "expected an acknowledged delete image");
}

fn run_job_dir_workload(workload: &htap_crashsim::WorkloadContext, volatile: bool) {
    let root = workload.root().join("movement");
    let jobs = root.join("jobs");
    if volatile {
        create_dir_all(jobs.join("dir-job")).unwrap();
    }

    let mover = LocalDataMover::new(&root).unwrap();
    let opts = options(workload.root(), "dir-job");
    mover.start_copy(MovementJobKind::Export, &opts).unwrap();
    workload.ack("job-published");
    drop(mover);
}

fn check_job_dir_images(harness: &CrashHarness) {
    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 79,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let job = reopen_job_twice(root, "dir-job");
                if acked(info, "job-published") {
                    checked_with_ack = true;
                    assert!(
                        job.is_some(),
                        "acknowledged job directory entry disappeared"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(checked_with_ack, "expected an acknowledged job image");
    }
}

#[test]
fn movement_fresh_dir_durable() {
    let harness = CrashHarness::new("movement_fresh_dir_durable").unwrap();
    harness
        .run_workload(|workload| run_job_dir_workload(workload, false))
        .unwrap();
    check_job_dir_images(&harness);
}

#[test]
fn movement_preexisting_volatile_job_dir() {
    let harness = CrashHarness::new("movement_preexisting_volatile_job_dir").unwrap();
    harness
        .run_workload(|workload| run_job_dir_workload(workload, true))
        .unwrap();
    check_job_dir_images(&harness);
}
