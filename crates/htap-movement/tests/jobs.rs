use std::fs;
use std::path::PathBuf;

use htap_catalog::{TableId, TabletId};
use htap_common::{HtapError, Version};
use htap_movement::{
    decode_job, encode_job, CopyOptions, CopyReport, DataFormat, JobCounters, LocalDataMover,
    MovementJob, MovementJobKind, MovementJobPhase, MovementJobRequest, FORMAT_VERSION, HEADER_LEN,
    HEADER_MAGIC, JOB_FILE_NAME, JOB_TMP_FILE_NAME, MAX_JOB_PAYLOAD_BYTES,
};
use tempfile::tempdir;

fn sample_request(job_id: &str) -> MovementJobRequest {
    MovementJobRequest {
        job_id: job_id.to_string(),
        kind: MovementJobKind::Import,
        table_id: TableId::new(42),
        tablet_id: TabletId::new(101),
        format: DataFormat::Csv,
        path: PathBuf::from("/tmp/sample.csv"),
        batch_rows: 1000,
        max_errors: 0,
        delimiter: b',',
        has_header: true,
        pinned_version: Some(Version::new(7)),
    }
}

#[test]
fn test_envelope_roundtrip_and_reopen() {
    let req = sample_request("job-roundtrip-1");
    let mut job = MovementJob::new(req.clone());
    job.counters = JobCounters {
        records_read: 1500,
        records_committed: 1000,
        rows_written: 1000,
        records_skipped: 500,
    };
    job.checkpoint = Some("chunk-002".to_string());

    let encoded = encode_job(&job).expect("encode job should succeed");
    assert!(encoded.len() > HEADER_LEN);
    assert_eq!(&encoded[0..8], HEADER_MAGIC);

    let decoded = decode_job(&encoded).expect("decode job should succeed");
    assert_eq!(decoded, job);
    assert_eq!(decoded.job_id(), "job-roundtrip-1");
    assert_eq!(decoded.table_id, TableId::new(42));
    assert_eq!(decoded.tablet_id, TabletId::new(101));
    assert_eq!(decoded.phase(), MovementJobPhase::Running);
    assert_eq!(decoded.counters.records_committed, 1000);
    assert_eq!(decoded.checkpoint.as_deref(), Some("chunk-002"));

    // Reopen with LocalDataMover
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let started = mover.start_job(req).expect("start job");
    assert_eq!(started.job_id(), "job-roundtrip-1");

    mover
        .commit_progress(
            "job-roundtrip-1",
            JobCounters {
                records_read: 200,
                records_committed: 200,
                rows_written: 200,
                records_skipped: 0,
            },
            Some("offset_200".to_string()),
        )
        .expect("commit progress");

    // Re-instantiate mover on same directory
    let mover2 = LocalDataMover::new(dir.path()).expect("reopen mover");
    let loaded = mover2
        .load_job("job-roundtrip-1")
        .expect("load job")
        .expect("job exists");
    assert_eq!(loaded.counters.records_committed, 200);
    assert_eq!(loaded.checkpoint.as_deref(), Some("offset_200"));
}

#[test]
fn test_corruption_crc_mismatch() {
    let req = sample_request("job-crc");
    let job = MovementJob::new(req);
    let mut encoded = encode_job(&job).expect("encode job");

    // Corrupt a byte in the payload region
    let payload_offset = HEADER_LEN + 10;
    encoded[payload_offset] ^= 0xFF;

    let err = decode_job(&encoded).expect_err("decoding corrupted payload must fail");
    match err {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("checksum mismatch"),
                "expected checksum mismatch error, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_corruption_truncation() {
    let req = sample_request("job-trunc");
    let job = MovementJob::new(req);
    let encoded = encode_job(&job).expect("encode job");

    // Truncate to less than HEADER_LEN
    let too_small = &encoded[..10];
    let err_small = decode_job(too_small).expect_err("truncated header must fail");
    assert!(matches!(err_small, HtapError::Corruption(_)));

    // Truncate by removing 1 byte from the end of the payload
    let truncated_payload = &encoded[..encoded.len() - 1];
    let err_trunc = decode_job(truncated_payload).expect_err("truncated payload must fail");
    match err_trunc {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("truncated"),
                "expected truncation message, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_corruption_header_magic() {
    let req = sample_request("job-magic");
    let job = MovementJob::new(req);
    let mut encoded = encode_job(&job).expect("encode job");

    // Corrupt magic
    encoded[0] = b'X';
    encoded[1] = b'Z';

    let err = decode_job(&encoded).expect_err("invalid magic must fail");
    match err {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("magic"),
                "expected magic corruption message, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_corruption_unsupported_version() {
    let req = sample_request("job-version");
    let job = MovementJob::new(req);
    let mut encoded = encode_job(&job).expect("encode job");

    // Change format version to 999
    let bad_version = 999u16;
    encoded[8..10].copy_from_slice(&bad_version.to_le_bytes());

    let err = decode_job(&encoded).expect_err("unsupported version must fail");
    match err {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("unsupported"),
                "expected unsupported version message, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_corruption_trailing_garbage() {
    let req = sample_request("job-trailing");
    let job = MovementJob::new(req);
    let mut encoded = encode_job(&job).expect("encode job");

    // Append extra garbage bytes
    encoded.extend_from_slice(b"extra_trailing_bytes");

    let err = decode_job(&encoded).expect_err("trailing bytes must be rejected as corruption");
    match err {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("trailing leftover bytes"),
                "expected trailing leftover error, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_payload_length_bound() {
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(HEADER_MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    // Specify a payload length greater than MAX_JOB_PAYLOAD_BYTES
    let excessive_len = MAX_JOB_PAYLOAD_BYTES + 1;
    header.extend_from_slice(&excessive_len.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes()); // dummy crc

    let err = decode_job(&header).expect_err("excessive payload len must be rejected");
    match err {
        HtapError::Corruption(msg) => {
            assert!(
                msg.contains("exceeds maximum limit"),
                "expected bound rejection, got: {msg}"
            );
        }
        other => panic!("expected HtapError::Corruption, got {other:?}"),
    }
}

#[test]
fn test_path_traversal_and_job_id_validation() {
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let invalid_ids = [
        "",
        "..",
        ".",
        "../traversal",
        "/absolute/path",
        "nested/sub/job",
        "windows\\separator",
        "job\0null",
        "-dashstart",
        ".dotstart",
        "spaces in id",
        "job!@#$",
    ];

    for invalid_id in invalid_ids {
        assert!(
            htap_movement::validate_job_id(invalid_id).is_err(),
            "job_id '{invalid_id}' should be rejected"
        );

        let mut req = sample_request("temp");
        req.job_id = invalid_id.to_string();

        let res = mover.start_job(req);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "mover.start_job with '{invalid_id}' should fail with InvalidArgument"
        );

        let load_res = mover.load_job(invalid_id);
        assert!(
            matches!(load_res, Err(HtapError::InvalidArgument(_))),
            "mover.load_job with '{invalid_id}' should fail with InvalidArgument"
        );
    }

    // Valid IDs must pass
    let valid_ids = ["job123", "job-456", "import_tbl_tab", "job.v1"];
    for valid_id in valid_ids {
        assert!(
            htap_movement::validate_job_id(valid_id).is_ok(),
            "valid job_id '{valid_id}' should pass"
        );
    }
}

#[test]
fn test_copy_options_validation() {
    // Valid options
    let valid = CopyOptions::new(
        "job-valid",
        TableId::new(1),
        TabletId::new(2),
        DataFormat::Csv,
        "/tmp/input.csv",
    );
    assert!(valid.validate().is_ok());

    // Zero batch_rows
    let invalid_batch = valid.clone().with_batch_rows(0);
    assert!(matches!(
        invalid_batch.validate(),
        Err(HtapError::InvalidArgument(_))
    ));

    // Excessive batch_rows
    let invalid_batch_high = valid
        .clone()
        .with_batch_rows(CopyOptions::MAX_BATCH_ROWS + 1);
    assert!(matches!(
        invalid_batch_high.validate(),
        Err(HtapError::InvalidArgument(_))
    ));

    // Empty path
    let invalid_path = CopyOptions::new(
        "job-valid",
        TableId::new(1),
        TabletId::new(2),
        DataFormat::Csv,
        "",
    );
    assert!(matches!(
        invalid_path.validate(),
        Err(HtapError::InvalidArgument(_))
    ));

    // Directory path
    let dir = tempdir().expect("tempdir");
    let invalid_dir_path = CopyOptions::new(
        "job-valid",
        TableId::new(1),
        TabletId::new(2),
        DataFormat::Csv,
        dir.path(),
    );
    assert!(matches!(
        invalid_dir_path.validate(),
        Err(HtapError::InvalidArgument(_))
    ));
}

#[test]
fn test_request_mismatch_conflict() {
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let req1 = sample_request("job-conflict");
    mover.start_job(req1.clone()).expect("start initial job");

    // Same request -> returns existing job without conflict
    let retry = mover.start_job(req1.clone()).expect("idempotent retry");
    assert_eq!(retry.job_id(), "job-conflict");

    // Different table_id -> Conflict
    let mut req2 = req1.clone();
    req2.table_id = TableId::new(999);
    let err_tbl = mover
        .start_job(req2)
        .expect_err("must reject table mismatch");
    assert!(matches!(err_tbl, HtapError::Conflict(_)));

    // Different tablet_id -> Conflict
    let mut req3 = req1.clone();
    req3.tablet_id = TabletId::new(999);
    let err_tab = mover
        .start_job(req3)
        .expect_err("must reject tablet mismatch");
    assert!(matches!(err_tab, HtapError::Conflict(_)));

    // Different format -> Conflict
    let mut req4 = req1.clone();
    req4.format = DataFormat::JsonLines;
    let err_fmt = mover
        .start_job(req4)
        .expect_err("must reject format mismatch");
    assert!(matches!(err_fmt, HtapError::Conflict(_)));

    // Different path -> Conflict
    let mut req5 = req1;
    req5.path = PathBuf::from("/tmp/other.csv");
    let err_path = mover
        .start_job(req5)
        .expect_err("must reject path mismatch");
    assert!(matches!(err_path, HtapError::Conflict(_)));
}

#[test]
fn test_terminal_idempotent_retry_and_transitions() {
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let req = sample_request("job-terminal");
    let started = mover.start_job(req.clone()).expect("start job");
    assert!(started.is_running());

    let report = CopyReport {
        job_id: "job-terminal".to_string(),
        records_read: 100,
        records_committed: 100,
        rows_written: 100,
        records_skipped: 0,
        duration_ms: 45,
    };

    let completed = mover
        .complete_job("job-terminal", report.clone())
        .expect("complete job");
    assert!(completed.is_complete());
    assert_eq!(completed.report.as_ref(), Some(&report));

    // Terminal idempotent retry: start_job returns saved report / job
    let retried = mover.start_job(req.clone()).expect("retry completed job");
    assert!(retried.is_complete());
    assert_eq!(retried.report.as_ref(), Some(&report));

    // complete_job again is a no-op returning saved complete state
    let re_completed = mover
        .complete_job("job-terminal", report.clone())
        .expect("complete already complete job");
    assert!(re_completed.is_complete());

    // Commit progress on terminal job must fail with Conflict
    let err_prog = mover
        .commit_progress("job-terminal", JobCounters::default(), None)
        .expect_err("progress on terminal job must fail");
    assert!(matches!(err_prog, HtapError::Conflict(_)));

    // Fail already completed job must fail with Conflict
    let err_fail = mover
        .fail_job("job-terminal", "fatal")
        .expect_err("failing completed job must fail");
    assert!(matches!(err_fail, HtapError::Conflict(_)));

    // Test failed job terminal idempotent retry
    let req_fail = sample_request("job-failed");
    mover.start_job(req_fail.clone()).expect("start job-failed");
    let failed = mover
        .fail_job("job-failed", "disk full")
        .expect("fail job-failed");
    assert!(failed.is_failed());
    assert_eq!(failed.error.as_deref(), Some("disk full"));

    // start_job retry on failed job returns failed job
    let retried_fail = mover
        .start_job(req_fail)
        .expect("retry failed job returns saved state");
    assert!(retried_fail.is_failed());
    assert_eq!(retried_fail.error.as_deref(), Some("disk full"));

    // fail_job again is a no-op
    let re_failed = mover
        .fail_job("job-failed", "disk full 2")
        .expect("fail again is no-op");
    assert!(re_failed.is_failed());

    // complete_job on failed job fails with Conflict
    let err_complete_on_failed = mover
        .complete_job("job-failed", report)
        .expect_err("completing failed job must fail");
    assert!(matches!(err_complete_on_failed, HtapError::Conflict(_)));
}

#[test]
fn test_running_restart_and_progress_persistence() {
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let req = sample_request("job-resume");
    mover.start_job(req.clone()).expect("start job");

    // Commit batch 1 progress
    let counters_1 = JobCounters {
        records_read: 500,
        records_committed: 500,
        rows_written: 500,
        records_skipped: 0,
    };
    mover
        .commit_progress(
            "job-resume",
            counters_1,
            Some("checkpoint-batch-1".to_string()),
        )
        .expect("commit batch 1");

    // Simulate crash and recovery by opening a fresh mover instance
    let mover2 = LocalDataMover::new(dir.path()).expect("reopen mover");
    let resumed = mover2.resume_job("job-resume").expect("resume job");
    assert!(resumed.is_running());
    assert_eq!(resumed.counters, counters_1);
    assert_eq!(resumed.checkpoint.as_deref(), Some("checkpoint-batch-1"));

    // Also calling start_job with the same request resumes running job from counters
    let started_again = mover2.start_job(req).expect("start_job resumes running");
    assert!(started_again.is_running());
    assert_eq!(started_again.counters, counters_1);

    // Advance progress in batch 2
    let counters_2 = JobCounters {
        records_read: 1000,
        records_committed: 1000,
        rows_written: 1000,
        records_skipped: 0,
    };
    mover2
        .commit_progress(
            "job-resume",
            counters_2,
            Some("checkpoint-batch-2".to_string()),
        )
        .expect("commit batch 2");

    let loaded = mover2
        .load_job("job-resume")
        .expect("load")
        .expect("exists");
    assert_eq!(loaded.counters, counters_2);
    assert_eq!(loaded.checkpoint.as_deref(), Some("checkpoint-batch-2"));
}

#[test]
fn test_atomic_temp_behavior() {
    let dir = tempdir().expect("create temp dir");
    let mover = LocalDataMover::new(dir.path()).expect("create mover");

    let req = sample_request("job-atomic");
    mover.start_job(req).expect("start job");

    let job_file = mover.job_file_path("job-atomic");
    let tmp_file = mover.job_tmp_file_path("job-atomic");

    // JOB file must exist, JOB.tmp must NOT exist after atomic publish
    assert!(job_file.exists(), "JOB file must exist");
    assert!(!tmp_file.exists(), "JOB.tmp must be cleaned up / renamed");
    assert_eq!(job_file.file_name().unwrap(), JOB_FILE_NAME);
    assert_eq!(tmp_file.file_name().unwrap(), JOB_TMP_FILE_NAME);

    // Simulate a crash leaving an orphaned JOB.tmp
    fs::write(&tmp_file, b"leftover orphan tmp data").expect("write orphan tmp");
    assert!(tmp_file.exists());

    // Reading the job ignores the leftover .tmp file and reads the valid JOB file
    let loaded = mover
        .load_job("job-atomic")
        .expect("load job")
        .expect("job exists");
    assert_eq!(loaded.job_id(), "job-atomic");

    // Committing progress cleanly overwrites .tmp and renames over JOB
    mover
        .commit_progress(
            "job-atomic",
            JobCounters {
                records_read: 50,
                records_committed: 50,
                rows_written: 50,
                records_skipped: 0,
            },
            None,
        )
        .expect("commit progress");

    assert!(job_file.exists());
    assert!(
        !tmp_file.exists(),
        "JOB.tmp must not exist after successful commit"
    );

    let updated = mover
        .load_job("job-atomic")
        .expect("load job")
        .expect("job exists");
    assert_eq!(updated.counters.records_committed, 50);
}
