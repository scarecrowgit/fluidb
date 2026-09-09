//! A real `kill -9` crash test for the LSM `Engine`.
//!
//! A child process (`engine_crash_child`) writes committed transactions through
//! the full `Engine` pipeline (active memtable, WAL, SST flush, MANIFEST).
//! The child calls `flush()` periodically so that some committed data is
//! published into SST files while newer data remains only in the WAL and memtable.
//!
//! The parent reads transaction IDs reported by the child as committed, then
//! abruptly terminates the child with `SIGKILL`.
//!
//! Upon reopening the `Engine`:
//! - (a) `Engine::open` succeeds and replays the WAL without error,
//! - (b) every transaction reported as committed is recovered with its exact row,
//! - (c) both SST-published data and WAL-replayed data are verified, and
//! - (d) an intentionally never-committed row is absent.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use htap_common::{Row, Value};
use htap_rowstore::{Engine, EngineOptions};

/// How many commits we want to see before pulling the trigger.
const TARGET_COMMITS: usize = 35;
/// Minimum number of commits for the run to be meaningful.
const MIN_COMMITS: usize = 10;
/// How long to wait for the child to reach `TARGET_COMMITS`.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How many times to retry a run that produced too few commits.
const MAX_ATTEMPTS: usize = 3;
/// Flush the active memtable every N commits to exercise both SSTs and WAL.
const FLUSH_EVERY: u64 = 5;

/// Path to the `engine_crash_child` binary built alongside this test.
fn child_binary() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("test executable path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop(); // .../target/<profile>
    }
    let exe = format!("engine_crash_child{}", std::env::consts::EXE_SUFFIX);
    let candidate = dir.join(&exe);
    assert!(
        candidate.is_file(),
        "child binary not found at {}; it should have been built by cargo \
         as a [[bin]] target of htap-rowstore",
        candidate.display()
    );
    candidate
}

/// Outcome of one crash run.
struct CrashRun {
    /// Txn ids the child printed, i.e. reported durably committed.
    reported: Vec<u64>,
}

/// Spawn the child, collect at least `TARGET_COMMITS` reported commits, then
/// SIGKILL it.
fn run_until_kill(dir: &std::path::Path) -> CrashRun {
    let mut child: Child = Command::new(child_binary())
        .arg(dir)
        .arg("0") // 0 = loop forever until killed
        .arg(FLUSH_EVERY.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning engine_crash_child");

    let stdout = child.stdout.take().expect("child stdout");

    let collected: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            match line.parse::<u64>() {
                Ok(id) => sink.lock().unwrap().push(id),
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + COMMIT_TIMEOUT;
    loop {
        if collected.lock().unwrap().len() >= TARGET_COMMITS {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break; // Child died on its own.
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    // Small jitter delay so the kill lands at an arbitrary point in child execution.
    let jitter_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 8_000)
        .unwrap_or(1_500);
    std::thread::sleep(Duration::from_micros(200 + jitter_us));

    // Abrupt SIGKILL.
    child.kill().expect("SIGKILL the child");
    let status = child.wait().expect("reaping the child");
    assert!(
        !status.success(),
        "child should have been killed, not exited cleanly: {status:?}"
    );

    let _ = reader.join();
    let reported = std::mem::take(&mut *collected.lock().unwrap());

    CrashRun { reported }
}

#[test]
fn engine_kill_9_recovers_all_reported_commits() {
    let mut last_count = 0usize;
    for attempt in 1..=MAX_ATTEMPTS {
        let dir = tempfile::tempdir().unwrap();
        let run = run_until_kill(dir.path());
        last_count = run.reported.len();

        if run.reported.len() < MIN_COMMITS {
            eprintln!(
                "attempt {attempt}/{MAX_ATTEMPTS}: child reported only {} commits \
                 (< {MIN_COMMITS}); retrying",
                run.reported.len()
            );
            continue;
        }

        // Verify that flush occurred and SST files were written to disk
        let sst_dir = dir.path().join("sst");
        let sst_count = std::fs::read_dir(&sst_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "sst"))
            .count();
        assert!(
            sst_count >= 1,
            "child should have flushed at least one SST file (FLUSH_EVERY={FLUSH_EVERY}, reported={})" ,
            run.reported.len()
        );

        // (a) Reopening Engine after SIGKILL must succeed without error.
        let options = EngineOptions::new(dir.path());
        let engine = Engine::open(options).expect("Engine::open after SIGKILL must not error");
        let snapshot = engine.snapshot();

        // (b) Every transaction the child reported as committed must be present with its exact row.
        for id in &run.reported {
            let key = format!("txn-key-{id}").into_bytes();
            let expected_row = Row::new(vec![
                Value::Int64(*id as i64),
                Value::String(format!("engine-val-{id}")),
            ]);
            let actual = engine
                .get(0, &key, snapshot)
                .unwrap_or_else(|e| panic!("engine.get failed for key {key:?}: {e}"));
            assert_eq!(
                actual,
                Some(expected_row),
                "txn {id} reported committed but row is missing or wrong after SIGKILL"
            );
        }

        // (c) Verify an intentionally never-committed row is absent.
        let highest_reported = *run.reported.iter().max().unwrap();
        let never_committed_key = format!("txn-key-{}", highest_reported + 10_000).into_bytes();
        assert_eq!(
            engine.get(0, &never_committed_key, snapshot).unwrap(),
            None,
            "never-committed transaction key must be absent"
        );
        assert_eq!(
            engine.get(0, b"never-committed-key", snapshot).unwrap(),
            None,
            "never-committed key must be absent"
        );

        println!(
            "Engine SIGKILL test passed: {} reported commits recovered across {} SSTs \
             and active memtable/WAL replay",
            run.reported.len(),
            sst_count
        );
        return;
    }

    panic!(
        "child never produced at least {MIN_COMMITS} commits in {MAX_ATTEMPTS} attempts \
         (last run: {last_count})"
    );
}

#[test]
fn engine_cleanly_exiting_child_recovers_every_commit() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(child_binary())
        .arg(dir.path())
        .arg("25")
        .arg("5")
        .stdout(Stdio::piped())
        .output()
        .expect("running engine_crash_child to completion");
    assert!(out.status.success(), "child failed: {out:?}");

    let reported: Vec<u64> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(reported, (1..=25).collect::<Vec<u64>>());

    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    let snapshot = engine.snapshot();

    for id in &reported {
        let key = format!("txn-key-{id}").into_bytes();
        let expected_row = Row::new(vec![
            Value::Int64(*id as i64),
            Value::String(format!("engine-val-{id}")),
        ]);
        assert_eq!(engine.get(0, &key, snapshot).unwrap(), Some(expected_row));
    }

    assert_eq!(engine.get(0, b"txn-key-99999", snapshot).unwrap(), None);
}
