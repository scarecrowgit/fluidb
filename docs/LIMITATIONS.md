# Limitations and Known Gaps

This file is the running record of everything specified but not yet complete,
and every deviation from the project brief.

---

## Missing input: ZooKeeper reference source

The project brief specified `examples/zookeeper` containing Apache ZooKeeper
source, to be used both for understanding ZAB, session and ephemeral-node
semantics, watches and the client wire protocol, and for running a real
ZooKeeper ensemble in integration tests. **That directory was not present.**
The provided `examples/` directory contained only `starrocks/`.

### Impact and mitigation

ZooKeeper protocol semantics were derived from the public ZooKeeper 3.9
documentation and the `zookeeper-async` Rust client crate rather than from
source. Integration tests for the ZooKeeper coordination backend run against
the official `zookeeper:3.9` Docker image.

This preserves the more important property: session expiry, ephemeral node
loss and watch re-registration are exercised against a real server rather than
a mock, which is where a mock would most likely be wrong.

### Completion plan

If the source tree is supplied, re-verify the session-expiry and
watch-re-registration code paths against the actual server implementation and
record any divergence here.

Cross-reference: ADR-006 in [`DECISIONS.md`](./DECISIONS.md).

---

## Rowstore external coordination: single-participant contiguous version subset

The transaction manager (`htap-txn`) coordinates two-phase commit across participants, using `RowstoreParticipant` to wrap an LSM `Engine`. The rowstore external coordination behavior supports restart-safe visibility (where applied-but-unpublished transactions stay completely hidden across reopen until explicitly published) and idempotent external re-apply during recovery.

### Exact limitation: dense contiguous versions

Rowstore MVCC visibility uses a single scalar watermark (`visible_version`) and enforces sequential monotonic progression (`version == visible_version.next()`). Full sparse global version support across multiple partitions/participants with version gaps (where transactions touch only a subset of participants, leaving non-contiguous version sequences on any single participant) is deferred:

- **Single scalar watermark:** A scalar watermark treats all records with `version <= visible_version` as readable. If a participant were to jump visibility across a sparse gap (e.g. from version 2 to 5) while intermediate transactions were concurrently in flight or applied but unpublished, those intermediate writes would become visible prematurely.
- **Ordered publication invariant:** `Engine::publish` enforces `version == visible_version.next()` to prevent out-of-order publication races across concurrent publishers.
- **Supported subset:** The implemented correct subset supports all single-participant contiguous transaction flows (such as single-rowstore coordinator transactions with monotonically consecutive versions), crash recovery replay, idempotent duplicate apply, and restart-safe visibility isolation via the durable `VISIBLE` watermark file. Multi-participant sparse global version jumps require either an active-transaction tracking bitmap/list or hybrid logical clocks.

---

## Durability testing is bounded by process-level fault injection

The write-ahead log and LSM engine in `htap-rowstore` are covered by integration
tests (`crates/htap-rowstore/tests/wal_crash.rs`, test
`kill_9_loses_no_committed_data`, and `crates/htap-rowstore/tests/engine_crash.rs`,
test `engine_kill_9_recovers_all_reported_commits`) that spawn a real child process,
let it durably commit transactions and report their ids (including periodic SST flushes),
then terminate it with `SIGKILL` and assert that every reported commit is recovered upon
reopening.

### What this proves, and what it does not

The test proves **replay integrity across abrupt process death**. It does not
prove **fsync durability**. This was verified by mutation testing:

| Mutation | Result |
| -------- | ------ |
| Drop the `write_all` in `append()` | Test FAILS (correctly detects data loss) |
| Stub `sync()` to a no-op | Test still PASSES (does not detect the bug) |

The reason is that `SIGKILL` destroys the process but not the operating system
page cache. Bytes written with `write_all` but never fsynced remain readable by
a subsequent reader on the same machine. Only a machine-level failure — power
loss, kernel panic, or a simulated block-device failure — distinguishes the two
cases.

### Completion plan

To close this gap, either (a) run the crash child inside a VM or container
whose storage is dropped without flushing, (b) interpose a FUSE or
device-mapper layer that discards non-fsynced writes on fault injection, or
(c) use a filesystem fault-injection tool such as `dm-flakey` in the chaos
suite planned for Phase 7.

Until one of these is in place, the fsync path is verified by code inspection
only.

---

## Column-store MVP scope and deferred features

The Phase 2 `htap-colstore` implementation delivers the core columnar segment storage format, encoding, compression, zone-map pushdown filtering, and vectorized scans. The following column-store features are deferred to subsequent phases:

- **Delta and delete vectors:** Columnar segments are currently immutable write-once files. Row-level deletions via bitmap delete vectors and merge-on-read delta store integration are deferred to Phase 4.
- **MVCC visibility:** Column segments currently store durable committed rows without transactional version ranges (`htap-common::Version`) per row. Snapshot isolation visibility across columnar data is deferred to transaction coordinator (`htap-txn`) and conversion integration.
- **Conversion and catalog integration:** Standalone segment read/write and scan execution are functional, but online row-to-column transcoding and tablet catalog metadata registration are deferred to Phase 4 (`htap-convert`) and Phase 5 (`htap-catalog`).
- **Richer predicates, joins, and aggregations:** The scan engine supports basic SQL 3-valued comparison predicates (`Eq`, `Lt`, `Lte`, `Gt`, `Gte`, `IsNull`, `IsNotNull`) on single columns with conservative zone-map skipping. Compound predicate expression trees (AND/OR), hash joins, group-by, and vectorized aggregations are deferred to Phase 3 (`htap-sql`).
- **Arrow and DataFusion integration:** Scans yield internal typed `RecordBatch` and `ColumnVector` structures. Exporting to Apache Arrow RecordBatches and DataFusion `TableProvider` / `ExecutionPlan` integration are deferred to the analytical query execution layer in Phase 3.
- **Atomic publication and manifest integration:** Segments are managed individually at filesystem paths. Multi-segment manifest commits, atomic segment swaps, and compaction lifecycle tracking are deferred to storage conversion and tablet management.

---

## SQL layer narrow local slice scope and deferred features

The Phase 3 implementation delivers a verified, crash-safe narrow local SQL slice integrating parsing, catalog binding, route classification, transactional DML execution, and point reads. It does not implement full SQL breadth, analytical query execution, or client network protocols.

### Completed narrow local slice

- **sqlparser MySQL dialect:** Strict single-statement parsing using `sqlparser::dialect::MySqlDialect`, accepting valid backtick identifiers and MySQL escape semantics while rejecting empty input, malformed SQL, and multi-statement input with stable `InvalidArgument` errors. Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Strict catalog binder:** Schema-validated binding for `CREATE TABLE` (scalar types and primary keys), literal schema-ordered `INSERT`, complete-primary-key `DELETE`, and complete-primary-key `SELECT`. Strictly rejects unsupported data types, composite key mismatches, expression evaluations, implicit coercions, and unhandled clauses. Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Structural rowstore route classifier:** Inspects bound statements and storage descriptors, classifying complete-PK queries as `Route::RowstorePointLookup` and single-partition mutations as `Route::RowstoreWrite`, while explicitly rejecting unsupported columnar/converting descriptors. Verified in `tests/route.rs` (`crates/htap-sql/tests/route.rs`).
- **Durable catalog with reopen recovery:** `LocalCatalogStore` persists catalog snapshots with atomic file replacement, generation tracking, and crash validation. Verified by catalog recovery tests in `crates/htap-catalog/tests/catalog_recovery.rs`.
- **Synchronous `LocalServer` execution façade:** Direct in-process engine façade binding the catalog, `htap-txn` transaction manager, and `htap-rowstore` LSM engine. Supports `CREATE TABLE` (with deterministic one-partition row topology), literal `INSERT`, primary-key `DELETE`, and complete-PK `SELECT` with recovery and version progression across reopen. Verified in `crates/htap-server/tests/local_server.rs`.

### Explicitly deferred features

- **MySQL wire protocol and `htapd` daemon:** No MySQL wire protocol server, handshake, packet serialization, or daemon network listener is implemented. All interaction is via the synchronous in-process `LocalServer` API. MySQL wire compatibility is not claimed.
- **Sessions and explicit transaction control:** No interactive session management or multi-statement transactions (`BEGIN`, `COMMIT`, `ROLLBACK`). Every statement is executed as an autonomous synchronous operation.
- **Extended DML and DDL:** Non-PK mutations and schema alterations (`UPDATE`, `ALTER TABLE`, `DROP TABLE`) are deferred.
- **Analytical queries and SQL breadth (R4):** Full SQL breadth is not complete. Table scans, vectorized filter pushdown from SQL, aggregations (`GROUP BY`, `COUNT`, `SUM`), hash joins, common table expressions (`WITH` / CTEs), window functions (`OVER`), subqueries, and cost-based query optimization are deferred.
- **Columnstore SQL execution:** `htap-colstore` vectorized scans are not yet wired to the SQL execution layer; queries targeting columnstore or converting tables are rejected at route classification.
- **Multi-partition and distributed routing:** LocalServer supports only the local single-partition row topology. Partition pruning, distributed fanout, cross-node coordination, and scatter-gather execution are deferred to Phase 5 and Phase 6.
- **Broad MySQL compatibility:** Broad MySQL syntax, built-in functions, variable setting, system tables, and loose type coercions are deliberately unsupported.

---

## HTAP conversion local MVP scope and deferred features

The Phase 4 implementation delivers an incrementally verified local single-tablet Row-to-Column storage conversion engine (`htap-convert`), integrating with `htap-catalog`, `htap-rowstore`, `htap-colstore`, `htap-sql`, and `htap-server`. It implements a crash-resumable cutover state machine, durable tablet columnar manifest envelopes with CRC32C validation, atomic manifest and catalog publication, rowstore-authoritative base-plus-delta overlay scans, and online point mutations and point reads during and after conversion.

### Completed local MVP

- **Four-phase cutover state machine:** Deterministic state machine governing partition conversion:
  `SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`.
  - In `SnapshotPinned`, validates local partition topology (requiring exactly one tablet with one healthy leader replica), pins the rowstore visible version (or `Version(1)` if unwritten), allocates a catalog generation, and advances catalog storage to `StorageDescriptor::Converting { from: Row, to: Column }` via CAS.
  - In `SegmentsWritten`, reads rowstore rows at the pinned snapshot version, collapses versions and tombstones, writes encoded/compressed columnar segments, writes the tablet manifest, and records completion via catalog CAS.
  - In `ReadyToPublish`, bumps catalog generation and verifies manifest durability prior to cutover.
  - In `Column`, executes final catalog CAS cutover, clearing conversion descriptors and recording the tablet's `ColumnManifestRef`.
- **Atomic per-tablet manifest and catalog publication:** Manifest files use the `HTAPTBM1` binary envelope format with format versioning and payload CRC32C checksums (`HEADER_MAGIC = b"HTAPTBM1"`, `HEADER_LEN = 18`). Publication uses atomic staging (`MANIFEST.tmp` replaced to `MANIFEST`) followed by generation-checked catalog CAS updates. The manifest guarantees that only durable, readable segments are referenced, preventing partially written or orphaned segment exposure.
- **Rowstore authoritative base-plus-delta overlay:** `read_materialized_partition` executes base-plus-delta queries by reading columnar segments up to the conversion snapshot version and overlaying rowstore mutations (`Put` and `Delete`) committed after that base version up to the target snapshot. Historical reads for versions before the conversion base version are served directly from rowstore.
- **Online point writes and reads:** Storage descriptors (`Row`, `Converting`, `Column`) route point mutations (`INSERT`, `DELETE`) to `Route::RowstoreWrite` and complete-primary-key reads to `Route::RowstorePointRead`. Writers and point readers operate against the authoritative rowstore without interruption during or after conversion.
- **Crash resumption and idempotency:** If interrupted, conversion safely resumes: `SnapshotPinned` reuses the persisted pinned version and generation without re-snapshotting; `SegmentsWritten` and `ReadyToPublish` verify disk manifests without overwriting existing files or corrupting state.
- **Verified by test suite:**
  - Manifest envelope roundtrips, corruption, truncation, path traversal checks, and atomic tmp replacement: `crates/htap-convert/tests/tablet_manifest.rs`.
  - End-to-end conversion, post-conversion put/delete overlays, historical snapshot queries, empty partition handling, and phase CAS crash-resumption boundaries: `tests/materialization.rs` (`crates/htap-convert/tests/materialization.rs`).
  - Storage descriptor routing for Row, Converting, and Column: `crates/htap-sql/tests/route.rs`.
  - Online point mutations and point reads during Converting and Column storage: `crates/htap-server/tests/local_server.rs`.
  - Catalog conversion descriptor roundtrips, state validation, manifest path validation, and stale CAS rejection: `crates/htap-catalog/tests/catalog_recovery.rs` (`test_conversion_metadata_and_manifest_roundtrip_and_reopen`, `test_invalid_conversion_combinations`, `test_invalid_column_manifest_and_paths`, `test_conversion_stale_cas`).

### Explicitly deferred features

- **Reverse `Column -> Row` conversion:** Reverse conversion (`Column -> Row`) is not implemented and is explicitly rejected with `HtapError::Unsupported`. It is not claimed.
- **Columnar delete vectors:** Per-segment bitmap delete vectors on columnar files are deferred. Deletion semantics are handled via tombstones in the rowstore base-plus-delta overlay.
- **Physical rowstore reclamation:** Converted rows are not purged or garbage-collected from rowstore SSTs/WAL. Rowstore remains authoritative and retains all history.
- **Delta-to-base background compaction:** No automatic compaction folds accumulated rowstore deltas into new columnar segments.
- **SQL analytical scans:** Full table scans and analytical vectorized query execution over columnar or converting tables via SQL (`LocalServer`) are deferred (non-point queries return `InvalidArgument` or `Unsupported`).
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions, range/list partition boundaries, and distributed multi-node coordinated cutovers are deferred to Phase 5 and Phase 6.

---

## Scope

- The brief describes a production HTAP database engine: an LSM row store, a
  columnar engine, MySQL wire protocol, MPP execution, online transactional
  storage-format conversion, two coordination backends, active-active
  replication, a chaos suite, and TPC-C and TPC-H benchmarks. That is a system
  normally built by a team over an extended period.
- Per the brief's own guidance in its autonomy contract — prefer a smaller
  feature set that is correct, tested and runnable over a larger one that is
  stubbed — the work is delivered as an incrementally verified vertical slice
  that exercises all six hard requirements, rather than a broad but stubbed
  implementation.
- Every phase adds its own entries to this file as gaps are discovered.

---

## Status by phase

| Phase | Status | Known gaps |
| ----- | ------ | ---------- |
| Phase 0 — Research and workspace bootstrap | `Complete` | None. |
| Phase 1 — Row store | `Complete` | fsync durability unverified — see above: SIGKILL tests prove restart/replay integrity across abrupt process death, not physical power-loss durability. |
| Phase 2 — Columnar store | `Complete` | Standalone columnar segments, zone-map pruning, and vectorized scans implemented. Deferred to later phases: delta/delete vectors, MVCC visibility, conversion/catalog integration, richer predicates/joins/aggregates, Arrow/DataFusion, and atomic publication/manifest integration. |
| Phase 3 — SQL layer | `In progress` | Narrow local slice completed (sqlparser MySQL dialect, strict binder, structural rowstore route classifier, durable catalog with reopen recovery, synchronous `LocalServer` for `CREATE TABLE`, literal `INSERT`, PK `DELETE`, complete-PK `SELECT`). Deferred: MySQL wire protocol/`htapd` daemon, sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`, scans/aggregates/joins/CTEs/windows/subqueries, columnstore SQL execution, multi-partition routing, and broad MySQL compatibility. |
| Phase 4 — HTAP conversion | `Complete (local MVP)` | Completed local single-tablet Row-to-Column conversion MVP (`htap-convert`). Explicitly deferred: reverse `Column -> Row` conversion, delete vectors, physical rowstore reclamation, compaction, SQL analytical scans, and distributed partition/table conversion semantics. |
| Phase 5 — Data movement | `Complete (local MVP)` | Single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and LocalServer façade implemented. Deferred: SQL COPY syntax, MySQL wire protocol streaming, distributed multi-node coordinated migrations, background replication stream, and cross-partition movement. |
| Phase 6 — Distribution and coordination | `Not started` | — |
| Phase 7 — Hardening, benchmarks, chaos | `Not started` | — |

---

## Deviations from the brief

| Brief requirement | Deviation | Rationale | Where recorded |
| ----------------- | --------- | --------- | -------------- |
| ZooKeeper reference source at `examples/zookeeper` (§3 of the brief) | Input absent; ZooKeeper semantics derived from the ZooKeeper 3.9 specification and the `zookeeper-async` crate, and validated against a real ensemble in Docker rather than a mock. | The source was not supplied. A mock would most likely be wrong precisely on session expiry and ephemeral-node loss, which is the behaviour the coordination layer depends on. | ADR-006 in [`DECISIONS.md`](./DECISIONS.md); "Missing input: ZooKeeper reference source" above. |

> **This table must remain exhaustive.** Anything omitted or changed relative
> to the brief is recorded here or in an ADR, never silently dropped.
