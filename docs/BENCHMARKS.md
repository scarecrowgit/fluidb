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
