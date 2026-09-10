# Progress

## Phases

| Phase | Status | Summary |
| ----- | ------ | ------- |
| Phase 0 — Research and workspace bootstrap | `Complete` | StarRocks source research completed and written up in [`RESEARCH.md`](./RESEARCH.md), covering the mechanisms adopted, those deliberately rejected, and the reasoning for each. A ten-crate cargo workspace was bootstrapped with crate boundaries drawn deliberately along the two seams that matter: the frontend/backend split and the row-format/column-format storage split, so that both remain deployment and routing choices rather than rewrites. `htap-common` carries the shared MVCC `Version` domain and the `FencingToken` type, together with the common error enum — these are shared by every other crate and so were built first. `ci.sh` runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace` and `cargo test --workspace`; all four stages are green on a clean rebuild. |
| Phase 1 — Row store | `Complete` | Built the LSM row-store engine (`htap-rowstore`) with WAL, memtable, immutable SSTs, manifest tracking, snapshot isolation, and point lookups. Covered by unit tests, integration tests, bounded property-based reference model tests, and process-level crash tests: `wal_recovery` (record framing, torn-tail repair, atomicity, GC), `wal_crash` (`kill_9_loses_no_committed_data`, `a_cleanly_exiting_child_recovers_every_commit`), `sst` (block index, Bloom filter, multi-block iter, CRC corruption detection), `engine` (`test_commit_and_snapshot_visibility`, `test_reopen_preserves_data_and_version`, `test_first_writer_wins_conflict`, `test_delete_resurrection_regression`, `test_auto_flush_and_manual_flush`, `test_flush_then_reopen_empty_wal`), `mvcc_properties` (`prop_mvcc_engine_matches_model` proving snapshot stability, no future visibility, delete correctness, first-writer-wins, and recovery equivalence), and `engine_crash` (`engine_kill_9_recovers_all_reported_commits`, `engine_cleanly_exiting_child_recovers_every_commit`). |
| Phase 2 — Column store | `Complete` | Built the columnar storage engine (`htap-colstore`) with immutable binary segment layout, plain and dictionary encodings, optional zstd compression, CRC32C frame and footer checksums, typed per-block zone maps (min/max/nullability), and vectorized scans with selective column decoding and conservative pushdown pruning. Covered by unit tests, segment roundtrip and corruption tests (`crates/htap-colstore/tests/segment_roundtrip.rs`), vectorized scan reference tests (`crates/htap-colstore/tests/scan.rs`), and deterministic zone-map pruning acceptance test (`crates/htap-colstore/tests/zone_map_skip.rs`). |
| Phase 3 — SQL layer | `In progress` | Completed narrow local slice: sqlparser MySQL dialect parsing, strict catalog binder, structural rowstore route classifier, and durable catalog-backed synchronous `LocalServer` supporting `CREATE TABLE`, literal `INSERT`, PK `DELETE`, and complete-PK `SELECT` with reopen recovery. Covered by tests in `crates/htap-sql/tests/parse_bind.rs`, `tests/route.rs` (`crates/htap-sql/tests/route.rs`), `crates/htap-server/tests/local_server.rs`, and catalog recovery tests (`crates/htap-catalog/tests/catalog_recovery.rs`). MySQL wire compatibility, the `htapd` daemon/protocol, sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`, scans/aggregates/joins/CTEs/windows/subqueries, columnstore SQL execution, multi-partition routing, and broad MySQL compatibility are explicitly deferred. |
| Phase 4 | `Not started` | — |
| Phase 5 | `Not started` | — |
| Phase 6 | `Not started` | — |
| Phase 7 | `Not started` | — |

---

## Requirement → Evidence map

Each requirement must be backed by a named, runnable test or benchmark. This
table is the contract between the requirements and the test suite.

| Requirement | Proving test / benchmark | Status |
| ----------- | ------------------------ | ------ |
| R1 — Block skipping via zone maps (≥90% skip criterion) | `crates/htap-colstore/tests/zone_map_skip.rs` (`test_deterministic_zone_map_skip_acceptance`) | `complete` |
| R2 — Storage-format conversion (row ↔ column) | *(to be filled)* | `pending` |
| R3 — Sharding, placement, replication and repair | *(to be filled)* | `pending` |
| R4 — SQL breadth (CTEs, window functions, subqueries, cost model) | Completed narrow local slice: MySQL-dialect parser and strict catalog binder (`crates/htap-sql/tests/parse_bind.rs`), structural route classifier (`tests/route.rs` / `crates/htap-sql/tests/route.rs`), durable catalog recovery tests (`crates/htap-catalog/tests/catalog_recovery.rs`), and synchronous `LocalServer` (`crates/htap-server/tests/local_server.rs`) supporting `CREATE TABLE`, literal `INSERT`, PK `DELETE`, and complete-PK `SELECT` with reopen recovery. Analytical SQL breadth (CTEs, window functions, subqueries, aggregates, joins, scans, cost model), MySQL wire compatibility, and non-PK DML (`UPDATE`, `ALTER`, `DROP`) are explicitly deferred. | `partial` |
| R5 — Mixed OLTP/OLAP: point lookups bypass the analytical engine | Row-store primary-key fast path: `crates/htap-rowstore/tests/engine.rs`, `crates/htap-rowstore/tests/mvcc_properties.rs`, and `crates/htap-rowstore/tests/engine_crash.rs`. Structural query router bypass and synchronous point read execution verified in `tests/route.rs` (`crates/htap-sql/tests/route.rs`) and `crates/htap-server/tests/local_server.rs` (`Route::RowstorePointLookup` bypasses analytical planning). Analytical execution engine and columnstore SQL execution deferred. | `partial` |
| R6 — Fencing tokens prevent stale-leader split-brain | *(to be filled)* | `pending` |

> **This table must be filled with concrete test names before the project can
> be considered done.** A requirement whose evidence column still reads
> `pending` is an unproven requirement, regardless of whether code exists that
> appears to implement it.
