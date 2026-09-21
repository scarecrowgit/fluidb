# Benchmark Suite Specification & Methodology

This document specifies the microbenchmarks implemented in `crates/htap-bench/benches/local_mvp.rs`.

---

## IMPORTANT NOTICE: Scope & Intended Interpretation

> **Disclaimer:**
> The benchmarks documented here are **local, single-node microbenchmarks** evaluating isolated internal subsystem operations under fixed synthetic fixtures.
>
> - They **must not** be interpreted as end-to-end database management system (DBMS) comparisons.
> - They **do not** make production performance, latency, or throughput claims.
> - They **do not** implement or comply with standardized database benchmarks such as **TPC-C** or **TPC-H**.
> - They **do not** exercise distributed network protocols, client wire protocols, concurrent connection contention, or multi-tenant workloads.

---

## Execution Commands

Criterion microbenchmarks are compiled and run via `cargo bench`:

```bash
# List all available benchmark targets
cargo bench -p htap-bench --bench local_mvp -- --list

# Verify compilation of all workspace benchmarks without executing timings (used in CI)
cargo bench --workspace --no-run

# Run the entire local microbenchmark suite
cargo bench -p htap-bench --bench local_mvp

# Run a specific benchmark target
cargo bench -p htap-bench --bench local_mvp -- <benchmark_name>
```

---

## Benchmark Definitions

Criterion configuration for all benchmarks uses explicit deterministic thresholds:
- Warm-up time: `500ms`
- Measurement time: `1s`
- Sample size: `10`

### 1. `rowstore/point_get_stable_snapshot`

- **Target Component:** `htap-rowstore`
- **Target API:** `htap_rowstore::Engine::get(partition_id, key, snapshot)`
- **Fixture:**
  - LSM engine opened in a fresh `TempDir`.
  - 1,000 rows (`id: Int64`, `payload: String`) committed in a single transaction at snapshot version 1.
  - Snapshot pinned at version 1. Target row ID: `420` (`key = "key_000420"`).
- **Correctness Precondition:**
  - Evaluated immediately prior to timing loop: `engine.get(0, &target_key, snap)` must return `Some(row)` with column 0 matching `Value::Int64(420)`.
- **Timing Units / Metric:** Time per point lookup (`ns` or `µs` per iteration) and throughput (`iter/s`).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- rowstore/point_get_stable_snapshot
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/rowstore/point_get_stable_snapshot/report/index.html`

---

### 2. `colstore/equality_zone_map_scan`

- **Target Component:** `htap-colstore`
- **Target API:** `htap_colstore::SegmentReader::scan(&ScanRequest)`
- **Fixture:**
  - Columnar segment (`.col`) written with 100 blocks of 1,024 rows each (102,400 total rows).
  - Schema: `id: Int64` (unindexed, block-monotonic), `payload: String`.
  - Target: Equality predicate `id == 730042` located in Block 73, Row 42. Projecting only column 1 (`payload`).
- **Correctness Precondition:**
  - Evaluated before timing loop: scan request execution must verify exact zone-map statistics:
    - `candidate_blocks == 100`
    - `skipped_blocks == 99` (deterministic 99% block pruning via zone maps)
    - `decoded_blocks == 1`
    - `batches.len() == 1`
    - Exactly 1 row returned.
- **Timing Units / Metric:** Time per scan operation (`µs` per iteration) and throughput (`iter/s`).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- colstore/equality_zone_map_scan
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/colstore/equality_zone_map_scan/report/index.html`

---

### 3. `convert/local_converter_row_to_column`

- **Target Component:** `htap-convert`
- **Target API:** `htap_convert::LocalConverter::convert_partition(partition_id)`
- **Fixture:**
  - Re-created fresh per iteration using `BatchSize::PerIteration`.
  - `LocalCatalogStore` and `Engine` in a `TempDir` populated with 100 rows (`id: Int64`, `val: String`).
  - Catalog registered with 1 table, 1 partition, 1 tablet, 1 replica at generation 1.
  - `LocalConverter` configured with `rows_per_block = 64`.
- **Correctness Precondition:**
  - Evaluated on a separate fixture before timing: conversion succeeds, producing a valid tablet manifest with `segment_count == 1` and `total_rows == 100`.
- **Timing Units / Metric:** Time per full partition conversion (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- convert/local_converter_row_to_column
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/convert/local_converter_row_to_column/report/index.html`

---

### 4. `movement/fixed_csv_import`

- **Target Component:** `htap-movement`
- **Target API:** `htap_movement::LocalDataMover::copy_from_csv_reader(&options, catalog, txn_manager, reader)`
- **Fixture:**
  - Re-created fresh per iteration using `BatchSize::PerIteration`.
  - `LocalCatalogStore`, `Engine`, and `TransactionManager` (with `txn.journal`) in a `TempDir`.
  - In-memory CSV byte buffer containing 50 records with header `id,name,score`.
- **Correctness Precondition:**
  - Evaluated on a separate fixture before timing: import execution succeeds, reporting:
    - `records_read == 50`
    - `records_committed == 50`
    - `records_skipped == 0`
- **Timing Units / Metric:** Time per CSV batch import (`ms` or `µs` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- movement/fixed_csv_import
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/movement/fixed_csv_import/report/index.html`

---

### 5. `coord/placement_planning`

- **Target Component:** `htap-coord`
- **Target API:** `htap_coord::placement::plan_placement(&snapshot, &candidates, target_rf)`
- **Fixture:**
  - Immutable catalog snapshot containing 1 table, 1 partition, 10 tablets with 1 initial replica each.
  - Candidate node set: 6 nodes (`NodeId(1)..=NodeId(6)`).
  - Target replication factor: `3`.
- **Correctness Precondition:**
  - Evaluated prior to timing: planner succeeds, producing a plan with 10 tablet updates where each tablet reaches target replication factor 3.
- **Timing Units / Metric:** Time per placement plan computation (`µs` or `ns` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- coord/placement_planning
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/coord/placement_planning/report/index.html`

---

### 6. `coord/leadership_fenced_cas`

- **Target Component:** `htap-coord`
- **Target API:**
  - `htap_coord::LocalCoordinator::acquire_leadership(scope, node_id)`
  - `htap_coord::LocalCoordinator::fenced_catalog_compare_and_set(scope, token, catalog, expected_gen, next_snap)`
- **Fixture:**
  - Re-created fresh per iteration using `BatchSize::PerIteration`.
  - `LocalCoordinator` and `LocalCatalogStore` in a `TempDir` initialized at catalog generation 1.
  - Prepared generation 2 catalog snapshot.
- **Correctness Precondition:**
  - Evaluated on a separate fixture before timing: leadership acquisition succeeds with a valid token, fenced catalog CAS succeeds from generation 1 to 2, and `catalog.current_generation()` returns 2.
- **Timing Units / Metric:** Time per combined leadership acquisition and fenced catalog CAS (`ms` or `µs` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- coord/leadership_fenced_cas
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/coord/leadership_fenced_cas/report/index.html`

---

### 7. `phase14/join_without_statistics`

- **Target Component:** `htap-server` query executor with cost-based optimization
- **Target API:** `LocalServer::execute_query_with_options(sql, OptimizationMode::Enabled, memory_budget, parallelism)`
- **Fixture:**
  - Two tables: `bench_join_large` (2,048 rows) and `bench_join_small` (32 rows), joined on `join_key`.
  - **Without statistics variant:** No `ANALYZE TABLE` executed; optimizer uses default heuristics.
  - Query: inner join projecting all columns.
- **Correctness Precondition:**
  - Executed before timing: join result row bag verified to be identical between with/without statistics variants.
  - EXPLAIN plans compared to verify that statistics enable different costing (plans must differ).
- **Timing Units / Metric:** Time per join execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/join_without_statistics
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/join_without_statistics/report/index.html`

---

### 8. `phase14/join_with_statistics`

- **Target Component:** `htap-server` query executor with cost-based optimization and collected statistics
- **Target API:** `LocalServer::execute_query_with_options(sql, OptimizationMode::Enabled, memory_budget, parallelism)` after `ANALYZE TABLE`
- **Fixture:**
  - Same two tables as benchmark 7: `bench_join_large` (2,048 rows) and `bench_join_small` (32 rows).
  - **With statistics variant:** `ANALYZE TABLE` executed on both tables; optimizer has access to collected statistics.
  - Query: identical inner join.
- **Correctness Precondition:**
  - Executed before timing: join result row bag verified to be identical to without-statistics variant.
  - EXPLAIN plans compared to verify that statistics enable different costing.
- **Timing Units / Metric:** Time per join execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/join_with_statistics
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/join_with_statistics/report/index.html`
- **Comparison Note:** Benchmark 8 vs. Benchmark 7 isolates the effect of cost-based join reordering when statistics are available.

---

### 9. `phase14/group_by_parallelism_1`

- **Target Component:** `htap-server` GROUP BY aggregation at fixed parallelism = 1
- **Target API:** `LocalServer::set_query_parallelism(1)` then `LocalServer::execute(sql)`
- **Fixture:**
  - Table `bench_parallel_groups` with 8,192 rows across multiple hash-distributed buckets.
  - Query: `GROUP BY` with `COUNT(*)`, `SUM`, `AVG`, and `COUNT(DISTINCT)` aggregates.
  - Parallelism explicitly set to serial (1 worker).
- **Correctness Precondition:**
  - Executed before timing: GROUP BY result row bag verified identical across different parallelism settings.
  - `query_parallelism()` confirmed to be 1.
- **Timing Units / Metric:** Time per GROUP BY execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/group_by_parallelism_1
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/group_by_parallelism_1/report/index.html`

---

### 10. `phase14/group_by_parallelism_available`

- **Target Component:** `htap-server` GROUP BY aggregation at machine's available parallelism
- **Target API:** `LocalServer::set_query_parallelism(available)` then `LocalServer::execute(sql)`
- **Fixture:**
  - Same table and query as benchmark 9: `bench_parallel_groups` with 8,192 rows.
  - Parallelism set to `std::thread::available_parallelism().get()` (machine-dependent, typically 4-8 on modern systems).
- **Correctness Precondition:**
  - Executed before timing: GROUP BY result row bag verified identical to single-threaded variant.
  - `query_parallelism()` confirmed to be the machine's available parallelism.
- **Timing Units / Metric:** Time per GROUP BY execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/group_by_parallelism_available
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/group_by_parallelism_available/report/index.html`
- **Comparison Note:** Benchmark 10 vs. Benchmark 9 isolates the effect of parallel GROUP BY execution.

---

### 11. `phase14/hash_join_in_memory`

- **Target Component:** `htap-server` hash join at generous memory budget (no spilling)
- **Target API:** `LocalServer::set_query_memory_budget(256MB)` then `LocalServer::execute(sql)`
- **Fixture:**
  - Two tables: `bench_spill_left` (1,024 rows) and `bench_spill_right` (1,280 rows), joined on `join_key`.
  - Memory budget: 256 MB (large enough that all intermediate state stays in memory).
  - Query: inner join.
- **Correctness Precondition:**
  - Executed before timing: join result row bag verified identical to spilled variant.
  - No spill files created on disk.
- **Timing Units / Metric:** Time per join execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/hash_join_in_memory
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/hash_join_in_memory/report/index.html`

---

### 12. `phase14/hash_join_forced_spill`

- **Target Component:** `htap-server` hash join at tiny memory budget (forced to spill)
- **Target API:** `LocalServer::set_query_memory_budget(32KB)` then `LocalServer::execute(sql)`
- **Fixture:**
  - Same two tables as benchmark 11: `bench_spill_left` (1,024 rows) and `bench_spill_right` (1,280 rows).
  - Memory budget: 32 KB (tiny, forces one-level partitioning spill via temp files).
  - Query: identical inner join.
- **Correctness Precondition:**
  - Executed before timing: join result row bag verified identical to in-memory variant (sorted for row-order independence).
  - Spill files created and cleaned up on disk.
- **Timing Units / Metric:** Time per join execution (`ms` per iteration).
- **Exact Command:**
  ```bash
  cargo bench -p htap-bench --bench local_mvp -- phase14/hash_join_forced_spill
  ```
- **Benchmark Target:** `local_mvp`
- **Criterion Report Path:** `target/criterion/phase14/hash_join_forced_spill/report/index.html`
- **Comparison Note:** Benchmark 12 vs. Benchmark 11 isolates the cost of one-level hash partitioning spill when memory is exhausted.

---

## Reproducibility Record Template

When recording benchmark execution results for verification or regression tracking, record all environmental metadata using the template below:

```markdown
### Benchmark Run Metadata
- **Date:** YYYY-MM-DD HH:MM:SS UTC
- **Git Commit SHA:** <git rev-parse HEAD>
- **Git State:** clean | dirty
- **Rust Toolchain:** rustc 1.95.0 (or output of `rustc -Vv`)
- **OS / Kernel:** <uname -srmo>
- **CPU Model & Architecture:** <lscpu or /proc/cpuinfo model name, physical cores, threads>
- **Memory (RAM):** <free -h total memory>
- **Filesystem Type & Mount:** <df -T . filesystem type (e.g., ext4, xfs, tmpfs) and mount options>
- **Criterion Report Directory:** `target/criterion/`

### Execution Results Summary
| Benchmark Name | Measurement Time | Mean Latency | Throughput | Status |
| -------------- | ---------------- | ------------ | ---------- | ------ |
| `rowstore/point_get_stable_snapshot` | 1s | ... | ... | PASS |
| `colstore/equality_zone_map_scan` | 1s | ... | ... | PASS |
| `convert/local_converter_row_to_column` | 1s | ... | ... | PASS |
| `movement/fixed_csv_import` | 1s | ... | ... | PASS |
| `coord/placement_planning` | 1s | ... | ... | PASS |
| `coord/leadership_fenced_cas` | 1s | ... | ... | PASS |
```

---

## Phase 14 Benchmark Results

### Benchmark Run: 2026-09-21

#### Environment Metadata
- **Date:** 2026-09-21 (benchmark run timestamp)
- **Git Commit SHA:** f27f6a84f02106a8e12adb24454ced88002749aa
- **Git State:** clean
- **Rust Toolchain:** rustc 1.95.0 (59807616e 2026-04-14)
- **OS / Kernel:** Linux 6.18.45 x86_64 GNU/Linux
- **Machine:** Linux dev box with 8 CPU cores
- **Criterion Report Directory:** `target/criterion/`

#### Phase 14 Execution Results Summary

| Benchmark Name | Measurement Time | Mean Latency | Status |
| -------------- | ---------------- | ------------ | ------ |
| `phase14/join_without_statistics` | 1s | 2.1017 ms | PASS |
| `phase14/join_with_statistics` | 1s | 1.9332 ms | PASS |
| `phase14/group_by_parallelism_1` | 1s | 10.354 ms | PASS |
| `phase14/group_by_parallelism_available` | 1s | 10.270 ms | PASS |
| `phase14/hash_join_in_memory` | 1s | 5.9440 ms | PASS |
| `phase14/hash_join_forced_spill` | 1s | 14.548 ms | PASS |

#### Observation & Validation Notes

**Join Cost-Based Reordering (Benchmarks 7 & 8):**
- Without statistics: 2.1017 ms (10 samples, mean)
- With statistics: 1.9332 ms (10 samples, mean)
- **Performance improvement with stats:** ~8.0% faster when optimizer has access to collected table statistics.
- **Validation:** EXPLAIN plans differed between the two variants, confirming that cost-based reordering was engaged. Join result row bags matched exactly.

**GROUP BY Parallelism (Benchmarks 9 & 10):**
- Serial (1 worker): 10.354 ms (10 samples, mean)
- Parallel (available): 10.270 ms (10 samples, mean)
- **Performance ratio:** ~0.8% difference (within noise; no significant speedup on this dataset size).
- **Validation:** Results were identical across parallelism settings, confirming determinism. Dataset size (8,192 rows, 257 distinct groups) is relatively modest; parallelism overhead may outweigh benefit at this scale.

**Hash Join Spilling (Benchmarks 11 & 12):**
- In-memory (256 MB budget): 5.9440 ms (10 samples, mean)
- Forced spill (32 KB budget): 14.548 ms (10 samples, mean)
- **Performance ratio:** Spilling costs ~2.45x more time than in-memory execution at the same data size.
- **Validation:** Join result row bags matched exactly after sorting for row-order independence (spilling uses partitioned processing, which can reorder rows). Spill files were created in `<data-root>/spill/` and cleaned up on completion.

#### Confirmation of Engagement

1. **Statistics enabled cost-based reordering:** Verified by running `EXPLAIN` for both the statistics and no-statistics variants; the join plans differed measurably, confirming the optimizer took different reordering decisions.

2. **Parallelism was available but not beneficial at this scale:** The machine reported `available_parallelism() = 8`; however, the GROUP BY data size (8,192 rows) and group cardinality (257 distinct groups) did not show throughput improvement. This is expected: parallelism overhead (thread spawning, synchronization) can exceed gains on small datasets.

3. **Spilling genuinely occurred:** Verified by:
   - Pre-execution check: memory budget of 32 KB is far below the estimated build-side size (~130 KB for 1,280 rows × 2 varchar payloads + overhead).
   - Post-execution validation: join results matched in-memory version after row sorting, confirming correctness despite partitioned processing.
   - Disk activity: spill directory (`<data-root>/spill/<statement-id>/`) was populated during execution and cleaned up on completion.
