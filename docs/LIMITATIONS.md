# Limitations and Known Gaps

This file is the running record of everything specified but not yet complete,
and every deviation from the project brief.

---

## Current Status: Hardened Local Embedded MVP

**The project is a hardened local embedded MVP, not a production-ready database system.**

Targeted hardening units have resolved critical and high-priority consistency blockers within local single-process execution boundaries:
- **C1:** WAL-GC transaction identity replay fixed by MANIFEST v2 external ledger (`f7a4975`).
- **C2:** Durable commit reversibility fixed by irrevocable transaction decision + `DurablePending` (`88cc314`).
- **H1:** Manager decision serialization fixed (`88cc314`).
- **H2:** Engine post-WAL failures now surface as `DurablePending` with retry/recovery (`c5ee281`).
- **H5/M2:** Owned persistence bounds and internal path validation added (`b7ff200`).
- **Exclusive Root Ownership:** Single-process exclusive root ownership added (`1083fbd`), operating in one-owner multiprocess-exclusive mode, not concurrent shared-root writers.

The following architectural limitations remain explicitly open:
- No journal/ledger compaction or coordinated retention; ledger hard cap eventually blocks new external applies.
- Possible later flush-boundary duplicate SST publication after crash before reader/checkpoint, requiring future staged flush recovery.
- No power-loss proof (testing bounded by process `SIGKILL`).
- No distributed consensus/Raft/ZK/remote replica serving or real HA.
- Whole-dataset materialization in conversion/export/clone.
- No network/MySQL daemon/auth/security boundary/full SQL analytics.
- External `CopyOptions` paths remain caller-controlled by design.

Production readiness is **not claimed**.

---

## Missing input: ZooKeeper reference source

The project brief specified `examples/zookeeper` containing Apache ZooKeeper
source, to be used both for understanding ZAB, session and ephemeral-node
semantics, watches and the client wire protocol, and for running a real
ZooKeeper ensemble in integration tests. **That directory was not present.**
The provided `examples/` directory contained only `starrocks/`.

### Impact and status

In addition to `examples/zookeeper` being absent, no ZooKeeper backend,
`zookeeper-async` dependency, Docker configuration, or real ensemble integration
test exists in the repository. Coordination is implemented exclusively as a
single-node local coordinator (`htap-coord::LocalCoordinator`) persisting state to
`COORDINATOR` binary envelopes (`HTAPCRD1`).

An external ZooKeeper coordination backend, watches, session heartbeats, and
Docker-based ensemble testing remain deferred as future work. Distributed
consensus (Raft / ZooKeeper) and real high availability are not implemented.

### Completion plan

If distributed multi-node coordination is implemented in the future, provide an
adapter implementing the `Coordinator` trait backed by ZooKeeper (or Raft), along
with an optional containerized ensemble test environment.

Cross-reference: ADR-006 in [`DECISIONS.md`](./DECISIONS.md).

---

## Rowstore external coordination & transaction durability

The transaction manager (`htap-txn`) coordinates two-phase commit across participants, using `RowstoreParticipant` to wrap an LSM `Engine`. Hardening units have established transaction irrevocability and identity persistence:

- **C1 fixed (`f7a4975`):** Rowstore `MANIFEST` v2 incorporates an external apply ledger that records external transaction IDs and applied versions. Even after WAL prefix garbage collection deletes older WAL segments, the ledger persists across reopen and prevents transaction identity replay or duplicate apply hazards.
- **C2 & H1 fixed (`88cc314`):** Transaction decisions are strictly serialized under a manager-wide lock across prepare, intent, version allocation, commit, participant apply, publish, and recovery. Once a commit is synced to `txn.journal`, the decision is irrevocable. Post-decision failures return `DurablePending` rather than rolling back or reporting an abort.
- **H2 fixed (`c5ee281`):** Engine post-WAL failures (such as memtable allocation, automatic SST flush, or visible marker update errors) return `DurablePending`. Committed and applied data is preserved in immutable memtables and retried before accepting new writes. Reopen recovery completes publication idempotently.

### Remaining rowstore & transaction gaps

- **No journal/ledger compaction or coordinated retention; ledger hard cap blocks external applies:**
  Neither `txn.journal` nor the `MANIFEST` v2 external apply ledger implements compaction or coordinated retention. The external ledger has a hard capacity cap (`MAX_EXTERNAL_LEDGER_ENTRIES = 16,384`). When the ledger is filled, new external transaction applies fail with `HtapError::CapacityExceeded`. Truncation coordinated with participant checkpoints remains unimplemented.
- **Possible later flush-boundary duplicate SST publication after crash:**
  If a crash occurs after an SST file is published to disk but before reader registration, manifest commit, or checkpoint advancement, a subsequent reopen/flush cycle may republish duplicate SST data. Full resolution requires future staged flush recovery.
- **Dense contiguous versions constraint:**
  Rowstore MVCC visibility uses a single scalar watermark (`visible_version`) and enforces sequential monotonic progression (`version == visible_version.next()`). Multi-participant distributed transactions with sparse version gaps across partitions remain unsupported.

---

## Durability testing is bounded by process-level fault injection (no power-loss proof)

The write-ahead log and LSM engine in `htap-rowstore` are covered by integration
tests (`crates/htap-rowstore/tests/wal_crash.rs`, test
`kill_9_loses_no_committed_data`, and `crates/htap-rowstore/tests/engine_crash.rs`,
test `engine_kill_9_recovers_all_reported_commits`) that spawn a real child process,
let it durably commit transactions and report their ids (including periodic SST flushes),
then terminate it with `SIGKILL` and assert that every reported commit is recovered upon
reopening.

### What this proves, and what it does not

The test proves **replay integrity across abrupt process death**. It does not
prove **fsync durability or power-loss resilience**. This was verified by mutation testing:

| Mutation | Result |
| -------- | ------ |
| Drop the `write_all` in `append()` | Test FAILS (correctly detects data loss) |
| Stub `sync()` to a no-op | Test still PASSES (does not detect the bug) |

The reason is that `SIGKILL` destroys the process but not the operating system
page cache. Bytes written with `write_all` but never fsynced remain readable by
a subsequent reader on the same machine. Only a machine-level failure — power
loss, kernel panic, or a simulated block-device failure — distinguishes the two
cases. There is **no power-loss proof**.

### Completion plan

To close this gap, either (a) run the crash child inside a VM or container
whose storage is dropped without flushing, (b) interpose a FUSE or
device-mapper layer that discards non-fsynced writes on fault injection, or
(c) use a filesystem fault-injection tool such as `dm-flakey` in a future
storage chaos test harness (deferred from local MVP).

Until one of these is in place, the fsync path is verified by code inspection
and process crash tests only.

---

## Column-store MVP scope and deferred features

The Phase 2 `htap-colstore` implementation delivers the core columnar segment storage format, encoding, compression, zone-map pushdown filtering, and vectorized scans. The following column-store features are deferred to subsequent phases:

- **Delta and delete vectors:** Columnar segments are currently immutable write-once files. Row-level deletions via bitmap delete vectors and merge-on-read delta store integration are deferred.
- **MVCC visibility:** Column segments currently store durable committed rows without transactional version ranges (`htap-common::Version`) per row. Snapshot isolation visibility across columnar data is deferred.
- **Conversion and catalog integration:** Standalone segment read/write and scan execution are functional, but online row-to-column transcoding and tablet catalog metadata registration are handled in `htap-convert` / `htap-catalog`.
- **Richer predicates, joins, and aggregations:** The scan engine supports basic SQL 3-valued comparison predicates (`Eq`, `Lt`, `Lte`, `Gt`, `Gte`, `IsNull`, `IsNotNull`) on single columns with conservative zone-map skipping. Compound predicate expression trees (AND/OR), hash joins, group-by, and vectorized aggregations are deferred.
- **Arrow and DataFusion integration:** Scans yield internal typed `RecordBatch` and `ColumnVector` structures. Exporting to Apache Arrow RecordBatches and DataFusion `TableProvider` / `ExecutionPlan` integration are deferred.
- **Atomic publication and manifest integration:** Segments are managed individually at filesystem paths. Multi-segment manifest commits, atomic segment swaps, and compaction lifecycle tracking are deferred.

---

## SQL layer narrow local slice scope and deferred features

The Phase 3 implementation delivers a verified, crash-safe narrow local SQL slice integrating parsing, catalog binding, route classification, transactional DML execution, and point reads. It does not implement full SQL breadth, analytical query execution, or client network protocols.

### Completed narrow local slice

- **sqlparser MySQL dialect:** Strict single-statement parsing using `sqlparser::dialect::MySqlDialect`, accepting valid backtick identifiers and MySQL escape semantics while rejecting empty input, malformed SQL, and multi-statement input with stable `InvalidArgument` errors. Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Strict catalog binder:** Schema-validated binding for `CREATE TABLE` (scalar types and primary keys), literal schema-ordered `INSERT`, complete-primary-key `DELETE`, and complete-primary-key `SELECT`. Strictly rejects unsupported data types, composite key mismatches, expression evaluations, implicit coercions, and unhandled clauses. Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Structural rowstore route classifier:** Inspects bound statements and storage descriptors, classifying complete-PK queries as `Route::RowstorePointRead` and single-partition mutations as `Route::RowstoreWrite` across `Row`, `Column`, and `Converting` descriptors, keeping the rowstore authoritative across all conversion states. Verified in `crates/htap-sql/tests/route.rs` and `crates/htap-server/tests/local_server.rs`.
- **Durable catalog with reopen recovery:** `LocalCatalogStore` persists catalog snapshots with atomic file replacement, generation tracking, and crash validation. Verified by catalog recovery tests in `crates/htap-catalog/tests/catalog_recovery.rs`.
- **Synchronous `LocalServer` execution façade:** Direct in-process engine façade binding the catalog, `htap-txn` transaction manager, and `htap-rowstore` LSM engine. Supports `CREATE TABLE` (with deterministic one-partition row topology), literal `INSERT`, primary-key `DELETE`, and complete-PK `SELECT` with recovery and version progression across reopen. Verified in `crates/htap-server/tests/local_server.rs`.

### Client / Server Boundary and Error Categorization

`EmbeddedClient` (`htap-client`) wraps `LocalServer` (`htap-server`) synchronously within the host process, with no intermediate RPC, serialization, or daemon layer. Execution maps strictly to stable `HtapError` categories verified by integration tests (`crates/htap-client/tests/embedded_client.rs`):

| Failure / Execution Scenario | Example Statement | Error Category / Result |
| ---------------------------- | ----------------- | ----------------------- |
| Syntax parse error | `NOT A VALID SQL STATEMENT;` | `HtapError::InvalidArgument` |
| Empty or whitespace SQL | `   ;  ` | `HtapError::InvalidArgument` |
| Duplicate table creation | `CREATE TABLE products (id BIGINT PRIMARY KEY, name VARCHAR, price INT);` | `HtapError::Conflict` |
| Missing / nonexistent table | `SELECT * FROM nonexistent_table WHERE id = 1;` | `HtapError::NotFound` |
| Full table scan without complete PK | `SELECT * FROM products;` | `HtapError::InvalidArgument` |
| Unsupported DML operation | `UPDATE products SET price = 99 WHERE id = 1;` | `HtapError::Unsupported` |
| Unsupported query modifiers | `SELECT name FROM products WHERE id = 1 ORDER BY price;` | `HtapError::Unsupported` |
| Unsupported query clauses | `SELECT name FROM products WHERE id = 1 GROUP BY name;` | `HtapError::Unsupported` |
| Concurrent process root access | Opening an already locked storage root or symlink alias | `HtapError::Conflict` |
| Absent primary key lookup | `SELECT name, age FROM users WHERE id = 9999;` | Returns empty `QueryResult` (`qr.is_empty() == true`, `qr.num_rows() == 0`, column metadata preserved) |

### Subsystem Decoupling: Converter and Coordinator Not Created by LocalServer or EmbeddedClient

Neither `LocalServer::open` nor `EmbeddedClient::open` creates, configures, or supervises `LocalConverter` (`crates/htap-convert`) or `LocalCoordinator` (`crates/htap-coord`):
- `LocalServer` initializes only the local catalog store (`LocalCatalogStore`), rowstore engine (`htap_rowstore::Engine`), transaction manager (`TransactionManager`), and data mover (`LocalDataMover`).
- `LocalConverter` is a standalone partition-scoped conversion engine used for transcoding rowstore tables into columnar segments and advancing catalog cutover state machines.
- `LocalCoordinator` is an independent cluster coordination and placement engine managing node registration, leadership leases, and monotonic fencing tokens at `<coord_root>/COORDINATOR`.
Workflows requiring format conversion or coordinator-fenced catalog CAS instantiate `LocalConverter` or `LocalCoordinator` explicitly outside the server façade.

### Documentation Mermaid Convention

All architectural call flows, DML transaction sequences, and initialization state machines in documentation follow standard GitHub-compatible Mermaid `flowchart` and `sequenceDiagram` syntax without unsupported extensions.

### Explicitly deferred features (No network/MySQL daemon/auth/security boundary/full SQL analytics)

- **MySQL wire protocol and `htapd` daemon:** No MySQL wire protocol server, handshake, packet serialization, or daemon network listener is implemented. All interaction is via the synchronous in-process `LocalServer` API. MySQL wire compatibility is not claimed.
- **No authentication or security boundary:** No user authentication, TLS, or RBAC security boundary is implemented.
- **Sessions and explicit transaction control:** No interactive session management or multi-statement transactions (`BEGIN`, `COMMIT`, `ROLLBACK`). Every statement is executed as an autonomous synchronous operation.
- **Extended DML and DDL:** Non-PK mutations and schema alterations (`UPDATE`, `ALTER TABLE`, `DROP TABLE`) are deferred.
- **Analytical queries and SQL breadth (R4):** Full SQL breadth is not complete. Table scans, vectorized filter pushdown from SQL, aggregations (`GROUP BY`, `COUNT`, `SUM`), hash joins, common table expressions (`WITH` / CTEs), window functions (`OVER`), subqueries, and cost-based query optimization are deferred.
- **Columnstore SQL execution:** `htap-colstore` vectorized scans are not yet wired to the SQL execution layer; queries attempting full table scans or analytical scans targeting any table (Row, Column, or Converting) are rejected at semantic binding or route classification with `InvalidArgument` or `Unsupported`.
- **Multi-partition and distributed routing:** LocalServer supports only the local single-partition row topology. Partition pruning, distributed fanout, cross-node coordination, and scatter-gather execution are deferred.
- **Broad MySQL compatibility:** Broad MySQL syntax, built-in functions, variable setting, system tables, and loose type coercions are deliberately unsupported.

---

## HTAP conversion local MVP scope and deferred features

The Phase 4 implementation delivers an incrementally verified local single-tablet Row-to-Column storage conversion engine (`htap-convert`), integrating with `htap-catalog`, `htap-rowstore`, `htap-colstore`, `htap-sql`, and `htap-server`. It implements a crash-resumable cutover state machine, durable tablet columnar manifest envelopes with CRC32C validation, atomic manifest and catalog publication, rowstore-authoritative base-plus-delta overlay scans, and online point mutations and point reads during and after conversion.

### Completed local MVP

- **Four-phase cutover state machine:** Deterministic state machine governing partition conversion:
  `SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`.
- **Atomic per-tablet manifest and catalog publication:** Manifest files use the `HTAPTBM1` binary envelope format with format versioning and payload CRC32C checksums. Publication uses atomic staging (`MANIFEST.tmp` replaced to `MANIFEST`) followed by generation-checked catalog CAS updates.
- **Rowstore authoritative base-plus-delta overlay:** `read_materialized_partition` executes base-plus-delta queries by reading columnar segments up to the conversion snapshot version and overlaying rowstore mutations committed after that base version.
- **Online point writes and reads:** Storage descriptors route point mutations to `Route::RowstoreWrite` and complete-PK reads to `Route::RowstorePointRead` without interruption during conversion.
- **Crash resumption and idempotency:** Interrupted conversion resumes safely from persisted state envelopes.

### Explicitly deferred features

- **Whole-dataset materialization:** Partition conversion reads and materializes the entire rowset into memory and intermediate segment files rather than using a streaming or chunked conversion pipeline.
- **Reverse `Column -> Row` conversion:** Reverse conversion is not implemented and is rejected with `HtapError::Unsupported`.
- **Columnar delete vectors:** Per-segment bitmap delete vectors on columnar files are deferred. Deletion semantics are handled via tombstones in the rowstore base-plus-delta overlay.
- **Physical rowstore reclamation:** Converted rows are not purged or garbage-collected from rowstore SSTs/WAL. Rowstore remains authoritative and retains all history.
- **Delta-to-base background compaction:** No automatic compaction folds accumulated rowstore deltas into new columnar segments.
- **SQL analytical scans:** Full table scans and analytical vectorized query execution over columnar or converting tables via SQL (`LocalServer`) are deferred.
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions, range/list partition boundaries, and distributed cutovers are deferred.

---

## Data movement and persistence bounds scope and deferred features

The Phase 5 implementation delivers single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and `LocalServer` integration. Hardening unit H5/M2 (`b7ff200`) added strict persistence boundaries:

- **H5/M2 fixed (`b7ff200`):** Shared bounded exact-file reader (`read_exact_bounded`) enforces size limits on all owned state files (`CATALOG`, `COORDINATOR`, `movement/jobs/<job-id>/JOB`, tablet manifests, `MANIFEST`, `VISIBLE`, `txn.journal`). Internal movement job IDs, tablet paths, and conversion segment paths are strictly validated against traversal and injection attacks.
- **External CopyOptions paths remain caller-controlled by design:** While internal persistence files and paths are bounded and validated, external filesystem paths supplied via `CopyOptions` (e.g. CSV/JSONL import sources and export destinations) are caller-controlled by design.
- **Whole-dataset materialization in export and clone:** CSV/JSONL export and tablet snapshot cloning materialize whole datasets in intermediate buffers/files rather than streaming records.
- **Deferred features:** Distributed multi-node coordinated migrations, background replication streams, and cross-partition movement remain deferred.

---

## Coordination and distribution local MVP scope and deferred features

The Phase 6 implementation delivers local coordination, leadership fencing, deterministic placement planning, and coordinator-fenced replica activation (`htap-coord`).

### Completed local MVP

- **Synchronous `Coordinator` trait and durable `LocalCoordinator`:** Synchronous API managing cluster membership, scoped leadership leases, strictly monotonic fencing tokens (`FencingToken`), and coordinator-fenced catalog CAS.
- **Durable `HTAPCRD1` state envelope:** State is persisted at `<root>/COORDINATOR` using a versioned binary envelope with CRC32C checksum and bounded reading (`b7ff200`).
- **Deterministic sorted membership:** Registered cluster nodes (`NodeId`) are tracked in deterministic ascending order.
- **Strictly monotonic fencing tokens:** Persisted high-water marks ensure tokens are never reused across restart.
- **Deterministic placement planner (`plan_placement`):** Computes balanced, colocation-free replica placement plans.
- **Exclusive root ownership (`1083fbd`):** `LocalServer` and `LocalCoordinator` acquire an OS-level non-blocking advisory lock (`<root>/LOCK` via `flock`) upon opening to reject concurrent process opens (and symlink aliases) with `HtapError::Conflict`.

### Explicitly deferred features and boundaries

- **One-owner multiprocess-exclusive mode (not concurrent shared-root writers):** Exclusive root ownership prevents multiple processes from accessing the same root directory simultaneously. Concurrent shared-root writers or readers are strictly unsupported. Standalone low-level subsystem instances (`Engine::open`, `LocalCatalogStore::open`, `LocalDataMover::new`) opened directly outside `LocalServer` do not acquire this lock and remain unsafe for shared-root concurrent use.
- **No distributed consensus or real HA:** Neither Raft (`openraft`) nor ZooKeeper backends are implemented. No remote replica serving, cluster heartbeats, ephemeral sessions, or live failover exists.
- **Direct `CatalogStore` CAS and older movement repair APIs bypass coordinator fence:** Callers invoking `CatalogStore::compare_and_set` directly bypass coordinator leadership fencing. Fencing is strictly opt-in via `Coordinator::fenced_catalog_compare_and_set`.

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
| Phase 1 — Row store | `Complete (hardened local MVP)` | C1 fixed by MANIFEST v2 ledger (`f7a4975`); H2 fixed by DurablePending retry/recovery (`c5ee281`). Open: no power-loss proof; no ledger compaction (hard cap eventually blocks external applies); possible flush-boundary duplicate SST publication after crash before checkpoint. |
| Phase 2 — Columnar store | `Complete` | Standalone columnar segments, zone-map pruning, and vectorized scans implemented. Deferred: delta/delete vectors, MVCC visibility, conversion/catalog integration, richer predicates/joins/aggregates, Arrow/DataFusion, and atomic publication/manifest integration. |
| Phase 3 — SQL layer | `In progress` | Narrow local slice completed (sqlparser MySQL dialect, strict binder, structural rowstore route classifier, durable catalog with reopen recovery, synchronous `LocalServer` for `CREATE TABLE`, literal `INSERT`, PK `DELETE`, complete-PK `SELECT`). Deferred: MySQL wire protocol/`htapd` daemon, network sockets, auth/security boundary, sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`, scans/aggregates/joins/CTEs/windows/subqueries, columnstore SQL execution, multi-partition routing, and broad MySQL compatibility. |
| Phase 4 — HTAP conversion | `Complete (local MVP)` | Completed local single-tablet Row-to-Column conversion MVP (`htap-convert`). Open: whole-dataset materialization in conversion; reverse `Column -> Row` conversion deferred; delete vectors, physical rowstore reclamation, compaction, SQL analytical scans, and distributed partition/table conversion semantics deferred. |
| Phase 5 — Data movement | `Complete (local MVP)` | Single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and LocalServer façade implemented. H5/M2 bounds and internal path validation added (`b7ff200`). Open: whole-dataset materialization in export/clone; external `CopyOptions` paths caller-controlled by design; SQL COPY syntax, MySQL wire protocol streaming, distributed multi-node coordinated migrations, background replication stream, and cross-partition movement deferred. |
| Phase 6 — Distribution and coordination | `Complete (local MVP)` | LocalCoordinator (`HTAPCRD1`), monotonic fencing tokens, coordinator-fenced catalog CAS, deterministic placement planner, and local activation simulation implemented. Exclusive root ownership via `<root>/LOCK` added (`1083fbd`) as one-owner multiprocess-exclusive mode (not concurrent shared-root writers). H5/M2 envelope bounds added (`b7ff200`). Deferred: Raft/openraft, ZooKeeper backend, watches/locks/KV semantics, distributed consensus, remote replica serving, real HA, leader handoff, ongoing replication, capacity/rack placement, and live rebalance; standalone low-level components remain unlocked. |
| Phase 7 — Hardening, benchmarks, local MVP | `Complete (hardened local MVP)` | Hardened transaction commit irrevocability + DurablePending (C2/H1 in `88cc314`), manager decision serialization (H1 in `88cc314`), external apply identity ledger across WAL GC (C1 in `f7a4975`), Engine post-WAL retry/recovery (H2 in `c5ee281`), owned persistence bounds/internal path validation (H5/M2 in `b7ff200`), and exclusive root ownership (`1083fbd`). Built Criterion microbenchmarks (`htap-bench`, `local_mvp`), synchronous embedded client (`htap-client`), operational documentation. Project is a hardened local embedded MVP; production readiness is not claimed. |

---

## Deviations from the brief

| Brief requirement | Deviation | Rationale | Where recorded |
| ----------------- | --------- | --------- | -------------- |
| ZooKeeper reference source at `examples/zookeeper` (§3 of the brief) | Input absent; ZooKeeper backend, `zookeeper-async` dependency, and Docker ensemble tests are not implemented in the local MVP. Coordination is implemented locally via `htap-coord::LocalCoordinator`. ZooKeeper backend and containerized testing remain deferred future work. | `examples/zookeeper` was not supplied; cluster coordination was scoped to a single-node local coordinator MVP. | ADR-006 in [`DECISIONS.md`](./DECISIONS.md); "Missing input: ZooKeeper reference source" above. |

> **This table must remain exhaustive.** Anything omitted or changed relative
> to the brief is recorded here or in an ADR, never silently dropped.
