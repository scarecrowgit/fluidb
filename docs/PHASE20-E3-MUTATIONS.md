# E3: real-source mutation evidence (Phase 20 Batch E, task 7)

- Commit: `b40aae10e690e9a3d4f8b62fe27e1767c625fbf2` (scratch worktree `$E/wt`, detached; main tree untouched)
- Date: Sun 4 Oct 13:14:26 UTC 2026 (start), runs finished 13:30 UTC
- rustc 1.95.0 (59807616e 2026-04-14), cargo 1.95.0 (f2d3ce0bd 2026-03-21)
- Environment for every cargo command: cwd `$WT=/mnt/storage/e3-scratch/wt`, `CARGO_TARGET_DIR=$E/target`, `CARGO_INCREMENTAL=0`,
  `TMPDIR=$E/tmp`. TMPDIR was set so the crash-image tempdirs also land on /mnt/storage, since /tmp was nearly full; this deviates from the brief only in that respect.
  No `POWERLOSS_*` env var was set, so the harness used its default (non-exhaustive) policy.
- One prebuild ran before M0: `cargo test -p <8 crates> --tests --no-run`, rc=0, 90 s, log `logs/M0_prebuild.log`.
- Disk: free space was 22 GB at start and 7.3 GB at the end of the runs. The ABORT threshold (6 GB) was never reached. Most of the drop came from
  outside the scratch dir (`$E/target` reached about 13 GB).
- Mutation authorship: each mutation was a single SEARCH/REPLACE block from `cx/gpt-5.6-sol` (mcp__9router__ask, target file attached,
  behavioral one-sentence description), applied verbatim with Edit. The hook denied nothing. After each mutation: `git -C $WT reset --hard HEAD`,
  then a check that `git -C $WT status --porcelain` was empty (it was, every time).
- Classification: **KILL** means at least one non-macro test's final panic message is a `POWERLOSS_REPRO=` line. **OTHER-FAIL** means a non-macro failure with any other
  message. Macro tests (`control_*`, `witness_*`, `survivor_*`) are listed separately (marked M) and do not drive the suite class. For the FAULT suites,
  the designed oracle message ("…silently/unexpectedly succeeded…") is counted as **KILL (fault oracle)**. Those suites emit no REPRO by design.
- Total wall time, from worktree creation to the end of the extra twin runs: about 18.5 min. Sum of per-suite run times: see each section.
- Raw logs: `$E/logs/` (`<M>_<crate>_<bin>.log`, `runs.tsv`, `agg.json`, `M<n>.diff`, `X_twin_*.log`).

## Summary

| M | mutation | OLD (kill-9) 4 suites | NEW suites: KILL / PASS / other | FAULT | matches expectation? |
|---|---|---|---|---|---|
| M0 | none | 4/4 PASS | 18 run: 0 / 18 / 0 | 2/2 PASS | yes, all pass |
| M1 | DurFile file fsync no-op | 4/4 PASS | 18 run: **18** / 0 / 0 | not run | yes |
| M2 | sync_dir no-op | 4/4 PASS | 18 run: **18** / 0 / 0 (4 non-macro OTHER-FAIL tests inside killed suites, listed below) | not run | yes |
| M3 | drop `wal:append_sync` | 4/4 PASS | 7 run: **6** / 1 (wal_tail) / 0 | not run | yes; twin-identical |
| M4 | drop `wal:roll_dir_sync` | 4/4 PASS | 7 run: **5** / 2 (wal_segment, wal_tail) / 0 | not run | yes; twin-identical |
| M5 | drop `engine:flush_sst_dir_sync` | 4/4 PASS | 7 run: **1** (engine) / 6 / 0 | not run | yes; twin-identical |
| M6 | sync_package_dir_chain swallows errors | not run | 5 movement powerloss: 0 / 5 / 0 | **2/2 KILL** (fault oracle) | yes |

Across all mutations, no run hit a timeout, a compile error, a disk error, or a non-harness panic in an OLD suite. The four kill-9 suites passed under every mutation M1-M5: they cannot see a missing fsync.

## Twin comparison

### M3/M4/M5 vs the per-site SkipSync kill sets (C5 discovery, rows wal:append_sync, wal:roll_dir_sync, engine:flush_sst_dir_sync)

Suites with at least one non-macro KILL:

| site | C5 twin suites (from the discovery table) | mutation suites (this run) | discrepancy |
|---|---|---|---|
| wal:append_sync (M3) | wal, wal_open, wal_segment, engine, engine_open (13 tests) | wal, wal_open, wal_segment, engine, engine_open, **txn** | **powerloss_txn**: M3 kills it, but the C5 table lists no txn killer |
| wal:roll_dir_sync (M4) | wal, wal_open, engine, engine_open (12 tests) | wal, wal_open, engine, engine_open, **txn** | **powerloss_txn**: M4 kills it, but the C5 table lists no txn killer |
| engine:flush_sst_dir_sync (M5) | engine (4 tests) | engine | none |

Test-level detail:
- **M3.** The rowstore killed tests match the C5 13 exactly:
  - powerloss_wal: wal_fresh_dir_entry_durable, wal_acked_commits_survive_strict, wal_acked_commits_survive_torn, wal_segment_roll_and_gc_survive, wal_recovery_repair_crash_depth1
  - wal_open: wal_open_syncs_preexisting_volatile_dir
  - wal_segment: wal_open_syncs_adopted_unsynced_segment
  - engine: engine_fresh_open_dirs_durable, engine_visible_marker_monotonic, engine_commit_flush_compact_prefix_consistent, engine_wal_gc_after_flush_survives, engine_flush_boundary_crash_depth1
  - engine_open: engine_open_syncs_preexisting_volatile_dir
  - The C5 witness repro `wal_acked_commits_survive_strict/strict/0/k=9` is reproduced exactly.
  - Extra under M3: six powerloss_txn tests (txn_recover_after_every_boundary, txn_preexisting_volatile_journal_dir, txn_fresh_journal_entry_durable, txn_commits_survive_journal_and_wal, txn_checkpoint_rewrite_atomic, txn_repair_torn_final_crash_depth1).
- **M4.** The killed rowstore tests are the same 12 C5 lists: the 5 wal tests, wal_open, the 5 engine tests and engine_open. powerloss_wal_segment and powerloss_wal_tail pass, as in C5. The C5 witness repro `wal_fresh_dir_entry_durable/strict/0/k=9` is reproduced exactly. Extra under M4: the same six powerloss_txn tests.
- **M5.** The killed tests are exactly the C5 four: engine_commit_flush_compact_prefix_consistent, engine_visible_marker_monotonic, engine_wal_gc_after_flush_survives and engine_flush_boundary_crash_depth1. engine_fresh_open_dirs_durable passes. The C5 witness repro `engine_commit_flush_compact_prefix_consistent/strict/0/k=46` is reproduced exactly.

**The powerloss_txn discrepancy, stated as found.** For wal:append_sync and wal:roll_dir_sync the C5 discovery table lists only rowstore tests as killers, while the real-source mutations M3 and M4 also kill all 6 non-macro powerloss_txn tests. The discovery file does not record whether those two sites were run against powerloss_txn; its "cross-suite runs" note covers survivor ids only.

To settle it, I ran three extra twin runs that the brief did not require. Each used the unmutated worktree at the same HEAD:
`POWERLOSS_SKIP_SYNC=site:<id> timeout -k 30 900 cargo test -p htap-txn --test powerloss_txn --no-fail-fast` (logs `logs/X_twin_*`).

| extra twin run | exit | test result | killed tests |
|---|---|---|---|
| site:wal:append_sync | 101 | `test result: FAILED. 8 passed; 6 failed; ... finished in 22.65s` | the same 6 txn tests, with the same REPROs as M3 (k=35/37/38/38/38, `txn_repair_torn_final_crash_depth1/torn-s=512/9/k=28/rk=23`) |
| site:wal:roll_dir_sync | 101 | `test result: FAILED. 8 passed; 6 failed; ... finished in 22.63s` | the same 6 txn tests, with the same REPROs as M4 |
| site:engine:flush_sst_dir_sync | 0 | `test result: ok. 14 passed; ... finished in 193.25s` | none (matches M5) |

So the live SkipSync twin and the real mutation agree suite-for-suite and repro-for-repro. The discrepancy is between the mutation and the C5 discovery table. That table omits powerloss_txn as a killer of wal:append_sync and wal:roll_dir_sync.

### M1 vs `control_file`, M2 vs `control_dir`

- **M1 vs control_file.** 18 NEW suites contain a `control_file` test that passed at M0, and M1 kills all 18 (KILL via non-macro REPRO).
  - Macro side: `control_file` itself fails ("control_file: site never exercised") in 16 of the 18. It still passes in powerloss_convert and powerloss_server_recovery.
  - 37 macro tests fail under M1 in total, all listed per suite below.
- **M2 vs control_dir.** 18 NEW suites contain a `control_dir` test that passed at M0, and M2 kills all 18.
  - Macro side: `control_dir` fails ("site never exercised") in all 18.
  - 53 macro tests fail under M2 in total.
  - Non-macro OTHER-FAIL tests inside killed suites: delete_artifacts_no_resurrection ("expected a pre-delete crash image containing the durable artifact"), clone_publish_has_data_dir_barrier, server_open_syncs_colstore_entry_before_ack and server_reopen_syncs_volatile_root_children. These are structural / non-vacuity oracles, the same set C5 recorded as OTHER under the `dir` spec. One difference: C5 O5 had delete_artifacts_no_resurrection passing vacuously, and it now fails on its non-vacuity guard.
- **First-kill crash points.** They match the C5 `file`/`dir` spec table wherever C5 gives a k:
  - wal: 9 / 6
  - wal_open: 8 / 6
  - wal_segment: 8 / 7
  - wal_tail: 18 / 11
  - engine: 25 / 19
  - engine_open: 24 / 19
  - segment: 7 / 7
  - import: 41 / 48
  - repair: 29 / 60
  - server_recovery conversion: 40 / 25

## M0: unmutated baseline (OLD + NEW-ALL + FAULT)

Sum of per-suite wall times: 207 s (24 suites).

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 8 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 2 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.69s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 10 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 37 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.61s` |
| htap-rowstore::powerloss_wal | NEW | 0 | PASS | 10 | `test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.42s` |
| htap-rowstore::powerloss_wal_open | NEW | 0 | PASS | 2 | `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.47s` |
| htap-rowstore::powerloss_wal_segment | NEW | 0 | PASS | 1 | `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.34s` |
| htap-rowstore::powerloss_wal_tail | NEW | 0 | PASS | 4 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.89s` |
| htap-rowstore::powerloss_engine | NEW | 0 | PASS | 7 | `test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.57s` |
| htap-rowstore::powerloss_engine_open | NEW | 0 | PASS | 4 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.45s` |
| htap-txn::powerloss_txn | NEW | 0 | PASS | 15 | `test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 11.97s` |
| htap-catalog::powerloss_catalog | NEW | 0 | PASS | 3 | `test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.37s` |
| htap-coord::powerloss_coord | NEW | 0 | PASS | 4 | `test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.75s` |
| htap-colstore::powerloss_segment | NEW | 0 | PASS | 2 | `test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s` |
| htap-convert::powerloss_convert | NEW | 0 | PASS | 2 | `test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s` |
| htap-movement::powerloss_movement | NEW | 0 | PASS | 2 | `test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.63s` |
| htap-movement::powerloss_movement_clone | NEW | 0 | PASS | 3 | `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.73s` |
| htap-movement::powerloss_movement_export | NEW | 0 | PASS | 1 | `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s` |
| htap-movement::powerloss_movement_import | NEW | 0 | PASS | 3 | `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.10s` |
| htap-movement::powerloss_movement_repair | NEW | 0 | PASS | 2 | `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.35s` |
| htap-server::powerloss_server | NEW | 0 | PASS | 45 | `test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.06s` |
| htap-server::powerloss_server_recovery | NEW | 0 | PASS | 36 | `test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 31.94s` |
| htap-movement::sync_fault_clone | FAULT | 0 | PASS | 2 | `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.12s` |
| htap-movement::sync_fault_repair | FAULT | 0 | PASS | 2 | `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.90s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M0_<crate>_<bin>.log`.

## M1: dur.rs DurFile::sync_all_inner / sync_data_inner return Ok(()) immediately (OLD + NEW-ALL)

Sum of per-suite wall times: 82 s (22 suites).

Diff (verbatim, `logs/M1.diff`):

```diff
diff --git a/crates/htap-common/src/fs/dur.rs b/crates/htap-common/src/fs/dur.rs
index 064a1d2..545b641 100644
--- a/crates/htap-common/src/fs/dur.rs
+++ b/crates/htap-common/src/fs/dur.rs
@@ -102,15 +102,7 @@ impl DurFile {
 
     #[inline]
     fn sync_all_inner(&self, _site: Option<&'static str>) -> io::Result<()> {
-        #[cfg(feature = "crashsim")]
-        if let Some(handle) = &self.recording {
-            recording::pre_sync_file(handle, true, _site)?;
-        }
-        self.file.sync_all()?;
-        #[cfg(feature = "crashsim")]
-        if let Some(handle) = &self.recording {
-            recording::sync_file(handle, true, _site);
-        }
+        let _ = (self, _site);
         Ok(())
     }
 
@@ -128,15 +120,7 @@ impl DurFile {
 
     #[inline]
     fn sync_data_inner(&self, _site: Option<&'static str>) -> io::Result<()> {
-        #[cfg(feature = "crashsim")]
-        if let Some(handle) = &self.recording {
-            recording::pre_sync_file(handle, false, _site)?;
-        }
-        self.file.sync_data()?;
-        #[cfg(feature = "crashsim")]
-        if let Some(handle) = &self.recording {
-            recording::sync_file(handle, false, _site);
-        }
+        let _ = (self, _site);
         Ok(())
     }
 
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 3 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 1 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 5 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 21 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.35s` |
| htap-rowstore::powerloss_wal | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 9 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s` |
| htap-rowstore::powerloss_wal_open | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s` |
| htap-rowstore::powerloss_wal_segment | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s` |
| htap-rowstore::powerloss_wal_tail | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.51s` |
| htap-rowstore::powerloss_engine | NEW | 101 | KILL | 3 | `test result: FAILED. 3 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.08s` |
| htap-rowstore::powerloss_engine_open | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.01s` |
| htap-txn::powerloss_txn | NEW | 101 | KILL | 6 | `test result: FAILED. 3 passed; 11 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.59s` |
| htap-catalog::powerloss_catalog | NEW | 101 | KILL | 3 | `test result: FAILED. 3 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s` |
| htap-coord::powerloss_coord | NEW | 101 | KILL | 2 | `test result: FAILED. 3 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.75s` |
| htap-colstore::powerloss_segment | NEW | 101 | KILL | 2 | `test result: FAILED. 3 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s` |
| htap-convert::powerloss_convert | NEW | 101 | KILL | 2 | `test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s` |
| htap-movement::powerloss_movement | NEW | 101 | KILL | 2 | `test result: FAILED. 5 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s` |
| htap-movement::powerloss_movement_clone | NEW | 101 | KILL | 2 | `test result: FAILED. 2 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.92s` |
| htap-movement::powerloss_movement_export | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s` |
| htap-movement::powerloss_movement_import | NEW | 101 | KILL | 2 | `test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.71s` |
| htap-movement::powerloss_movement_repair | NEW | 101 | KILL | 2 | `test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s` |
| htap-server::powerloss_server | NEW | 101 | KILL | 9 | `test result: FAILED. 5 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.08s` |
| htap-server::powerloss_server_recovery | NEW | 101 | KILL | 11 | `test result: FAILED. 4 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.36s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M1_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-rowstore::powerloss_wal** (KILL)
  - wal_fresh_dir_entry_durable: KILL: `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
  - wal_segment_roll_and_gc_survive: KILL: `POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - wal_acked_commits_survive_strict: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_strict/strict/0/k=9`
  - wal_acked_commits_survive_torn: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_torn/torn-s=4096/17/k=9`
  - wal_recovery_repair_crash_depth1: KILL: `POWERLOSS_REPRO=wal_recovery_repair_crash_depth1/torn-s=4096/17/k=9/rk=0`
  - M witness_wal_append_sync: `witness_wal_append_sync: site never exercised`
  - M witness_wal_roll_sync: `witness_wal_roll_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - M survivor_wal_gc_sync: `survivor_wal_gc_sync: POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
- **htap-rowstore::powerloss_wal_open** (KILL)
  - wal_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
- **htap-rowstore::powerloss_wal_segment** (KILL)
  - wal_open_syncs_adopted_unsynced_segment: KILL: `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=8`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=8`
- **htap-rowstore::powerloss_wal_tail** (KILL)
  - adopted_unsynced_wal_tail_is_synced_before_publish: KILL: `POWERLOSS_REPRO=adopted_unsynced_wal_tail_is_synced_before_publish/strict/0/k=18`
  - M control_file: `control_file: site never exercised`
  - M witness_wal_open_adopt_sync: `witness_wal_open_adopt_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=adopted_unsynced_wal_tail_is_synced_before_publish/strict/0/k=18`
- **htap-rowstore::powerloss_engine** (KILL)
  - engine_fresh_open_dirs_durable: KILL: `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=25`
  - engine_visible_marker_monotonic: KILL: `POWERLOSS_REPRO=engine_visible_marker_monotonic/strict/0/k=25`
  - engine_commit_flush_compact_prefix_consistent: KILL: `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=25`
  - engine_wal_gc_after_flush_survives: KILL: `POWERLOSS_REPRO=engine_wal_gc_after_flush_survives/strict/0/k=25`
  - engine_flush_boundary_crash_depth1: KILL: `POWERLOSS_REPRO=engine_flush_boundary_crash_depth1/strict/0/k=25/rk=recover`
  - M witness_sst_write_sync: `witness_sst_write_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=25`
- **htap-rowstore::powerloss_engine_open** (KILL)
  - engine_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=24`
  - M survivor_engine_open_dir_sync: `survivor_engine_open_dir_sync: POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=23`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=24`
- **htap-txn::powerloss_txn** (KILL)
  - txn_recover_after_every_boundary: KILL: `POWERLOSS_REPRO=txn_recover_after_every_boundary/strict/0/k=29`
  - txn_preexisting_volatile_journal_dir: KILL: `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=31`
  - txn_fresh_journal_entry_durable: KILL: `POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=32`
  - txn_commits_survive_journal_and_wal: KILL: `POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=32`
  - txn_checkpoint_rewrite_atomic: KILL: `POWERLOSS_REPRO=txn_checkpoint_rewrite_atomic/strict/0/k=32`
  - txn_repair_torn_final_crash_depth1: KILL: `POWERLOSS_REPRO=txn_repair_torn_final_crash_depth1/torn-s=512/9/k=25/rk=20`
  - M control_file: `control_file: site never exercised`
  - M survivor_txn_journal_open_sync: `survivor_txn_journal_open_sync: POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=32`
  - M survivor_txn_journal_append_sync: `survivor_txn_journal_append_sync: POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=32`
  - M witness_write_new_tmp_file_sync: `witness_write_new_tmp_file_sync: site never exercised`
  - M witness_txn_journal_sync: `witness_txn_journal_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=txn_recover_after_every_boundary/strict/0/k=29`
- **htap-catalog::powerloss_catalog** (KILL)
  - catalog_fresh_dir_durable: KILL: `POWERLOSS_REPRO=catalog_fresh_dir_durable/strict/0/k=8`
  - catalog_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=catalog_preexisting_volatile_dir/strict/0/k=7`
  - catalog_cas_sequence_prefix_consistent: KILL: `POWERLOSS_REPRO=catalog_cas_sequence_prefix_consistent/strict/0/k=8`
  - M survivor_catalog_open_dir_sync: `survivor_catalog_open_dir_sync: POWERLOSS_REPRO=catalog_fresh_dir_durable/strict/0/k=7`
  - M witness_write_new_tmp_file_sync: `witness_write_new_tmp_file_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=catalog_fresh_dir_durable/strict/0/k=8`
- **htap-coord::powerloss_coord** (KILL)
  - coord_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=coord_preexisting_volatile_dir/strict/0/k=14`
  - coord_fresh_dir_durable: KILL: `POWERLOSS_REPRO=coord_fresh_dir_durable/strict/0/k=15`
  - coord_state_atomic: KILL: `POWERLOSS_REPRO=coord_state_atomic/strict/0/k=15`
  - coord_fencing_token_never_regresses: KILL: `POWERLOSS_REPRO=coord_fencing_token_never_regresses/strict/0/k=15`
  - M survivor_coord_open_dir_sync: `survivor_coord_open_dir_sync: POWERLOSS_REPRO=coord_fresh_dir_durable/strict/0/k=14`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=coord_preexisting_volatile_dir/strict/0/k=14`
- **htap-colstore::powerloss_segment** (KILL)
  - segment_torn_write_never_decodes_silently: KILL: `POWERLOSS_REPRO=segment_torn_write_never_decodes_silently/strict/0/k=7`
  - M witness_colstore_segment_write_sync: `witness_colstore_segment_write_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=segment_torn_write_never_decodes_silently/strict/0/k=7`
- **htap-convert::powerloss_convert** (KILL)
  - convert_preexisting_volatile_root: KILL: `POWERLOSS_REPRO=convert_preexisting_volatile_root/strict/0/k=19`
  - convert_fresh_dir_durable: KILL: `POWERLOSS_REPRO=convert_fresh_dir_durable/strict/0/k=20`
  - convert_resumes_after_incomplete_boundary: KILL: `POWERLOSS_REPRO=convert_resumes_after_incomplete_boundary/strict/0/k=21`
  - convert_manifest_old_or_new_and_segments_valid: KILL: `POWERLOSS_REPRO=convert_manifest_old_or_new_and_segments_valid/strict/0/k=20`
  - M survivor_fsync_file_sync: `survivor_fsync_file_sync: POWERLOSS_REPRO=convert_fresh_dir_durable/strict/0/k=19`
  - REPRO (verbatim): `POWERLOSS_REPRO=convert_preexisting_volatile_root/strict/0/k=19`
- **htap-movement::powerloss_movement** (KILL)
  - movement_preexisting_volatile_job_dir: KILL: `POWERLOSS_REPRO=movement_preexisting_volatile_job_dir/strict/0/k=12`
  - movement_fresh_dir_durable: KILL: `POWERLOSS_REPRO=movement_fresh_dir_durable/strict/0/k=14`
  - job_state_monotonic_no_regression: KILL: `POWERLOSS_REPRO=job_state_monotonic_no_regression/strict/0/k=14`
  - M survivor_movement_open_dir_sync: `survivor_movement_open_dir_sync: POWERLOSS_REPRO=movement_preexisting_volatile_job_dir/strict/0/k=11`
  - M witness_write_new_tmp_file_sync: `witness_write_new_tmp_file_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=movement_preexisting_volatile_job_dir/strict/0/k=12`
- **htap-movement::powerloss_movement_clone** (KILL)
  - clone_retry_completes_interrupted_job: KILL: `POWERLOSS_REPRO=clone_retry_completes_interrupted_job/strict/0/k=76`
  - clone_survives_fresh_tablet_dirs: KILL: `POWERLOSS_REPRO=clone_survives_fresh_tablet_dirs/strict/0/k=68`
  - tablet_package_rename_order_chaos: KILL: `POWERLOSS_REPRO=tablet_package_rename_order_chaos/chaos/42/k=68`
  - M control_file: `control_file: site never exercised`
  - M survivor_movement_package_data_dir_sync: `survivor_movement_package_data_dir_sync: POWERLOSS_REPRO=clone_survives_fresh_tablet_dirs/strict/0/k=67`
  - REPRO (verbatim): `POWERLOSS_REPRO=clone_retry_completes_interrupted_job/strict/0/k=76`
- **htap-movement::powerloss_movement_export** (KILL)
  - export_file_complete_or_absent: KILL: `POWERLOSS_REPRO=export_file_complete_or_absent/strict/0/k=59`
  - export_to_fresh_nested_destination: KILL: `POWERLOSS_REPRO=export_to_fresh_nested_destination/strict/0/k=61`
  - export_preexisting_volatile_destination: KILL: `POWERLOSS_REPRO=export_preexisting_volatile_destination/strict/0/k=67`
  - M witness_movement_export_write_sync: `witness_movement_export_write_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=export_file_complete_or_absent/strict/0/k=59`
- **htap-movement::powerloss_movement_import** (KILL)
  - import_only_on_fresh_root_keeps_complete_job: KILL: `POWERLOSS_REPRO=import_only_on_fresh_root_keeps_complete_job/strict/0/k=41`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=import_only_on_fresh_root_keeps_complete_job/strict/0/k=41`
- **htap-movement::powerloss_movement_repair** (KILL)
  - repair_after_unsynced_publish_requires_durable_package: KILL: `POWERLOSS_REPRO=repair_after_unsynced_publish_requires_durable_package/strict/0/k=29`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=repair_after_unsynced_publish_requires_durable_package/strict/0/k=29`
- **htap-server::powerloss_server** (KILL)
  - server_preexisting_volatile_root_durable: KILL: `POWERLOSS_REPRO=srv_vol_1/strict/0/k=44`
  - server_fresh_root_bootstrap_durable: KILL: `POWERLOSS_REPRO=srv_fresh_1/strict/0/k=45`
  - server_acked_sql_survives_power_loss: KILL: `POWERLOSS_REPRO=srv_sql_ack/strict/0/k=40`
  - M witness_sync_ancestors_sync: `witness_sync_ancestors_sync: site never exercised`
  - M survivor_server_open_parent_sync: `survivor_server_open_parent_sync: POWERLOSS_REPRO=srv_fresh_1/strict/0/k=44`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=srv_vol_1/strict/0/k=44`
- **htap-server::powerloss_server_recovery** (KILL)
  - server_conversion_and_reclaim_survive: KILL: `POWERLOSS_REPRO=srv_conv_reclaim/strict/0/k=40`
  - server_recovery_crash_depth1: KILL: `POWERLOSS_REPRO=srv_recover_d1/strict/0/k=40/rk=recover`
  - REPRO (verbatim): `POWERLOSS_REPRO=srv_conv_reclaim/strict/0/k=40`

## M2: dur.rs sync_dir_site_inner returns Ok(()) immediately (OLD + NEW-ALL)

Sum of per-suite wall times: 65 s (22 suites).

Diff (verbatim, `logs/M2.diff`):

```diff
diff --git a/crates/htap-common/src/fs/dur.rs b/crates/htap-common/src/fs/dur.rs
index 064a1d2..98669b2 100644
--- a/crates/htap-common/src/fs/dur.rs
+++ b/crates/htap-common/src/fs/dur.rs
@@ -470,19 +470,7 @@ pub fn sync_dir_site(path: impl AsRef<Path>, site: &'static str) -> io::Result<(
 
 #[inline]
 fn sync_dir_site_inner(path: &Path, _site: Option<&'static str>) -> io::Result<()> {
-    #[cfg(unix)]
-    {
-        let file = File::open(path)?;
-        #[cfg(feature = "crashsim")]
-        recording::pre_sync_dir(path, _site)?;
-        file.sync_all()?;
-        #[cfg(feature = "crashsim")]
-        recording::sync_dir(path, _site);
-    }
-
-    #[cfg(not(unix))]
     let _ = (path, _site);
-
     Ok(())
 }
 
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 3 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 1 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.44s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 5 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 21 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s` |
| htap-rowstore::powerloss_wal | NEW | 101 | KILL | 1 | `test result: FAILED. 3 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s` |
| htap-rowstore::powerloss_wal_open | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s` |
| htap-rowstore::powerloss_wal_segment | NEW | 101 | KILL | 0 | `test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s` |
| htap-rowstore::powerloss_wal_tail | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s` |
| htap-rowstore::powerloss_engine | NEW | 101 | KILL | 2 | `test result: FAILED. 2 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s` |
| htap-rowstore::powerloss_engine_open | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s` |
| htap-txn::powerloss_txn | NEW | 101 | KILL | 3 | `test result: FAILED. 3 passed; 11 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s` |
| htap-catalog::powerloss_catalog | NEW | 101 | KILL | 3 | `test result: FAILED. 2 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s` |
| htap-coord::powerloss_coord | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.29s` |
| htap-colstore::powerloss_segment | NEW | 101 | KILL | 2 | `test result: FAILED. 2 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s` |
| htap-convert::powerloss_convert | NEW | 101 | KILL | 3 | `test result: FAILED. 1 passed; 9 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s` |
| htap-movement::powerloss_movement | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 9 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s` |
| htap-movement::powerloss_movement_clone | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s` |
| htap-movement::powerloss_movement_export | NEW | 101 | KILL | 2 | `test result: FAILED. 2 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s` |
| htap-movement::powerloss_movement_import | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s` |
| htap-movement::powerloss_movement_repair | NEW | 101 | KILL | 1 | `test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.17s` |
| htap-server::powerloss_server | NEW | 101 | KILL | 6 | `test result: FAILED. 2 passed; 9 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.51s` |
| htap-server::powerloss_server_recovery | NEW | 101 | KILL | 5 | `test result: FAILED. 1 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.52s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M2_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-rowstore::powerloss_wal** (KILL)
  - wal_fresh_dir_entry_durable: KILL: `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=6`
  - wal_acked_commits_survive_strict: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_strict/strict/0/k=6`
  - wal_acked_commits_survive_torn: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_torn/torn-s=4096/17/k=6`
  - wal_recovery_repair_crash_depth1: KILL: `POWERLOSS_REPRO=wal_recovery_repair_crash_depth1/torn-s=4096/17/k=6/rk=0`
  - wal_segment_roll_and_gc_survive: KILL: `POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=6`
  - M witness_wal_roll_dir_sync: `witness_wal_roll_dir_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - M survivor_wal_gc_sync: `survivor_wal_gc_sync: POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=6`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=6`
- **htap-rowstore::powerloss_wal_open** (KILL)
  - wal_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=6`
  - M control_dir: `control_dir: site never exercised`
  - M witness_wal_open_parent_sync: `witness_wal_open_parent_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=6`
- **htap-rowstore::powerloss_wal_segment** (KILL)
  - wal_open_syncs_adopted_unsynced_segment: KILL: `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=7`
  - M control_dir: `control_dir: site never exercised`
  - M witness_wal_open_dir_sync: `witness_wal_open_dir_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=7`
- **htap-rowstore::powerloss_wal_tail** (KILL)
  - adopted_unsynced_wal_tail_is_synced_before_publish: KILL: `POWERLOSS_REPRO=adopted_unsynced_wal_tail_is_synced_before_publish/strict/0/k=11`
  - M control_dir: `control_dir: site never exercised`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=adopted_unsynced_wal_tail_is_synced_before_publish/strict/0/k=11`
- **htap-rowstore::powerloss_engine** (KILL)
  - engine_fresh_open_dirs_durable: KILL: `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=19`
  - engine_flush_boundary_crash_depth1: KILL: `POWERLOSS_REPRO=engine_flush_boundary_crash_depth1/strict/0/k=19/rk=0`
  - engine_visible_marker_monotonic: KILL: `POWERLOSS_REPRO=engine_visible_marker_monotonic/strict/0/k=19`
  - engine_commit_flush_compact_prefix_consistent: KILL: `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=19`
  - engine_wal_gc_after_flush_survives: KILL: `POWERLOSS_REPRO=engine_wal_gc_after_flush_survives/strict/0/k=19`
  - M witness_engine_flush_sst_dir_sync: `witness_engine_flush_sst_dir_sync: site never exercised`
  - M witness_engine_compact_sst_dir_sync: `witness_engine_compact_sst_dir_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=19`
- **htap-rowstore::powerloss_engine_open** (KILL)
  - engine_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=19`
  - M witness_engine_open_parent_sync: `witness_engine_open_parent_sync: site never exercised`
  - M survivor_engine_open_dir_sync: `survivor_engine_open_dir_sync: POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=19`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=19`
- **htap-txn::powerloss_txn** (KILL)
  - txn_preexisting_volatile_journal_dir: KILL: `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=28`
  - txn_fresh_journal_entry_durable: KILL: `POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=28`
  - txn_recover_after_every_boundary: KILL: `POWERLOSS_REPRO=txn_recover_after_every_boundary/strict/0/k=27`
  - txn_commits_survive_journal_and_wal: KILL: `POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=28`
  - txn_checkpoint_rewrite_atomic: KILL: `POWERLOSS_REPRO=txn_checkpoint_rewrite_atomic/strict/0/k=28`
  - txn_repair_torn_final_crash_depth1: KILL: `POWERLOSS_REPRO=txn_repair_torn_final_crash_depth1/torn-s=512/8/k=28/rk=0`
  - M witness_txn_journal_grandparent_sync: `witness_txn_journal_grandparent_sync: site never exercised`
  - M survivor_txn_journal_append_sync: `survivor_txn_journal_append_sync: POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=27`
  - M witness_txn_journal_parent_sync: `witness_txn_journal_parent_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - M survivor_txn_journal_open_sync: `survivor_txn_journal_open_sync: POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=27`
  - REPRO (verbatim): `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=28`
- **htap-catalog::powerloss_catalog** (KILL)
  - catalog_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=catalog_preexisting_volatile_dir/strict/0/k=6`
  - catalog_fresh_dir_durable: KILL: `POWERLOSS_REPRO=catalog_fresh_dir_durable/strict/0/k=6`
  - catalog_cas_sequence_prefix_consistent: KILL: `POWERLOSS_REPRO=catalog_cas_sequence_prefix_consistent/strict/0/k=6`
  - M survivor_catalog_open_dir_sync: `survivor_catalog_open_dir_sync: POWERLOSS_REPRO=catalog_fresh_dir_durable/strict/0/k=6`
  - M witness_catalog_open_parent_sync: `witness_catalog_open_parent_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=catalog_preexisting_volatile_dir/strict/0/k=6`
- **htap-coord::powerloss_coord** (KILL)
  - coord_state_atomic: KILL: `POWERLOSS_REPRO=coord_state_atomic/strict/0/k=21`
  - coord_fresh_dir_durable: KILL: `POWERLOSS_REPRO=coord_fresh_dir_durable/strict/0/k=21`
  - coord_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=coord_preexisting_volatile_dir/strict/0/k=21`
  - coord_fencing_token_never_regresses: KILL: `POWERLOSS_REPRO=coord_fencing_token_never_regresses/strict/0/k=17`
  - M control_dir: `control_dir: site never exercised`
  - M witness_coord_open_parent_sync: `witness_coord_open_parent_sync: site never exercised`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - M survivor_coord_open_dir_sync: `survivor_coord_open_dir_sync: POWERLOSS_REPRO=coord_fresh_dir_durable/strict/0/k=21`
  - REPRO (verbatim): `POWERLOSS_REPRO=coord_state_atomic/strict/0/k=21`
- **htap-colstore::powerloss_segment** (KILL)
  - segment_torn_write_never_decodes_silently: KILL: `POWERLOSS_REPRO=segment_torn_write_never_decodes_silently/strict/0/k=7`
  - M control_dir: `control_dir: site never exercised`
  - M witness_create_dir_all_durable_parent_sync: `witness_create_dir_all_durable_parent_sync: site never exercised`
  - M witness_sync_dir_sync: `witness_sync_dir_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=segment_torn_write_never_decodes_silently/strict/0/k=7`
- **htap-convert::powerloss_convert** (KILL)
  - segment_published_name_never_torn: KILL: `POWERLOSS_REPRO=segment_published_name_never_torn/strict/0/k=9`
  - convert_preexisting_volatile_root: KILL: `POWERLOSS_REPRO=convert_preexisting_volatile_root/strict/0/k=13`
  - convert_resumes_after_incomplete_boundary: KILL: `POWERLOSS_REPRO=convert_resumes_after_incomplete_boundary/strict/0/k=9`
  - convert_fresh_dir_durable: KILL: `POWERLOSS_REPRO=convert_fresh_dir_durable/strict/0/k=13`
  - convert_manifest_old_or_new_and_segments_valid: KILL: `POWERLOSS_REPRO=convert_manifest_old_or_new_and_segments_valid/strict/0/k=13`
  - M witness_sync_dir_sync: `witness_sync_dir_sync: site never exercised`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - M survivor_fsync_file_sync: `survivor_fsync_file_sync: POWERLOSS_REPRO=convert_fresh_dir_durable/strict/0/k=12`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=segment_published_name_never_torn/strict/0/k=9`
- **htap-movement::powerloss_movement** (KILL)
  - movement_fresh_dir_durable: KILL: `POWERLOSS_REPRO=movement_fresh_dir_durable/strict/0/k=9`
  - movement_preexisting_volatile_job_dir: KILL: `POWERLOSS_REPRO=movement_preexisting_volatile_job_dir/strict/0/k=9`
  - delete_artifacts_no_resurrection: OTHER-FAIL: `expected a pre-delete crash image containing the durable artifact`
  - job_state_monotonic_no_regression: KILL: `POWERLOSS_REPRO=job_state_monotonic_no_regression/strict/0/k=9`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - M witness_sync_dir_sync: `witness_sync_dir_sync: site never exercised`
  - M witness_movement_open_parent_sync: `witness_movement_open_parent_sync: site never exercised`
  - M survivor_movement_open_dir_sync: `survivor_movement_open_dir_sync: POWERLOSS_REPRO=movement_preexisting_volatile_job_dir/strict/0/k=9`
  - REPRO (verbatim): `POWERLOSS_REPRO=movement_fresh_dir_durable/strict/0/k=9`
- **htap-movement::powerloss_movement_clone** (KILL)
  - clone_publish_has_data_dir_barrier: OTHER-FAIL: `clone workload did not fsync the package data directory`
  - clone_survives_fresh_tablet_dirs: KILL: `POWERLOSS_REPRO=clone_survives_fresh_tablet_dirs/strict/0/k=56`
  - tablet_package_rename_order_chaos: KILL: `POWERLOSS_REPRO=tablet_package_rename_order_chaos/chaos/42/k=56`
  - clone_retry_completes_interrupted_job: KILL: `POWERLOSS_REPRO=clone_retry_completes_interrupted_job/strict/0/k=56`
  - M survivor_movement_package_data_dir_sync: `survivor_movement_package_data_dir_sync: POWERLOSS_REPRO=clone_survives_fresh_tablet_dirs/strict/0/k=56`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=clone_survives_fresh_tablet_dirs/strict/0/k=56`
- **htap-movement::powerloss_movement_export** (KILL)
  - export_file_complete_or_absent: KILL: `POWERLOSS_REPRO=export_file_complete_or_absent/strict/0/k=49`
  - export_to_fresh_nested_destination: KILL: `POWERLOSS_REPRO=export_to_fresh_nested_destination/strict/0/k=50`
  - export_preexisting_volatile_destination: KILL: `POWERLOSS_REPRO=export_preexisting_volatile_destination/strict/0/k=50`
  - M witness_sync_ancestors_sync: `witness_sync_ancestors_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=export_file_complete_or_absent/strict/0/k=49`
- **htap-movement::powerloss_movement_import** (KILL)
  - import_only_on_fresh_root_keeps_complete_job: KILL: `POWERLOSS_REPRO=import_only_on_fresh_root_keeps_complete_job/strict/0/k=48`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=import_only_on_fresh_root_keeps_complete_job/strict/0/k=48`
- **htap-movement::powerloss_movement_repair** (KILL)
  - repair_after_unsynced_publish_requires_durable_package: KILL: `POWERLOSS_REPRO=repair_after_unsynced_publish_requires_durable_package/strict/0/k=60`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=repair_after_unsynced_publish_requires_durable_package/strict/0/k=60`
- **htap-server::powerloss_server** (KILL)
  - server_open_syncs_colstore_entry_before_ack: OTHER-FAIL: `server_open_syncs_colstore_entry_before_ack: colstore mkdir was not followed by a server-root directory sync before open ack`
  - server_reopen_syncs_volatile_root_children: OTHER-FAIL: `server_reopen_syncs_volatile_root_children: no final server-root sync followed the volatile child mkdirs before open ack`
  - server_fresh_root_bootstrap_durable: KILL: `POWERLOSS_REPRO=srv_fresh_1/strict/0/k=21`
  - server_preexisting_volatile_root_durable: KILL: `POWERLOSS_REPRO=srv_vol_1/strict/0/k=21`
  - server_acked_sql_survives_power_loss: KILL: `POWERLOSS_REPRO=srv_sql_ack/strict/0/k=25`
  - M survivor_server_open_parent_sync: `survivor_server_open_parent_sync: POWERLOSS_REPRO=srv_fresh_1/strict/0/k=21`
  - M witness_sync_ancestors_sync: `witness_sync_ancestors_sync: site never exercised`
  - M witness_atomic_publish_dir_sync: `witness_atomic_publish_dir_sync: site never exercised`
  - M control_dir: `control_dir: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=srv_fresh_1/strict/0/k=21`
- **htap-server::powerloss_server_recovery** (KILL)
  - server_conversion_and_reclaim_survive: KILL: `POWERLOSS_REPRO=srv_conv_reclaim/strict/0/k=25`
  - server_recovery_crash_depth1: KILL: `POWERLOSS_REPRO=srv_recover_d1/strict/0/k=25/rk=0`
  - M control_dir: `control_dir: site never exercised`
  - M witness_sync_dir_sync: `witness_sync_dir_sync: site never exercised`
  - M witness_server_reclaim_colstore_sync: `witness_server_reclaim_colstore_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=srv_conv_reclaim/strict/0/k=25`

## M3: wal.rs: WAL commit sync (site wal:append_sync) removed (OLD + NEW-ROWSTORE)

Sum of per-suite wall times: 45 s (11 suites).

Diff (verbatim, `logs/M3.diff`):

```diff
diff --git a/crates/htap-rowstore/src/wal.rs b/crates/htap-rowstore/src/wal.rs
index 0410f9f..f5ccdb3 100644
--- a/crates/htap-rowstore/src/wal.rs
+++ b/crates/htap-rowstore/src/wal.rs
@@ -419,9 +419,6 @@ impl Wal {
 
     /// fsync the active segment.
     pub fn sync(&mut self) -> Result<()> {
-        if let Some(file) = self.active.as_ref() {
-            file.sync_all_site("wal:append_sync")?;
-        }
         Ok(())
     }
 
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 3 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 1 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.53s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 6 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 20 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.35s` |
| htap-rowstore::powerloss_wal | NEW | 101 | KILL | 1 | `test result: FAILED. 3 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s` |
| htap-rowstore::powerloss_wal_open | NEW | 101 | KILL | 1 | `test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s` |
| htap-rowstore::powerloss_wal_segment | NEW | 101 | KILL | 1 | `test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s` |
| htap-rowstore::powerloss_wal_tail | NEW | 0 | PASS | 2 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.55s` |
| htap-rowstore::powerloss_engine | NEW | 101 | KILL | 2 | `test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.17s` |
| htap-rowstore::powerloss_engine_open | NEW | 101 | KILL | 2 | `test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.04s` |
| htap-txn::powerloss_txn | NEW | 101 | KILL | 6 | `test result: FAILED. 6 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.38s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M3_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-rowstore::powerloss_wal** (KILL)
  - wal_fresh_dir_entry_durable: KILL: `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
  - wal_acked_commits_survive_strict: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_strict/strict/0/k=9`
  - wal_segment_roll_and_gc_survive: KILL: `POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - wal_acked_commits_survive_torn: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_torn/torn-s=4096/17/k=9`
  - wal_recovery_repair_crash_depth1: KILL: `POWERLOSS_REPRO=wal_recovery_repair_crash_depth1/torn-s=4096/17/k=9/rk=0`
  - M witness_wal_append_sync: `witness_wal_append_sync: site never exercised`
  - M control_file: `control_file: site never exercised`
  - M survivor_wal_gc_sync: `survivor_wal_gc_sync: POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
- **htap-rowstore::powerloss_wal_open** (KILL)
  - wal_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
  - M control_file: `control_file: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
- **htap-rowstore::powerloss_wal_segment** (KILL)
  - wal_open_syncs_adopted_unsynced_segment: KILL: `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=9`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_adopted_unsynced_segment/strict/0/k=9`
- **htap-rowstore::powerloss_engine** (KILL)
  - engine_fresh_open_dirs_durable: KILL: `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=26`
  - engine_visible_marker_monotonic: KILL: `POWERLOSS_REPRO=engine_visible_marker_monotonic/strict/0/k=26`
  - engine_commit_flush_compact_prefix_consistent: KILL: `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=26`
  - engine_wal_gc_after_flush_survives: KILL: `POWERLOSS_REPRO=engine_wal_gc_after_flush_survives/strict/0/k=26`
  - engine_flush_boundary_crash_depth1: KILL: `POWERLOSS_REPRO=engine_flush_boundary_crash_depth1/strict/0/k=26/rk=recover`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=26`
- **htap-rowstore::powerloss_engine_open** (KILL)
  - engine_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=25`
  - M survivor_engine_open_dir_sync: `survivor_engine_open_dir_sync: POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=24`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=25`
- **htap-txn::powerloss_txn** (KILL)
  - txn_preexisting_volatile_journal_dir: KILL: `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=37`
  - txn_recover_after_every_boundary: KILL: `POWERLOSS_REPRO=txn_recover_after_every_boundary/strict/0/k=35`
  - txn_fresh_journal_entry_durable: KILL: `POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=38`
  - txn_commits_survive_journal_and_wal: KILL: `POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=38`
  - txn_checkpoint_rewrite_atomic: KILL: `POWERLOSS_REPRO=txn_checkpoint_rewrite_atomic/strict/0/k=38`
  - txn_repair_torn_final_crash_depth1: KILL: `POWERLOSS_REPRO=txn_repair_torn_final_crash_depth1/torn-s=512/9/k=28/rk=23`
  - M survivor_txn_journal_append_sync: `survivor_txn_journal_append_sync: POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=37`
  - M survivor_txn_journal_open_sync: `survivor_txn_journal_open_sync: POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=37`
  - REPRO (verbatim): `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=37`

## M4: wal.rs: segment-roll dir sync (site wal:roll_dir_sync) removed (OLD + NEW-ROWSTORE)

Sum of per-suite wall times: 42 s (11 suites).

Diff (verbatim, `logs/M4.diff`):

```diff
diff --git a/crates/htap-rowstore/src/wal.rs b/crates/htap-rowstore/src/wal.rs
index 0410f9f..c92dc23 100644
--- a/crates/htap-rowstore/src/wal.rs
+++ b/crates/htap-rowstore/src/wal.rs
@@ -567,8 +567,6 @@ impl Wal {
         // metadata. Without this fsync a crash can leave a WAL whose newest
         // segment simply does not exist any more, silently losing every commit
         // in it. This is the classic missed-fsync durability bug.
-        sync_dir_site(&self.opts.dir, "wal:roll_dir_sync")?;
-
         self.segments.push(SegmentMeta {
             first_lsn: self.next_lsn,
             path,
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 3 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.44s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 1 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.71s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 5 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 18 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.37s` |
| htap-rowstore::powerloss_wal | NEW | 101 | KILL | 1 | `test result: FAILED. 4 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s` |
| htap-rowstore::powerloss_wal_open | NEW | 101 | KILL | 1 | `test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s` |
| htap-rowstore::powerloss_wal_segment | NEW | 0 | PASS | 1 | `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s` |
| htap-rowstore::powerloss_wal_tail | NEW | 0 | PASS | 2 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.60s` |
| htap-rowstore::powerloss_engine | NEW | 101 | KILL | 2 | `test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.27s` |
| htap-rowstore::powerloss_engine_open | NEW | 101 | KILL | 2 | `test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.99s` |
| htap-txn::powerloss_txn | NEW | 101 | KILL | 6 | `test result: FAILED. 6 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.38s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M4_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-rowstore::powerloss_wal** (KILL)
  - wal_fresh_dir_entry_durable: KILL: `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
  - wal_acked_commits_survive_strict: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_strict/strict/0/k=9`
  - wal_acked_commits_survive_torn: KILL: `POWERLOSS_REPRO=wal_acked_commits_survive_torn/torn-s=4096/17/k=9`
  - wal_segment_roll_and_gc_survive: KILL: `POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - wal_recovery_repair_crash_depth1: KILL: `POWERLOSS_REPRO=wal_recovery_repair_crash_depth1/torn-s=4096/17/k=9/rk=0`
  - M witness_wal_roll_dir_sync: `witness_wal_roll_dir_sync: site never exercised`
  - M survivor_wal_gc_sync: `survivor_wal_gc_sync: POWERLOSS_REPRO=wal_segment_roll_and_gc_survive/strict/0/k=9`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_fresh_dir_entry_durable/strict/0/k=9`
- **htap-rowstore::powerloss_wal_open** (KILL)
  - wal_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
  - REPRO (verbatim): `POWERLOSS_REPRO=wal_open_syncs_preexisting_volatile_dir/strict/0/k=8`
- **htap-rowstore::powerloss_engine** (KILL)
  - engine_fresh_open_dirs_durable: KILL: `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=26`
  - engine_visible_marker_monotonic: KILL: `POWERLOSS_REPRO=engine_visible_marker_monotonic/strict/0/k=26`
  - engine_commit_flush_compact_prefix_consistent: KILL: `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=26`
  - engine_wal_gc_after_flush_survives: KILL: `POWERLOSS_REPRO=engine_wal_gc_after_flush_survives/strict/0/k=26`
  - engine_flush_boundary_crash_depth1: KILL: `POWERLOSS_REPRO=engine_flush_boundary_crash_depth1/strict/0/k=26/rk=recover`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_fresh_open_dirs_durable/strict/0/k=26`
- **htap-rowstore::powerloss_engine_open** (KILL)
  - engine_open_syncs_preexisting_volatile_dir: KILL: `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=25`
  - M survivor_engine_open_dir_sync: `survivor_engine_open_dir_sync: POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=24`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_open_syncs_preexisting_volatile_dir/strict/0/k=25`
- **htap-txn::powerloss_txn** (KILL)
  - txn_preexisting_volatile_journal_dir: KILL: `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=37`
  - txn_recover_after_every_boundary: KILL: `POWERLOSS_REPRO=txn_recover_after_every_boundary/strict/0/k=35`
  - txn_fresh_journal_entry_durable: KILL: `POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=38`
  - txn_commits_survive_journal_and_wal: KILL: `POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=38`
  - txn_checkpoint_rewrite_atomic: KILL: `POWERLOSS_REPRO=txn_checkpoint_rewrite_atomic/strict/0/k=38`
  - txn_repair_torn_final_crash_depth1: KILL: `POWERLOSS_REPRO=txn_repair_torn_final_crash_depth1/torn-s=512/9/k=28/rk=23`
  - M survivor_txn_journal_append_sync: `survivor_txn_journal_append_sync: POWERLOSS_REPRO=txn_fresh_journal_entry_durable/strict/0/k=37`
  - M survivor_txn_journal_open_sync: `survivor_txn_journal_open_sync: POWERLOSS_REPRO=txn_commits_survive_journal_and_wal/strict/0/k=37`
  - REPRO (verbatim): `POWERLOSS_REPRO=txn_preexisting_volatile_journal_dir/strict/0/k=37`

## M5: engine.rs: SST dir sync after flush rename (site engine:flush_sst_dir_sync) removed (OLD + NEW-ROWSTORE)

Sum of per-suite wall times: 51 s (11 suites).

Diff (verbatim, `logs/M5.diff`):

```diff
diff --git a/crates/htap-rowstore/src/engine.rs b/crates/htap-rowstore/src/engine.rs
index 22499b8..1518645 100644
--- a/crates/htap-rowstore/src/engine.rs
+++ b/crates/htap-rowstore/src/engine.rs
@@ -1737,8 +1737,6 @@ impl Engine {
                 let _ = remove_file(&tmp_path);
                 return Err(HtapError::Io(e));
             }
-            sync_dir_site(&sst_dir, "engine:flush_sst_dir_sync")?;
-
             // 4. Write and fsync MANIFEST
             let committed_version = self.read_state.read().committed_version;
             let mut new_manifest = commit_guard.manifest.clone();
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-rowstore::wal_crash | OLD | 0 | PASS | 2 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.28s` |
| htap-rowstore::engine_crash | OLD | 0 | PASS | 2 | `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s` |
| htap-coord::local_coordinator | OLD | 0 | PASS | 4 | `test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s` |
| htap-server::ipc_multiprocess | OLD | 0 | PASS | 19 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.35s` |
| htap-rowstore::powerloss_wal | NEW | 0 | PASS | 2 | `test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.72s` |
| htap-rowstore::powerloss_wal_open | NEW | 0 | PASS | 1 | `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s` |
| htap-rowstore::powerloss_wal_segment | NEW | 0 | PASS | 1 | `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s` |
| htap-rowstore::powerloss_wal_tail | NEW | 0 | PASS | 2 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.59s` |
| htap-rowstore::powerloss_engine | NEW | 101 | KILL | 3 | `test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.84s` |
| htap-rowstore::powerloss_engine_open | NEW | 0 | PASS | 2 | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.03s` |
| htap-txn::powerloss_txn | NEW | 0 | PASS | 13 | `test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.17s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M5_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-rowstore::powerloss_engine** (KILL)
  - engine_commit_flush_compact_prefix_consistent: KILL: `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=46`
  - engine_visible_marker_monotonic: KILL: `POWERLOSS_REPRO=engine_visible_marker_monotonic/strict/0/k=55`
  - engine_wal_gc_after_flush_survives: KILL: `POWERLOSS_REPRO=engine_wal_gc_after_flush_survives/strict/0/k=76`
  - engine_flush_boundary_crash_depth1: KILL: `POWERLOSS_REPRO=engine_flush_boundary_crash_depth1/strict/0/k=46/rk=recover`
  - M witness_engine_flush_sst_dir_sync: `witness_engine_flush_sst_dir_sync: site never exercised`
  - REPRO (verbatim): `POWERLOSS_REPRO=engine_commit_flush_compact_prefix_consistent/strict/0/k=46`

## M6: tablet.rs sync_package_dir_chain discards every sync_dir result and returns Ok (FAULT + 5 movement powerloss)

Sum of per-suite wall times: 17 s (7 suites).

Diff (verbatim, `logs/M6.diff`):

```diff
diff --git a/crates/htap-movement/src/tablet.rs b/crates/htap-movement/src/tablet.rs
index 9a862ef..2cd13b5 100644
--- a/crates/htap-movement/src/tablet.rs
+++ b/crates/htap-movement/src/tablet.rs
@@ -227,14 +227,14 @@ fn sync_package_dir_chain(mover: &LocalDataMover, package_dir: &std::path::Path)
     let mut current = Some(package_dir);
 
     while let Some(dir) = current {
-        sync_dir(dir)?;
+        let _ = sync_dir(dir);
         if dir == mover.root_dir() {
             break;
         }
         current = dir.parent();
     }
 
-    sync_dir(parent_or_current_dir(mover.root_dir()))?;
+    let _ = sync_dir(parent_or_current_dir(mover.root_dir()));
 
     Ok(())
 }
```

| suite | set | exit | class | wall s | test result |
|---|---|---|---|---|---|
| htap-movement::sync_fault_clone | FAULT | 101 | KILL (fault oracle: injected sync error was swallowed; no REPRO by design) | 4 | `test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s` |
| htap-movement::sync_fault_repair | FAULT | 101 | KILL (fault oracle: injected sync error was swallowed; no REPRO by design) | 2 | `test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s` |
| htap-movement::powerloss_movement | NEW | 0 | PASS | 1 | `test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.65s` |
| htap-movement::powerloss_movement_clone | NEW | 0 | PASS | 3 | `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.72s` |
| htap-movement::powerloss_movement_export | NEW | 0 | PASS | 2 | `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.37s` |
| htap-movement::powerloss_movement_import | NEW | 0 | PASS | 2 | `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.05s` |
| htap-movement::powerloss_movement_repair | NEW | 0 | PASS | 3 | `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.38s` |

Commands: each row ran `timeout -k 30 900 cargo test -p <crate> --test <bin> --no-fail-fast` (cwd `$WT`, `CARGO_TARGET_DIR=$E/target CARGO_INCREMENTAL=0 TMPDIR=$E/tmp`), log `logs/M6_<crate>_<bin>.log`.

Failing tests (M = control/witness/survivor macro test, classified separately; final panic message shown):

- **htap-movement::sync_fault_clone** (KILL (fault oracle: injected sync error was swallowed; no REPRO by design))
  - clone_sync_failure_is_never_swallowed: KILL (fault oracle): `clone silently succeeded when sync fault ordinal 11 fired`
- **htap-movement::sync_fault_repair** (KILL (fault oracle: injected sync error was swallowed; no REPRO by design))
  - repair_sync_failure_is_never_swallowed: KILL (fault oracle): `repair unexpectedly succeeded with sync fault at ordinal 1`

## Cleanup (13:34 UTC)

- `git -C /mnt/storage/projects worktree remove --force $E/wt` and `worktree prune` succeeded. `worktree list` now shows only `/mnt/storage/projects b40aae1 [master]`.
- `rm -rf $E/target $E/tmp` was run. `$E/logs` and this file are kept.
- `git -C /mnt/storage/projects status --porcelain` is not empty. It shows modifications to CLAUDE.md, README.md, crates/htap-crashsim/tests/mutation_controls.rs and docs/*.md. Other agents made these edits concurrently, as the coordinator announced. E3 never wrote to the main tree.
- After cleanup, `df -h /mnt/storage` shows 20G free (72G used of 96G).
- Total wall time from worktree creation to the end of cleanup: 19 min 18 s.
