//! A real `kill -9` durability test.
//!
//! This is deliberately *not* a simulation. A child process (`wal_crash_child`)
//! writes committed transactions to a WAL as fast as it can, fsyncing every
//! commit and printing each txn id only after `append_commit` has returned.
//! The parent reads those ids, then kills the child with `SIGKILL` — no
//! unwinding, no destructors, no flush, the process simply stops, possibly
//! in the middle of a `write(2)`.
//!
//! The acceptance criterion is then checked against the resulting on-disk log:
//!
//! - (a) replay does not error, despite the near-certain torn tail,
//! - (b) **every txn id the child reported as committed is recovered**, and
//! - (c) no data from an uncommitted transaction is exposed.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use htap_rowstore::{Wal, WalRecord};

/// How many commits we want to see before pulling the trigger.
const TARGET_COMMITS: usize = 50;
/// Minimum number of commits for the run to be meaningful.
const MIN_COMMITS: usize = 10;
/// How long to wait for the child to reach `TARGET_COMMITS`.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How many times to retry a run that produced too few commits.
const MAX_ATTEMPTS: usize = 3;
/// `Put` records per transaction in the crash run.
///
/// Deliberately large: the child spends most of its time writing data records,
/// so a kill at an arbitrary instant very probably lands *between* a
/// transaction's puts and its commit marker. That is the case that
/// distinguishes correct recovery from a log that leaks uncommitted writes; a
/// narrow transaction would let the kill fall on a clean boundary almost every
/// time and check (c) below would pass vacuously.
const PUTS_PER_TXN: usize = 40;

/// Path to the `wal_crash_child` binary built alongside this test.
///
/// Cargo builds `[[bin]]` targets of the crate under test before running
/// integration tests and places them next to the test binary's directory, so
/// we locate it relative to the current executable rather than guessing a
/// profile name.
fn child_binary() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("test executable path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop(); // .../target/<profile>
    }
    let exe = format!("wal_crash_child{}", std::env::consts::EXE_SUFFIX);
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
        .arg(PUTS_PER_TXN.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning wal_crash_child");

    let stdout = child.stdout.take().expect("child stdout");

    // A helper thread accumulates every id the child prints. Collecting into
    // shared state rather than streaming over a channel matters: after the
    // kill we join the thread, which drains the pipe to EOF, so `reported`
    // ends up holding *everything* the child ever announced as committed —
    // including ids still sitting in the pipe when the kill landed. Those are
    // the most interesting ones, since they were fsynced closest to the crash.
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

    // Wait until the child has announced enough commits to make the run
    // meaningful.
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

    // Let the child run on past the last commit we saw so the kill lands at an
    // arbitrary point inside a transaction rather than always just after a
    // commit marker. The delay is drawn from the clock, so repeated runs probe
    // different offsets instead of replaying one fixed interleaving.
    let jitter_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 8_000)
        .unwrap_or(1_500);
    std::thread::sleep(Duration::from_micros(200 + jitter_us));

    // SIGKILL on Unix: the child gets no chance to flush, close or clean up.
    // Anything it already told us was committed must nevertheless be on disk.
    child.kill().expect("SIGKILL the child");
    let status = child.wait().expect("reaping the child");
    assert!(
        !status.success(),
        "child should have been killed, not exited cleanly: {status:?}"
    );

    // Joining reads the pipe to EOF, so nothing the child printed is missed.
    let _ = reader.join();
    let reported = std::mem::take(&mut *collected.lock().unwrap());

    CrashRun { reported }
}

#[test]
fn kill_9_loses_no_committed_data() {
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

        // (a) Replay must not error, even though the child was killed
        //     mid-write and the tail is very likely torn.
        let replay = Wal::replay(dir.path()).expect("replay after SIGKILL must not error");

        println!(
            "child reported {} committed txns; replay read {} records from {} segment(s), \
             truncated_at = {:?}",
            run.reported.len(),
            replay.records.len(),
            replay.segments_read,
            replay.truncated_at
        );

        let committed = replay.committed_records();
        let recovered_ids: HashSet<u64> = replay.committed_txn_ids();

        // (b) THE acceptance criterion: everything the child said was
        //     committed is still there after a kill -9.
        for id in &run.reported {
            assert!(
                recovered_ids.contains(id),
                "txn {id} was reported committed by the child but is missing after \
                 SIGKILL; reported {} txns, recovered {} (truncated_at = {:?})",
                run.reported.len(),
                recovered_ids.len(),
                replay.truncated_at
            );
        }

        // ...and its data records survived too, both of them, not just the
        // commit marker.
        let mut data_per_txn: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::new();
        for (_, rec) in &committed {
            assert!(
                rec.is_data(),
                "committed_records must strip markers: {rec:?}"
            );
            *data_per_txn.entry(rec.txn_id().unwrap()).or_default() += 1;
        }
        for id in &run.reported {
            assert_eq!(
                data_per_txn.get(id).copied().unwrap_or(0),
                PUTS_PER_TXN,
                "txn {id} committed but not all {PUTS_PER_TXN} of its Put records survived"
            );
        }

        // (c) Nothing uncommitted may be exposed. Every txn id appearing in
        //     committed_records() must have a commit marker in the log.
        let marker_ids: HashSet<u64> = replay
            .records
            .iter()
            .filter_map(|(_, r)| match r {
                WalRecord::Commit { txn_id, .. } => Some(*txn_id),
                _ => None,
            })
            .collect();
        for (_, rec) in &committed {
            let id = rec.txn_id().unwrap();
            assert!(
                marker_ids.contains(&id),
                "txn {id} has no commit record but its data was exposed"
            );
        }

        // The child writes txn ids 1, 2, 3, ...; the recovered set must be a
        // superset of what it reported and must not invent ids beyond the last
        // one it could possibly have started.
        let highest_reported = *run.reported.iter().max().unwrap();
        for id in &recovered_ids {
            assert!(
                *id >= 1 && *id <= highest_reported + 1,
                "recovered implausible txn id {id} (child reached {highest_reported})"
            );
        }

        // At most one extra committed txn beyond what we read: the child can
        // commit one more between our final read and the kill landing.
        assert!(
            recovered_ids.len() >= run.reported.len(),
            "recovered fewer committed txns ({}) than reported ({})",
            recovered_ids.len(),
            run.reported.len()
        );

        // Evidence that the kill actually landed mid-transaction: data records
        // that replay read but recovery refused to expose, because their
        // transaction never got a commit marker onto disk.
        let data_records = replay.records.iter().filter(|(_, r)| r.is_data()).count();
        let dropped = data_records - committed.len();
        println!(
            "all {} reported commits recovered after SIGKILL (recovered set size {}); \
             {dropped} data record(s) from the interrupted transaction were correctly dropped",
            run.reported.len(),
            recovered_ids.len()
        );
        return;
    }

    panic!(
        "child never produced at least {MIN_COMMITS} commits in {MAX_ATTEMPTS} attempts \
         (last run: {last_count}); the environment may be too slow or fsync may be failing"
    );
}

#[test]
fn a_cleanly_exiting_child_recovers_every_commit() {
    // Control case: no kill at all. Establishes that the child and the replay
    // agree when nothing goes wrong, so a failure of the crash test above
    // points at crash handling rather than at the harness.
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(child_binary())
        .arg(dir.path())
        .arg("25")
        .arg("2")
        .stdout(Stdio::piped())
        .output()
        .expect("running wal_crash_child to completion");
    assert!(out.status.success(), "child failed: {out:?}");

    let reported: Vec<u64> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(reported, (1..=25).collect::<Vec<u64>>());

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.truncated_at, None, "clean exit leaves no torn tail");
    assert_eq!(replay.records.len(), 25 * 3);
    assert_eq!(replay.committed_records().len(), 25 * 2);
    assert_eq!(replay.committed_txn_ids().len(), 25);
}
