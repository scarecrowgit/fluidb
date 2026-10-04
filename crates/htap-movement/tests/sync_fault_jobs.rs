use std::collections::{BTreeMap, BTreeSet};

use htap_catalog::{TableId, TabletId};
use htap_common::fs::{
    create_dir_all_durable,
    dur::{self, create_dir_all},
    sync_dir, write_new_tmp_file, Op,
};
use htap_crashsim::{CrashHarness, SyncFault};
use htap_movement::{
    CopyOptions, CopyReport, DataFormat, LocalDataMover, MovementJobKind, MovementJobPhase,
};

const TABLE_ID: TableId = TableId::new(1);
const TABLET_ID: TabletId = TabletId::new(1);

const JOB_PERSIST_AMBIGUOUS_AFTER_RENAME: &[(u64, &str, u64, &str)] = &[
    (
        2,
        "atomic_publish:dir_sync",
        1,
        "F12: the completed job-file rename is visible before its post-rename directory sync fails",
    ),
    (
        3,
        "sync_dir:sync",
        1,
        "F12: the completed job-file rename is visible before the subsequent jobs-directory sync fails",
    ),
];

const DELETE_ARTIFACTS_AMBIGUOUS_AFTER_UNLINK: &[(u64, &str)] = &[
    (
        1,
        "unlink is not transactional; the Err keeps the pending reclaim entry, which retries (F12 class)",
    ),
    (
        2,
        "unlink is not transactional; the Err keeps the pending reclaim entry, which retries (F12 class)",
    ),
    (
        3,
        "unlink is not transactional; the Err keeps the pending reclaim entry, which retries (F12 class)",
    ),
];

fn options(root: &std::path::Path, job_id: &str) -> CopyOptions {
    CopyOptions::new(
        job_id,
        TABLE_ID,
        TABLET_ID,
        DataFormat::JsonLines,
        root.join("output.jsonl"),
    )
}

fn complete_job_attempt(fault: SyncFault) -> (bool, bool, bool, u64, u64) {
    let harness = CrashHarness::new("job_persist_sync_failure_is_never_swallowed").unwrap();
    let mut completed = false;
    let mut operation_succeeded = false;

    harness
        .run_workload(|workload| {
            let movement_root = workload.root().join("movement");
            create_dir_all_durable(&movement_root).unwrap();

            let mover = LocalDataMover::new(&movement_root).unwrap();
            mover
                .start_copy(
                    MovementJobKind::Export,
                    &options(workload.root(), "persist-job"),
                )
                .unwrap();

            harness.set_sync_fault(fault);
            let result = mover.complete_job(
                "persist-job",
                CopyReport {
                    job_id: "persist-job".into(),
                    records_read: 1,
                    records_committed: 1,
                    rows_written: 1,
                    records_skipped: 0,
                    duration_ms: 0,
                },
            );
            operation_succeeded = result.is_ok();

            if result.is_ok() {
                workload.ack("complete");
            }

            completed = mover
                .load_job("persist-job")
                .unwrap()
                .is_some_and(|job| job.phase == MovementJobPhase::Complete);
        })
        .unwrap();

    let acknowledged = harness.snapshot().unwrap().log.iter().any(|operation| {
        matches!(
            operation,
            Op::Ack { label } if label == "complete"
        )
    });

    (
        operation_succeeded,
        acknowledged,
        completed,
        harness.sync_attempts(),
        harness.sync_faults_fired(),
    )
}

#[test]
fn job_persist_sync_failure_is_never_swallowed() {
    let (succeeded, acknowledged, completed, attempts, faults) =
        complete_job_attempt(SyncFault::None);
    assert!(succeeded, "baseline complete_job failed");
    assert!(
        acknowledged,
        "baseline complete_job produced no completion acknowledgement"
    );
    assert!(
        completed,
        "baseline complete_job did not make the Complete job state visible"
    );
    assert!(
        attempts >= 1,
        "baseline complete_job performed no durable sync attempts"
    );
    assert_eq!(faults, 0);
    eprintln!("job_persist_sync_failure_is_never_swallowed N={attempts}");

    let expected_ambiguous: BTreeSet<_> = JOB_PERSIST_AMBIGUOUS_AFTER_RENAME
        .iter()
        .map(|(ordinal, _, _, _)| *ordinal)
        .collect();
    let mut observed_ambiguous = BTreeSet::new();
    let mut sweep_attempts = BTreeMap::new();

    for nth in 1..=attempts {
        let (succeeded, acknowledged, completed, observed_attempts, faults) =
            complete_job_attempt(SyncFault::Nth(nth));

        // complete_job exposes its durable persistence failure directly to the caller.
        // F12 permits only the documented post-rename ordinals to expose Complete.
        assert!(
            !succeeded,
            "complete_job swallowed sync failure at attempt {nth}"
        );
        assert_eq!(
            faults, 1,
            "complete_job did not fire the injected sync fault at attempt {nth}"
        );
        assert!(
            !acknowledged,
            "complete_job acknowledged success after sync failure at attempt {nth}"
        );
        assert!(
            observed_attempts >= nth,
            "complete_job did not reach injected sync attempt {nth}"
        );

        if completed {
            observed_ambiguous.insert(nth);
        }
        sweep_attempts.insert(nth, observed_attempts);
    }

    assert_eq!(
        observed_ambiguous, expected_ambiguous,
        "Complete job visibility after Err must match the documented F12 ambiguous outcomes"
    );

    for &(ordinal, site, occurrence, reason) in JOB_PERSIST_AMBIGUOUS_AFTER_RENAME {
        let (succeeded, acknowledged, completed, observed_attempts, faults) =
            complete_job_attempt(SyncFault::Site { site, occurrence });

        assert!(
            !succeeded,
            "complete_job swallowed sync failure at {site} occurrence {occurrence}"
        );
        assert_eq!(
            faults, 1,
            "complete_job did not fire the injected sync fault at {site} occurrence {occurrence}"
        );
        assert_eq!(
            observed_attempts, sweep_attempts[&ordinal],
            "complete_job sync attempts for {site} occurrence {occurrence} did not match Nth ordinal {ordinal}"
        );
        assert!(
            !acknowledged,
            "complete_job acknowledged success after sync failure at {site} occurrence {occurrence}"
        );
        assert!(
            completed,
            "sync fault at {site} occurrence {occurrence} did not produce the expected F12 ambiguous outcome: {reason}"
        );
    }
}

fn delete_artifacts_attempt(fault: SyncFault) -> (bool, bool, bool, u64, u64) {
    let harness = CrashHarness::new("delete_artifacts_sync_failure_is_never_swallowed").unwrap();
    let mut operation_succeeded = false;
    let mut artifacts_deleted = false;

    harness
        .run_workload(|workload| {
            let movement_root = workload.root().join("movement");
            create_dir_all_durable(&movement_root).unwrap();

            let mover = LocalDataMover::new(&movement_root).unwrap();
            mover
                .start_copy(
                    MovementJobKind::Export,
                    &options(workload.root(), "delete-artifacts-job"),
                )
                .unwrap();

            let artifact_dir = mover.tablets_dir().join(TABLET_ID.as_u64().to_string());
            let job_dir = mover.job_dir("delete-artifacts-job").unwrap();
            create_dir_all(&artifact_dir).unwrap();
            let temporary = artifact_dir.join("artifact.tmp");
            let artifact = artifact_dir.join("artifact");
            write_new_tmp_file(&temporary, b"artifact", None).unwrap();
            dur::fsync_path(&temporary).unwrap();
            dur::rename(&temporary, &artifact).unwrap();
            sync_dir(&artifact_dir).unwrap();

            harness.set_sync_fault(fault);
            let result = mover.reclaim_tablet_artifacts(TABLET_ID, || {
                mover.delete_tablet_movement_artifacts(TABLET_ID)
            });
            operation_succeeded = result.is_ok();

            if result.is_ok() {
                workload.ack("deleted");
            }

            artifacts_deleted = !artifact_dir.exists() && !job_dir.exists();
        })
        .unwrap();

    let acknowledged = harness.snapshot().unwrap().log.iter().any(|operation| {
        matches!(
            operation,
            Op::Ack { label } if label == "deleted"
        )
    });

    (
        operation_succeeded,
        acknowledged,
        artifacts_deleted,
        harness.sync_attempts(),
        harness.sync_faults_fired(),
    )
}

#[test]
fn delete_artifacts_sync_failure_is_never_swallowed() {
    let (succeeded, acknowledged, artifacts_deleted, attempts, faults) =
        delete_artifacts_attempt(SyncFault::None);
    assert!(succeeded, "baseline artifact deletion failed");
    assert!(
        acknowledged,
        "baseline artifact deletion produced no acknowledgement"
    );
    assert!(
        artifacts_deleted,
        "baseline artifact deletion left the tablet artifact package or movement job directory present"
    );
    assert!(
        attempts >= 1,
        "baseline artifact deletion performed no durable sync attempts"
    );
    assert_eq!(faults, 0);
    eprintln!("delete_artifacts_sync_failure_is_never_swallowed N={attempts}");

    let expected_ambiguous: BTreeSet<_> = DELETE_ARTIFACTS_AMBIGUOUS_AFTER_UNLINK
        .iter()
        .map(|(ordinal, _)| *ordinal)
        .collect();
    let mut observed_ambiguous = BTreeSet::new();
    let mut sweep_attempts = BTreeMap::new();

    for nth in 1..=attempts {
        let (succeeded, acknowledged, artifacts_deleted, observed_attempts, faults) =
            delete_artifacts_attempt(SyncFault::Nth(nth));

        assert!(
            !succeeded,
            "delete_tablet_movement_artifacts swallowed sync failure at attempt {nth}"
        );
        assert_eq!(
            faults, 1,
            "artifact deletion did not fire the injected sync fault at attempt {nth}"
        );
        assert!(
            !acknowledged,
            "artifact deletion acknowledged success after sync failure at attempt {nth}"
        );
        assert!(
            observed_attempts >= nth,
            "artifact deletion did not reach injected sync attempt {nth}"
        );

        if artifacts_deleted {
            observed_ambiguous.insert(nth);
        }
        sweep_attempts.insert(nth, observed_attempts);
    }

    assert_eq!(
        observed_ambiguous, expected_ambiguous,
        "artifact absence after Err must match the documented F12-class ambiguous outcomes"
    );
    assert_eq!(
        sweep_attempts.len() as u64,
        attempts,
        "artifact deletion did not record an attempt total for every swept ordinal"
    );
}
