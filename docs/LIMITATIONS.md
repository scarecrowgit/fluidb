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
- **Owner plus IPC (Phase 16, ADR-025):** A second and later process opening an already-owned root now becomes an IPC client that forwards SQL/session calls to the owner over `<root>/htap.sock`, instead of failing outright; exactly one process still ever touches storage directly.

The following architectural limitations remain explicitly open:
- As of Phase 15, `txn.journal` itself is checkpointed/compacted (`TransactionManager::checkpoint()`, a new `HTAPTXC1` baseline envelope) — see "Transaction journal checkpoint scope and deferred features" below. The `MANIFEST` v2 external-apply ledger's own hard cap is unrelated and unaddressed: it still eventually blocks new external applies once `MAX_APPLIED_EXTERNAL_TXNS` entries are used, with no compaction of that cap. Do not conflate the two.
- Possible later flush-boundary duplicate SST publication after crash before reader/checkpoint, requiring future staged flush recovery.
- No power-loss proof (testing bounded by process `SIGKILL`).
- No distributed consensus/Raft/ZK/remote replica serving or real HA.
- Whole-dataset materialization in conversion, export (exports materialize full logical partition before writing), and clone.
- No full SQL analytics. A network MySQL daemon (`htapd`/`htap-wire`) is now implemented (Phase 8) with a
  narrow security model (loopback default, catalog-backed per-user accounts as of Phase 12, optional TLS as of
  Phase 12 — see below); it does not add broader SQL support. Phase 9 added a general query executor (joins, expressions,
  subqueries, `UNION`, `UPDATE`, `DROP TABLE`, `SHOW`/`DESCRIBE`); Phase 13 extended it with window functions,
  depth-1 correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, arbitrarily nested join trees,
  `WITH RECURSIVE`, `EXCEPT`/`INTERSECT`, `GROUP BY`/`ORDER BY` ordinals, filtered `DELETE`, `TRUNCATE`,
  `INSERT ... SELECT`, and integer `DIV` (see ADR-022); Phase 14 added `ANALYZE TABLE` statistics, a
  statistics-driven cost-based optimizer stage (enabled by default), `EXPLAIN`/`EXPLAIN ANALYZE`, disk
  spilling under a per-statement memory budget, and bounded parallelism for `GROUP BY`/`INNER`/`CROSS` joins
  (see ADR-023) — but it is still not full SQL analytics: no vectorized execution, and no worker-pool
  parallelism for `LEFT`/`RIGHT`/`FULL` joins (memory-bounded spilling itself is not kind-restricted in code,
  but is exercised by a test only for `INNER` joins — see "General query executor scope
  and deferred features" below). Phase 10 added
  server-side sessions and explicit transactions (`BEGIN`/`COMMIT`/`ROLLBACK`, `autocommit`, session
  variables), both in-process and over the wire — see "Sessions and explicit transaction control" below for
  the exact contract and remaining gaps (no locking reads, no idle-transaction
  reaping, `REPEATABLE READ` only). Phase 11 added the MySQL binary protocol and prepared statements
  (`COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`/`SEND_LONG_DATA`), `COM_RESET_CONNECTION`/`COM_CHANGE_USER`,
  ≥16 MiB message reassembly with a real `max_allowed_packet`, a CSPRNG handshake scramble, negotiated
  `CLIENT_MULTI_STATEMENTS`, and shutdown force-close — see "Prepared statements and binary protocol scope
  and deferred features" below for the exact contract and remaining gaps (server-side cursors, unsigned
  64-bit values above `i64::MAX`, exact `DECIMAL`, `TIME`-typed parameters, best-effort `PREPARE` metadata).
  Phase 12 added TLS, MySQL protocol compression, and catalog-backed per-user accounts/privileges — see
  "TLS, compression, and account/privilege scope and deferred features" below for the exact contract and
  remaining gaps (no roles/delegated administration, `%`-only host, `mysql_native_password` only, no
  `SIGHUP`-triggered TLS cert reload).
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

- **No shared multi-format atomic WAL:**
  There is no single shared WAL that atomically commits writes across both row and column formats within a single transaction. In the current implementation, the rowstore remains authoritative for all mutations, and transactions commit exclusively through `TransactionManager` using `RowstoreParticipant` (`crates/htap-server/src/lib.rs`). Columnar storage is generated via partition-scoped conversion (`htap-convert`), with columnar segments and tablet manifests published separately via catalog CAS updates. Single transactions touching both formats simultaneously are deferred.
- **`txn.journal` is checkpointed as of Phase 15; the `MANIFEST` v2 external-apply ledger's hard cap is a separate, still-open gap.** `TransactionManager::checkpoint()` compacts `txn.journal` by dropping resolved `Intent`/`Commit`/`Abort` records past a durable checkpoint baseline (`txn.checkpoint`, `HTAPTXC1`) — see "Transaction journal checkpoint scope and deferred features" below for the full contract. This does **not** touch the rowstore `MANIFEST` v2 external apply ledger, which still has no compaction or coordinated retention and still enforces a hard capacity cap (`MAX_APPLIED_EXTERNAL_TXNS = 1_000_000`). When the ledger is filled, new external transaction applies fail with `HtapError::InvalidArgument` (not a dedicated `CapacityExceeded` variant, which does not exist in `HtapError`); a Phase 10 fix pass moved this check into `Engine::prepare` as well, so a real 2PC/direct-commit transaction is rejected before any journal write rather than only at apply time (`crates/htap-txn/tests/two_phase_commit.rs::test_ledger_full_commit_rejected_at_prepare_before_journal_growth`). Truncation of the external-apply ledger coordinated with participant checkpoints remains unimplemented.
- **Effective 2PC transaction payload cap is about 4 MiB, not the nominal 16 MiB:**
  `htap_txn::participant::MAX_PAYLOAD_SIZE` (16 MiB) bounds the raw mutation JSON, but the durable journal
  `Intent` frame re-encodes each participant's payload as a JSON array of decimal byte values
  (`serde_json`'s default `Vec<u8>` encoding), roughly 3-4x larger than the raw bytes, inside a journal frame
  itself capped at `DEFAULT_MAX_FRAME_SIZE` (16 MiB). `TransactionManager::commit` now rejects with
  `HtapError::InvalidArgument` before prepare if a conservative bound on the resulting frame size
  (`intent_frame_size_bound`, `crates/htap-txn/src/journal.rs`) would exceed the journal's frame limit; the
  same check runs per statement in `WriteSet::try_merge` and in `Session::commit` before the transaction is
  removed from the open state (so a rejected `COMMIT` leaves it open), and covers autocommit statements as
  well. `intent_frame_size_bound` also scales its conservative overhead by participant count (128 bytes per
  participant, on top of a fixed 512-byte overhead) since a transaction with many small-payload participants
  can add more JSON punctuation than one fixed constant covers — a fix pass that lowered the effective single-
  participant cap slightly. Working the bound backward for a single-participant transaction (the only shape
  any current 2PC transaction uses; `RowstoreParticipant` is the sole registered participant), the effective
  cap on raw mutation payload bytes is 4,194,143 bytes (about 4 MiB), not 16 MiB. A compact (non-JSON-array)
  payload encoding in the journal frame would raise this back toward the nominal 16 MiB, but would require a
  journal format version bump and is deferred. See ADR-018's second and third fix passes and
  `crates/htap-server/tests/session.rs::test_commit_of_write_set_exceeding_intent_frame_is_rejected_and_txn_stays_open`,
  `test_autocommit_oversize_insert_rejected_cleanly_before_any_journal_write`.
- **`txn.journal` is checkpointed as of Phase 15, but `max_journal_size` is still checked only at open, and
  checkpointing is best-effort, not guaranteed.** `Journal` checks its total file size against
  `max_journal_size` (`DEFAULT_MAX_JOURNAL_SIZE` = 64 MiB) in `Journal::open_with_options` and in `Journal::scan`
  (which only ordinary `open`/`repair_torn_final`/`recover_records` call — an `append`/`append_nosync`/`sync`
  never re-checks total file size). `TransactionManager::checkpoint()` now drops resolved `Intent`/`Commit`/
  `Abort` records past a durable baseline (`txn.checkpoint`, `HTAPTXC1`), fired opportunistically after a commit
  once the journal's valid byte count exceeds half of `configured_max_journal_size`
  (`with_checkpoint_trigger_bytes`) and finalized once at `LocalServer::open` (`finalize_open`). This closes the
  unconditional-growth case for the common workload, but does not make growth-past-the-limit impossible: a
  workload dominated by long-lived, still-unresolved `Intent`s (no matching `Commit`/`Abort` yet) has nothing
  for `checkpoint()` to drop, and `checkpoint()`/`finalize_open()` both refuse outright (rather than partially
  compacting) while recovery is required or the journal is poisoned. `Journal::open_for_bootstrap` temporarily
  raises the read-time ceiling to 2 GiB during `TransactionManager::open`/`recover()` so an already-oversized
  journal can still be read and folded once, but `finalize_open`'s final re-open at the *configured* limit still
  fails with `HtapError::Corruption` if that one checkpoint could not shrink the file below it.
- **A `JournalIo`-caused `RecoveryLatch` now also comes from a failed `Intent`/`Abort` journal write, and the
  underlying `Journal` can independently poison itself:** see ADR-018's third fix pass. While either the
  manager's latch or the journal's own `is_poisoned()` state holds, `TransactionManager::recover()` refuses
  outright with `HtapError::RecoveryRequired` and applies nothing — it does not attempt a partial replay.
  Because `LocalServer` only ever calls `recover()` at `open` (never during normal operation), a
  `RecoveryCause::ParticipantIo` latch — even though `recover()` is in principle capable of clearing it
  in-process — is in practice cleared only by restarting the process, exactly like a `JournalIo` latch or a
  poisoned journal.
- **After a journal `fsync` error, restarting the process alone is not proof of durability:** a fresh
  `Journal::open` call opens a brand-new file descriptor, which will not report whatever error the earlier,
  now-closed descriptor's `fsync` returned; the `Commit` (or `Intent`/`Abort`) record that failed to sync may
  still be sitting only in the OS page cache when the process exits, not on stable storage. The safe operator
  action after such an error is to reboot the host (or otherwise ensure the page cache backing the journal's
  filesystem is dropped) before reopening the server, not merely restart the process on the same still-warm
  page cache. If the journal and the rowstore engine have genuinely diverged (e.g. because of a restore from
  backups taken at different times, not a routine fsync failure), the open-time `committed_version()`
  cross-check (see "Reopen Recovery Guarantees" in `docs/OPERATIONS.md`) fails closed with
  `HtapError::Corruption` rather than silently starting up on an inconsistent state.
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

The Phase 3 implementation delivers a verified, crash-safe local SQL slice integrating parsing, catalog binding, route classification, transactional DML execution, complete-PK point reads, and narrow single-table analytical scan execution with compact base scan pushdown. It does not implement full SQL breadth, vectorized aggregation / operator pipelines, or client network protocols.

### Completed narrow local slice

- **sqlparser MySQL dialect:** Strict single-statement parsing using `sqlparser::dialect::MySqlDialect`, accepting valid backtick identifiers and MySQL escape semantics while `htap_sql::parse_one` rejects empty input, malformed SQL, and multi-statement input with stable `InvalidArgument` errors. Verified in `crates/htap-sql/tests/parse_bind.rs`. (Phase 11 added `htap_sql::parse_many`, which splits multi-statement text and calls `parse_one` on each piece, backing the wire layer's negotiated `CLIENT_MULTI_STATEMENTS` batches — see "Prepared statements and binary protocol scope and deferred features" below; `parse_one` itself is unchanged and still rejects multi-statement text.)
- **Strict catalog binder:** Schema-validated binding for `CREATE TABLE` (scalar types and primary keys), literal schema-ordered `INSERT`, complete-primary-key `DELETE`, complete-primary-key `SELECT` (`PointSelect`), and typed `AnalyticSelect`. Strictly rejects unsupported data types, composite key mismatches, implicit coercions, and unhandled clauses (joins, CTEs, window functions, `LIMIT`/`OFFSET`, `HAVING`, `OR`, arithmetic; in `ORDER BY`, expressions, aliases, aggregate ordering, and non-AnalyticSelect usage are rejected while simple unqualified source/projected column `ORDER BY` with ASC/DESC and NULLS FIRST/LAST/default policy is supported for `AnalyticSelect`). Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Structural route classifier:** Inspects bound statements and storage descriptors, classifying complete-PK queries as `Route::RowstorePointRead`, literal `INSERT` as `Route::RowstoreWrite`, and analytical selects as `Route::OlapScan` across `Row`, `Column`, and `Converting` descriptors. Complete-PK lookups strictly take the rowstore fast path, remaining separate and unchanged, and bypass OLAP execution and the converter. Since Phase 13, `DELETE` (point-key or filtered, including `TRUNCATE`) routes separately to `Route::RowstoreDelete { key: Option<Vec<u8>> }`, mirroring `Route::RowstoreUpdate` rather than sharing `Route::RowstoreWrite` with `INSERT` — see "General query executor scope and deferred features" below. Verified in `crates/htap-sql/tests/route.rs` and `crates/htap-server/tests/local_server.rs`.
- **Durable catalog with reopen recovery:** `LocalCatalogStore` persists catalog snapshots with atomic file replacement, generation tracking, and crash validation. Verified by catalog recovery tests in `crates/htap-catalog/tests/catalog_recovery.rs`.
- **Synchronous `LocalServer` execution façade:** Direct in-process engine façade binding the catalog, `htap-txn` transaction manager, and `htap-rowstore` LSM engine. Supports unpartitioned tables (via `CREATE TABLE` with default single-partition row topology) and partitioned tables (via SQL `CREATE TABLE ... PARTITION BY RANGE/LIST` or native `LocalServer::create_partitioned_table` with finite Range or List topology). Executes literal `INSERT` (routing multi-row inserts by partition key across partitions in a single commit version), primary-key `DELETE` (routed by partition key), complete-PK `SELECT` (routed by partition key, strictly taking `Route::RowstorePointRead` and preserving the `Engine::get` fast path), and narrow analytical scans (`AnalyticSelect` / `Route::OlapScan`) across all partitions over logical rowstore and base-plus-delta rows using server-root `<root>/colstore` for materialized `Column`/`Converting` partitions with recovery and version progression across reopen. The `Column`/`Converting` analytical path uses projection-aware compact reads unioning PK and requested columns (`read_column_partition_compact_core`), one safe predicate-leaf `SegmentReader` pushdown, rowstore delta suppression/overlay, deterministic PK ordering, and full residual SQL filter/aggregate/group evaluation. `ScanStats`/pruning is available as internal execution evidence, while SQL evaluation operates on materialized logical rows (vectorized aggregation is not implemented). Format conversion (`LocalServer::convert_table`) is guarded to single-partition tables and rejects multi-partition tables (`HtapError::Unsupported`). Verified in `crates/htap-server/tests/local_server.rs` (including `test_analytic_row_vs_column_base_plus_delta_equivalence`, `test_pushdown_predicate_selection_and_pruning_stats`, `test_partitioned_native_range_topology_catalog_reopen_continuation`, `test_partitioned_native_list_topology_catalog_reopen_continuation`, `test_partitioned_boundary_unmatched_null_type_errors`, `test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`, `test_partitioned_composite_pk_partition_key_not_first`, `test_partitioned_olap_across_partitions_and_empty_aggregate`, `test_convert_table_multi_partition_guard`, `test_partitioned_empty_topology_rejection_no_catalog_mutation`) and `crates/htap-convert/tests/materialization.rs` (including `test_compact_read_multiblock_pruning_and_stats`, `test_compact_read_mutation_sequence_and_deterministic_order`). Each partition requires exactly one bucket-0 row tablet and one healthy local leader; physical sharded SQL serving across nodes is not implemented.

### Client / Server Boundary and Error Categorization

`EmbeddedClient` (`htap-client`) wraps `LocalServer` (`htap-server`) synchronously within the host process, with no intermediate RPC, serialization, or daemon layer. Execution maps strictly to stable `HtapError` categories verified by integration tests (`crates/htap-client/tests/embedded_client.rs`):

| Failure / Execution Scenario | Example Statement | Error Category / Result |
| ---------------------------- | ----------------- | ----------------------- |
| Syntax parse error | `NOT A VALID SQL STATEMENT;` | `HtapError::InvalidArgument` |
| MySQL partition DDL | `CREATE TABLE t (id INT, val INT) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10));` | Supported via typed AST (`MysqlPartitionBy`), binding to `BoundPartitioning` |
| Empty or whitespace SQL | `   ;  ` | `HtapError::InvalidArgument` |
| Duplicate table creation | `CREATE TABLE products (id BIGINT PRIMARY KEY, name VARCHAR, price INT);` | `HtapError::Conflict` |
| Missing / nonexistent table | `SELECT * FROM nonexistent_table WHERE id = 1;` | `HtapError::NotFound` |
| Valid analytical scan | `SELECT * FROM products;` | `StatementResult::Query(QueryResult)` (succeeds via `Route::OlapScan`) |
| Unsupported DML operation | `UPDATE products SET price = 99 WHERE id = 1;` | `HtapError::Unsupported` |
| Unsupported query modifiers | `SELECT name FROM products WHERE id = 1 ORDER BY price + 1;` | `HtapError::Unsupported` |
| Unsupported query clauses | `SELECT name FROM products GROUP BY name HAVING count(*) > 1;` | `HtapError::Unsupported` |
| Joins or multiple tables in FROM | `SELECT * FROM orders, products;` | `HtapError::Unsupported` |
| Concurrent process root access | Opening an already locked storage root or symlink alias | As of Phase 16 (ADR-025), `EmbeddedClient::open` normally succeeds as an IPC client instead; `HtapError::Conflict` is returned only when IPC forwarding is also unavailable (see "Owner plus IPC (Phase 16) scope and deferred features" below) |
| Absent primary key lookup | `SELECT name, age FROM users WHERE id = 9999;` | Returns empty `QueryResult` (`qr.is_empty() == true`, `qr.num_rows() == 0`, column metadata preserved) |

### Subsystem Decoupling: Converter and Coordinator Not Created by LocalServer or EmbeddedClient

Neither `LocalServer::open` nor `EmbeddedClient::open` creates, configures, or supervises `LocalConverter` (`crates/htap-convert`) or `LocalCoordinator` (`crates/htap-coord`):
- `LocalServer` initializes only the local catalog store (`LocalCatalogStore`), rowstore engine (`htap_rowstore::Engine`), transaction manager (`TransactionManager`), and data mover (`LocalDataMover`).
- `LocalConverter` is a standalone partition-scoped conversion engine used for transcoding rowstore tables into columnar segments and advancing catalog cutover state machines.
- `LocalCoordinator` is an independent cluster coordination and placement engine managing node registration, leadership leases, and monotonic fencing tokens at `<coord_root>/COORDINATOR`.
Workflows requiring format conversion or coordinator-fenced catalog CAS instantiate `LocalConverter` or `LocalCoordinator` explicitly outside the server façade.

### Documentation Mermaid Convention

All architectural call flows, DML transaction sequences, and initialization state machines in documentation follow standard GitHub-compatible Mermaid `flowchart` and `sequenceDiagram` syntax without unsupported extensions.

### Explicitly deferred features (network daemon security boundary/full SQL analytics)

- **MySQL wire protocol and `htapd` daemon (implemented, Phase 8; binary protocol and prepared statements
  implemented, Phase 11; TLS, compression, and per-user accounts implemented, Phase 12):** A hand-written,
  synchronous MySQL protocol server (`htap-wire::WireServer`) and daemon binary (`htapd`) are implemented; all
  interaction can now go through `LocalServer` either in-process (`EmbeddedClient`) or over TCP
  (`htap-client::RemoteClient`, or any MySQL client, including prepared statements, TLS, and compression). See
  "TLS, compression, and account/privilege scope and deferred features" below for the Phase 12 contract and
  remaining gaps, ADR-016, ADR-019, ADR-020, ADR-021, and the "Network layer" / "TLS and compression (Phase
  12)" / "Accounts and privileges (Phase 12)" / "Prepared statements and binary protocol" sections of
  `docs/ARCHITECTURE.md` for the full contract; verified in `crates/htap-wire/tests/{wire_server,tls,compression,accounts}.rs`,
  `crates/htap-wire/src/*.rs` unit tests, and `crates/htap-client/tests/remote_client.rs`.
- **Sessions and explicit transaction control (implemented, Phase 10):** `htap-server::session::Session`
  (`LocalServer::open_session`, `EmbeddedClient::open_session`, and one `htap-wire` connection = one session)
  supports `BEGIN`/`START TRANSACTION [READ ONLY | READ WRITE]`, `COMMIT`, `ROLLBACK`, `autocommit`, and
  `@user`/`@@system` variables, with session-buffered uncommitted writes that overlay reads
  (read-your-own-writes) until `COMMIT` runs the existing 2PC path once. See "Sessions and explicit
  transactions (Phase 10)" in `docs/ARCHITECTURE.md`, ADR-018, and `docs/PROGRESS.md` for the full contract
  and test evidence. Remaining gaps:
  - **No `SELECT ... FOR UPDATE` / locking reads:** reads never take row or table locks; concurrency control
    is snapshot isolation with first-writer-wins detected at `COMMIT` only.
  - **Prepared statements (Phase 11) go through the same session and quarantine gate as `COM_QUERY`:**
    `COM_STMT_EXECUTE` calls `Session::execute_statement` exactly like a text-protocol statement, so
    `CommitOutcomePending` rejects it the same way; see "Prepared statements and binary protocol scope and
    deferred features" below for the remaining prepared-statement-specific gaps (cursors, unsigned 64-bit
    values, exact `DECIMAL`, `TIME` parameters, best-effort `PREPARE` metadata).
  - **IPC and concurrent multiprocess access are implemented (Phase 16, owner plus IPC, Unix only):** it is no
    longer true that a session belongs to one process forever. The first process to acquire `<root>/LOCK` is
    still the sole storage owner (unchanged since Phase 6), but a second process opening the same root now
    becomes a client instead of failing outright: a client-mode session forwards its calls over a persistent
    connection on a Unix domain socket at `<root>/htap.sock` to a real, owner-side session the listener
    constructs for it. See "Concurrent multiprocess use: owner plus IPC (Phase 16)" in `docs/ARCHITECTURE.md`
    and ADR-025 for the full design, and the "Owner plus IPC (Phase 16) scope and deferred features" list
    below for what this does and does not cover.
  - **Per-user ACL is implemented (Phase 12):** a session's `Principal` (`Superuser` or a catalog `Account`)
    is authenticated at connect time and re-checked against the freshly loaded catalog on every statement; see
    "TLS, compression, and account/privilege scope and deferred features" below and "Accounts and privileges
    (Phase 12)" in `docs/ARCHITECTURE.md`. Remaining gaps: no roles, no delegated administration, and only the
    `%` host is accepted.
  - **No idle-transaction timeout or reaping; MVCC GC is now internal-only, not a user-facing feature:** an
    open transaction with no following `COMMIT`/`ROLLBACK` (or a session that is never dropped) holds its
    pinned snapshot indefinitely. As of Phase 15, `LocalServer.pinned_snapshots` (registered at `BEGIN`,
    unregistered on `COMMIT`/`ROLLBACK`/session drop) *is* consulted by `compaction_tick()`'s GC-horizon
    computation, so a genuinely idle, never-closed transaction now has a real, observable cost: it holds the
    rowstore compaction horizon at its own pinned version indefinitely, blocking version collapse for any key
    it could still read — there is no idle-transaction timeout to release that hold automatically. There is
    still no operator-facing "run GC now" command or automatic staleness-based session reaping; `gc_low_water`
    is an internal mechanism supporting `Engine::compact_once`, not a standalone feature. This is also why a
    stale-snapshot-vs-conversion conflict is used instead of making conversion itself wait on the
    pinned-snapshot registry (see ADR-018).
  - **Only `REPEATABLE READ` is offered:** snapshot isolation with write skew permitted; `READ COMMITTED`,
    `READ UNCOMMITTED`, and `SERIALIZABLE` requests are rejected with `HtapError::Unsupported`, never silently
    downgraded or upgraded.
  - **DDL is rejected inside an open transaction** (explicit or implicit, `autocommit = 0`); the transaction
    survives, unpoisoned, and can simply `COMMIT`/`ROLLBACK` before the DDL is retried outside a transaction.
  - **No grace period before a new columnar base dooms an open transaction:** a conversion that publishes a
    new base version while a transaction holds an older snapshot poisons that transaction's next read against
    the affected partition immediately (`Conflict`), with no window to finish first.
  - **`SET [SESSION] TRANSACTION READ ONLY | READ WRITE` scope quirk:** this sets the default for the *next*
    `BEGIN`/`START TRANSACTION` only, even when written with the `SESSION` keyword, because the vendored
    parser's AST does not distinguish a session-persistent default from a next-transaction-only one.
  - **`SET CHARACTER SET <x>` / `SET CHARSET <x>` are answered by the `htap-wire` shim, not the session:**
    `vendor/sqlparser` has no AST node for these MySQL-specific positional forms at all, so they fail to
    parse before a session ever sees them.
  - **Savepoints are not supported:** `SAVEPOINT`/`RELEASE SAVEPOINT`/`ROLLBACK TO SAVEPOINT` are rejected
    (parsed as unsupported statements, or as an explicit `HtapError::Unsupported` for `ROLLBACK ... TO
    SAVEPOINT`); there is only one transaction-wide undo point.
  - **XA (distributed transactions) is not supported.**
- **Owner plus IPC (Phase 16) scope and deferred features:** the first process to acquire `<root>/LOCK`
  is still the sole storage owner; every other process on the same root becomes a client that forwards SQL
  and session calls over a Unix domain socket at `<root>/htap.sock`. See "Concurrent multiprocess use: owner
  plus IPC (Phase 16)" in `docs/ARCHITECTURE.md` and ADR-025 for the full design. Disclosed gaps:
  - **Fixed (batch E, with a batch F follow-up): the socket used to have a brief permission-tightening window right after `bind`.** A
    storage re-review found that the socket was originally bound directly at `<root>/htap.sock` with the
    process's default umask permissions and only tightened to `0600` a moment afterward; since the root
    directory is itself world-traversable, another local user who connected in that window would have gotten
    an unauthenticated superuser session. The socket is now created inside a private, owner-only (`0700`)
    directory (`<root>/.htap-ipc-<pid>-<id>/`), tightened to `0600` there, and only then atomically renamed to
    `<root>/htap.sock` — no window exists at any permission level, and peer-credential checking is not needed
    to close it. Test: `crates/htap-server/tests/ipc_owner_socket_permissions.rs::startup_publishes_owner_only_socket_and_cleans_up_staging_directory`
    asserts the published socket's mode (this replaces an earlier unit test, `ipc::owner::tests::published_socket_is_owner_only`,
    that a batch F storage re-review found reimplemented the publication sequence itself and could not have
    caught a regression). The private `.htap-ipc-<pid>-<id>` staging directory used to publish the socket is
    removed immediately after the successful rename, and any left by a crashed prior owner are swept up at
    startup before a new one is created — **fixed (batch F):** that staging directory was never removed on the
    ordinary successful path before this fix, so every server that ever started leaked one into `<root>`, not
    only a crashed one; the same test above asserts none remains after a normal open/drop cycle. The trust model is still same-OS-user, matching `<root>/LOCK`
    (DECISIONS #6 in the Phase 16 plan) — this is not a security boundary against another local user with
    filesystem access to the whole root, only against a race during creation.
  - **A socket path too long for a Unix socket, or a non-socket file already at that path, falls back to
    lock-only mode.** The owner keeps working normally, but a second opener in that case gets the ordinary
    `Conflict` (lock contention), not IPC forwarding, exactly as it would have before Phase 16.
    `LocalServer::is_listener_up()` reports `false` in this case so a caller can tell a degraded owner from a
    healthy one.
  - **The wire format is tied to the build's AST shape.** Enabling the vendored parser's `serde` feature to
    forward a bound statement directly (rather than re-rendering it to text) ties the wire protocol to the
    exact statement-AST shape of whichever build produced it. A version-skewed owner/client pair (e.g. an
    unrestarted process after an in-place binary upgrade) fails cleanly at frame-decode time — the connection
    is closed with a diagnostic, never a panic or a silent misinterpretation — rather than being caught by a
    schema-compatibility scheme, which this phase does not build.
  - **Changing users on a client session is unsupported:** `Session::change_user` (backing
    `COM_CHANGE_USER`/`COM_RESET_CONNECTION`'s re-authentication path) returns `HtapError::Unsupported` for a
    client-mode session; only `authenticate_session` at session-open time is forwarded. This check runs before
    the terminal-state (quarantine/disconnected) gate, so a client-mode session already in the
    `AmbiguousOutcomePending`/`RemoteDisconnected` state gets the plain `Unsupported` error for a
    `change_user` call rather than its stored terminal error — a cosmetic difference, since `change_user` was
    always going to fail in client mode either way, found by the batch D storage review and left as-is (ADR-025).
  - **A client-mode `LocalServer`'s configuration setters have no effect:** both the `with_*` builder forms
    (`with_scan_workers`, `with_query_parallelism`, `with_query_memory_budget`, `with_analyze_distinct_limit`,
    `with_gc_horizon_retention_slack`) and their mutable `set_*` counterparts are accepted no-ops on a
    client-mode handle, and their matching getters report the compiled-in default rather than the owner's
    actual configuration, because that configuration lives entirely in the owner process's storage core and is
    not itself part of the forwarded SQL/session surface. Found by the batch D storage review as a symptom of
    the same defect as the panic fix above (see
    ADR-025); fixed as a safe no-op rather than as forwarding, since no in-tree caller configures these on
    anything but the one process that opens the root. The setters being no-ops is correct behavior
    (configuration is process-global, owner-only); a batch E storage re-review flagged that this was
    previously undocumented, along with a related gap: `LocalServer`'s `last_query_*` diagnostic getters
    (parallel workers, spill flags per operator, optimizer invocation count) also report fixed client-mode
    defaults (`1`/`false`/`0`) rather than the owner's actual last-query state, which a client-mode handle
    cannot see. Both are now documented here rather than silently returning fabricated-looking values.
  - **Administrative, movement, and conversion operations are owner-only:** partitioning DDL execution
    helpers, table conversion, `compaction_tick`/`reclaim_tick`, CSV/JSONL import/export, and tablet
    clone/verify/repair all return `HtapError::Unsupported` on a client-mode `LocalServer` handle.
  - **A client session that loses its connection is terminal.** A confirmed-dead connection moves the session
    to a permanent `RemoteDisconnected` state carrying the ordinary `Conflict` error; a session never silently
    reconnects and resumes the same transaction. A request whose outcome is unknown because request bytes may
    already have reached the owner instead moves the session to a permanent `Ambiguous`-outcome quarantine
    state (the same shape as the existing `DurablePending` quarantine) — see ADR-025's ambiguous-versus-
    conflict-versus-durable-pending reasoning. Neither state is ever exited by reconnecting; both require a
    fresh session.
  - **Standalone subsystem opens remain unaffected and still unsafe for concurrent use.** `Engine::open`,
    `LocalCatalogStore::open`, and `LocalDataMover::new`, called directly instead of through `LocalServer`, do
    not participate in the owner/client split at all and remain unsafe for direct concurrent shared-root use,
    unchanged by this phase.
- **Extended DML and DDL (Phase 9, extended Phase 13):** `UPDATE t [alias] SET col = expr, ... [WHERE ...]`
  (point and filtered-scan forms, `Route::RowstoreUpdate`) and `DROP TABLE [IF EXISTS] t` (catalog CAS,
  `Route::CatalogDdl` — as of Phase 15, that CAS also marks the dropped tablets `pending_reclaim`, and
  `LocalServer::reclaim_tick`/`compaction_tick` physically reclaim their rowstore/columnar/movement artifacts
  eventually, not necessarily by the time the statement returns; see "Rowstore compaction, garbage collection,
  and DROP TABLE reclaim scope and deferred features" below) are implemented — see "General query executor
  scope and deferred
  features" below for the exact contract and gaps. Since Phase 13, `DELETE` accepts an arbitrary `WHERE`
  filter (`Route::RowstoreDelete`, not just a complete-PK predicate), `TRUNCATE TABLE`/`TRUNCATE t` binds to
  the same unfiltered-`DELETE` representation (transactional, rollback-able, payload-capped — a disclosed
  deviation from real MySQL `TRUNCATE`, which is non-transactional and unbounded), and `INSERT ... SELECT`
  binds a query source with an exact per-column static type match (no widening) and, like every `INSERT`,
  still requires an explicit target column list naming every schema column (no partial-column `INSERT`).
  Non-partition generic `ALTER TABLE` (`ADD COLUMN`, `RENAME TABLE`, etc.) and `UPDATE` with joins/subqueries/
  `ORDER BY`/`LIMIT` remain deferred. Typed partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE
  PARTITION` on empty source partitions) is supported.
- **Analytical queries and SQL breadth (R4):** As of Phase 9, extended by Phase 13, a general query executor
  (`Route::Query`, `htap-server::query_exec`) implements `INNER`/`LEFT`/`RIGHT`/`CROSS`/`FULL OUTER` joins
  (left-deep chains, comma joins, and arbitrarily nested parenthesized join trees), `NATURAL`/`USING` joins
  with real column coalescing, table aliases, qualified names, `*`/`t.*`, arithmetic (`+ - * / % DIV`, checked
  overflow), comparisons, `AND`/`OR`/`NOT`, `IS [NOT] NULL`/`TRUE`/`FALSE`, `LIKE`, `IN (list)`, `BETWEEN`,
  `CASE`, `CAST`, scalar functions (`UPPER`/`LOWER`/`LENGTH`/`CHAR_LENGTH`/`CONCAT`/`ABS`/`COALESCE`/`IFNULL`/
  `NULLIF`), aggregates (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with `DISTINCT`), window functions (ranking/offset
  functions, aggregates as window functions, `ROWS`/peer/value-offset `RANGE` frames), `GROUP BY`/`HAVING`,
  `SELECT DISTINCT`, `ORDER BY` (expressions/aliases/ordinals, `NULLS FIRST`/`LAST`), `LIMIT`/`OFFSET`,
  `UNION`/`UNION ALL`/`EXCEPT`/`INTERSECT`, derived tables, non-recursive and recursive (`WITH RECURSIVE`)
  CTEs, and uncorrelated and one-level-deep correlated scalar/`IN`/`EXISTS` subqueries; as of Phase 14, also
  `ANALYZE TABLE` statistics, a statistics-driven cost-based optimizer stage (enabled by default), `EXPLAIN`/
  `EXPLAIN ANALYZE`, disk spilling under a per-statement memory budget, and bounded parallelism for
  `GROUP BY`/`INNER`/`CROSS` joins — see "General query executor scope and deferred features" below for the
  full contract, what is still deferred (vectorized/pipelined execution, worker-pool parallelism for
  `LEFT`/`RIGHT`/`FULL` joins — spilling itself is not kind-restricted in code but is covered by a test only
  for `INNER` joins — statistics histograms, and more), and the
  narrow-shape gate (`is_narrow_select_shape`) that keeps complete-PK point reads and narrow single-table
  scans unchanged (and, since Phase 14, never invokes the optimizer at all for those two routes). The
  narrow `AnalyticSelect` / `Route::OlapScan` path itself
  is unchanged from Phase 3-8.
- **Direct SegmentReader pushdown optimization and vectorized execution:** Direct `SegmentReader` pushdown optimization is now implemented for the compact base path in `LocalServer` (using `read_column_partition_compact_core` with PK+requested column union and single safe predicate-leaf pushdown). Compound `AND` pushdown beyond one leaf and `!=` remain evaluated as residual SQL filters. `ScanStats`/pruning is available as internal execution evidence, but SQL evaluation still operates on materialized logical rows; vectorized aggregation, vectorized operator pipelines, memory quotas for the narrow `AnalyticSelect`/`Route::OlapScan` path itself (the general `Route::Query` path has a memory budget as of Phase 14, see above), query cancellation, and DataFusion/Arrow integration are deferred.
- **Partition Execution & Multi-Partition Routing Scope:**
  - `LocalServer` supports multi-partition tables created via SQL DDL (`CREATE TABLE ... PARTITION BY RANGE/LIST`) or via the native non-SQL `LocalServer::create_partitioned_table` API with finite `PartitionTopology::Range` or `PartitionTopology::List`.
  - Catalog validation requires that the partition key column is non-null and contained in the primary key, validates range half-open intervals `[lower, upper)` with strictly increasing bounds and optional final `MAXVALUE`, list exact values, and enforces uniqueness, ordering, and type constraints (`test_range_partitioning_routing_and_boundaries`, `test_list_partitioning_routing`, `test_partitioning_duplicate_violations`, `test_range_overlap_and_order_violations`, `test_partitioning_type_and_null_violations`, `test_partitioning_ownership_and_method_consistency`, `test_partitioning_cas_and_reopen_lifecycle`).
  - Local topology invariant: Each individual partition currently has exactly one bucket-0 row tablet and one healthy local leader replica on node 1 (`NodeId(1)`). Hash buckets and physical distributed sharding across nodes are not implemented.
  - Multi-row `INSERT` routes rows across partitions by partition key and atomically commits all mutations in one transaction payload/version (`test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`).
  - Complete-PK `DELETE` and `SELECT` route by the partition-key position within the primary key, and `SELECT` preserves the `Engine::get` fast path (`Route::RowstorePointRead`), bypassing analytical execution (`test_partitioned_composite_pk_partition_key_not_first`).
  - Analytic `SELECT` evaluates queries across partitions at one visible snapshot: conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented for narrow local OLAP (verified by `test_partitioned_olap_across_partitions_and_empty_aggregate`, `test_partition_pruning_range_and_list_and_conservative_cases`, `test_scan_worker_count_equivalence`, and `test_multi_partition_order_by_directions_nulls_and_tie_breaking`); distributed fanout, disk spilling, query cancellation, and resource quotas remain deferred.
  - Storage conversion: `convert_table` is guarded to single-partition tables. Table-wide conversion (`convert_table_to_column`), Column->Row metadata demotion (`convert_table_to_row`), and explicit ticks (`conversion_tick`, `tick`) execute synchronously; `tick` resumes persisted jobs only, without autonomous background scheduling. Fail-closed startup validation (`validate_storage_state_on_open`) ensures catalog and disk consistency on reopen (returning `HtapError::Corruption` or `HtapError::Io` depending on the cause). Autonomous background conversion scheduling is deferred.
  - Placement planning and activation in `htap-coord`/`htap-movement` provide metadata planning and local simulation, not physical sharded SQL serving.
- **MySQL Partition DDL and SQL Parser Boundary:**
  - Typed MySQL partition DDL statements (`CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...`, including `MAXVALUE`, `VALUES LESS THAN`, `VALUES IN`, and `COLUMNS (...)`) are supported via a vendored, minimally patched Apache-2.0 `sqlparser` crate under `vendor/sqlparser` (`test_mysql_partition_ddl_parsed_and_bound`, `test_sql_range_partitioning_ddl_and_maxvalue_routing`, `test_sql_list_partitioning_ddl_and_routing`).
  - Typed MySQL partition lifecycle DDL (`ALTER TABLE <table> ADD PARTITION`, `DROP PARTITION`, and `REORGANIZE PARTITION`) is supported for strict finite range/list forms and final MAXVALUE where supported, routed to `Route::CatalogDdl` and validated by candidate catalog CAS and empty-source rowstore collapse checks before execution (`test_mysql_alter_partition_parsed_and_bound`, `test_server_sql_alter_partition_lifecycle`). Populated DROP/REORGANIZE is rejected.
  - Native `LocalServer::alter_partitions` API remains available with candidate catalog validation, atomic CAS, empty-source safety, and checked ID allocation without ID burn.
  - Strict binding in `htap-sql` validates partition keys against PK and non-null constraints, verifies increasing order for RANGE bounds, and checks disjointness of LIST values.
  - Unsupported partitioning forms: Partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), `SUBPARTITION`, `LIST DEFAULT`, expressions in partition keys, multi-column `COLUMNS`, non-final/malformed `MAXVALUE`, and unrelated ALTER operations are strictly rejected with parse or binder errors (`test_mysql_partition_ddl_negative_parser_and_binder`, `test_mysql_alter_partition_negative`).
  - Deferred partition capabilities: Physical data migration for populated partition reorganization, physical storage reclamation for `ALTER TABLE ... DROP/REORGANIZE PARTITION` (which only ever operates on empty source partitions, so this is a currently-inert gap) or demoted column files, hash tablets, distributed/remote partition serving across network nodes, replica failover, and an inter-node distributed-serving network protocol remain deferred (the client-facing MySQL wire protocol is implemented; see "MySQL wire protocol and `htapd` daemon" above). `DROP TABLE`'s own artifacts are physically reclaimed as of Phase 15 — see "Rowstore compaction, garbage collection, and DROP TABLE reclaim scope and deferred features" below — this bullet is only about the `ALTER TABLE` partition-lifecycle path. `UPDATE` is implemented (Phase 9; see below), but the binder strictly rejects assigning a partition-key column, so cross-partition row movement via `UPDATE` remains unsupported.
- **Broad MySQL compatibility:** Broad MySQL syntax, built-in functions, variable setting, system tables, and loose type coercions are deliberately unsupported.

---

## General query executor scope and deferred features (Phase 9, extended Phase 13)

The Phase 9 implementation delivers a general query executor (`htap-sql::{query, expr, binder_query}`,
`htap-server::query_exec`) that handles the full breadth of statement shapes the Phase 3-8 narrow binders
rejected, while structurally preserving the narrow point-read and single-table-scan fast paths (R5) via a
purely syntactic shape gate (`is_narrow_select_shape` in `crates/htap-sql/src/binder.rs`). Phase 13 extends
the same executor with windows, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, arbitrary
parenthesized join trees, `WITH RECURSIVE`, `EXCEPT`/`INTERSECT`, `GROUP BY`/`ORDER BY` ordinals, filtered
`DELETE`, `TRUNCATE`, `INSERT ... SELECT`, and integer `DIV` — no on-disk format change; see ADR-022 for the
two design decisions this required (the correlated-subquery execution boundary, and the `JoinTree`/
flat-lowering/differential-test design).

### Completed local MVP

- **Joins and query shapes:** `INNER`/`LEFT`/`RIGHT`/`CROSS` joins (left-deep chains and comma joins),
  table aliases, qualified names (`t.c`), `*`/`t.*` wildcards.
- **Expressions:** arithmetic (`+ - * / %`; `/` always widens to `Float64`; checked integer overflow is a
  runtime error), comparisons (including column-vs-column and literal-on-left), `AND`/`OR`/`NOT` with SQL
  three-valued logic, `IS [NOT] NULL`, `IS [NOT] TRUE`/`FALSE`, case-insensitive `LIKE` (`%`/`_`), `IN
  (list)`, `BETWEEN`, `CASE` (simple and searched, lazily evaluated), `CAST` to
  `bool`/`int`/`bigint`/`double`/`varchar`/`varbinary`/`timestamp`, and scalar functions `UPPER`/`LOWER`/
  `LENGTH`/`CHAR_LENGTH`/`CONCAT`/`ABS`/`COALESCE`/`IFNULL`/`NULLIF`.
- **Aggregation:** `COUNT(*)`/`COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with `DISTINCT`, `GROUP BY` expressions with
  strict grouping validation, `HAVING` (aliases allowed), `SELECT DISTINCT`.
- **Ordering and paging:** `ORDER BY` expressions/aliases/ordinals with `ASC`/`DESC` and `NULLS FIRST`/`LAST`
  (`ASC` defaults `NULLS FIRST`, `DESC` defaults `NULLS LAST`), `LIMIT`/`OFFSET` and MySQL `LIMIT off, cnt`.
- **Set operations and composition:** `UNION`/`UNION ALL` with numeric widening (`Int32`->`Int64`->
  `Float64`), derived tables (subquery in `FROM`, alias required, unique column names), non-recursive `WITH`
  CTEs (chained), uncorrelated scalar/`IN`/`EXISTS` subqueries (a scalar subquery returning more than one row
  is a runtime error), and `FROM`-less `SELECT`.
- **Cross-engine materialization:** Every base table side of a join is read through the same
  `scan_partition_compact` storage path the narrow `Route::OlapScan` executor uses — `Row` from the rowstore,
  `Column`/manifest-bearing `Converting` from columnar segments with the rowstore delta overlay, and
  `SnapshotPinned` manifest-less `Converting` falling back to the rowstore — all at **one** `Snapshot` per
  statement, so a join between a `Row` table and a converted `Column` table observes a single consistent
  version. Per-slot partition pruning and single-leaf predicate pushdown are derived exactly as in
  `Route::OlapScan`, except on the null-supplying side of an outer join, where a would-be-pushed conjunct is
  kept as a residual filter to avoid dropping rows that should be null-padded.
- **R5 preserved structurally:** `is_narrow_select_shape` is evaluated before any deep binding; a complete-PK
  simple `SELECT` still binds `PointSelect` -> `Route::RowstorePointRead`, and a narrow single-table shape
  still binds `AnalyticSelect` -> `Route::OlapScan` with pruning/pushdown/scan-worker behavior unchanged.
  Clauses are never silently dropped: a `LIMIT`, alias, join, or `OR` on what looks like a PK lookup fails
  the shape test and binds through the general path instead. Pinned by
  `crates/htap-sql/tests/route.rs::test_point_read_fast_path_pinned_against_general_query_path`.
- **`UPDATE`:** `UPDATE t [alias] SET col = expr, ... [WHERE ...]` routes to `Route::RowstoreUpdate`. A
  complete-PK `WHERE` takes a point read-modify-write at the statement's snapshot and commits one
  `Mutation::Put` through the same 2PC path as `INSERT`; otherwise every partition is scanned at one snapshot
  through the general executor's storage path and all rewritten rows commit in **one** transaction.
  Assignments evaluate left to right against the progressively updated row (`SET a = a + 1, b = a` sees the
  new `a`). Values are coerced to the column type at bind time; `NOT NULL` is enforced.
- **`DROP TABLE`:** `DROP TABLE [IF EXISTS] t` removes the table/partitions/tablets/replicas via one catalog
  CAS; refuses (`Conflict`) while any partition is `Converting`. `SHOW`-visible and reopen-safe.
- **`SHOW`/`DESCRIBE`:** `SHOW TABLES [LIKE p]`, `SHOW DATABASES`, `SHOW COLUMNS FROM t` / `DESCRIBE t` /
  `DESC t`, answered purely from the catalog snapshot (`Route::CatalogRead`), no storage access.

### Completed local MVP — Phase 13 additions

- **`GROUP BY`/`ORDER BY` ordinals:** `GROUP BY N` binds the `N`th select-list item's own expression (a
  wildcard or aggregate-containing target, or an out-of-range `N`, is a bind error naming the position);
  `ORDER BY N` resolves by output-column position instead of being a no-op literal.
- **Integer `DIV`:** truncating-toward-zero division; both operands must already be `Int32`/`Int64` (a
  `Float64` operand is a bind-time type error, unlike `/`, which always widens); result is always `Int64`,
  never `Float64`; division by zero is `NULL` (matching `/`/`%`); `i64::MIN DIV -1` is a checked-overflow
  runtime error.
- **`EXCEPT`/`INTERSECT` (`ALL` and `DISTINCT`):** reuse `UNION`'s existing branch-binding/column-count/
  type-widening validation; correct occurrence-count multiset semantics in the executor (`EXCEPT DISTINCT`
  keeps left-distinct values absent on the right; `EXCEPT ALL` keeps `max(0, left_count - right_count)`
  copies per distinct left value; `INTERSECT DISTINCT`/`INTERSECT ALL` mirror this with presence/`min`).
- **`FULL OUTER`/`NATURAL`/`USING` joins with real column coalescing:** built per join-tree node (see the
  `JoinTree` bullet below), not as a flat `COALESCE` over every physical position sharing a name.
  `INNER`/`LEFT` -> the left operand's own (possibly already-merged) expression, `RIGHT` -> the right
  operand's, `FULL` -> `Coalesce` of both, correctly recursing through an already-merged operand in a chained
  `a FULL JOIN b USING(id) FULL JOIN c USING(id)` rather than re-flattening to raw physical columns.
  Unqualified name resolution searches the whole visible schema (physical + merged) and requires a *unique*
  match — it does not prefer a coalesced column. `SELECT *` expands merged/common columns first (left-input
  order), then each side's remaining physical columns in table order; qualified `t.col`/`t.*` always resolve
  through the existing per-slot physical path, unaffected. A merged column's nullability is derived per
  operand, from each side's own nullability just before *this* join's own null-extension (not generically
  re-derived from post-padding physical nullability, which can be needlessly conservative). `NATURAL`'s
  empty-intersection case degrades to plain `CROSS` only for `INNER`; `LEFT`/`RIGHT`/`FULL` keep their own
  kind with an always-true condition, since literal `CROSS` would silently change empty-right-side null
  preservation. A duplicate/ambiguous `USING` name, or a `NATURAL` join colliding with an already-ambiguous
  accumulated visible name, is a bind error naming the column.
- **Arbitrarily nested parenthesized join trees (`query::JoinTree`):** built directly and always from the
  parsed `FROM` clause, 1:1 with the SQL's own parenthesization (comma-separated items fold in as
  unconditional-`CROSS` nodes in encounter order) — the single binding source of truth for every join,
  replacing the old flat-only construction. At the time of Phase 13, a total, pure `lower_to_flat` function
  derived a flat `Vec<JoinSpec>` form whenever the tree was purely left-deep in shape (every join node's right
  child a bare slot leaf; the join *kind* at each step unrestricted), and every such query ran a separate,
  unchanged flat executor, while a genuinely nested/parenthesized shape (e.g.
  `a LEFT JOIN (b JOIN c ON ...) ON ...`, `(a LEFT JOIN b ON ...) FULL JOIN c ON ...`) ran a second, recursive
  tree evaluator; each tree node's `ON` condition is rebased to a locally-compact offset at bind time,
  regenerated fresh every time it is needed and never cached, while top-level `WHERE`/`GROUP BY`/projection/
  `HAVING` keep using the original globally-offset expressions unchanged. **Superseded by Phase 14:** the
  separate flat-loop executor and `SelectBody`'s `joins`/`tree_only` fields were removed; a flat `Vec<JoinSpec>`
  chain is now synthesized into the same tree shape at bind time (`left_deep_join_tree`), so
  `evaluate_join_tree` is the sole join execution path for every query, flat or nested. The mandatory,
  permanent differential test
  (`crates/htap-server/tests/query_exec.rs::test_flat_and_tree_join_evaluators_match_for_lowerable_queries`)
  now pins a flat-written query and its explicitly-parenthesized equivalent to identical results under that
  one evaluator, rather than comparing two separate evaluators as it did through Phase 13 (see ADR-022 for the
  original two-executor design and ADR-023 for the Phase 14 unification). The `JoinTree` itself was purely
  structural through Phase 13 (never reordered, commuted, or cost-estimated joins); as of Phase 14, an
  `INNER`/`CROSS` join component may be cost-reordered by `htap_sql::optimize` (enabled by default) before
  `evaluate_join_tree` ever runs — see "Completed local MVP — Phase 14 additions" below and ADR-023.
- **`join_rows` generalized to explicit left/right widths**, backing both join executors; in the same pass, a
  pre-existing `Int64`/`Timestamp` -> `Float64` hash-key-widening collision above `2^53` was fixed (exact
  integers keep an integer-keyed representation; only widen to `Float64` when one side of the same key
  position is actually `Float64`).
- **`DELETE` by arbitrary filter:** `DELETE FROM t WHERE <any filter>` (not just a complete-PK equality
  predicate) routes through `Route::RowstoreDelete { key: None }`, scanning every matching partition at one
  snapshot and committing all matching-row deletes in **one** transaction — same ~4 MiB effective 2PC payload
  cap and no-chunking rule as filtered `UPDATE` (see "Effective 2PC transaction payload cap" above).
  `check_privileges` requires `DELETE` on the table always, and additionally `SELECT` when the target is a
  filter (mirroring `UPDATE`'s existing rule).
- **`TRUNCATE`:** `TRUNCATE TABLE t` / `TRUNCATE t` bind to the exact same unfiltered `DELETE FROM t`
  representation task above produces — zero new execution code. This means `TRUNCATE` here is transactional,
  rollback-able, and subject to the same payload cap as any other filtered/unfiltered `DELETE`, a disclosed
  deviation from real MySQL `TRUNCATE`, which is non-transactional, cannot be rolled back, and has no payload
  limit. Multiple targets, `PARTITION`, `IDENTITY`, `CASCADE`/`RESTRICT`/`ON CLUSTER`, or any other option
  beyond a bare table name are bind errors; `IF EXISTS` behaves like `DROP TABLE`'s (a missing table is a
  silent no-op; an existing-but-invisible table under per-user privileges masks as missing). `check_privileges`
  requires only `DELETE`, not `SELECT` (there is no filter to evaluate).
- **`INSERT ... SELECT`:** the bound `Insert` carries either literal `VALUES` rows (unchanged) or a bound
  source query plus the target column mapping; the source's output column count must equal the target list's
  length, and each source expression's static type must be exactly the target column's type (NULL literal/
  variable is the only permissive case) — no widening, using UPDATE's assignment-coercion "type mismatch"
  wording. Like every `INSERT`, an explicit target column list naming every schema column is required (no
  partial-column `INSERT`); `CTE`s/`ORDER BY`/`LIMIT` directly on the `INSERT` statement remain rejected.
  Execution validates every base table of the source query, executes the source fully at the statement's one
  snapshot *before* building any target mutation, then commits all inserted rows in one transaction — a
  self-referencing `INSERT INTO t SELECT ... FROM t` therefore reads only the pre-insert snapshot and inserts
  each source row exactly once, never looping or double-counting.
  `check_privileges` requires `INSERT` on the target always, and additionally `SELECT` on every source table
  when a query source is present.
- **Correlated subqueries, depth-1 only:** a child subquery's binder sees only its immediate parent's own
  slots, never a flattened ancestor chain; a name that would need to skip a level to resolve (a grandparent
  reference) is the specific bind error "correlated subqueries may only reference the immediately enclosing
  query," not a silent mis-resolution or a generic unknown-column error. Allowed in `WHERE`/`SELECT`/`HAVING`;
  in an aggregate query, a correlated subquery in `HAVING`/the projection may only correlate on the outer
  query's own `GROUP BY` keys (otherwise a bind error naming the offending column and clause) — this closes a
  trap where a correlated subquery could otherwise silently read an arbitrary representative row of a group.
  Execution reuses the statement's single pinned `Snapshot`/write-set overlay unchanged (never re-pinning or
  advancing it) via a `SubqueryRunner` callback trait (`htap-sql::expr`, zero storage types in its signature)
  and a per-statement `SubqueryBudget` (`SubqueryBudget::new(10_000, 20)` in `query_exec.rs`: 10,000 total
  invocations, 20 re-entrant nesting levels) bounding runaway re-execution, independent of the recursive-CTE
  cap, erroring `HtapError::InvalidArgument` and naming which cap fired. See ADR-022.
- **`WITH RECURSIVE`:** exactly one self-referencing CTE whose body is a two-branch `UNION`/`UNION ALL`; the
  recursive term resolves its own CTE name to a working-table placeholder slot (never a real table/CTE
  lookup, resolved by the executor to the previous iteration's rows) and must appear exactly once, at
  top-level `FROM`, not inside a subquery, not on the null-supplying side of an outer join, and must contain
  no aggregates/`GROUP BY`/window functions/`DISTINCT`/`ORDER BY`/`LIMIT`; anchor/recursive output types
  reconcile via the same numeric-widening rule `UNION` already uses; nested/mutually-recursive CTEs are
  rejected. Execution runs a fixed-point loop bounded by three independent, explicitly named caps in
  `crates/htap-server/src/query_exec.rs` — `MAX_RECURSIVE_ITERATIONS = 1_000`, `MAX_RECURSIVE_ROWS =
  1_000_000`, `MAX_RECURSIVE_BYTES = 256 * 1024 * 1024` (approximate, not exact) — each erroring
  `HtapError::InvalidArgument` naming exactly which cap fired; the `UNION` (distinct) form deduplicates via a
  global seen-set across all iterations, the `UNION ALL` form does not.
- **Window functions:** `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `LAG`, `LEAD`, `FIRST_VALUE`,
  `LAST_VALUE`, and `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` as window functions, with `PARTITION BY`/`ORDER BY` and
  three frame kinds: `ROWS` (integer-literal-or-unbounded bounds), peer-based `RANGE` (any number of `ORDER
  BY` keys, `UNBOUNDED`/`CURRENT ROW` bounds only), and a project-specific value-offset `RANGE` frame
  (exactly one numeric/`Timestamp` `ORDER BY` key; boundary algebra stated in comparator-order terms — ASC:
  `n PRECEDING -> V-n`, `n FOLLOWING -> V+n`; DESC: flipped; NULL order-key rows form their own peer group
  rather than erroring; a negative or non-finite literal offset is a bind-time error; boundary-arithmetic
  overflow is a checked-arithmetic runtime error, never silent wraparound; a `Timestamp` offset is a plain
  integer of microseconds, a documented project-specific extension since there is no `INTERVAL` type).
  Aggregate discovery scans every window's `PARTITION BY`/`ORDER BY`/argument expressions too, so an ordinary
  aggregate appearing only inside a window spec (e.g. `RANK() OVER (ORDER BY SUM(rev) DESC)`) still makes the
  whole statement an aggregate query. A window function is rejected as another window's own argument/
  `PARTITION BY`/`ORDER BY` (no nested windows) and as an ordinary aggregate's argument; windows remain
  forbidden directly inside `WHERE`/`GROUP BY`/`HAVING`/`JOIN ON`. Windows are evaluated in a dedicated stage
  that runs unconditionally right after the existing `HAVING`-filter step and before final projection (a
  no-op pass-through for a non-aggregate query, exactly as an absent `GROUP BY` already is), so a window can
  combine with `GROUP BY`/aggregates and read a surviving group's aggregate values — and `HAVING` referencing
  a window function's result by alias is a bind error, since the window stage has not run yet when `HAVING`
  runs.

### Completed local MVP — Phase 14 additions

- **`ANALYZE TABLE t`:** binds to `BoundStatement::AnalyzeTable`, routes as `Route::CatalogDdl` (`FOR COLUMNS`,
  `NOSCAN`, and partition-scoped forms are bind errors). Execution scans every partition at one MVCC snapshot
  via the existing `scan_partition_compact` path and records an exact row count and, per column, null count,
  min, max, and an exact distinct count capped at a configurable limit (`LocalServer::with_analyze_distinct_limit`,
  default 200,000 — past the cap, `distinct_count` is `None`, everything else is still collected). Stats
  publish by CAS of only the `stats` field after re-verifying the table's id, retrying on an unrelated
  concurrent CAS conflict; `HTAPCAT1` `FORMAT_VERSION` bumped 3 -> 4 (`TableDescriptor.stats: Option<TableStats>`,
  `#[serde(default)]`; a v3-or-earlier catalog decodes with `stats: None`). Statistics are table-level only
  (aggregated across every partition of the table), never expire automatically, and are never checked for
  staleness against subsequent writes. **Stated behavior, proven by test:** like `CREATE TABLE`/
  `DROP TABLE`/user-management DDL, `ANALYZE TABLE` is included in `Session::is_ddl`'s match list, so it
  is rejected by the "no DDL inside an open transaction" gate (explicit or implicit autocommit-off), and the
  rejection does not poison the transaction; `EXPLAIN ANALYZE` follows the transaction rules of the statement
  it actually executes, while plain `EXPLAIN` (which only plans) remains permitted inside an open transaction.
  See `crates/htap-server/tests/session.rs::{test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives, test_explain_analyze_wrapping_ddl_rejected_inside_open_transaction, test_explain_analyze_wrapping_insert_rejected_inside_read_only_transaction}` and `crates/htap-server/tests/explain.rs::test_plain_explain_select_permitted_inside_open_transaction`.
  Each column's min/max bound reserves the replacement's memory before dropping the old reservation
  (`AnalyzeAccumulator::replace_bound`), so a failed reservation leaves the existing bound and its
  accounting exact rather than double-counting or under-counting
  (`crates/htap-server/src/analyze.rs::analyze::tests::replace_bound_preserves_existing_reservation_when_budget_is_exhausted`).
  **Disclosed, not fixed:** `ANALYZE TABLE` fully materializes each partition before reserving its
  memory, so usage can overshoot before the check runs, and a partition whose estimate lands above the
  budget fails `ANALYZE` outright rather than being handled incrementally.
- **Cost-based optimization:** a storage-agnostic `htap_sql::optimize` stage, enabled by default for every
  `Route::Query` execution. Cost estimators (`estimate_row_count`/`estimate_equality_selectivity`/
  `estimate_range_selectivity`/`estimate_join_cardinality`) report whether each estimate came from real
  statistics or a hard-coded default (`EstimateSource::{Stats,Default}`). A predicate-atom inventory classifies
  every `WHERE`/`ON` conjunct by provenance and mobility (a conjunct pinned to one join, restricted to
  post-join-only, or freely movable within a connected `Inner`/`Cross` component — correctly handling both
  classic outer-join traps: a `WHERE`-clause filter on a null-supplying slot, and an `ON`-clause filter that
  touches the null-supplying side); join reordering runs subset dynamic programming for up to 8 relations per
  component and falls back to a greedy cheapest-connected-extension heuristic above that. An always-on
  predicate-conservation validator asserts no predicate was dropped or duplicated after reordering; on any
  internal inconsistency, `optimize` falls back to the identity (unoptimized) plan rather than ever risking a
  wrong result. A recursive CTE's recursive term is a permanent optimization barrier — never reordered into or
  merged with an enclosing join component, by design.
- **`EXPLAIN`/`EXPLAIN ANALYZE`:** `BoundStatement::Explain{inner, analyze}` (`verbose`/`query_plan`/
  `estimate`/non-default `format` are bind errors). For a general query, renders one row per plan node
  (`node_id, parent_id, operation, table, est_rows, estimate_source, build_side`); for a complete-PK point
  lookup or a narrow single-table scan, renders a single-node plan and never calls `htap_sql::optimize::optimize`
  at all — preserving R5 through `EXPLAIN` too. `EXPLAIN ANALYZE` additionally executes the statement and
  reports root-level `actual_rows`/`actual_time_ms` (child-node attribution is best effort, not exact).
- **Scope: `Route::Query` only.** The memory budget and every spilling behavior below apply to the general
  executor only. A single-table `SELECT` with `ORDER BY`, `GROUP BY`, or a plain aggregate and no join routes
  to the narrow analytic scan path (`Route::OlapScan`) instead, which has no memory budget and never spills —
  by design, not an oversight. (Several early spill tests were silently exercising this unbudgeted path; they
  were rewritten to use a join or another shape that reaches the general executor.) The budget also only
  bounds each operator's own working memory (hash tables, sort runs, aggregate state, partition buffers); it
  does not bound the rows materialized between operators, because the executor is not pipelined.
- **Spilling:** a per-statement `MemoryBudget` (default 256 MiB, `LocalServer::with_query_memory_budget`) with
  one level of disk spilling — non-durable scratch files under `<data-root>/spill/<statement-id>/` (magic
  `HTAPSPIL`, deliberately no CRC/fsync/version-range contract, since this is disposable scratch, not a
  durable artifact), swept in full on every `LocalServer::open` after the root lock is acquired — for
  hash-equi-joins whose build side would exceed budget (partition count sized from the input and the
  remaining budget, capped at 128 — windows share this cap, to bound open file descriptors and the writer
  buffers the budget doesn't count), `GROUP BY` (including `AVG`/`DISTINCT` aggregates,
  merged correctly across partitions rather than averaging-of-averages; its own fixed 16 partitions), `ORDER BY` (external merge,
  preserving `NULLS FIRST`/`LAST` and tie-breaking), `DISTINCT`/`EXCEPT`/`INTERSECT`/`UNION DISTINCT`
  (preserving `ALL`-variant multiplicities; also a fixed 16 partitions), and window functions (hash-partitioned
  by the `PARTITION BY` key — a window with no `PARTITION BY` is a single partition — processed one partition
  at a time and reassembled in original input row order). A partition still over its budget after one level of
  spilling — skew, or the partition-count cap reached — fails cleanly (the memory-budget error) rather than
  recursing into a second spill level. **Disclosed, not fixed:** unlike the hash join and window, whose
  partition counts scale with input size and the remaining budget (`GROUP_BY_SPILL_PARTITIONS` and
  `SET_OPERATION_SPILL_PARTITIONS` in `crates/htap-server/src/query_exec.rs` are each a fixed `16`, not
  budget-scaled), `GROUP BY`'s and the set operators' partition count does not grow with a larger budget: an
  input much larger than roughly 16x the budget fails with the memory-budget error instead of spilling
  successfully. Observed directly: a set operation over 4,096 awkward-double rows failed at a 64 KiB budget,
  and succeeded once the input was reduced to 1,536 rows
  (`crates/htap-server/tests/spill.rs::test_spilled_set_operations_and_distinct_preserve_awkward_doubles`,
  which also asserts `last_query_set_operation_spilled()`/`last_query_distinct_spilled()` fired). Budget-sized
  partitioning for `GROUP BY`/the set operators, like the hash join's, is a deferred improvement.
  `GROUP BY` spill reserves each partition's rows as they are read back,
  then releases that reservation before in-memory aggregation runs so the same bytes are not charged twice on
  the way in; peak memory during one partition's aggregation can therefore approach about twice the budget —
  a disclosed imprecision, not an exact bound. **Disclosed, not fixed:** the abandoned-spill sweep at
  `LocalServer::open` only logs a failure to remove the directory (`eprintln!`) rather than returning an
  error; a stale spill file left behind by a failed sweep can collide once with a statement id reused after
  a process restart (`SpillWriter::create` opens with `create_new(true)`), and that one statement fails with
  an `AlreadyExists` I/O error. The collision heals on its own once that statement id has been consumed.
  Spill telemetry is per operator: `LocalServer` exposes
  `last_query_hash_join_spilled()`, `last_query_group_by_spilled()`, `last_query_sort_spilled()`,
  `last_query_distinct_spilled()`, `last_query_set_operation_spilled()`, and `last_query_window_spilled()` as
  test telemetry (not a monitoring API), and every spill test asserts its own named operator actually spilled,
  not merely that the statement succeeded.
- **Float overflow and catalog statistics finiteness:** float `+`/`-`/`*`/`/` and `SUM`/`AVG` overflow now
  return `"DOUBLE value is out of range"` (MySQL-compatible) instead of producing `NaN`/`Infinity`; division
  by zero is unaffected and still yields `NULL`. This closes a real risk: `serde_json`, used to encode both row
  mutation payloads and catalog statistics, cannot represent a non-finite float (it encodes as `null`, which
  fails to decode back into a non-`Option` float field). On the row path this was already caught — confusingly,
  but safely — by the 2PC participant's pre-commit payload decode (`RowstoreParticipant::decode_payload`,
  called from `prepare`); the catalog statistics path had no equivalent protection and is now closed by
  rejecting non-finite bounds during `ANALYZE TABLE` and by structural statistics validation (column count,
  null count, distinct count, min/max type agreement, `min <= max`, and finiteness) on every catalog publish —
  no format change. `CAST(... AS DOUBLE)` from a string (`CAST('nan'/'inf'/'1e999' AS DOUBLE)`) and an
  out-of-range float literal (e.g. `1e400`) return the same `"DOUBLE value is out of range"` error
  (`crates/htap-server/tests/query_exec.rs::{test_non_finite_double_casts_and_literals_are_rejected,
  test_non_finite_cast_update_fails_at_statement_and_leaves_value_unchanged}`). The narrow analytic scan
  path's own `SUM` accumulator (`crates/htap-server/src/olap.rs`) now carries the same overflow check as the
  general executor's `SUM`/`AVG`
  (`crates/htap-server/tests/query_exec.rs::test_analytic_float_sum_overflow_returns_out_of_range_error`),
  closing what was previously an unverified, code-inspection-only gap.
- **`serde_json`'s `float_roundtrip` feature is a precaution, not a bug fix.** Enabled workspace-wide because
  a review claimed the default ("best-effort precision") float parser could decode a stored `DOUBLE` one ULP
  off from the value that was encoded. Tested with the feature switched off, the default parser round-tripped
  50,000 random finite bit patterns plus adjacent doubles, subnormals, the extremes, and `-0.0`
  bit-identically on this `serde_json` version — no drift was demonstrated. The feature is kept anyway
  because it guarantees exact round-trips regardless of `serde_json` version, at some parse-speed cost; there
  is no on-disk format change, only a parsing-library behavior. Covered by
  `crates/htap-common/src/types.rs::types::tests::test_serde_json_float64_roundtrips_bit_identically`,
  `crates/htap-server/tests/session_recovery.rs::test_double_values_round_trip_bit_identically_after_reopen`,
  and `crates/htap-server/tests/spill.rs::test_spilled_set_operations_and_distinct_preserve_awkward_doubles`.
  Spilled set operations and `SELECT DISTINCT` emit rows by source index rather than by looking one up in a
  map keyed by a decoded row, making them independent of round-trip exactness — hardening, not a fix for a
  demonstrated bug.
- **Parallelism:** bounded intra-query parallelism (`std::thread::scope`, no new dependency;
  `LocalServer::query_parallelism`, default `available_parallelism()`) for `GROUP BY` above a size threshold
  and for `INNER`/`CROSS` hash joins with the build side on the right, deterministic across worker counts and
  sharded by `hash(join key) % k`/`hash(group key) % k`, reassembled by originating row ordinal to preserve
  the documented no-`ORDER BY` determinism contract; one shared worker budget per statement so a join nested
  inside a parallel `GROUP BY` does not multiply thread counts. `LEFT`/`RIGHT`/`FULL` joins stay
  single-threaded.
- **Code health (P2, partially fixed — see ADR-023 and `docs/PROBLEMS.md`):** one `EvalContext` constructor
  replacing every hand-built literal across `query_exec.rs`/`lib.rs`/`session.rs`; `SelectBody.join_tree` is
  always populated at bind time now, so `evaluate_join_tree` is the only join execution path (the old
  flat-loop branch and `SelectBody.joins`/`tree_only` fields are gone); the
  `#[allow(clippy::too_many_arguments)]` count in `query_exec.rs` dropped to zero. **Not delivered:** the
  planned shared leaf-helper module between `htap_sql::binder` and `htap_sql::binder_query` (literal binding,
  cast-target mapping, comparability checks, scalar-function signature checks, schema column lookup) — the two
  binder entry points are kept deliberately separate (so R5 stays structural, not a runtime check — see
  ADR-023), but they still each hold their own copy of that leaf-level logic.

### Explicitly deferred features

- **`UPDATE` transaction payload cap, no chunking:** The scan form of `UPDATE` commits every rewritten row
  in one transaction, inheriting the 2PC transaction's effective payload cap — nominally
  `htap_txn::participant::MAX_PAYLOAD_SIZE` (16 MiB), but actually about 4 MiB once the journal `Intent`
  frame's own encoding is accounted for (see "Effective 2PC transaction payload cap" above). There is no
  chunking across multiple transactions for a single `UPDATE` statement that would exceed it.
- **Data-mover / `UPDATE` lock asymmetry:** `LocalServer::execute` (and therefore `UPDATE`) serializes under
  `execution_lock`, but `LocalServerDataMover`'s methods (`import`, `repair_tablet`) do not take that lock. A
  caller sharing one `LocalServer` across threads can race an `UPDATE`'s read-modify-write against a
  concurrent import or repair on the same table. This is a pre-existing gap in the data-mover facade, not
  introduced by `UPDATE`, but `UPDATE` is the first read-modify-write SQL path to make it observable.
- **`DROP TABLE` is metadata-only at statement-return time; reclamation is eventual, not immediate (Phase 15):**
  the same catalog CAS that removes the table marks its tablets `pending_reclaim`, but the rowstore data and
  columnar segments of the dropped table's tablets may still be on disk, unreachable, when the `DROP TABLE`
  statement itself returns — `LocalServer::reclaim_tick`/`compaction_tick` reclaim them over as many
  maintenance calls as it takes (see "Rowstore compaction, garbage collection, and DROP TABLE reclaim scope
  and deferred features" below). Dropped identifiers are never reissued in the meantime (see the catalog
  identifier high-water mark below), so unreclaimed data can never be aliased by a new table.
- **Catalog format version 2:** `CatalogSnapshot.id_high_water` (`crates/htap-catalog/src/model.rs`) persists
  the highest allocated `table`/`partition`/`tablet`/`replica` id so dropped ids are never reissued. The
  `HTAPCAT1` envelope format version bumped 1 -> 2 (`crates/htap-catalog/src/local.rs`); a version-1 catalog
  still decodes, with counters falling back to the live maximum, and is rewritten as version 2 on the next
  CAS; a version-1-only binary refuses to open a version-2 catalog; a version-2 payload that omits
  `id_high_water` is rejected as corruption. `LocalServer::open` also runs a one-time legacy migration
  (`migrate_legacy_id_high_water`) that raises the tablet counter to the highest `colstore/tablet-*`
  directory actually on disk before the first v2 CAS, because a v1 catalog could have removed empty
  partitions via `ALTER TABLE ... DROP PARTITION` whose tablet directories survive on disk with no live
  catalog reference. `LocalCatalogStore::compare_and_set` additionally rejects any successor whose
  high-water mark would regress. Replica ids (used by `htap-coord`, since a `ReplicaId` names a movement
  package directory) are allocated the same way, from the persisted high-water mark, not just the live
  maximum. **Known remaining gap:** the v1-to-v2 legacy migration (`migrate_legacy_id_high_water`) seeds
  only the *tablet* high-water mark from the `colstore/` on-disk inventory; it does not recover *replica*
  ids that were removed by a v1-era `ALTER TABLE ... DROP PARTITION` from any on-disk trace. A movement
  snapshot package lives at `<root>/movement/tablets/<source_tablet_id>/<target_replica_id>/<job_id>`, so a
  reissued replica id could in principle coincide with a stale package directory left behind by a replica
  that was removed along with its tablet under version 1 — but only if a *new* replica for a now-empty
  tablet is later assigned both that same numeric id and the same job id as a prior clone job, which is a
  narrow coincidence, not a routine hazard. This is a documented gap, not fixed in code. See "Catalog
  identifier high-water mark" in `docs/ARCHITECTURE.md`.
- **Memory, concurrency, and optimizer limits of the general executor (Phase 14, see ADR-023):** A
  per-statement `MemoryBudget` (default 256 MiB, `LocalServer::with_query_memory_budget`), applying to
  `Route::Query` only (a single-table `SELECT` with `ORDER BY`/`GROUP BY`/a plain aggregate routes to the
  unbudgeted `Route::OlapScan` path instead — see "Scope: `Route::Query` only" above), now bounds hash
  joins, `GROUP BY`, `ORDER BY`, `DISTINCT`/`EXCEPT`/`INTERSECT`, and window partitions, spilling to
  non-durable scratch under `<data-root>/spill/` one level deep; a partition still over budget after that one
  level fails cleanly (`HtapError::InvalidArgument`) rather than running unbounded — this is real memory
  *bounding*, not unlimited spill depth, and a query whose data is skewed enough to blow one partition still
  fails. `LEFT`/`RIGHT`/`FULL` joins are not parallelized (structurally restricted to `INNER` with the build
  side on the right); the hash-join *spill* path is not itself kind-restricted in code, but only `INNER`-join
  spilling is covered by a test, so outer-join spilling is not asserted correct — see `docs/PROGRESS.md`'s
  Phase 14 row. Window evaluation carries every materialized column of the joined input into its spill
  partitions, not just the columns the query needs (measured: 8 columns / ~424 B per row where 3 are needed),
  inflating window partition sizes under a budget; a column-trimming improvement is deferred. The optimizer's
  leaf-cost and outer-join cardinality estimates are cost-quality-only weaknesses, confirmed by an external
  reviewer to never affect correctness: outer joins are never reordered by the DP/greedy join-reordering step,
  and null-padding on an outer join's unmatched rows is independent of which side was chosen as the hash build
  side. Non-equi joins and `CROSS` joins (no usable equality key) are still evaluated by an in-memory
  nested loop with no memory budget check at all. Bounded intra-query parallelism now covers `GROUP BY` and
  `INNER`/`CROSS` hash joins (`LocalServer::query_parallelism`, default `available_parallelism()`); every
  other stage (filter, `ORDER BY`, `DISTINCT`, set operations, window functions, non-`INNER`/`CROSS` joins)
  still runs single-threaded (each slot's own partition scan still uses the narrow path's scan workers
  internally, so per-slot scanning is still parallel independent of all of the above). A recursive CTE's
  recursive term is a permanent optimizer/parallelism barrier by design (never reordered into or merged with
  an enclosing join component), not a stopgap. Statistics (`ANALYZE TABLE`) are table-level only (no
  per-partition breakdown), have no histograms (min/max/null-count/capped-exact-distinct-count only), and
  have no automatic staleness detection — a table that changes shape after `ANALYZE` keeps using the old
  numbers until the next explicit `ANALYZE TABLE`. `ANALYZE TABLE` is gated by `Session::is_ddl`'s "no DDL
  inside an open transaction" rule exactly like `CREATE TABLE`/`DROP TABLE` are — it is rejected inside any
  open transaction (explicit or implicit autocommit-off) and the transaction survives the rejection; see
  `crates/htap-server/tests/session.rs::test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives`.
- **Still deferred regardless of route:** `LIMIT BY`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`,
  non-partition `ALTER TABLE`, vectorized/pipelined execution, worker-pool parallelism for `LEFT`/`RIGHT`/
  `FULL` joins, and memory-bounded spilling for non-equi/`CROSS` joins (evaluated by an in-memory nested loop
  with no memory budget check at all — genuinely deferred, unlike `LEFT`/`RIGHT`/`FULL` equi-hash joins, whose
  spill code path exists but is only covered by a test for `INNER`), statistics histograms and
  per-partition statistics, automatic statistics staleness detection, semi-join rewrites of `IN`/`EXISTS`,
  broader string/date function coverage (no `DATE`/`DECIMAL`/`EXTRACT`/`SUBSTRING`/`INTERVAL`/views), and
  MySQL's implicit string<->number coercion (a comparison between incompatible types is a bind error here,
  not an implicit cast).
- **Fixed test gap from Phase 13:**
  `crates/htap-client/tests/embedded_client.rs::test_embedded_client_unsupported_sql_preserves_error_categories`
  and `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`
  used to assert `SELECT name, COUNT(*) OVER () FROM t` returns `HtapError::Unsupported`, which stopped being
  true once Phase 13 implemented window functions. Both tests now use a construct genuinely still unsupported
  (`SELECT name FROM products FOR UPDATE`) and pass (`cargo test -p htap-client`, verified with
  `-- --list`/`--exact`). This section is kept as a record that the gap was tracked and closed, not silently
  dropped.

### Verification and test coverage

- `crates/htap-sql/src/expr.rs` unit tests: `three_valued_logic_tables`, `numeric_promotion_and_overflow`,
  `like_in_between_case_cast`, `scalar_functions`, `subquery_and_aggregate_context`, `expr_type_inference`.
- `crates/htap-sql/tests/query_bind.rs`: `test_join_binding_kinds_aliases_and_wildcards`,
  `test_join_binding_errors`, `test_expressions_functions_and_type_checks`,
  `test_aggregates_group_by_having_and_grouping_rules`, `test_order_by_limit_distinct`,
  `test_subqueries_ctes_derived_tables_and_union`, `test_update_drop_show_binding`,
  `test_bound_predicate_evaluation_with_joined_rows`.
- `crates/htap-sql/tests/route.rs`: `test_route_classification`,
  `test_point_read_fast_path_pinned_against_general_query_path`.
- `crates/htap-sql/tests/parse_bind.rs`: `test_negative_select_and_delete`,
  `test_bind_analytic_select_negative`.
- `crates/htap-server/tests/query_exec.rs`: `test_joins_across_row_column_and_converting_tables`,
  `test_outer_joins_null_padding_residual_on_and_null_keys`,
  `test_expressions_aggregates_having_order_limit_distinct`,
  `test_union_derived_tables_ctes_and_subqueries`, `test_partition_pruning_and_pushdown_through_general_path`,
  `test_single_snapshot_across_engines_and_freshness`, `test_general_query_over_reopened_server`,
  `test_update_by_primary_key_and_reopen_recovery`,
  `test_update_by_filter_across_partitions_and_storage_formats_with_reopen`,
  `test_update_by_primary_key_on_column_and_converting_partitions`,
  `test_show_tables_databases_columns_and_describe`, `test_drop_table_reopen_and_no_id_reuse`,
  `test_legacy_catalog_seeds_tablet_high_water_from_colstore_inventory`.
- `crates/htap-server/tests/local_server.rs`: `test_analytic_unsupported_clauses` (rewritten as positive plus
  still-unsupported cases), `test_storage_descriptors_dml_and_point_reads_and_unsupported_non_point`.
- `crates/htap-catalog/tests/catalog_recovery.rs`:
  `test_catalog_v1_envelope_decodes_and_counters_fall_back_to_live_max`,
  `test_catalog_id_high_water_prevents_reuse_after_removal`,
  `test_catalog_cas_rejects_regressing_id_high_water`,
  `test_catalog_v2_payload_without_id_high_water_is_rejected`, `test_corruption_and_truncation`.
- `crates/htap-coord/tests/placement_movement.rs::test_plan_placement_allocates_above_id_high_water`.
- `crates/htap-client/tests/embedded_client.rs::test_embedded_client_unsupported_sql_preserves_error_categories`,
  `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`
  (both pass — see "Fixed test gap from Phase 13" above).
- `crates/htap-wire/tests/wire_server.rs::test_general_sql_over_wire` (joins, `UPDATE`, `SHOW`/`DESCRIBE`,
  `DROP TABLE` over the MySQL wire protocol, exercising the same `LocalServer::execute` path unchanged).

### Verification and test coverage — Phase 13 additions

- `crates/htap-sql/src/expr.rs` unit tests: `numeric_promotion_and_overflow` (extended for `IntDiv`),
  `subquery_runner_trait_and_correlation_eval`, `subquery_invocation_and_nesting_caps`.
- `crates/htap-sql/tests/query_bind.rs`: `test_join_tree_lowering_and_nested_groups`,
  `test_natural_using_coalescing_all_join_kinds`, `test_natural_using_ambiguity_and_errors`,
  `test_correlated_subquery_binding_and_depth_limit`, `test_correlated_subquery_grouped_context`,
  `test_correlated_subquery_binding_is_case_insensitive`, `test_recursive_cte_binding_and_output_schema`,
  `test_recursive_cte_binding_rejects_invalid_shapes`,
  `test_recursive_cte_binding_restrictions_and_non_recursive_forms`,
  `test_window_ranking_and_offset_function_binding`, `test_window_aggregate_and_value_functions_with_frames`,
  `test_value_offset_range_frame_binding_and_key_restrictions`,
  `test_window_functions_over_group_by_and_aggregate_discovery`, `test_having_cannot_reference_window_result`,
  `test_expressions_functions_and_type_checks` (extended for `DIV`).
- `crates/htap-sql/tests/parse_bind.rs`: `test_bind_truncate`, `test_bind_insert_select`,
  `test_bind_delete_general_filters`.
- `crates/htap-sql/tests/prepare.rs` / `crates/htap-wire/tests/wire_server.rs`:
  `test_prepared_insert_select_with_bound_parameter`, `test_prepared_delete_with_filter_reports_affected_rows`.
- `crates/htap-server/tests/query_exec.rs`: `test_order_by_and_group_by_ordinals_execute`,
  `test_except_and_intersect_multiset_semantics`, `test_intersect_binds_tighter_than_union`,
  `test_nested_join_groups_and_full_join_execute`,
  `test_nested_join_groups_using_qualified_access_derived_tables_and_subqueries`,
  `test_using_and_natural_joins_merge_columns`,
  `test_flat_and_tree_join_evaluators_match_for_lowerable_queries` (mandatory differential harness),
  `test_hash_join_preserves_large_integer_keys`,
  `test_delete_by_filter_on_row_table_reports_affected_rows_and_remaining_rows`,
  `test_delete_by_filter_on_column_table_reports_affected_rows_and_remaining_rows`,
  `test_delete_by_filter_prunes_range_partition_and_preserves_other_partitions`,
  `test_unfiltered_delete_empties_table_and_reports_affected_rows`, `test_insert_select_basic`,
  `test_insert_select_halloween`, `test_insert_select_from_join_with_aggregate`,
  `test_insert_select_into_partitioned_target`, `test_insert_select_from_column_format_source`,
  `test_insert_select_duplicate_pk_aborts_atomically`, `test_correlated_exists_in_where`,
  `test_correlated_in_subquery`, `test_correlated_subquery_nested_two_levels`,
  `test_correlated_subquery_caps_fire_during_execution`,
  `test_correlated_subquery_column_outer_pruning_and_having`, `test_correlated_scalar_subquery_in_select`,
  `test_correlated_subquery_in_having_grouped`, `test_correlated_subquery_self_reference`,
  `test_correlated_subquery_across_row_column_converting_and_partitions`,
  `test_recursive_cte_counting_and_hierarchy_traversal`, `test_recursive_cte_union_distinct_vs_all_semantics`,
  `test_recursive_cte_iteration_and_row_cap_bounded_time`, `test_recursive_cte_large_working_set`,
  `test_recursive_cte_over_column_and_partitioned_tables`, `test_recursive_cte_with_type_widening`,
  `test_recursive_cte_outer_query_uses_cte`, `test_window_ranking_functions_ties_and_peers`,
  `test_window_ntile_lag_lead`, `test_window_functions_over_group_by_execute`,
  `test_window_result_used_in_order_by_and_limit`, `test_window_functions_across_storage_formats_and_partitions`,
  `test_window_aggregate_and_value_functions_execute`, `test_window_aggregate_default_and_explicit_frames`,
  `test_window_first_last_value_frames`, `test_window_peer_range_frames`,
  `test_value_offset_range_frame_numeric_with_nulls`, `test_value_offset_range_frame_timestamp_with_nulls`,
  `test_window_aggregates_across_storage_formats_and_partitions`, `test_truncate_empties_table_and_reports_affected_rows`.
- `crates/htap-server/tests/session.rs`: `test_read_your_own_writes_nested_join_group`,
  `test_truncate_in_transaction_rollback_restores_rows`, `test_truncate_in_read_only_transaction_is_rejected`,
  `test_delete_by_filter_in_transaction_rollback`, `test_delete_by_filter_reads_own_writes`,
  `test_delete_by_filter_payload_cap_rejects_atomically`, `test_delete_by_filter_rejected_in_read_only_transaction`,
  `test_delete_by_filter_commit_is_one_version`, `test_insert_select_uncommitted_within_transaction`,
  `test_insert_select_payload_cap_exceeded`, `test_insert_select_in_read_only_transaction`,
  `test_correlated_subquery_sees_uncommitted_session_writes`,
  `test_recursive_cte_reads_uncommitted_rows_and_rollback_hides_them`.
- `crates/htap-server/tests/session_recovery.rs`: `test_uncommitted_delete_by_filter_vanishes_after_reopen`,
  `test_uncommitted_insert_select_vanishes_after_reopen`.
- `crates/htap-server/tests/privileges.rs`: `test_join_using_ungranted_table_is_masked`,
  `test_natural_join_with_ungranted_table_is_masked`, `test_nested_join_tree_with_ungranted_table_is_masked`,
  `test_full_outer_join_with_ungranted_table_is_masked`, `test_visible_but_unselectable_join_tables_are_denied_post_bind`,
  `test_filtered_delete_subquery_with_ungranted_table_is_masked`, `test_delete_without_grants_masks_filtered_target_table`,
  `test_delete_requires_only_delete_for_point_targets_but_select_for_filters`,
  `test_truncate_requires_delete_only_and_masks_hidden_tables`, `test_truncate_if_exists_missing_table_is_a_noop`,
  `test_truncate_if_exists_invisible_table_masks_as_missing`,
  `test_insert_select_requires_select_on_source_and_leaves_target_unchanged_when_denied`,
  `test_insert_select_masks_invisible_source_as_missing_table`,
  `test_insert_select_masks_hidden_sources_in_subquery_and_cte`, `test_insert_select_requires_insert_on_target`,
  `test_recursive_cte_privilege_checks_masking_and_self_reference`.

### Verification and test coverage — Phase 14 additions

See `docs/PROGRESS.md`'s Phase 14 row for the exhaustive, cross-checked (`cargo test -p <crate> -- --list`)
test list; summarized by file:

- `crates/htap-sql/tests/parse_bind.rs::test_analyze_table_accept_and_reject_forms`.
- `crates/htap-catalog/tests/catalog_recovery.rs::{test_catalog_v3_envelope_defaults_table_stats_to_none, test_catalog_v4_corrupted_crc_is_rejected}`.
- `crates/htap-server/tests/analyze.rs` (6 tests: row/column stats, distinct-count cap, reopen recovery,
  concurrent unrelated catalog mutation, missing-table error, a larger `analyze_target` case).
- `crates/htap-sql/src/optimize.rs` unit tests (32, estimators/predicate-atom classification/join
  reordering/conservation-validator mutation tests, including the storage review's outer-join-pinning and
  cross-join-regression tests) and `crates/htap-sql/tests/optimize.rs` (5, named
  outer-join traps plus a property/fuzz reordering test).
- `crates/htap-server/tests/explain.rs` (9 tests, including the two R5-bypass tests
  `test_explain_primary_key_point_lookup_uses_rowstore_fast_path` and
  `test_explain_analytic_scan_uses_olap_fast_path`, `test_plain_explain_select_permitted_inside_open_transaction`,
  and the two `EXPLAIN [ANALYZE] CREATE TABLE` tests `test_explain_create_table_renders_plan_without_creating_table`
  and `test_explain_analyze_create_table_executes_and_creates_table`).
- `crates/htap-server/src/memory_budget.rs` and `crates/htap-server/src/spill.rs` unit tests (4 + 5, the fifth
  spill unit test proving the reader now streams through a fixed buffer instead of loading a whole file).
- `crates/htap-server/tests/spill.rs` (17 tests, each asserting its own operator's per-operator spill-telemetry
  accessor: server-root-and-reopen sweep
  (`test_spill_uses_server_root_and_reopen_sweeps_abandoned_files`), hash-join spill/skew/mixed-key-normalization/
  cross-storage-format/large-join-with-`ORDER BY`, `GROUP BY` spill, an ungrouped-aggregate spill returning exactly one row including on
  empty input, `ORDER BY` spill, `SELECT DISTINCT` spill, `UNION DISTINCT`/`EXCEPT`/`INTERSECT ALL` spill, five window-function spill
  tests (`ROW_NUMBER`/`RANK`/running-sum/no-`PARTITION BY`/skewed-partition)).
- `crates/htap-server/tests/parallel.rs` (7 tests: nested-parallelism budget, `GROUP BY` determinism/`AVG`/
  `DISTINCT` correctness, a `GROUP BY` with a variable in its aggregate expression correctly not parallelizing,
  `INNER` join determinism/row-order/key-normalization).
- `crates/htap-server/tests/differential.rs` (3 tests: `differential_query_execution_paths_agree`,
  `differential_outer_join_hoisting_predicate_from_inner_on_clause`,
  `differential_outer_join_sinking_inner_join_predicate_referencing_null_side` — identity-optimized
  baseline vs. fully optimized vs. fully optimized under a tiny memory budget, same row bag and, where an
  order contract exists, the same row order).
- `crates/htap-server/tests/session.rs` (`test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives`,
  `test_explain_analyze_wrapping_ddl_rejected_inside_open_transaction`,
  `test_explain_analyze_wrapping_insert_rejected_inside_read_only_transaction`): `ANALYZE TABLE` is DDL and is
  rejected inside an open transaction like every other catalog DDL statement.

---

## Prepared statements and binary protocol scope and deferred features (Phase 11)

The Phase 11 implementation delivers the MySQL binary protocol and prepared statements
(`htap-wire::{binary_codec, prepared}`, `htap-sql::prepare`, `htap-client::RemoteClient`), plus
`COM_RESET_CONNECTION`/`COM_CHANGE_USER`, ≥16 MiB message reassembly with a real `max_allowed_packet`, a
CSPRNG handshake scramble, negotiated `CLIENT_MULTI_STATEMENTS`, and shutdown force-close of connections
blocked mid-packet. See "Prepared statements and binary protocol (Phase 11)" and the "Network layer" section
of `docs/ARCHITECTURE.md`, and ADR-019, for the full contract.

### Completed local MVP

- `COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`/`SEND_LONG_DATA`, with `COM_STMT_FETCH` and server-side
  cursors cleanly rejected.
- AST-level placeholder substitution (no text re-render/reparse) covering every literal-bearing position the
  binder accepts, including subqueries, derived tables, CTEs, UNION branches, and `LIMIT`/`OFFSET`.
- Best-effort `PREPARE` response metadata, probed through the real, unmodified binder.
- A per-statement bound-parameter-type cache for `new_params_bound_flag = 0` re-executes.
- The full binary parameter type matrix except `TIME` (`TINY`/`SHORT`/`INT24`/`LONG`/`LONGLONG`/`YEAR`/
  `FLOAT`/`DOUBLE`/`NEWDECIMAL`/`DECIMAL`-as-text/string and blob family/`DATE`/`DATETIME`/`TIMESTAMP`/`NULL`).
- `COM_RESET_CONNECTION` and `COM_CHANGE_USER`, both respecting the ADR-018 `CommitOutcomePending`
  quarantine; `COM_CHANGE_USER` re-authenticates via a new `verify_credentials` seam.
- Messages ≥16 MiB are reassembled/split, bounded by a real, configurable `max_allowed_packet` (default 64
  MiB, `htapd --max-allowed-packet`/`HTAPD_MAX_ALLOWED_PACKET`, reported as `@@max_allowed_packet`).
- A CSPRNG handshake scramble (`getrandom`, no fallback).
- `CLIENT_MULTI_STATEMENTS`, honored only when negotiated, sequential execution with
  `SERVER_MORE_RESULTS_EXISTS`, stopping at the first error (including `DurablePending`/`RecoveryRequired`).
- Shutdown force-closes connections blocked mid-packet.
- `RemoteClient`/`WireClient` gained `prepare`/`execute_prepared`/`close_prepared`/`close_stmt` and
  `query_multi`.
- Every pre-authentication read (handshake response, auth-switch response, `COM_CHANGE_USER` auth-switch
  reply) is bounded by `min(max_allowed_packet, 64 KiB)`, checked before any allocation.
- OK/EOF status flags reflect the connection's real session state (`SERVER_STATUS_AUTOCOMMIT` follows
  `autocommit`, `SERVER_STATUS_IN_TRANS` is set while a transaction is open) instead of a hardcoded constant.

### Explicitly deferred features and known gaps

- **`COM_STMT_FETCH` / server-side cursors:** rejected cleanly; there is no cursor support of any kind.
- **Unsigned 64-bit values above `i64::MAX` are rejected, not represented:** there is no `UInt64` value type
  in the engine. A permanent limitation, not a "not yet implemented" gap — adding one would be a
  storage/type-system change reaching well outside the wire layer (see ADR-019).
- **No exact `DECIMAL`:** `NEWDECIMAL`/`DECIMAL` parameters are decoded and substituted as exact numeric
  literal text (`htap_sql::ParamLiteral::NumericText`), never through a lossy `f64`/`i64` round trip — the
  wire-to-AST step itself loses no precision. From there the binder treats it exactly like the same numeric
  literal typed directly into SQL text, coercing it into the target `Int64`/`Float64` column; precision
  beyond what `Int64`/`Float64` can hold is still lost at that step, not before it, because there is no
  arbitrary-precision decimal type in the engine.
- **`TIME`-typed parameters are rejected:** there is no engine representation for a time-of-day value
  independent of a date.
- **`PREPARE` response metadata is best-effort, not exhaustive:** `num_columns = 0` and generic parameter
  definitions whenever any placeholder's type cannot be inferred from purely local context (e.g.
  `SELECT ? AS x`, a join, or an ambiguous position) — this is a real client-facing limitation, not just an
  internal detail, since some clients size bind buffers off `COM_STMT_PREPARE_OK`.
- **Shim-only forms are not usable inside a multi-statement batch:** `SET CHARACTER SET`/`SET CHARSET` are
  answered by `htap_wire::shim`, which only ever sees one statement at a time, not a `CLIENT_MULTI_STATEMENTS`
  batch's constituent statements.
- **TLS, protocol compression, and per-user ACL/RBAC are implemented as of Phase 12** (`COM_CHANGE_USER` now
  re-authenticates against a catalog-backed account via `LocalServer::authenticate_session`, and prepared
  statements are enforced through the same `check_privileges` gate as text-protocol statements) — see "TLS,
  compression, and account/privilege scope and deferred features" below.
- **Registry limits:** a connection's prepared-statement registry holds at most 4096 statements; buffered
  `SEND_LONG_DATA` bytes across every parameter of every statement are capped by the connection's configured
  `max_allowed_packet`.

### Verification and test coverage

See "Prepared statements and binary protocol (Phase 11)" in `docs/ARCHITECTURE.md` and ADR-019 in
`docs/DECISIONS.md` for the full named-test evidence list (`crates/htap-sql/tests/prepare.rs`,
`crates/htap-wire/src/binary_codec.rs` and `crates/htap-wire/src/prepared.rs` unit tests,
`crates/htap-wire/tests/wire_server.rs`, `crates/htap-client/tests/prepared.rs`).

---

## TLS, compression, and account/privilege scope and deferred features (Phase 12)

The Phase 12 implementation delivers TLS (`htap-wire::tls`, `rustls` 0.23 + `ring`), MySQL compressed-packet
framing (`htap-wire::compression`), and a catalog-backed per-user account/privilege model (`htap-catalog`
format v3, `htap-server::privilege`, new `htap-sql` account-management statements). See "TLS and compression
(Phase 12)" and "Accounts and privileges (Phase 12)" in `docs/ARCHITECTURE.md`, and ADR-020/ADR-021 in
`docs/DECISIONS.md`, for the full contract.

### Completed local MVP

- TLS via `rustls`/`ring` (not `aws-lc-rs`, which needs `cmake`, confirmed unavailable in this environment):
  `htapd --tls-cert`/`--tls-key`/`--require-secure-transport` and matching `HTAPD_*` env vars; `CLIENT_SSL`
  advertised only when configured; a strict 32-byte `SSLRequest`, a real second `HandshakeResponse41` read
  over the upgraded stream; cert/key match validated at startup and on reload; live cert reload
  (`WireServer::reload_tls_certs`) that keeps the old cert on a failed reload and never disturbs existing
  connections.
- MySQL compressed-packet framing (zlib via `flate2`, zstd via `zstd`), negotiated only when the server has it
  enabled (`--disable-compression`/`HTAPD_DISABLE_COMPRESSION` to opt out), activating only after the
  authentication OK packet, with a stateful frame reader/writer bounded by independent decompression limits
  (declared-length rejection before decode, a `.take(declared_length + 1)` decode cap, an exact-length check,
  and a zstd `window_log_max(24)` (16 MiB) window-size cap). A framing error (bad sequence id, truncated or
  corrupt frame, or an empty raw frame) permanently latches the stream as failed rather than resynchronizing;
  a read timeout does not latch it and resumes on the next call.
- Catalog-backed accounts and privileges: `CREATE/ALTER/DROP USER`, `GRANT`/`REVOKE`, `SHOW GRANTS`; a
  bootstrap latch (`accounts_initialized`) that creates `root` from `--password` exactly once and never
  re-fires; `check_privileges` enforcing per-statement, per-table privileges (including `SHOW TABLES`
  filtering, `PREPARE`/`EXECUTE`, and multi-statement batches) under the same `execution_lock`-held snapshot
  as bind and dispatch.

### Explicitly deferred features and known gaps

- **No roles or delegated administration:** `WITH GRANT OPTION` parses but is rejected at bind time; account
  DDL and `GRANT`/`REVOKE` require `Principal::Superuser` outright, with no way to delegate a subset of that
  authority to another account.
- **Only the `%` host is accepted:** `'user'@'host'` with any other host is a bind-time `Unsupported`
  rejection — there is no real network-source-based ACL.
- **No `ACCOUNT LOCK`/`UNLOCK` SQL syntax:** `Account::locked` exists in the model and is enforced at
  authentication, but nothing sets it yet; locking an account today requires the embedded `LocalServer::execute`
  API directly.
- **`mysql_native_password` only, no `caching_sha2_password`:** MySQL 9.x client tooling that removed the
  legacy plugin entirely cannot connect. MySQL 8.x clients (and this server's own `AuthSwitchRequest`
  machinery from ADR-016/019) are unaffected.
- **Password hashes have no per-account salt or KDF work factor:** `SHA1(SHA1(password))` (the same scheme
  `mysql_native_password` uses on the wire) is a fast, unsalted hash; a leaked catalog file's password hashes
  are crackable offline. Mitigated, not eliminated, by `0600` catalog file permissions (Unix) and `Debug`
  redaction (see ADR-021). The final 20-byte hash comparison itself is constant-time
  (`htap_common::password::constant_time_eq_20`), and a failed login (unknown user, locked account, or wrong
  password) always performs the same dummy verification work, so response timing does not reveal whether an
  attempted username exists.
- **`CREATE TABLE` does not auto-grant:** a non-superuser holding a global `CREATE` privilege who creates a
  table gets no implicit privilege on it (matches MySQL); a subsequent `GRANT` (or an existing `Global` grant)
  is required to see the table.
- **`LocalServer::execute` remains unchecked by design:** the embedded, wire-unreachable API is still an
  implicit superuser with no `check_privileges` call — this is the phase's intentional break-glass path, not
  an oversight.
- **No `SIGHUP`-triggered TLS cert reload:** `WireServer::reload_tls_certs()` exists but has no automatic
  trigger; reload must be called explicitly (e.g. by an embedding host process).
- **zstd compression level is clamped to `1..=3`,** narrower than MySQL's full `1..=5` range.
- **`--require-secure-transport` only gates the initial login;** it does not retroactively affect an
  already-established plaintext connection, and there is no server-side enforcement that all *clients* on a
  network actually use TLS beyond this one login-time check.

### Verification and test coverage

See "TLS and compression (Phase 12)" and "Accounts and privileges (Phase 12)" in `docs/ARCHITECTURE.md` and
ADR-020/ADR-021 in `docs/DECISIONS.md` for the full named-test evidence list
(`crates/htap-wire/tests/{tls,compression,accounts}.rs`, `crates/htap-catalog/tests/catalog_recovery.rs`,
`crates/htap-server/tests/{accounts,bootstrap,privileges,session}.rs`, `crates/htap-sql/tests/parse_bind.rs`,
`vendor/sqlparser/src/parser/mod.rs::tests::test_user_management_statements`).

---

## HTAP conversion local MVP scope and deferred features

The Phase 4 implementation delivers an incrementally verified local single-tablet Row-to-Column storage conversion engine (`htap-convert`), integrating with `htap-catalog`, `htap-rowstore`, `htap-colstore`, `htap-sql`, and `htap-server`. It implements a crash-resumable cutover state machine, durable tablet columnar manifest envelopes with CRC32C validation, atomic manifest and catalog publication, rowstore-authoritative base-plus-delta overlay scans, and online point mutations and point reads during and after conversion.

### Completed local MVP

- **Four-phase cutover state machine:** Deterministic state machine governing partition conversion:
  `SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`.
- **Atomic per-tablet manifest and catalog publication:** Manifest files use the `HTAPTBM1` binary envelope format with format versioning and payload CRC32C checksums. Publication uses atomic staging (`MANIFEST.tmp` replaced to `MANIFEST`) followed by generation-checked catalog CAS updates.
- **Rowstore authoritative base-plus-delta overlay:** Converter API `read_column_partition` (`LocalConverter::read_column_partition` / `htap_convert::read_column_partition`, verified in `crates/htap-convert/tests/materialization.rs`; note this is a converter API, not SQL execution) executes base-plus-delta queries by reading columnar segments up to the conversion snapshot version and overlaying rowstore mutations committed after that base version.
- **Online point writes and reads:** Storage descriptors route point mutations to `Route::RowstoreWrite` (`INSERT`) and, since Phase 13, `Route::RowstoreDelete` (`DELETE`/`TRUNCATE`), and complete-PK reads to `Route::RowstorePointRead`, all without interruption during conversion.
- **Crash resumption and idempotency:** Interrupted conversion resumes safely from persisted state envelopes.

### Explicitly deferred features

- **Whole-dataset materialization:** Partition conversion reads and materializes the entire rowset into memory and intermediate segment files rather than using a streaming or chunked conversion pipeline.
- **Reverse physical data transcoding:** Metadata-only demotion from Column to Row is supported (`convert_table_to_row` / `demote_partition_to_row`), switching catalog metadata to `Row` and clearing `column_manifest` via atomic CAS, while keeping the rowstore authoritative and retaining columnar segment files on disk. Physical reverse data transcoding and physical deletion/reclamation of column files remain deferred.
- **Autonomous background conversion scheduler:** Conversion and demotion advance strictly via explicit synchronous method calls (`conversion_tick`, `tick`, `convert_table_to_column`, `convert_table_to_row`); `tick()` resumes persisted jobs only, without autonomous background scheduling.
- **Columnar delete vectors:** Per-segment bitmap delete vectors on columnar files are deferred. Deletion semantics are handled via tombstones in the rowstore base-plus-delta overlay.
- **Delta-to-base background compaction:** No automatic compaction folds accumulated rowstore deltas into new columnar segments, and `ConversionDescriptor.snapshot_version` never advances on its own — this is unrelated to Phase 15's rowstore compaction (below) and remains deferred.
- **Rowstore-side physical reclamation (Phase 15, narrow local MVP):** `Engine::compact_once` now physically collapses superseded MVCC versions and purges dropped-partition bytes in the rowstore generically, including for converted tables' historical row versions — see "Rowstore compaction, garbage collection, and DROP TABLE reclaim scope and deferred features" below for the full contract and disclosed gaps (tier-driven, not instant; shared-keyspace protection is per SST, not per row; tombstones never elided).
- **Direct SegmentReader pushdown optimization and distributed OLAP scans:** Direct SegmentReader pushdown optimization is now implemented for the compact base path (projection-aware compact reads with single safe predicate-leaf pushdown and delta suppression/overlay); compound AND pushdown beyond one leaf, != pushdown, vectorized aggregation / operator pipelines, joins, and multi-tablet/distributed scans are deferred.
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions, range/list partition boundaries, and distributed cutovers are deferred.

---

## Rowstore compaction, garbage collection, and DROP TABLE reclaim scope and deferred features

The Phase 15 implementation delivers a narrow local slice: contiguous-run, entry-count-tiered `Engine::compact_once`
(`htap-rowstore`), a per-tablet movement/reclaim lease set (`htap-movement`), catalog `pending_reclaim`
(`HTAPCAT1` v5), and `DROP TABLE` artifact reclamation driven by `LocalServer::reclaim_tick`/`compaction_tick`
(`htap-server`), plus an unrelated `txn.journal` checkpoint (`htap-txn`, see below). See ADR-024 and
`docs/ARCHITECTURE.md`'s "Rowstore compaction, garbage collection, and DROP TABLE reclaim (Phase 15)" section
for the full design.

### Completed local MVP

- **Contiguous-run compaction, spliced in place:** `Engine::compact_once` selects only SST ids that form one
  contiguous run in the manifest's current order, and splices the merged output into that run's original
  position — never prepends it — so a newer, unselected SST always still wins a shared key. An initial draft
  prepended the output and could resurrect a stale value behind a tombstone; fixed and pinned by
  `crates/htap-rowstore/tests/compaction_ordering.rs`.
- **Manifest watermarks (`HTAPMAN1` v3):** `committed_version_high_water` and `gc_low_water`, both monotonic,
  refusing to publish a regression. A read at a real snapshot below `gc_low_water` is rejected with a clear
  error rather than silently served collapsed data.
- **The engine's own directory lock:** `Engine::open` takes an exclusive advisory lock on `<rowstore>/LOCK`
  for its whole lifetime; a second `Engine::open` on the same directory, in or out of process, fails.
- **Lease-gated compaction:** `DROP TABLE`'s forced tablet set is protected all-or-nothing; the ordinary
  tiered pass is protected best-effort, so one busy tablet does not stall the whole tick.
- **`DROP TABLE` artifact reclamation:** column-store directories, movement package directories, and that
  tablet's movement job records (S2) are deleted once a lease is available; rowstore purge is confirmed via
  exact (not range-conservative) partition-presence checks across as many `compaction_tick` calls as it takes,
  including a `flush_roll_and_gc` step so no replayable row of a purged partition survives a crash from an
  un-rolled WAL segment.
- **`gc_low_water` is mirrored into `read_state` (storage-review fix):** `Engine::get`/`scan_partition` used to
  read `gc_low_water` through `commit_lock`, so a pure reader could block behind a concurrent writer — a
  lock-order hazard for the hot read path, not merely a slowdown. `ReadState` now carries its own mirrored
  copy, kept in sync at every point `read_state.ssts` is already swapped, so readers only ever take
  `read_state.read()`. `crates/htap-rowstore/tests/concurrent_reads.rs`.
- **`compaction_tick`'s convergence loop protects every denied partition per pass, with a bounded fallback
  (storage-review fix):** if the lease-protection loop does not stabilize within its 8-iteration cap,
  `compaction_tick` skips only the `compact_once` rewrite for that tick and still runs `flush_roll_and_gc`,
  purge confirmation, and reclaim, reporting `ran: true` with an explicit non-convergence reason rather than
  silently skipping everything or erroring. `crates/htap-server/tests/compaction_convergence.rs`.
- **Reclaim skips undecodable job files belonging to other tablets (storage-review fix):** a corrupt `JOB`
  file for a *different* tablet no longer blocks reclamation of the tablet actually being cleaned up.
  `crates/htap-movement/tests/corrupt_job_isolation.rs`.
- **Exports hold their tablet lease for the whole scan-and-write ("X batch" fix):** COPY TO CSV/JSONL and file
  export now hold the same per-tablet movement lease for the export's entire pinned-snapshot scan and write,
  so an export and a reclaim lease on the same tablet are mutually exclusive in either direction — an export
  attempted while a reclaim lease is held fails with `HtapError::Conflict` instead of racing the reclaim.
  `crates/htap-movement/tests/export_leasing.rs`.
- **`compact_once`'s GC horizon clamp has no write-side exemption, and clamps to `visible_version`, not
  `committed_version` ("X batch" fix, corrected in a follow-up "Y batch" pass):** the horizon used to decide
  which versions collapse and the horizon used to raise `gc_low_water` are now always the same clamped value
  (`input.gc_horizon.min(read_state.visible_version)`), including for the `u64::MAX` "collapse everything"
  sentinel. An earlier draft's write-side exemption let a `u64::MAX`-horizon compaction collapse versions
  without raising the read-side floor to cover them, so a real snapshot at an older, now-collapsed version
  could silently read stale data instead of being rejected ("X batch" fix). That fix clamped to
  `committed_version`, which a second review found unsound: `apply_external` can advance `committed_version`
  without yet publishing it to `visible_version`, so clamping to `committed_version` could raise `gc_low_water`
  above `visible_version` and reject every fresh snapshot, since no snapshot is ever bounded by anything but
  `visible_version`. The clamp now uses `visible_version`; `committed_version` is unaffected and still feeds
  `committed_version_high_water`. `crates/htap-rowstore/tests/horizon_clamp.rs::{test_infinite_gc_horizon_is_clamped_to_committed_version, test_infinite_gc_horizon_does_not_exceed_visible_version}`
  (the first test's committed and visible versions happen to coincide; the second constructs a
  committed-ahead-of-visible scenario to exercise the distinction).
- **`delete_tablet_movement_artifacts` fsyncs its parent directories ("X batch" fix):** after removing a
  tablet's package directory and referencing job directories, the tablets directory, jobs directory, and
  movement root are all fsynced, so a crash right after deletion cannot leave those entries resurrectable from
  stale directory metadata.
- **`delete_tablet_movement_artifacts` tolerates a missing `jobs/` directory ("Y batch" fix, Y3):** an earlier
  draft propagated the `NotFound` error from opening `jobs/` for the scan, so a tablet reclaimed on a root that
  had never run a movement job (and therefore never created `movement/jobs/`) failed reclamation outright. A
  missing `jobs/` directory is now treated as empty and the parent-directory fsyncs still run.
  `crates/htap-movement/tests/missing_jobs_dir.rs::reclaim_succeeds_when_jobs_directory_is_missing`.
- **The colstore reclaim step now fsyncs `<root>/colstore` ("Y batch" fix, Y4):** `reclaim_tick_locked`'s
  colstore-deletion callback calls `sync_dir` on the colstore root immediately after removing a tablet's
  colstore directory, before `delete_tablet_movement_artifacts` and the catalog CAS that marks
  `colstore_and_movement_reclaimed`, closing the same durability gap Y3/X3 close for the movement side. No
  dedicated test: an fsync's effect on a non-crash-injected test run is not independently observable.
- **`entries_purged` only counts confirmed CAS successes ("X batch" fix):** `compaction_tick`'s report used to
  count every `pending_reclaim` entry it attempted to confirm, regardless of whether the confirming catalog
  CAS actually landed; a lost race against a concurrent catalog writer could overstate how many entries were
  durably confirmed. It is now assigned only after the CAS succeeds. Exercised by the existing `entries_purged`
  assertion in `crates/htap-server/tests/compaction_tick.rs`; there is no dedicated regression test for the
  concurrent-CAS-conflict undercount path itself.
- **`LocalDataMover`'s global lease mutex is held across artifact deletion, disclosed, not fixed (F5).**
  `delete_tablet_movement_artifacts` holds the same single mutex the lease-acquire/release paths use for its
  entire body — the colstore-adjacent tablets-directory and job-record deletions, the job-directory scan, and
  all three `sync_dir` calls. Every lease acquire or release for *any* tablet (not just the one being
  reclaimed) stalls for that duration. This is a performance/liveness issue only, not a correctness one: no
  data race, just reduced concurrency during reclaim deletion.

### Explicitly deferred features and disclosed limitations

- **Shared-keyspace protection is per SST, not per row.** The rowstore is one shared keyspace; a flushed SST
  can hold rows from every partition written since the previous flush. A movement- or reclaim-leased (busy)
  partition therefore blocks compaction of *every* SST that contains or spans it, not just that partition's
  own rows — this is the dominant reason compaction can be slow to make progress on a busy root.
- **The explicit-SST-id compaction path compacts only the first contiguous run per `compact_once` call.**
  `compaction_tick`'s convergence loop may need several calls to fully process one preview's candidate set,
  and a dropped partition scattered across several non-adjacent SSTs purges over as many contiguous sub-run
  passes as it takes, not in one pass.
- **Compaction is explicit-tick-only, with no background thread and no SQL trigger**, matching
  `conversion_tick`'s existing operational model; `compaction_tick()` blocks all SQL for its duration.
  There is still no SQL flush statement — tests force SSTs by dropping the server, opening the engine
  directly, flushing, and reopening the server.
- **Tombstones are never elided**, even below `gc_low_water` — a design choice, not an oversight, since
  full-coverage tombstone elision under an arbitrary partial-compaction schedule is a harder correctness
  problem this phase deliberately did not take on.
- **Movement/reclaim leases are intentionally non-durable**, lost on crash. This is sound only because nothing
  that survives such a crash can still be reading the tablet the lease protected — see ADR-024's stated
  precondition, which a future resumable-movement-job feature (one that continues an in-flight read against a
  fixed historical snapshot across a restart) would break, at which point leases would need to become durable.
- **`ALTER TABLE ... DROP/REORGANIZE PARTITION` reclamation remains untouched** — it only ever operates on
  empty source partitions (so this is a currently-inert gap, not an observed data leak), allocates no
  `pending_reclaim` entry, and reclaims nothing.
- **A resurrected row from an un-rolled WAL segment is a disk leak, not visible corruption** — it lands under
  a partition id that is never reused (`IdHighWater`), so no live query can reach it, but `flush_roll_and_gc`
  only runs when purge confirmation actually calls it, not on every compaction.
- **The catalog's `pending_reclaim` entry only clears once both flags are true**, which can take several
  `compaction_tick` calls for a large or heavily contended table; a dropped table's disk space is reclaimed
  eventually, not synchronously with the `DROP TABLE` statement's return.
- **This does not touch the columnar side at all**: delete vectors on columnar segments and delta-to-base
  background compaction (folding rowstore deltas into new columnar segments) remain deferred, as before — see
  "HTAP conversion local MVP scope and deferred features" above.
- **Golden-bytes/format coverage:** `HTAPMAN1` v3 decode cross-checks the external-apply ledger's max version
  against `committed_version_high_water`, rejecting the combination as `Corruption` — this closes one specific
  hand-corruption case, not a general v3 fuzzing pass.
- **Post-manifest-publish `compact_once` error leaves the in-memory manifest stale until reopen (liveness
  only, no data loss).** `compact_once` fsyncs `MANIFEST` to disk before updating its own in-memory copy; an
  error in between (e.g. a directory-fsync-adjacent failure) leaves the on-disk file ahead of the in-memory
  one. A later flush then builds its new manifest from the stale copy, and `Manifest::atomic_publish`'s
  regression-refusal check (comparing against what is actually on disk) correctly refuses it — so no
  corruption occurs, but every subsequent flush is refused until the engine is reopened (a fresh `Engine::open`
  re-reads the real, current `MANIFEST` and resolves the staleness).
- **A flush's new SST reader can fail to open after the manifest already lists it (liveness only, no data
  loss).** `flush_locked` publishes the manifest before opening the new SST's reader; if that open fails, a
  later compaction selecting a run that includes that SST id finds no matching reader in `read_state` and
  fails safely with `HtapError::Corruption` rather than compacting a truncated view, until the engine is
  reopened (which reloads both structures consistently from the same on-disk manifest).
- **Undecodable movement job directories are skipped by reclaim, never deleted or reported.** A `JOB` file
  `LocalDataMover::delete_tablet_movement_artifacts` cannot decode while scanning for the target tablet's
  records is now skipped rather than failing the whole scan (closing a finding that one corrupt, unrelated
  tablet's job directory could block reclamation of a different, healthy tablet) — but the corrupt directory
  itself is left on disk indefinitely, with no sweep or report calling it out.

### Transaction journal checkpoint scope and deferred features

`TransactionManager::checkpoint()` compacts `txn.journal` (a new `HTAPTXC1` baseline envelope,
`txn.checkpoint`) by dropping resolved `Intent`/`Commit`/`Abort` records once every participant's committed
version is cross-checked against the fold's effective maximum. It is triggered opportunistically after a
commit (once the journal exceeds half its configured size limit) and finalized once at `LocalServer::open`
(`finalize_open`). Deferred/disclosed:

- **Best-effort, not guaranteed:** a workload dominated by long-lived, still-unresolved `Intent`s has nothing
  to drop, and `checkpoint()`/`finalize_open()` both refuse outright (never partially) while recovery is
  required or the journal is poisoned — the underlying "journal can still eventually exceed
  `max_journal_size`" limitation above is narrowed, not eliminated.
- **Any error partway through the journal rewrite unconditionally latches `RecoveryRequired`**, even if a
  defensive re-open of the *old* handle happens to succeed — a deliberately conservative choice (a rewrite
  error leaves the durability of the replacement unknown) that trades availability for never silently
  operating on an untrusted handle. Only `recover()` or a full manager reopen clears the latch.
- **The `open_for_bootstrap` read-time ceiling (`max(configured max_journal_size, 2 GiB)`) is temporary and
  read-only:** it lets `TransactionManager::open`/`recover()`, and now `checkpoint()` itself ("X batch" fix
  below), read and fold an already-oversized journal; `finalize_open`'s subsequent re-open at the *configured*
  limit still fails with `HtapError::Corruption` if checkpointing could not shrink the file below it.
- **`checkpoint()` reads through `max(configured_max_journal_size, RECOVERY_BOOTSTRAP_MAX_BYTES)`, not the live
  handle's configured limit and not a fixed 2 GiB cap ("X batch" fix):** an earlier draft read journal records
  through the live `Journal` handle's own `max_journal_size`, which after `finalize_open` is the *configured*
  limit — so a journal one oversized commit had already pushed past that limit could never be read back for
  checkpointing at all, and the opportunistic post-commit trigger would fail on every subsequent commit instead
  of shrinking the file. `checkpoint()` now reads through the larger of the two ceilings, the same one `open`/
  `recover()` already use, so a journal within that ceiling but over the configured limit can still be folded
  and rewritten back under it; a fixed 2 GiB cap (an earlier "X batch" formulation of this fix) would have
  refused a journal between 2 GiB and a larger configured limit instead. No test covers the more-than-2-GiB
  case, since it would require constructing a journal over 2 GiB. `crates/htap-txn/src/manager.rs`'s
  `test_checkpoint_compacts_journal_that_exceeds_configured_limit` and
  `test_finalize_open_restores_configured_journal_limit`.
- **The `MANIFEST` v2 external-apply ledger's own hard cap (`MAX_APPLIED_EXTERNAL_TXNS`) is unrelated and
  unaddressed** — see the top-level bullet and "Remaining rowstore & transaction gaps" above; do not conflate
  the two.
- **`TransactionManager::new` does not itself load the durable checkpoint baseline (latent API hazard, not
  reachable today).** `TransactionManager::new(journal)` initializes `checkpoint_baseline` to
  `CheckpointBaseline::default()` rather than reading `txn.checkpoint`; only `open_with_options` loads the
  baseline (`checkpoint::load_checkpoint`) and it does so immediately after calling `Self::new`, overwriting
  the default before any caller can observe it. `new` is `pub`, but its only caller in this workspace is
  `open_with_options` itself; a future caller that constructs a `TransactionManager` directly from a `Journal`
  (bypassing `open`/`open_with_options`) would silently start from an empty checkpoint baseline instead of the
  durable one, which would be an easy way to accidentally resurrect intents/commits a real checkpoint had
  already dropped. Fix this before adding any such caller, not after.
- **The checkpoint file name is fixed per directory (latent API hazard, not reachable today).** `txn.checkpoint`
  (`CHECKPOINT_FILE` in `crates/htap-txn/src/checkpoint.rs`) is derived from the journal's parent directory
  alone, not from the journal's own file name; two `Journal`s opened against the same directory would silently
  share one baseline file and corrupt each other's checkpoint state. `LocalServer` always gives each data root
  exactly one `txn.journal`, so this cannot arise through any path this workspace exercises today, but it
  would need addressing before any future feature opens more than one journal per directory (e.g. per-tenant
  or per-shard journals sharing a root).

---

## Data movement and persistence bounds scope and deferred features

The Phase 5 implementation delivers single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and `LocalServer` integration. Hardening unit H5/M2 (`b7ff200`) added strict persistence boundaries:

- **H5/M2 fixed (`b7ff200`):** Shared bounded exact-file reader (`read_file_exact_bounded`) enforces size limits on all owned state files (`CATALOG`, `COORDINATOR`, `movement/jobs/<job-id>/JOB`, tablet manifests, `MANIFEST`, `VISIBLE`, `txn.journal`). Internal movement job IDs, tablet paths, and conversion segment paths are strictly validated against traversal and injection attacks.
- **External CopyOptions paths remain caller-controlled by design:** While internal persistence files and paths are bounded and validated, external filesystem paths supplied via `CopyOptions` (e.g. CSV/JSONL import sources and export destinations) are caller-controlled by design.
- **Whole-dataset materialization in export and clone:** CSV/JSONL export (where exports materialize the full logical partition before writing) and tablet snapshot cloning materialize whole datasets in intermediate buffers/files rather than streaming records.
- **Deferred features:** Distributed multi-node coordinated migrations, background replication streams, and cross-partition movement remain deferred.

---

## Coordination and distribution local MVP scope and deferred features

The Phase 6 implementation delivers local coordination, leadership fencing, deterministic placement planning, and coordinator-fenced replica activation (`htap-coord`).

### Completed local MVP

- **Synchronous `Coordinator` trait and durable `LocalCoordinator`:** Synchronous API managing cluster membership, scoped leadership leases, strictly monotonic fencing tokens (`FencingToken`), and coordinator-fenced catalog CAS.
- **Durable `HTAPCRD1` state envelope:** State is persisted at `<root>/COORDINATOR` using a versioned binary envelope with CRC32C checksum and bounded reading (`b7ff200`).
- **Deterministic sorted membership:** Registered cluster nodes (`NodeId`) are tracked in deterministic ascending order.
- **Strictly monotonic fencing tokens:** Persisted high-water marks ensure tokens are never reused across restart.
- **Deterministic placement planner and local activation simulation (`plan_placement`, `activate_placement_plan`):** Computes balanced, colocation-free replica placement plans and simulates local staging and activation under coordinator fencing (verified in `crates/htap-coord/tests/placement_movement.rs`). Placement is metadata planning and local simulation, not physical sharded SQL serving.
- **Exclusive root ownership (`1083fbd`):** `LocalServer` and `LocalCoordinator` acquire an OS-level non-blocking advisory lock (`<root>/LOCK` via `flock`) upon opening to reject concurrent process opens (and symlink aliases) with `HtapError::Conflict`.

### Explicitly deferred features and boundaries

- **One-owner multiprocess-exclusive mode (not concurrent shared-root writers):** Exclusive root ownership means exactly one process ever touches storage directly. As of Phase 16 (ADR-025), a second and later process opening the same root is no longer refused outright: it becomes an IPC client and forwards SQL/session work to the owner over `<root>/htap.sock`, so multiple processes *can* usefully use the same root concurrently — but there is still exactly one storage-level accessor, never concurrent shared-root writers or readers at the storage layer. Standalone low-level subsystem instances (`Engine::open`, `LocalCatalogStore::open`, `LocalDataMover::new`) opened directly outside `LocalServer` do not acquire this lock, do not participate in the owner/client split, and remain unsafe for shared-root concurrent use.
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
| Phase 3 — SQL layer | `Complete (local MVP)` | Completed local slice: sqlparser MySQL dialect parsing, strict binder with typed `PointSelect` and `AnalyticSelect`, structural route classifier, durable catalog with reopen recovery, synchronous `LocalServer` executing across unpartitioned tables (SQL `CREATE TABLE`) and partitioned tables created via SQL DDL (`CREATE TABLE ... PARTITION BY RANGE/LIST`) or native non-SQL API `LocalServer::create_partitioned_table` (finite Range/List, 1 bucket-0 tablet and 1 healthy leader per partition). Supports multi-row INSERT routing across partitions in one commit version, complete-PK DELETE and SELECT routed by partition key (preserving rowstore fast path), and narrow OLAP scans across all partitions over rowstore or base-plus-delta rows using `<root>/colstore` with projection-aware compact reads (PK+requested column union, single safe predicate-leaf SegmentReader pushdown, delta suppression/overlay, and residual SQL evaluation; conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented for narrow local OLAP; distributed fanout, disk spilling, query cancellation, and resource quotas remain deferred), and `EmbeddedClient` façade. Supported partition grammar includes MySQL `CREATE TABLE ... PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` (with optional final `MAXVALUE` for range), as well as typed partition lifecycle DDL (`ALTER TABLE <table> ADD/DROP/REORGANIZE PARTITION`) on empty source partitions with rowstore collapse safety gates and native `LocalServer::alter_partitions` API. Remaining exclusions: partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), subpartitioning (`SUBPARTITION BY`), expressions in partition keys, multi-column `COLUMNS`, non-final `MAXVALUE`, populated DROP/REORGANIZE, and generic non-partition ALTER statements are strictly rejected. Format conversion guarded to single-partition tables for `convert_table`. Verified by `crates/htap-server/tests/local_server.rs` (including `test_sql_range_partitioning_ddl_and_maxvalue_routing`, `test_sql_list_partitioning_ddl_and_routing`, `test_server_sql_alter_partition_lifecycle`, `test_server_alter_partitions_drop_empty_and_populated_guard`, `test_server_alter_partitions_reorganize_empty_and_populated_guard`), `crates/htap-catalog/tests/catalog_recovery.rs`, `crates/htap-sql/tests/parse_bind.rs`, `crates/htap-sql/tests/route.rs`, and `crates/htap-client/tests/embedded_client.rs`. Deferred: populated partition reorganization data migration, physical data reclamation for dropped partitions (`DROP TABLE`'s own artifacts are reclaimed as of Phase 15, see that row; `ALTER TABLE ... DROP/REORGANIZE PARTITION` reclamation remains deferred, and is inert today since that path only ever operates on empty partitions), multi-partition movement, hash tablets, distributed serving/failover, compound AND pushdown beyond one leaf, `!=` pushdown, vectorized aggregation / operator pipelines, multi-tablet/distributed scans, quotas/spill/cancellation, DataFusion/Arrow, and broader auth/security boundary (RBAC, per-user credentials, TLS). MySQL wire protocol serving (`htapd`/`htap-wire`) is implemented as of Phase 8 (see that row); joins/CTEs/subqueries/`UNION`/expressions/`ORDER BY` expressions/aliases/`LIMIT`/`OFFSET`/`HAVING`/`OR`/`NOT`/arithmetic/casts/`AVG`/`DISTINCT` aggregates and non-PK DML (`UPDATE`, `DROP TABLE`) are implemented as of Phase 9 (see that row); sessions/`BEGIN`/`COMMIT`/`ROLLBACK` are implemented as of Phase 10 (see that row); window functions, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, recursive CTEs, and `EXCEPT`/`INTERSECT` are implemented as of Phase 13 (see that row); cost-based query optimization remains deferred. |
| Phase 4 — HTAP conversion | `Complete (local MVP)` | Completed local Row-to-Column conversion MVP (`htap-convert`) with converter APIs `read_column_partition` / `read_column_partition_compact` and `LocalServer` base-plus-delta OLAP scans over `<root>/colstore` with compact base scan pushdown. Table-wide conversion reports (`convert_table_to_column`), Column-to-Row metadata demotion (`convert_table_to_row`) retaining rowstore authority and column files on disk, explicit synchronous policy ticks (`conversion_tick`, `tick`), and fail-closed startup validation on reopen (`LocalServer::open`) are implemented. Open/deferred: whole-dataset materialization in conversion; autonomous background conversion scheduling deferred (ticks are explicit); physical reverse data transcoding deferred; delete vectors, delta-to-base background columnar compaction, compound AND pushdown beyond one leaf, `!=` pushdown, vectorized aggregation / operator pipelines, and distributed partition/table conversion semantics deferred (physical rowstore reclamation and the rowstore's own generic LSM compaction were delivered in Phase 15, see that row — columnar-side reclamation/compaction above remains deferred). |
| Phase 5 — Data movement | `Complete (local MVP)` | Single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and LocalServer façade implemented. H5/M2 bounds and internal path validation added (`b7ff200`). Open: whole-dataset materialization in export (exports materialize full logical partition before writing) and clone; external `CopyOptions` paths caller-controlled by design; SQL `COPY` syntax and bulk-load streaming over the wire protocol (`htap-wire` is a query/result-set protocol, not a bulk data-movement protocol), distributed multi-node coordinated migrations, background replication stream, and cross-partition movement deferred. |
| Phase 6 — Distribution and coordination | `Complete (local MVP)` | LocalCoordinator (`HTAPCRD1`), monotonic fencing tokens, coordinator-fenced catalog CAS, deterministic placement planner, and local activation simulation implemented (placement is metadata/planning/local simulation, not sharded SQL serving). Exclusive root ownership via `<root>/LOCK` added (`1083fbd`) as one-owner multiprocess-exclusive mode (not concurrent shared-root writers; a second process was rejected outright at the time — Phase 16/ADR-025 later added IPC forwarding so a second process becomes a client instead, see that row). H5/M2 envelope bounds added (`b7ff200`). Deferred: Raft/openraft, ZooKeeper backend, watches/locks/KV semantics, distributed consensus, remote replica serving, physical sharded SQL serving, real HA, leader handoff, ongoing replication, capacity/rack placement, and live rebalance; standalone low-level components remain unlocked. |
| Phase 7 — Hardening, benchmarks, local MVP | `Complete (hardened local MVP)` | Hardened transaction commit irrevocability + DurablePending (C2/H1 in `88cc314`), manager decision serialization (H1 in `88cc314`), external apply identity ledger across WAL GC (C1 in `f7a4975`), Engine post-WAL retry/recovery (H2 in `c5ee281`), owned persistence bounds/internal path validation (H5/M2 in `b7ff200`), and exclusive root ownership (`1083fbd`). Built Criterion microbenchmarks (`htap-bench`, `local_mvp`), synchronous embedded client (`htap-client`), operational documentation. Project is a hardened local embedded MVP; production readiness is not claimed. Deferred at the time: `htapd` daemon, MySQL wire protocol, and network endpoints (delivered in Phase 8, see below); Docker image/Compose, ZooKeeper/Raft backends, physical power-loss fsync testing, and TPC-C/TPC-H compliance remain deferred. |
| Phase 8 — Network server | `Complete (local MVP)` | Built a hand-written, synchronous MySQL text-protocol server (`htap-wire::WireServer`, `WireServerConfig`) over `Arc<LocalServer>` (std::net, thread-per-connection, statements serialized by the server's own execution lock), a daemon binary (`htapd`) with `--root`/`--listen`/`--max-connections`/`--password`/`HTAPD_PASSWORD`, and `htap-client::RemoteClient` returning the same `StatementResult` shape as `EmbeddedClient`. Supports handshake v10 with `mysql_native_password`, `COM_QUERY`/`COM_PING`/`COM_INIT_DB`/`COM_QUIT`, and a start-up compatibility shim (`SET`, `USE`, `SELECT 1`/`VERSION()`/`DATABASE()`/`@@sysvar`). Default bind is loopback-only; no TLS (see ADR-016 and the "Network layer" / "Security model" sections of `docs/ARCHITECTURE.md`). Covered by `crates/htap-wire/src/*.rs` unit tests, `crates/htap-wire/tests/wire_server.rs` (23 tests, including a real-driver interop test `test_mysql_crate_driver_interop` against the `mysql` crate v28), and `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`. Deferred at the time: TLS, compression, prepared statements/binary protocol, multi-statements/multi-results, per-user ACL/RBAC, Docker packaging, and a selectable single-role `htapd` mode (sessions/`BEGIN`/`COMMIT`/`ROLLBACK` delivered in Phase 10, see that row; prepared statements/binary protocol, `COM_RESET_CONNECTION`/`COM_CHANGE_USER`, ≥16 MiB messages, CSPRNG scramble, and multi-statements delivered in Phase 11, see that row; TLS, compression, and per-user ACL/RBAC delivered in Phase 12, see that row). |
| Phase 9 — SQL breadth and cross-engine joins | `Complete (local MVP)` | Built a general query executor (`htap-sql::{query, expr, binder_query}`, `htap-server::query_exec`, `Route::Query`) that materializes every base table side of a join through the existing `scan_partition_compact` storage path at one MVCC snapshot per statement, then hash-joins/filters/groups/orders in memory: `INNER`/`LEFT`/`RIGHT`/`CROSS` joins, aliases, qualified names, arithmetic/comparisons/`AND`/`OR`/`NOT`/`IS [NOT] NULL`/`TRUE`/`FALSE`/`LIKE`/`IN`/`BETWEEN`/`CASE`/`CAST`, scalar functions, aggregates with `DISTINCT`, `GROUP BY`/`HAVING`, `SELECT DISTINCT`, `ORDER BY`/`LIMIT`/`OFFSET`, `UNION`/`UNION ALL`, derived tables, non-recursive CTEs, uncorrelated scalar/`IN`/`EXISTS` subqueries. Added `UPDATE` (point and filtered-scan forms, `Route::RowstoreUpdate`, one transaction per statement, effective ~4 MiB payload cap — see Phase 10 row and "Effective 2PC transaction payload cap" above), `DROP TABLE` (metadata-only, `Route::CatalogDdl`), and `SHOW TABLES`/`SHOW DATABASES`/`SHOW COLUMNS`/`DESCRIBE` (`Route::CatalogRead`). A purely syntactic shape gate (`is_narrow_select_shape`) keeps R5's complete-PK `Route::RowstorePointRead` and narrow `Route::OlapScan` unchanged and structurally isolated, pinned by `test_point_read_fast_path_pinned_against_general_query_path`. Catalog envelope `HTAPCAT1` bumped format version 1 -> 2 to persist `id_high_water` so dropped table/partition/tablet/replica ids are never reissued; version-1 catalogs still decode. See "General query executor scope and deferred features" above for full evidence. Deferred at the time (window functions, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, recursive CTEs, `EXCEPT`/`INTERSECT`, `GROUP BY`/`ORDER BY` ordinals, integer `DIV`, `INSERT ... SELECT`, filtered `DELETE`, and `TRUNCATE` were delivered in Phase 13, see that row; physical reclamation on `DROP TABLE` was delivered in Phase 15, see that row): cost-based optimization, vectorized/pipelined execution, worker-pool parallelism on the general path, memory bounds/spilling, `UPDATE` with joins/subqueries, and non-partition `ALTER TABLE`. |
| Phase 10 — Sessions and explicit transactions | `Complete (local MVP)` | Built server-side sessions (`htap-server::session::Session`, `LocalServer::open_session`, `EmbeddedClient::open_session`, one `htap-wire` connection = one session) with `BEGIN`/`START TRANSACTION [READ ONLY \| READ WRITE]`, `COMMIT`, `ROLLBACK`, `autocommit` (0/1/ON/OFF/TRUE/FALSE), `SET @x = expr` (multi-assign), `SET [SESSION] TRANSACTION ISOLATION LEVEL REPEATABLE READ` (only accepted level) and `READ ONLY`/`READ WRITE` (next-transaction-only, even with `SESSION`), and a new `htap-sql::variables` system-variable registry (`@@name`, aliases, dynamic `autocommit`/`transaction_isolation`/`transaction_read_only`) that replaced the old ad hoc wire-shim fakes. Uncommitted writes are buffered in a session's own `WriteSet` (never journaled — a crash is an implicit `ROLLBACK`) and overlaid below relational operators for point reads, narrow scans, the general executor, and `UPDATE`, across `Row`/`Column`/`Converting` partitions; `COMMIT` runs the existing 2PC path once against the transaction's own pinned snapshot. Snapshot isolation with first-writer-wins, write skew permitted, reported as `REPEATABLE READ`; a write-write conflict at `COMMIT` and a stale snapshot vs. a mid-transaction columnar base both poison the transaction as `Conflict`; commit-time catalog revalidation catches a concurrent `DROP TABLE`/`ALTER`; `DurablePending` moves the session to a quarantined state rejecting every further statement, including `ROLLBACK`, with the original non-retryable error. Fixed two correctness gaps found during this work: the first-writer-wins check now runs in `Engine::prepare` before any journal write (previously only after the `Commit` record was fsynced), and an autocommit write now commits against its own read snapshot instead of a fresh one (previously could lose a race against a concurrent `copy_from_*`/import). Added a manager-wide `TransactionManager` recovery latch that rejects every *other* commit once any commit returns `DurablePending`, with a distinct non-retryable `HtapError::RecoveryRequired` (not the blocking transaction's own `DurablePending`), until `recover()` resolves it in-process (only for a `RecoveryCause::ParticipantIo` latch) or the manager reopens (always clears it, any cause). A follow-up fix pass also: made `recover()` fsync the journal before replaying any commit and cross-check each participant's own `committed_version()` against the journal's replayed max version, failing as `HtapError::Corruption` on mismatch; restored `next_txn_id` via `fetch_max` (never regressing it); truncated a failed journal append back to its pre-append offset; moved the applied-external-transactions ledger capacity check into `Engine::prepare` (before any journal write, not just at apply, rejecting with `HtapError::InvalidArgument`); and found the actual effective 2PC transaction payload cap is about 4 MiB, not the nominal 16 MiB (see "Effective 2PC transaction payload cap" above). A third fix pass added a `Journal`-level `poisoned` state (rejecting further appends/syncs until reopened), latched the manager as `JournalIo` for a failed `Intent`/`Abort` write too (previously only `Commit`), made `recover()` refuse outright and apply nothing while already latched `JournalIo` or while the journal is poisoned, and scaled the payload-cap bound by participant count (lowering the effective single-participant cap to 4,194,143 bytes). See "Sessions and explicit transactions (Phase 10)" in `docs/ARCHITECTURE.md`, ADR-018, and `docs/PROGRESS.md` for the full contract and test evidence. Deferred at the time: `SELECT ... FOR UPDATE`/locking reads, prepared statements/binary protocol, savepoints, XA, IPC/multiprocess sessions, per-user ACL, idle-transaction timeout/reaping, MVCC garbage collection, and a grace period before a new columnar base dooms an open transaction (prepared statements/binary protocol delivered in Phase 11, see that row; IPC/multiprocess sessions delivered in Phase 16, see that row). |
| Phase 11 — MySQL binary protocol, prepared statements, connection commands | `Complete (local MVP)` | Built the MySQL binary protocol and prepared statements (`htap-wire::{binary_codec, prepared}`, `htap-sql::prepare`): `COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`/`SEND_LONG_DATA` (`COM_STMT_FETCH` cleanly rejected), AST-level placeholder substitution covering every literal-bearing position the binder accepts (subqueries, derived tables, CTEs, UNION, `LIMIT`/`OFFSET`), best-effort `PREPARE` metadata probed through the real binder, a per-statement bound-parameter-type cache for `new_params_bound_flag = 0`, and the full binary parameter type matrix except `TIME`. Added `COM_RESET_CONNECTION`/`COM_CHANGE_USER` (both respecting the ADR-018 `CommitOutcomePending` quarantine, via a new `verify_credentials` seam), ≥16 MiB message reassembly/splitting with a real configurable `max_allowed_packet` (default 64 MiB, `htapd --max-allowed-packet`/`HTAPD_MAX_ALLOWED_PACKET`), a CSPRNG handshake scramble (`getrandom`, no fallback, replacing the seeded xorshift generator), negotiated `CLIENT_MULTI_STATEMENTS` (sequential execution, `SERVER_MORE_RESULTS_EXISTS`, stop on first error including `DurablePending`/`RecoveryRequired`), and shutdown force-close of connections blocked mid-packet (`live_connections` registry with RAII unregistration). `RemoteClient`/`WireClient` gained `prepare`/`execute_prepared`/`close_prepared`/`close_stmt`/`query_multi`. Deferred at the time: TLS, compression, and per-user ACL (delivered in Phase 12, see that row). See "Prepared statements and binary protocol scope and deferred features" above, "Prepared statements and binary protocol (Phase 11)" in `docs/ARCHITECTURE.md`, ADR-019, and `docs/PROGRESS.md` for the full contract and test evidence. Deferred/permanent gaps: `COM_STMT_FETCH`/server-side cursors, unsigned 64-bit values above `i64::MAX` (no `UInt64` value type), exact `DECIMAL` (kept as text, not arbitrary precision), `TIME`-typed parameters, best-effort (not exhaustive) `PREPARE` metadata, and shim-only `SET CHARACTER SET`/`SET CHARSET` forms not usable inside a multi-statement batch. |
| Phase 12 — TLS, compression, and per-user accounts | `Complete (local MVP)` | Built TLS (`htap-wire::tls`, `rustls` 0.23 with the `ring` provider — `aws-lc-rs` needs `cmake`, unavailable here), MySQL compressed-packet framing (`htap-wire::compression`, zlib via `flate2` and zstd via `zstd`), and a catalog-backed per-user account/privilege model (`htap-catalog` format v3: `accounts`, `grants`, `accounts_initialized`; `htap-server::privilege::check_privileges`; new `htap-sql` `CREATE/ALTER/DROP USER`, `GRANT`/`REVOKE`, `SHOW GRANTS`, requiring a disclosed `vendor/sqlparser` patch for `IDENTIFIED BY`/`'user'@'host'` in `DROP USER`/`SHOW GRANTS`). `htapd` gained `--tls-cert`/`--tls-key`/`--require-secure-transport`/`--disable-compression` and matching `HTAPD_*` env vars; `WireServer::reload_tls_certs()` swaps TLS certs live (no `SIGHUP` trigger); `--password`/`HTAPD_PASSWORD` now only seeds the `root` account once via an `accounts_initialized` latch that never re-fires. See "TLS and compression (Phase 12)" and "Accounts and privileges (Phase 12)" in `docs/ARCHITECTURE.md`, ADR-020/ADR-021, "TLS, compression, and account/privilege scope and deferred features" above, and `docs/PROGRESS.md` for the full contract and test evidence. Deferred/permanent gaps: roles and delegated administration (`WITH GRANT OPTION` parses but is rejected at bind time), hosts other than `%`, `ACCOUNT LOCK`/`UNLOCK` SQL syntax, `caching_sha2_password`, `SIGHUP`-triggered TLS cert reload, unsalted password hashes with no KDF work factor (the comparison itself is constant-time — see ADR-021's fix pass), and `CREATE TABLE` auto-granting privileges on the table just created. |
| Phase 13 — SQL query breadth (windows, correlated subqueries, join trees, recursion, set ops) | `Complete (local MVP)` | Extended the Phase 9 general query executor with `GROUP BY`/`ORDER BY` ordinals, integer `DIV`, `EXCEPT`/`INTERSECT` (`ALL`/`DISTINCT`), `FULL OUTER`/`NATURAL`/`USING` joins with real per-join-node column coalescing, arbitrarily nested parenthesized join trees (a purely structural `query::JoinTree`, a total/pure `lower_to_flat` fast path, a generalized `join_rows`, and a mandatory permanent flat/tree differential test), `DELETE` by arbitrary filter (`Route::RowstoreDelete`), `TRUNCATE` (as a transactional unfiltered `DELETE`), `INSERT ... SELECT` (exact per-column type match), depth-1-only correlated subqueries (a bounded `SubqueryRunner`/`SubqueryBudget` execution boundary), `WITH RECURSIVE` (capped iterations/rows/bytes), and window functions (`ROWS`/peer/value-offset `RANGE` frames, evaluated post-`HAVING`) — no on-disk format change. See ADR-022, `docs/PROGRESS.md`'s Phase 13 row, and "General query executor scope and deferred features" above for the full contract and evidence. Fixed test gap: two pre-existing `htap-client` tests (`test_embedded_client_unsupported_sql_preserves_error_categories`, `test_remote_client_matches_embedded_client_ddl_dml_select`) used to assert a window function is `Unsupported`, which stopped being true once this row's window functions landed; both now use `SELECT ... FOR UPDATE` as their still-unsupported example and pass — see "Fixed test gap from Phase 13" above. Deferred at the time (cost-based optimization, spilling, and bounded parallelism were added in Phase 14, see that row): vectorized/pipelined execution, worker-pool parallelism, memory bounds/spilling, `LIMIT BY`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, and non-partition `ALTER TABLE`. |
| Phase 14 — Cost-based optimization, `EXPLAIN`, spilling, and bounded parallelism | `Complete (local MVP)` | Lifted exactly three named gaps for the general executor only (`Route::RowstorePointRead`/`Route::OlapScan` byte-for-byte unchanged): `ANALYZE TABLE` (exact table/column statistics, capped exact-distinct count, `HTAPCAT1` format v3 -> v4, `stats` published by single-field CAS); a storage-agnostic `htap-sql::optimize` cost-based optimizer (statistics-driven estimators with disclosed provenance, predicate-atom reordering with an always-on conservation validator, subset-DP join reordering up to 8 relations then greedy, enabled by default); `EXPLAIN`/`EXPLAIN ANALYZE` (verified to bypass the optimizer entirely for the two narrow routes, including on DDL such as `EXPLAIN CREATE TABLE`); a per-statement memory budget, `Route::Query` only (a single-table `SELECT` with `ORDER BY`/`GROUP BY`/a plain aggregate still routes to the unbudgeted, non-spilling `Route::OlapScan` path — by design), with one level of disk spilling (non-durable scratch under `<data-root>/spill/`, swept on `LocalServer::open`; hash joins and windows capped at 128 partitions, `GROUP BY`/set operators at a fixed 16) for hash joins/`GROUP BY`/`ORDER BY`/set operators/windows, with per-operator spill test telemetry; and bounded intra-query parallelism for `GROUP BY` and `INNER`/`CROSS` hash joins. A second external-review round also closed a catalog-statistics brick risk (structural validation, including finite-float bounds — no format change) and made float overflow (`+`/`-`/`*`/`/`/`SUM`/`AVG`) return an out-of-range error instead of writing a non-finite value. A third, independent storage review then enabled `serde_json`'s `float_roundtrip` feature workspace-wide as a precaution (tested with the feature off, the default parser round-tripped every value tried, so no drift was demonstrated — not a bug fix), made spilled set operations/`DISTINCT` emit rows by source index rather than a decoded-row-keyed map (hardening), tightened the hash-join/window spill partition cap from 256 to 128, closed the previously-unverified analytic-path `SUM` overflow gap (`CAST(... AS DOUBLE)` from a string and non-finite literals are now rejected the same way), and fixed `ANALYZE`'s min/max bound to reserve memory before swapping so a failed reservation leaves accounting exact — see ADR-023's second external-review round. Also partially fixed `docs/PROBLEMS.md` P2: one `EvalContext` constructor, one join evaluator (`SelectBody.join_tree` always populated, the old flat-loop branch removed), and zero `too_many_arguments` allows in `query_exec.rs` — but the planned shared binder leaf-helper module was **not** delivered; the two binder entry points still each hold their own copy of that logic, by the deliberate Option B choice recorded in ADR-023 (keeping R5 structural). See ADR-023, `docs/PROGRESS.md`'s Phase 14 row, and "General query executor scope and deferred features" above for the full contract and evidence. `ANALYZE TABLE` is gated by the "no DDL inside an open transaction" rule exactly like other catalog DDL (`crates/htap-server/tests/session.rs::test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives`, `test_explain_analyze_wrapping_ddl_rejected_inside_open_transaction`, `test_explain_analyze_wrapping_insert_rejected_inside_read_only_transaction`), and `EXPLAIN ANALYZE` follows the transaction rules of the statement it executes while plain `EXPLAIN` remains permitted (`crates/htap-server/tests/explain.rs::test_plain_explain_select_permitted_inside_open_transaction`). One disclosed gap remains: hash-join spilling is not itself kind-restricted in code but is only tested for `INNER` joins. Deferred: vectorized/pipelined execution, worker-pool parallelism for `LEFT`/`RIGHT`/`FULL` joins, memory-bounded spilling for non-equi/`CROSS` joins (a genuine gap — evaluated by an in-memory nested loop with no budget check at all, unlike `LEFT`/`RIGHT`/`FULL` equi-hash spilling, which exists in code but only `INNER` is tested), statistics histograms and per-partition statistics, automatic statistics staleness detection, `LIMIT BY`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, and non-partition `ALTER TABLE`. |
| Phase 15 — DROP reclaim, rowstore compaction/GC, journal checkpoint | `Complete (local MVP)` | Built contiguous-run, entry-count-tiered `Engine::compact_once` (`crates/htap-rowstore/src/engine.rs`) splicing its output into the selected run's original manifest position — fixing an initial-draft prepend bug a storage-review panel found that could resurrect a stale value behind a tombstone (pinned by `crates/htap-rowstore/tests/compaction_ordering.rs`); a rowstore `MANIFEST` format bump (`HTAPMAN1` v2 -> v3) adding `committed_version_high_water`/`gc_low_water`, both monotonic, refusing to publish a regression, with a hard read-rejection below `gc_low_water`; an exclusive `Engine::open` lock on `<rowstore>/LOCK`; a per-tablet movement/reclaim lease set in `LocalDataMover` (all-or-nothing for `DROP TABLE`'s forced set, best-effort/partial for the ordinary tiered pass); catalog `pending_reclaim` (`HTAPCAT1` v4 -> v5) driving `DROP TABLE` column-store/movement artifact deletion (including that tablet's movement job records) and rowstore purge confirmation to completion across ticks; and `TransactionManager::checkpoint()` (a new `HTAPTXC1` journal-checkpoint envelope) compacting `txn.journal`, triggered opportunistically after commits and finalized once at `LocalServer::open`, latching `RecoveryRequired` unconditionally on any rewrite-step error. See ADR-024, `docs/PROGRESS.md`'s Phase 15 row, and "Rowstore compaction, garbage collection, and DROP TABLE reclaim scope and deferred features" / "Transaction journal checkpoint scope and deferred features" above for the full contract. Deferred/disclosed: shared-keyspace protection is per SST not per row (a busy tablet blocks every SST that spans it); the explicit-SST-id compaction path compacts only one contiguous run per call; compaction is explicit-tick-only with no background thread or SQL trigger; tombstones are never elided; movement/reclaim leases are intentionally non-durable (a stated precondition, not a gap, given today's non-resumable movement jobs); `ALTER TABLE ... DROP/REORGANIZE PARTITION` reclamation remains untouched; the `MANIFEST` v2 external-apply ledger's own hard cap is untouched and unrelated to the journal checkpoint; and delete vectors / delta-to-base columnar background compaction remain deferred, unchanged. |
| Phase 16 — Owner plus IPC (concurrent multiprocess use) | `Complete (local MVP)` | Built owner/client forwarding so a second local process opening an already-owned root becomes an IPC client instead of failing outright, over a length-prefixed JSON protocol on a Unix domain socket at `<root>/htap.sock` (mode `0600`, Unix-only; a locked root on a non-Unix target still returns the ordinary `Conflict`, unchanged). Fixed `docs/PROBLEMS.md` P3 (the server's two independent statement pipelines each ran the privilege check) with a shared `prepare_statement` step. Added a new `HtapError::Ambiguous` variant, distinct from `Conflict` (safe to retry) and `DurablePending` (has load-bearing txn identity), for an IPC request that may or may not have reached the owner; a client-side write-boundary rule classifies a failure as `Ambiguous` unless zero request bytes were sent or the request is one of the two statically-read-only kinds. A `Session` gained two new terminal states mirroring the existing `DurablePending` quarantine: `AmbiguousOutcomePending` (re-raises the stored `Ambiguous` error on every later call, including `reset`) and `RemoteDisconnected` (a confirmed-dead connection; re-raises `Conflict`); neither is ever exited by silent reconnection. An explicit ownership graph (`OwnedServer`, `ServerMode`, `OwnerRuntime`) replaces `LocalServer`'s single struct so the IPC listener can mint owner-side sessions directly from the storage core without needing the caller's own `Arc` wrapper, and `OwnerRuntime`'s hand-written `Drop` joins the listener (stop flag, force-close every live connection, unbounded join of the accept and connection threads) before its own storage-core handle drops, guaranteeing the socket is gone before `<root>/LOCK` is released. A bound statement forwards as a serialized AST (the vendored parser's own `serde` feature, turned on additively in two package manifests, not a vendored-source change) rather than re-rendered SQL text, avoiding a lossy round trip for already-substituted binary/non-finite-float literals; the plain-text execute/query path is unaffected. A first storage/external-review round (batch D) found and fixed 12 of 13 defects the passing suite had missed, the clearest a completely broken prepared-statement path for every client-mode session and process panics on roughly 30 owner-only methods for a client-mode handle; a second storage re-review (batch E) found and fixed 8 more, including a socket creation-window that would have granted an unauthenticated local user a superuser session, and a second client-mode `open_session`/`authenticate_session` panic path that batch D's own "panics are fixed" claim had missed. A third storage re-review (batch F) found and fixed 7 more: two of the batch D/E fixes above had shipped with tests that could not have caught a regression (the socket-permission fix's own test reimplemented publication instead of calling the real startup path; the no-panic fix had no test at all), and replacing the first of those with a real test immediately exposed a leak the review had only partly identified — the private staging directory used to publish the socket was never removed on a successful start, not only after a crash. Batch F also fixed a post-dispatch response-serialization failure misreported as bad input instead of an unknown outcome, a failed write that left a connection looking reusable after bytes had already reached the owner, an owner-gone login misreported as bad credentials, and a handshake read that failed spuriously on an interrupted system call. See ADR-025 (including its "Post-review fixes (batch D)", "Post-review fixes (batch E)", and "Post-review fixes (batch F)" sections), `docs/PROGRESS.md`'s Phase 16 row, and "Owner plus IPC (Phase 16) scope and deferred features" above for the full contract. Deferred/disclosed: a socket path too long for a Unix socket, or a non-socket file at that path, falls back to the pre-existing lock-only mode; the wire format is tied to the build's AST shape (a version-skewed pair fails cleanly at decode, no schema-compatibility scheme); changing users on a client session is unsupported; a client-mode handle's `last_query_*` diagnostic getters report fixed defaults, not the owner's real state; administrative/data-mover/conversion/compaction/reclaim operations remain owner-only; standalone subsystem opens bypassing `LocalServer` remain unsafe for concurrent use, unchanged. |

---

## Deviations from the brief

| Brief requirement | Deviation | Rationale | Where recorded |
| ----------------- | --------- | --------- | -------------- |
| ZooKeeper reference source at `examples/zookeeper` (§3 of the brief) | Input absent; ZooKeeper backend, `zookeeper-async` dependency, and Docker ensemble tests are not implemented in the local MVP. Coordination is implemented locally via `htap-coord::LocalCoordinator`. ZooKeeper backend and containerized testing remain deferred future work. | `examples/zookeeper` was not supplied; cluster coordination was scoped to a single-node local coordinator MVP. | ADR-006 in [`DECISIONS.md`](./DECISIONS.md); "Missing input: ZooKeeper reference source" above. |

> **This table must remain exhaustive.** Anything omitted or changed relative
> to the brief is recorded here or in an ADR, never silently dropped.
