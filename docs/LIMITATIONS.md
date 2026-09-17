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
- No journal/ledger compaction or coordinated retention; ledger hard cap eventually blocks new external applies, and `txn.journal`'s own `max_journal_size` (checked only at open) can eventually block `LocalServer::open` on a long-running root.
- Possible later flush-boundary duplicate SST publication after crash before reader/checkpoint, requiring future staged flush recovery.
- No power-loss proof (testing bounded by process `SIGKILL`).
- No distributed consensus/Raft/ZK/remote replica serving or real HA.
- Whole-dataset materialization in conversion, export (exports materialize full logical partition before writing), and clone.
- No full SQL analytics. A network MySQL daemon (`htapd`/`htap-wire`) is now implemented (Phase 8) with a
  narrow security model (loopback default, single shared password, no TLS — see below); it does not add
  prepared statements or broader SQL support. Phase 9 added a general query executor (joins, expressions,
  subqueries, `UNION`, `UPDATE`, `DROP TABLE`, `SHOW`/`DESCRIBE`) that is still not full SQL analytics: no
  window functions, correlated subqueries, cost-based optimization, vectorized execution, or worker-pool
  parallelism on that path — see "General query executor scope and deferred features" below. Phase 10 added
  server-side sessions and explicit transactions (`BEGIN`/`COMMIT`/`ROLLBACK`, `autocommit`, session
  variables), both in-process and over the wire — see "Sessions and explicit transaction control" below for
  the exact contract and remaining gaps (no locking reads, no prepared statements, no idle-transaction
  reaping, `REPEATABLE READ` only).
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
- **No journal/ledger compaction or coordinated retention; ledger hard cap blocks external applies:**
  Neither `txn.journal` nor the `MANIFEST` v2 external apply ledger implements compaction or coordinated retention. The external ledger has a hard capacity cap (`MAX_APPLIED_EXTERNAL_TXNS = 1_000_000`). When the ledger is filled, new external transaction applies fail with `HtapError::InvalidArgument` (not a dedicated `CapacityExceeded` variant, which does not exist in `HtapError`); a Phase 10 fix pass moved this check into `Engine::prepare` as well, so a real 2PC/direct-commit transaction is rejected before any journal write rather than only at apply time (`crates/htap-txn/tests/two_phase_commit.rs::test_ledger_full_commit_rejected_at_prepare_before_journal_growth`). Truncation coordinated with participant checkpoints remains unimplemented.
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
- **`txn.journal` has no compaction, and `max_journal_size` is enforced only at open:** `Journal` checks its
  total file size against `max_journal_size` (`DEFAULT_MAX_JOURNAL_SIZE` = 64 MiB) in `Journal::open_with_options`
  and in `Journal::scan` (which only ordinary `open`/`repair_torn_final`/`recover_records` call — an `append`/
  `append_nosync`/`sync` never re-checks total file size). The journal file only ever grows (`Intent`/`Commit`/
  `Abort` records are appended, never pruned or checkpointed against participant state), so a long-running
  workload can eventually make a later `LocalServer::open` fail with `HtapError::Corruption` even though every
  individual append along the way succeeded. Journal checkpoint/retention tied to participant durability
  (dropping records for transactions no participant could still need) is planned for a later phase, not this
  one.
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

- **sqlparser MySQL dialect:** Strict single-statement parsing using `sqlparser::dialect::MySqlDialect`, accepting valid backtick identifiers and MySQL escape semantics while rejecting empty input, malformed SQL, and multi-statement input with stable `InvalidArgument` errors. Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Strict catalog binder:** Schema-validated binding for `CREATE TABLE` (scalar types and primary keys), literal schema-ordered `INSERT`, complete-primary-key `DELETE`, complete-primary-key `SELECT` (`PointSelect`), and typed `AnalyticSelect`. Strictly rejects unsupported data types, composite key mismatches, implicit coercions, and unhandled clauses (joins, CTEs, window functions, `LIMIT`/`OFFSET`, `HAVING`, `OR`, arithmetic; in `ORDER BY`, expressions, aliases, aggregate ordering, and non-AnalyticSelect usage are rejected while simple unqualified source/projected column `ORDER BY` with ASC/DESC and NULLS FIRST/LAST/default policy is supported for `AnalyticSelect`). Verified in `crates/htap-sql/tests/parse_bind.rs`.
- **Structural route classifier:** Inspects bound statements and storage descriptors, classifying complete-PK queries as `Route::RowstorePointRead`, single-partition mutations as `Route::RowstoreWrite`, and analytical selects as `Route::OlapScan` across `Row`, `Column`, and `Converting` descriptors. Complete-PK lookups strictly take the rowstore fast path, remaining separate and unchanged, and bypass OLAP execution and the converter. Verified in `crates/htap-sql/tests/route.rs` and `crates/htap-server/tests/local_server.rs`.
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

### Explicitly deferred features (network daemon security boundary/full SQL analytics)

- **MySQL wire protocol and `htapd` daemon (implemented, Phase 8):** A hand-written, synchronous MySQL
  text-protocol server (`htap-wire::WireServer`) and daemon binary (`htapd`) are implemented; all interaction
  can now go through `LocalServer` either in-process (`EmbeddedClient`) or over TCP
  (`htap-client::RemoteClient`, or any MySQL client). Remaining gaps specific to the network layer:
  - **No TLS / cleartext query and result traffic:** The password exchange is a `mysql_native_password`
    challenge/response hash, but query text and result rows travel in cleartext. Binding a non-loopback
    address requires a trusted network or an external tunnel (e.g. SSH).
  - **No prepared statements / binary protocol:** `COM_STMT_PREPARE`/`COM_STMT_EXECUTE` and every other
    command besides `COM_QUERY`/`COM_PING`/`COM_INIT_DB`/`COM_QUIT` return `ERR 1047`.
  - **No multi-statements or multi-results, no compression.**
  - **Payloads ≥16 MB (multi-packet messages) are unsupported;** the connection is closed rather than
    reassembled.
  - **Non-cryptographic scramble RNG:** the handshake scramble uses a seeded xorshift generator, not a
    CSPRNG.
  - **Shutdown does not force-close a connection blocked mid-packet:** `WireServer::shutdown` observes the
    stop flag only at packet boundaries, so a connection stalled inside a partial packet keeps waiting for
    the rest of that packet rather than being torn down.
  - **Single shared password, no per-user ACL:** one implicit user; the username is logged but never
    checked; `--password`/`HTAPD_PASSWORD` is the only credential, shared by every client.
  - See ADR-016 and the "Network layer" section of `docs/ARCHITECTURE.md` for the full contract; verified in
    `crates/htap-wire/tests/wire_server.rs`, `crates/htap-wire/src/*.rs` unit tests, and
    `crates/htap-client/tests/remote_client.rs`.
- **No authentication or security boundary beyond the wire password:** No RBAC, no per-user credentials, no
  TLS.
- **Sessions and explicit transaction control (implemented, Phase 10):** `htap-server::session::Session`
  (`LocalServer::open_session`, `EmbeddedClient::open_session`, and one `htap-wire` connection = one session)
  supports `BEGIN`/`START TRANSACTION [READ ONLY | READ WRITE]`, `COMMIT`, `ROLLBACK`, `autocommit`, and
  `@user`/`@@system` variables, with session-buffered uncommitted writes that overlay reads
  (read-your-own-writes) until `COMMIT` runs the existing 2PC path once. See "Sessions and explicit
  transactions (Phase 10)" in `docs/ARCHITECTURE.md`, ADR-018, and `docs/PROGRESS.md` for the full contract
  and test evidence. Remaining gaps:
  - **No `SELECT ... FOR UPDATE` / locking reads:** reads never take row or table locks; concurrency control
    is snapshot isolation with first-writer-wins detected at `COMMIT` only.
  - **No prepared statements or binary protocol yet:** sessions run only the existing text-protocol
    `COM_QUERY` path (see "Network layer" above); `COM_STMT_PREPARE`/`EXECUTE` remain unsupported.
  - **No IPC or multiprocess access:** a session is owned by one `Arc<LocalServer>` inside one process; there
    is no shared-memory or socket-based session handoff across processes.
  - **No per-user ACL:** sessions inherit the wire layer's single shared credential and unchecked username
    (see "Security model" above); there is no per-session identity or permission boundary.
  - **No idle-transaction timeout or reaping, and no MVCC garbage collection yet:** an open transaction with
    no following `COMMIT`/`ROLLBACK` (or a session that is never dropped) holds its pinned snapshot
    indefinitely; this is currently inexpensive only because there is no MVCC GC to block, but it is also why
    a stale-snapshot-vs-conversion conflict is used instead of a pinned-snapshot registry that conversion
    would have to wait on (see ADR-018).
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
- **Extended DML and DDL (Phase 9):** `UPDATE t [alias] SET col = expr, ... [WHERE ...]` (point and
  filtered-scan forms, `Route::RowstoreUpdate`) and `DROP TABLE [IF EXISTS] t` (metadata-only, catalog CAS,
  `Route::CatalogDdl`) are now implemented — see "General query executor scope and deferred features" below
  for the exact contract and gaps. Non-partition generic `ALTER TABLE` (`ADD COLUMN`, `RENAME TABLE`, etc.),
  `TRUNCATE`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, `INSERT ... SELECT`, and complete-PK-only
  `DELETE` (no `DELETE ... WHERE <non-PK filter>`) remain deferred. Typed partition lifecycle DDL
  (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION` on empty source partitions) is supported.
- **Analytical queries and SQL breadth (R4):** As of Phase 9, a general query executor (`Route::Query`,
  `htap-server::query_exec`) implements `INNER`/`LEFT`/`RIGHT`/`CROSS` joins (left-deep chains, comma joins),
  table aliases, qualified names, `*`/`t.*`, arithmetic (`+ - * / %`, checked overflow), comparisons,
  `AND`/`OR`/`NOT`, `IS [NOT] NULL`/`TRUE`/`FALSE`, `LIKE`, `IN (list)`, `BETWEEN`, `CASE`, `CAST`, scalar
  functions (`UPPER`/`LOWER`/`LENGTH`/`CHAR_LENGTH`/`CONCAT`/`ABS`/`COALESCE`/`IFNULL`/`NULLIF`), aggregates
  (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with `DISTINCT`), `GROUP BY`/`HAVING`, `SELECT DISTINCT`, `ORDER BY`
  (expressions/aliases/ordinals, `NULLS FIRST`/`LAST`), `LIMIT`/`OFFSET`, `UNION`/`UNION ALL`, derived
  tables, non-recursive `WITH` CTEs, and uncorrelated scalar/`IN`/`EXISTS` subqueries — see "General query
  executor scope and deferred features" below for the full contract, what is still deferred (window
  functions, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, recursive CTEs, cost-based
  optimization, and more), and the narrow-shape gate (`is_narrow_select_shape`) that keeps complete-PK point
  reads and narrow single-table scans unchanged. The narrow `AnalyticSelect` / `Route::OlapScan` path itself
  is unchanged from Phase 3-8.
- **Direct SegmentReader pushdown optimization and vectorized execution:** Direct `SegmentReader` pushdown optimization is now implemented for the compact base path in `LocalServer` (using `read_column_partition_compact_core` with PK+requested column union and single safe predicate-leaf pushdown). Compound `AND` pushdown beyond one leaf and `!=` remain evaluated as residual SQL filters. `ScanStats`/pruning is available as internal execution evidence, but SQL evaluation still operates on materialized logical rows; vectorized aggregation, vectorized operator pipelines, memory quotas, disk spilling, query cancellation, and DataFusion/Arrow integration are deferred.
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
  - Deferred partition capabilities: Physical data migration for populated partition reorganization, physical storage reclamation (space of dropped partitions or demoted column files is not physically reclaimed), hash tablets, distributed/remote partition serving across network nodes, replica failover, and an inter-node distributed-serving network protocol remain deferred (the client-facing MySQL wire protocol is implemented; see "MySQL wire protocol and `htapd` daemon" above). `UPDATE` is implemented (Phase 9; see below), but the binder strictly rejects assigning a partition-key column, so cross-partition row movement via `UPDATE` remains unsupported.
- **Broad MySQL compatibility:** Broad MySQL syntax, built-in functions, variable setting, system tables, and loose type coercions are deliberately unsupported.

---

## General query executor scope and deferred features (Phase 9)

The Phase 9 implementation delivers a general query executor (`htap-sql::{query, expr, binder_query}`,
`htap-server::query_exec`) that handles the full breadth of statement shapes the Phase 3-8 narrow binders
rejected, while structurally preserving the narrow point-read and single-table-scan fast paths (R5) via a
purely syntactic shape gate (`is_narrow_select_shape` in `crates/htap-sql/src/binder.rs`).

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
- **`DROP TABLE` is metadata-only:** Rowstore data and columnar segments of the dropped table's tablets stay
  on disk, unreachable; physical reclamation is deferred. Dropped identifiers are never reissued (see the
  catalog identifier high-water mark below), so the unreachable data can never be aliased by a new table.
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
- **Memory and concurrency limits of the general executor:** Intermediate results (scanned rows, hash
  tables, groups) are held in memory without bounds; there is no spilling and no cost-based planning. Unlike
  `Route::OlapScan`'s bounded in-process partition scan workers, the general executor's join/filter/group/
  order/limit/union stages run single-threaded in memory (each slot's own partition scan still uses the
  narrow path's scan workers internally, so per-slot scanning is still parallel; the stages above the scan
  are not).
- **Still deferred regardless of route:** window functions (`OVER`), correlated subqueries, `FULL OUTER`/
  `NATURAL`/`USING` joins, parenthesized nested join trees, recursive CTEs, `EXCEPT`/`INTERSECT`, `GROUP BY`
  ordinals, `LIMIT BY`, `INSERT ... SELECT`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, `DELETE` by
  filter (still complete-PK only), `TRUNCATE`, non-partition `ALTER TABLE`, cost-based optimization,
  vectorized/pipelined execution, semi-join rewrites of `IN`/`EXISTS`, integer `DIV`, broader string/date
  function coverage, and MySQL's implicit string<->number coercion (a comparison between incompatible types
  is a bind error here, not an implicit cast).

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
  `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`.
- `crates/htap-wire/tests/wire_server.rs::test_general_sql_over_wire` (joins, `UPDATE`, `SHOW`/`DESCRIBE`,
  `DROP TABLE` over the MySQL wire protocol, exercising the same `LocalServer::execute` path unchanged).

---

## HTAP conversion local MVP scope and deferred features

The Phase 4 implementation delivers an incrementally verified local single-tablet Row-to-Column storage conversion engine (`htap-convert`), integrating with `htap-catalog`, `htap-rowstore`, `htap-colstore`, `htap-sql`, and `htap-server`. It implements a crash-resumable cutover state machine, durable tablet columnar manifest envelopes with CRC32C validation, atomic manifest and catalog publication, rowstore-authoritative base-plus-delta overlay scans, and online point mutations and point reads during and after conversion.

### Completed local MVP

- **Four-phase cutover state machine:** Deterministic state machine governing partition conversion:
  `SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`.
- **Atomic per-tablet manifest and catalog publication:** Manifest files use the `HTAPTBM1` binary envelope format with format versioning and payload CRC32C checksums. Publication uses atomic staging (`MANIFEST.tmp` replaced to `MANIFEST`) followed by generation-checked catalog CAS updates.
- **Rowstore authoritative base-plus-delta overlay:** Converter API `read_column_partition` (`LocalConverter::read_column_partition` / `htap_convert::read_column_partition`, verified in `crates/htap-convert/tests/materialization.rs`; note this is a converter API, not SQL execution) executes base-plus-delta queries by reading columnar segments up to the conversion snapshot version and overlaying rowstore mutations committed after that base version.
- **Online point writes and reads:** Storage descriptors route point mutations to `Route::RowstoreWrite` and complete-PK reads to `Route::RowstorePointRead` without interruption during conversion.
- **Crash resumption and idempotency:** Interrupted conversion resumes safely from persisted state envelopes.

### Explicitly deferred features

- **Whole-dataset materialization:** Partition conversion reads and materializes the entire rowset into memory and intermediate segment files rather than using a streaming or chunked conversion pipeline.
- **Reverse physical data transcoding:** Metadata-only demotion from Column to Row is supported (`convert_table_to_row` / `demote_partition_to_row`), switching catalog metadata to `Row` and clearing `column_manifest` via atomic CAS, while keeping the rowstore authoritative and retaining columnar segment files on disk. Physical reverse data transcoding and physical deletion/reclamation of column files remain deferred.
- **Autonomous background conversion scheduler:** Conversion and demotion advance strictly via explicit synchronous method calls (`conversion_tick`, `tick`, `convert_table_to_column`, `convert_table_to_row`); `tick()` resumes persisted jobs only, without autonomous background scheduling.
- **Columnar delete vectors:** Per-segment bitmap delete vectors on columnar files are deferred. Deletion semantics are handled via tombstones in the rowstore base-plus-delta overlay.
- **Physical rowstore reclamation:** Converted rows are not purged or garbage-collected from rowstore SSTs/WAL. Rowstore remains authoritative and retains all history.
- **Delta-to-base background compaction:** No automatic compaction folds accumulated rowstore deltas into new columnar segments.
- **Direct SegmentReader pushdown optimization and distributed OLAP scans:** Direct SegmentReader pushdown optimization is now implemented for the compact base path (projection-aware compact reads with single safe predicate-leaf pushdown and delta suppression/overlay); compound AND pushdown beyond one leaf, != pushdown, vectorized aggregation / operator pipelines, joins, and multi-tablet/distributed scans are deferred.
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions, range/list partition boundaries, and distributed cutovers are deferred.

---

## Data movement and persistence bounds scope and deferred features

The Phase 5 implementation delivers single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and `LocalServer` integration. Hardening unit H5/M2 (`b7ff200`) added strict persistence boundaries:

- **H5/M2 fixed (`b7ff200`):** Shared bounded exact-file reader (`read_exact_bounded`) enforces size limits on all owned state files (`CATALOG`, `COORDINATOR`, `movement/jobs/<job-id>/JOB`, tablet manifests, `MANIFEST`, `VISIBLE`, `txn.journal`). Internal movement job IDs, tablet paths, and conversion segment paths are strictly validated against traversal and injection attacks.
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
| Phase 3 — SQL layer | `Complete (local MVP)` | Completed local slice: sqlparser MySQL dialect parsing, strict binder with typed `PointSelect` and `AnalyticSelect`, structural route classifier, durable catalog with reopen recovery, synchronous `LocalServer` executing across unpartitioned tables (SQL `CREATE TABLE`) and partitioned tables created via SQL DDL (`CREATE TABLE ... PARTITION BY RANGE/LIST`) or native non-SQL API `LocalServer::create_partitioned_table` (finite Range/List, 1 bucket-0 tablet and 1 healthy leader per partition). Supports multi-row INSERT routing across partitions in one commit version, complete-PK DELETE and SELECT routed by partition key (preserving rowstore fast path), and narrow OLAP scans across all partitions over rowstore or base-plus-delta rows using `<root>/colstore` with projection-aware compact reads (PK+requested column union, single safe predicate-leaf SegmentReader pushdown, delta suppression/overlay, and residual SQL evaluation; conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented for narrow local OLAP; distributed fanout, disk spilling, query cancellation, and resource quotas remain deferred), and `EmbeddedClient` façade. Supported partition grammar includes MySQL `CREATE TABLE ... PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` (with optional final `MAXVALUE` for range), as well as typed partition lifecycle DDL (`ALTER TABLE <table> ADD/DROP/REORGANIZE PARTITION`) on empty source partitions with rowstore collapse safety gates and native `LocalServer::alter_partitions` API. Remaining exclusions: partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), subpartitioning (`SUBPARTITION BY`), expressions in partition keys, multi-column `COLUMNS`, non-final `MAXVALUE`, populated DROP/REORGANIZE, and generic non-partition ALTER statements are strictly rejected. Format conversion guarded to single-partition tables for `convert_table`. Verified by `crates/htap-server/tests/local_server.rs` (including `test_sql_range_partitioning_ddl_and_maxvalue_routing`, `test_sql_list_partitioning_ddl_and_routing`, `test_server_sql_alter_partition_lifecycle`, `test_server_alter_partitions_drop_empty_and_populated_guard`, `test_server_alter_partitions_reorganize_empty_and_populated_guard`), `crates/htap-catalog/tests/catalog_recovery.rs`, `crates/htap-sql/tests/parse_bind.rs`, `crates/htap-sql/tests/route.rs`, and `crates/htap-client/tests/embedded_client.rs`. Deferred: populated partition reorganization data migration, physical data reclamation for dropped partitions, multi-partition movement, hash tablets, distributed serving/failover, compound AND pushdown beyond one leaf, `!=` pushdown, vectorized aggregation / operator pipelines, multi-tablet/distributed scans, quotas/spill/cancellation, DataFusion/Arrow, and broader auth/security boundary (RBAC, per-user credentials, TLS). MySQL wire protocol serving (`htapd`/`htap-wire`) is implemented as of Phase 8 (see that row); joins/CTEs/subqueries/`UNION`/expressions/`ORDER BY` expressions/aliases/`LIMIT`/`OFFSET`/`HAVING`/`OR`/`NOT`/arithmetic/casts/`AVG`/`DISTINCT` aggregates and non-PK DML (`UPDATE`, `DROP TABLE`) are implemented as of Phase 9 (see that row); sessions/`BEGIN`/`COMMIT`/`ROLLBACK` are implemented as of Phase 10 (see that row); window functions and cost-based query optimization remain deferred. |
| Phase 4 — HTAP conversion | `Complete (local MVP)` | Completed local Row-to-Column conversion MVP (`htap-convert`) with converter APIs `read_column_partition` / `read_column_partition_compact` and `LocalServer` base-plus-delta OLAP scans over `<root>/colstore` with compact base scan pushdown. Table-wide conversion reports (`convert_table_to_column`), Column-to-Row metadata demotion (`convert_table_to_row`) retaining rowstore authority and column files on disk, explicit synchronous policy ticks (`conversion_tick`, `tick`), and fail-closed startup validation on reopen (`LocalServer::open`) are implemented. Open/deferred: whole-dataset materialization in conversion; autonomous background conversion scheduling deferred (ticks are explicit); physical reverse data transcoding deferred; delete vectors, physical rowstore/columnar reclamation, compaction, compound AND pushdown beyond one leaf, `!=` pushdown, vectorized aggregation / operator pipelines, and distributed partition/table conversion semantics deferred. |
| Phase 5 — Data movement | `Complete (local MVP)` | Single-node tablet clone, verify, repair, CSV/JSONL import/export, durable job tracking, and LocalServer façade implemented. H5/M2 bounds and internal path validation added (`b7ff200`). Open: whole-dataset materialization in export (exports materialize full logical partition before writing) and clone; external `CopyOptions` paths caller-controlled by design; SQL `COPY` syntax and bulk-load streaming over the wire protocol (`htap-wire` is a query/result-set protocol, not a bulk data-movement protocol), distributed multi-node coordinated migrations, background replication stream, and cross-partition movement deferred. |
| Phase 6 — Distribution and coordination | `Complete (local MVP)` | LocalCoordinator (`HTAPCRD1`), monotonic fencing tokens, coordinator-fenced catalog CAS, deterministic placement planner, and local activation simulation implemented (placement is metadata/planning/local simulation, not sharded SQL serving). Exclusive root ownership via `<root>/LOCK` added (`1083fbd`) as one-owner multiprocess-exclusive mode (not concurrent shared-root writers). H5/M2 envelope bounds added (`b7ff200`). Deferred: Raft/openraft, ZooKeeper backend, watches/locks/KV semantics, distributed consensus, remote replica serving, physical sharded SQL serving, real HA, leader handoff, ongoing replication, capacity/rack placement, and live rebalance; standalone low-level components remain unlocked. |
| Phase 7 — Hardening, benchmarks, local MVP | `Complete (hardened local MVP)` | Hardened transaction commit irrevocability + DurablePending (C2/H1 in `88cc314`), manager decision serialization (H1 in `88cc314`), external apply identity ledger across WAL GC (C1 in `f7a4975`), Engine post-WAL retry/recovery (H2 in `c5ee281`), owned persistence bounds/internal path validation (H5/M2 in `b7ff200`), and exclusive root ownership (`1083fbd`). Built Criterion microbenchmarks (`htap-bench`, `local_mvp`), synchronous embedded client (`htap-client`), operational documentation. Project is a hardened local embedded MVP; production readiness is not claimed. Deferred at the time: `htapd` daemon, MySQL wire protocol, and network endpoints (delivered in Phase 8, see below); Docker image/Compose, ZooKeeper/Raft backends, physical power-loss fsync testing, and TPC-C/TPC-H compliance remain deferred. |
| Phase 8 — Network server | `Complete (local MVP)` | Built a hand-written, synchronous MySQL text-protocol server (`htap-wire::WireServer`, `WireServerConfig`) over `Arc<LocalServer>` (std::net, thread-per-connection, statements serialized by the server's own execution lock), a daemon binary (`htapd`) with `--root`/`--listen`/`--max-connections`/`--password`/`HTAPD_PASSWORD`, and `htap-client::RemoteClient` returning the same `StatementResult` shape as `EmbeddedClient`. Supports handshake v10 with `mysql_native_password`, `COM_QUERY`/`COM_PING`/`COM_INIT_DB`/`COM_QUIT`, and a start-up compatibility shim (`SET`, `USE`, `SELECT 1`/`VERSION()`/`DATABASE()`/`@@sysvar`). Default bind is loopback-only; no TLS (see ADR-016 and the "Network layer" / "Security model" sections of `docs/ARCHITECTURE.md`). Covered by `crates/htap-wire/src/*.rs` unit tests, `crates/htap-wire/tests/wire_server.rs` (23 tests, including a real-driver interop test `test_mysql_crate_driver_interop` against the `mysql` crate v28), and `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`. Deferred: TLS, compression, prepared statements/binary protocol, multi-statements/multi-results, per-user ACL/RBAC, Docker packaging, and a selectable single-role `htapd` mode (sessions/`BEGIN`/`COMMIT`/`ROLLBACK` delivered in Phase 10, see that row). |
| Phase 9 — SQL breadth and cross-engine joins | `Complete (local MVP)` | Built a general query executor (`htap-sql::{query, expr, binder_query}`, `htap-server::query_exec`, `Route::Query`) that materializes every base table side of a join through the existing `scan_partition_compact` storage path at one MVCC snapshot per statement, then hash-joins/filters/groups/orders in memory: `INNER`/`LEFT`/`RIGHT`/`CROSS` joins, aliases, qualified names, arithmetic/comparisons/`AND`/`OR`/`NOT`/`IS [NOT] NULL`/`TRUE`/`FALSE`/`LIKE`/`IN`/`BETWEEN`/`CASE`/`CAST`, scalar functions, aggregates with `DISTINCT`, `GROUP BY`/`HAVING`, `SELECT DISTINCT`, `ORDER BY`/`LIMIT`/`OFFSET`, `UNION`/`UNION ALL`, derived tables, non-recursive CTEs, uncorrelated scalar/`IN`/`EXISTS` subqueries. Added `UPDATE` (point and filtered-scan forms, `Route::RowstoreUpdate`, one transaction per statement, effective ~4 MiB payload cap — see Phase 10 row and "Effective 2PC transaction payload cap" above), `DROP TABLE` (metadata-only, `Route::CatalogDdl`), and `SHOW TABLES`/`SHOW DATABASES`/`SHOW COLUMNS`/`DESCRIBE` (`Route::CatalogRead`). A purely syntactic shape gate (`is_narrow_select_shape`) keeps R5's complete-PK `Route::RowstorePointRead` and narrow `Route::OlapScan` unchanged and structurally isolated, pinned by `test_point_read_fast_path_pinned_against_general_query_path`. Catalog envelope `HTAPCAT1` bumped format version 1 -> 2 to persist `id_high_water` so dropped table/partition/tablet/replica ids are never reissued; version-1 catalogs still decode. See "General query executor scope and deferred features" above for full evidence. Deferred: window functions, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, recursive CTEs, `EXCEPT`/`INTERSECT`, cost-based optimization, vectorized/pipelined execution, worker-pool parallelism on the general path, memory bounds/spilling, physical reclamation on `DROP TABLE`, `INSERT ... SELECT`, `UPDATE` with joins/subqueries, filtered `DELETE`, `TRUNCATE`, and non-partition `ALTER TABLE`. |
| Phase 10 — Sessions and explicit transactions | `Complete (local MVP)` | Built server-side sessions (`htap-server::session::Session`, `LocalServer::open_session`, `EmbeddedClient::open_session`, one `htap-wire` connection = one session) with `BEGIN`/`START TRANSACTION [READ ONLY \| READ WRITE]`, `COMMIT`, `ROLLBACK`, `autocommit` (0/1/ON/OFF/TRUE/FALSE), `SET @x = expr` (multi-assign), `SET [SESSION] TRANSACTION ISOLATION LEVEL REPEATABLE READ` (only accepted level) and `READ ONLY`/`READ WRITE` (next-transaction-only, even with `SESSION`), and a new `htap-sql::variables` system-variable registry (`@@name`, aliases, dynamic `autocommit`/`transaction_isolation`/`transaction_read_only`) that replaced the old ad hoc wire-shim fakes. Uncommitted writes are buffered in a session's own `WriteSet` (never journaled — a crash is an implicit `ROLLBACK`) and overlaid below relational operators for point reads, narrow scans, the general executor, and `UPDATE`, across `Row`/`Column`/`Converting` partitions; `COMMIT` runs the existing 2PC path once against the transaction's own pinned snapshot. Snapshot isolation with first-writer-wins, write skew permitted, reported as `REPEATABLE READ`; a write-write conflict at `COMMIT` and a stale snapshot vs. a mid-transaction columnar base both poison the transaction as `Conflict`; commit-time catalog revalidation catches a concurrent `DROP TABLE`/`ALTER`; `DurablePending` moves the session to a quarantined state rejecting every further statement, including `ROLLBACK`, with the original non-retryable error. Fixed two correctness gaps found during this work: the first-writer-wins check now runs in `Engine::prepare` before any journal write (previously only after the `Commit` record was fsynced), and an autocommit write now commits against its own read snapshot instead of a fresh one (previously could lose a race against a concurrent `copy_from_*`/import). Added a manager-wide `TransactionManager` recovery latch that rejects every *other* commit once any commit returns `DurablePending`, with a distinct non-retryable `HtapError::RecoveryRequired` (not the blocking transaction's own `DurablePending`), until `recover()` resolves it in-process (only for a `RecoveryCause::ParticipantIo` latch) or the manager reopens (always clears it, any cause). A follow-up fix pass also: made `recover()` fsync the journal before replaying any commit and cross-check each participant's own `committed_version()` against the journal's replayed max version, failing as `HtapError::Corruption` on mismatch; restored `next_txn_id` via `fetch_max` (never regressing it); truncated a failed journal append back to its pre-append offset; moved the applied-external-transactions ledger capacity check into `Engine::prepare` (before any journal write, not just at apply, rejecting with `HtapError::InvalidArgument`); and found the actual effective 2PC transaction payload cap is about 4 MiB, not the nominal 16 MiB (see "Effective 2PC transaction payload cap" above). A third fix pass added a `Journal`-level `poisoned` state (rejecting further appends/syncs until reopened), latched the manager as `JournalIo` for a failed `Intent`/`Abort` write too (previously only `Commit`), made `recover()` refuse outright and apply nothing while already latched `JournalIo` or while the journal is poisoned, and scaled the payload-cap bound by participant count (lowering the effective single-participant cap to 4,194,143 bytes). See "Sessions and explicit transactions (Phase 10)" in `docs/ARCHITECTURE.md`, ADR-018, and `docs/PROGRESS.md` for the full contract and test evidence. Deferred: `SELECT ... FOR UPDATE`/locking reads, prepared statements/binary protocol, savepoints, XA, IPC/multiprocess sessions, per-user ACL, idle-transaction timeout/reaping, MVCC garbage collection, and a grace period before a new columnar base dooms an open transaction. |

---

## Deviations from the brief

| Brief requirement | Deviation | Rationale | Where recorded |
| ----------------- | --------- | --------- | -------------- |
| ZooKeeper reference source at `examples/zookeeper` (§3 of the brief) | Input absent; ZooKeeper backend, `zookeeper-async` dependency, and Docker ensemble tests are not implemented in the local MVP. Coordination is implemented locally via `htap-coord::LocalCoordinator`. ZooKeeper backend and containerized testing remain deferred future work. | `examples/zookeeper` was not supplied; cluster coordination was scoped to a single-node local coordinator MVP. | ADR-006 in [`DECISIONS.md`](./DECISIONS.md); "Missing input: ZooKeeper reference source" above. |

> **This table must remain exhaustive.** Anything omitted or changed relative
> to the brief is recorded here or in an ADR, never silently dropped.
