# Decisions

An architecture decision record (ADR) log. Each entry follows the same shape:
**Context → Options considered → Decision → Consequences → How to reverse it.**

---

## ADR-001: DataFusion for the analytical path only; hand-write the transactional fast path

`Status: Proposal / Deferred (reference only; not implemented in local MVP)`
`Date: 2026-09-07`

### Context

R4 demands broad SQL coverage — CTEs, window functions, subqueries, a cost
model. R5 explicitly prohibits routing point lookups through analytical
machinery. These two requirements pull in opposite directions.

### Options considered

- **(a)** Use DataFusion for everything.
- **(b)** Hand-write both execution paths.
- **(c)** Split: DataFusion for analytics, hand-written fast path for OLTP.

### Decision

Option **(c)** (architectural proposal / future work).

DataFusion delivers the breadth R4 requires at a fraction of the cost of
building it. But routing a primary-key point lookup through logical planning,
physical planning, and a `RecordBatch` pipeline would violate R5's explicit
prohibition.

Splitting also makes the guarantee **structural** rather than advisory: the
OLTP crate does not depend on the OLAP crate, so a point lookup cannot
accidentally acquire analytical overhead.

**Implementation note:** In the current local MVP, DataFusion is not integrated
or implemented. Instead, `htap-sql` binds narrow single-table analytical queries
to `AnalyticSelect` (`Route::OlapScan`) and `LocalServer` executes them via a hand-written
evaluator (`htap-server::olap`). For materialized `Column` and `Converting` partitions
(using `<root>/colstore`), `LocalServer` uses projection-aware compact reads
(`read_column_partition_compact_core`) unioning primary-key and requested columns,
pushes down at most one safe predicate leaf directly into columnar `SegmentReader::scan`,
suppresses stale base rows using post-base rowstore deltas, overlays delta puts and deletes,
and sorts into deterministic primary-key order before full residual SQL filter, aggregate,
and group evaluation. Columnar `ScanStats` and block pruning are available internally as
execution evidence, but SQL evaluation still operates on materialized logical rows;
vectorized aggregation is not implemented. Complete-PK point queries strictly take the
hand-written transactional fast path (`Route::RowstorePointRead`), structurally bypassing
the analytical evaluator and converter, remaining separate and unchanged. Compound `AND`
pushdown beyond one leaf, `!=` pushdown, joins, CTEs, windows, ORDER BY, LIMIT, HAVING, OR,
expressions, AVG, DISTINCT, multi-tablet/distributed scans, quotas, spill, cancellation,
and DataFusion/Arrow analytical integration remain deferred future work.

### Consequences

- Two execution paths must be kept semantically consistent.
- A conformance test asserting that both paths return identical results for
  overlapping queries is **required**, not optional.
- DataFusion analytical integration remains deferred future work.

### How to reverse it

Collapse into DataFusion by implementing the fast path as a custom physical
operator.

---

## ADR-002: Adopt the delete-vector / delete-and-insert MVCC model

`Status: Proposal / Reference design (reference only; not implemented in local MVP)`
`Date: 2026-09-07`

### Context

MVCC cost has to be paid somewhere. A key-ordered merge-on-read model pays it
on every scan, which directly penalizes the analytical workload.

### Options considered

- **(a)** Key-ordered merge-on-read (merge at scan time).
- **(b)** Delete-and-insert with per-segment delete vectors, re-derived from
  StarRocks primary-key tables (see finding 1 in [`RESEARCH.md`](./RESEARCH.md)).

### Decision

Option **(b)** (architectural proposal / reference design). Reads become a UNION
of rowsets with each segment's delete vector subtracted by a single bitmap
ANDNOT — no key comparison, no merge, no sort at read time.

**Implementation note:** Not implemented in the current local MVP. The local
engine relies on LSM rowstore tombstones and rowstore-authoritative base-plus-delta
overlays (`read_column_partition`). Columnar bitmap delete vectors remain
deferred future work.

### Consequences

- Write amplification and publish-time cost, in exchange for zero read-side
  merge cost.
- Delete vectors must be **version-scoped** and **copy-on-write**, so that
  applying a delete clones the bitmap and bumps its version without disturbing
  existing readers.
- Implementation deferred to post-MVP columnar updates.

### How to reverse it

Switch to key-ordered merge-on-read, at the cost of scan performance.

---

## ADR-003: Two-phase visibility with version-density gating

`Status: Accepted`
`Date: 2026-09-07`

### Context

If commit must wait for every storage node to acknowledge, commit latency is
bounded by the slowest node.

### Options considered

- **(a)** Fuse durability and visibility into a single commit step.
- **(b)** Separate commit (assigns a version, journals one record, durable but
  invisible) from publish (idempotent background fan-out that makes data
  readable), gated on version density.

### Decision

Option **(b)**. A transaction becomes visible only when its version is exactly
`visibleVersion + 1` for every partition it touched. `committedVersion` is
derived as `nextVersion - 1` and never stored.

### Consequences

- Readers compare only against a per-partition visible version.
- Recovery is a replay in version order.
- A committed-but-unpublished transaction is durable, and becomes visible
  after a crash.

### How to reverse it

Fuse publish into commit, accepting commit latency bounded by the slowest
replica.

---

## ADR-004: One MVCC version domain and one WAL shared by both storage formats

`Status: Proposal / Reference design (reference only; not implemented in local MVP)`
`Date: 2026-09-07`

### Context

The system has two storage formats. They can either share a version domain and
a log, or maintain their own.

### Options considered

- **(a)** One shared MVCC version domain and one shared WAL.
- **(b)** Per-format version domains and per-format WALs.

### Decision

Option **(a)** (architectural proposal / reference design). This allows a single
transaction to touch a row-format partition and a column-format partition
atomically, and it makes the R2 format swap a single metadata record.

**Implementation note:** While a single MVCC version domain (`Version`) is
implemented across formats, there is no single shared WAL mutating both row
and column formats atomically in the current codebase. The rowstore maintains
its own WAL, while columnar storage is written via partition conversion and
tablet manifest envelopes.

### Consequences

- The WAL becomes a shared bottleneck and must support **group commit**.
- Single shared multi-format atomic WAL remains deferred to future unified
  storage work.

### How to reverse it

Move to per-format WALs plus a two-phase protocol between them. This is
explicitly more complex — which is precisely why it was not chosen.

---

## ADR-005: `sqlparser-rs` with the MySQL dialect

`Status: Accepted`
`Date: 2026-09-07`

### Context

The system needs a SQL front end and a client-facing dialect.

### Options considered

- **(a)** `sqlparser-rs` with the MySQL dialect.
- **(b)** A hand-written parser.

### Decision

Option **(a)**.

### Consequences

- The system is bound by `sqlparser-rs` dialect coverage.
- Unsupported syntax must fail with a **clear error** rather than be silently
  mis-parsed.
- Typed MySQL partition DDL (`PARTITION BY RANGE [COLUMNS]`, `PARTITION BY LIST [COLUMNS]`)
  is supported via vendored `sqlparser` with typed AST representations (see ADR-013),
  while unsupported forms (options, subpartitioning, expressions, multi-column COLUMNS,
  lifecycle ALTER) fail with clear errors rather than lossy reinterpretation.

### How to reverse it

Write a hand-written parser.

---

## ADR-006: ZooKeeper reference source not provided; defer ZooKeeper backend and Docker testing to future work, proceed with local coordinator MVP

`Status: Superseded by ADR-009`
`Date: 2026-09-07 (Updated: 2026-09-10)`

### Context

The project brief specified a reference source at `examples/zookeeper`, but
only `examples/starrocks` was present. Furthermore, no external ZooKeeper backend,
`zookeeper-async` client dependency, Docker container configuration, or real ensemble
test exists in the workspace. See [`LIMITATIONS.md`](./LIMITATIONS.md).

### Options considered

- **(a)** Build against an in-memory mock ZooKeeper without persistence.
- **(b)** Require external Docker daemon and ZooKeeper runtime for tests.
- **(c)** Implement a clean synchronous `Coordinator` trait with a local durable coordinator implementation (`LocalCoordinator`), deferring external ZooKeeper/Raft backends and Docker-based testing to future distributed work.

### Decision

Option **(c)** — superseded by ADR-009. The local MVP coordinates cluster state,
membership, scoped leadership, and monotonic fencing via `htap-coord::LocalCoordinator`
persisting to `COORDINATOR` binary envelopes (`HTAPCRD1`). Distributed backends (ZooKeeper
or Raft) and containerized ensemble testing are deferred to future work.

### Consequences

- No external ZooKeeper service, daemon, or Docker environment is required to build, test, or benchmark the repository.
- Coordination guarantees are verified locally and synchronously in single-node integration tests.
- Pluggable `Coordinator` trait allows future integration of a real ZooKeeper backend when multi-node distribution is implemented.

### How to reverse it

Implement a ZooKeeper-backed adapter struct implementing `Coordinator` using an async runtime or client library and add optional containerized integration tests.

---

## ADR-007: Single binary with selectable roles rather than separate FE/BE binaries

`Status: Accepted`
`Date: 2026-09-07`

### Context

The frontend and backend are distinct roles, but forcing two binaries
complicates the single-node development experience and the demo.

### Options considered

- **(a)** One binary, `htapd`, with a selectable role (frontend, backend, or
  both).
- **(b)** Two separate binaries.

### Decision

Option **(a)**.

### Consequences

- Future demo can expose unified frontend/backend roles. Update (Phase 8, ADR-016): the `htapd` daemon binary
  and network listener (`htap-wire`) are now implemented as a single process always running both roles
  together; a selectable single-role mode and Docker/Compose packaging remain deferred future work. The
  in-process `LocalServer`/`EmbeddedClient` façade is unchanged and still has no daemon or network dependency.
- The module boundary must be **policed by crate dependencies**, so that
  splitting into separate processes remains possible.

### How to reverse it

Add two thin binary crates over the same libraries.

---

## ADR-008: Partition-scoped four-phase conversion state machine with authoritative rowstore overlay

`Status: Accepted`
`Date: 2026-09-10`

### Context

Storage-format conversion (R2) converts partitions from row-oriented storage (`htap-rowstore`)
to columnar storage (`htap-colstore`). The conversion process must not interrupt concurrent
OLTP transactions, must not expose incomplete or unchecksummed columnar data to readers,
and must survive abrupt process crashes at any intermediate step without corrupting catalog
or storage state.

### Options considered

- **(a)** Stop-the-world offline migration: block incoming writes and reads, transcode data,
  and update catalog format in a single pause.
- **(b)** Synchronous dual-writing during conversion: transcode background data while active
  writers simultaneously mirror incoming writes to both rowstore and columnar segments.
- **(c)** Partition-scoped four-phase state machine (`SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`)
  with atomic manifest and catalog publication and authoritative rowstore base-plus-delta overlay.

### Decision

Option **(c)**.

- **Four-phase state machine:**
  1. `SnapshotPinned`: pins rowstore visible version `V`, allocates a new catalog generation,
     and transitions the partition to `StorageDescriptor::Converting { from: Row, to: Column }`
     via catalog CAS. Resuming an existing conversion reuses the persisted pinned snapshot version
     and generation.
  2. `SegmentsWritten`: transcodes rowstore entries at snapshot `V` into columnar segments,
     writes the durable tablet manifest envelope (`HTAPTBM1` with CRC32C checksum) atomically
     via temporary file replacement (`MANIFEST.tmp` -> `MANIFEST`), and records the phase in the
     catalog via CAS.
  3. `ReadyToPublish`: advances the catalog generation and phase via CAS, verifying manifest
     durability before cutover.
  4. `Column`: executes the final catalog CAS cutover to `StorageDescriptor::Column`, clearing
     conversion descriptors and binding the tablet's `ColumnManifestRef`.
- **Authoritative rowstore base-plus-delta overlay:** The rowstore remains the authoritative
  truth for point operations (`Route::RowstoreWrite` and `Route::RowstorePointRead`), executing
  without interruption across `Row`, `Converting`, and `Column` states. Materialized scans
  via converter APIs (`read_column_partition` and projection-aware compact reads
  `read_column_partition_compact_core`, verified in `crates/htap-convert/tests/materialization.rs`)
  and `LocalServer` analytical scans (`Route::OlapScan` using `<root>/colstore`) scan columnar base
  segments up to `V` and overlay post-`V` rowstore puts and deletes. For `Column` and `Converting`
  partitions, `LocalServer` analytical scans execute projection-aware compact reads (PK + requested
  column union), safely push down one eligible predicate leaf into `SegmentReader::scan`, suppress
  stale base rows using rowstore deltas, and evaluate residual SQL logic.
- **Scope boundaries:** Direct SegmentReader pushdown optimization is now implemented for the compact
  base path. Compound `AND` pushdown remains limited to one leaf, and `!=` remains residual.
  Metadata-only `Column -> Row` demotion is implemented via catalog CAS (see ADR-015), while physical
  reverse transcoding is not implemented. Columnar bitmap delete vectors, physical rowstore reclamation,
  delta-to-base background compaction, autonomous background conversion scheduling, vectorized aggregation,
  vectorized operator pipelines, joins/CTEs/windows, and distributed multi-tablet conversion are explicitly deferred.

### Consequences

- Zero downtime or blocking for online point reads and writes throughout conversion.
- Crash-safe and resumable: any crash during conversion resumes from the persisted phase and
  pinned snapshot without duplicate manifest generation or orphaned segment leaks.
- Storage footprint temporarily retains rowstore data post-conversion because physical rowstore
  reclamation is deferred.
- Physical reverse transcoding is unsupported (metadata demotion back to Row is supported via ADR-015).

### How to reverse it

Replace the online state machine and rowstore overlay with an offline transcode and catalog swap
utility.

---

## ADR-009: Durable synchronous local coordinator with monotonic fencing tokens and fenced catalog CAS

`Status: Accepted`
`Date: 2026-09-10`

### Context

Requirement R6 demands that fencing tokens prevent stale-leader split-brain during leadership transitions and metadata mutations. Additionally, cluster coordination requires tracking node membership, assigning replica placements across tablets, and managing the lifecycle of replica additions. Distributed coordination backends (such as Raft or ZooKeeper) require network transports, distributed consensus state machines, and external dependencies.

### Options considered

- **(a)** Full distributed consensus (`openraft` / ZooKeeper) with network RPC from the start.
- **(b)** In-memory mock coordinator without persistence or crash-recovery guarantees.
- **(c)** Object-safe synchronous `Coordinator` trait with durable local directory-backed implementation (`LocalCoordinator`), monotonic fencing token allocator, coordinator-fenced catalog CAS, deterministic placement planner, and local replica activation simulation.

### Decision

Option **(c)**.

1. **Synchronous `Coordinator` trait:** Defines clean abstractions for node membership (`register_node`, `remove_node`, `list_nodes`), scoped leadership leases (`acquire_leadership`, `release_leadership`, `replace_leadership`), fence validation (`validate_fence`), and coordinator-fenced catalog updates (`fenced_catalog_compare_and_set`).
2. **Durable `LocalCoordinator` (`HTAPCRD1`):** Persists cluster state at `<root>/COORDINATOR` using a versioned binary envelope with format magic `b"HTAPCRD1"`, format version 1, payload length, and CRC32C checksum. State transitions follow atomic two-phase staging (`COORDINATOR.tmp` -> fsync -> rename -> directory fsync).
3. **Monotonic fencing tokens:** Allocates strictly increasing tokens (`FencingToken`) on every leadership transition. Persists high-water mark tokens per scope and globally to guarantee tokens are never reused across restarts.
4. **Fenced catalog compare-and-set:** Serializes active leader fence token validation and catalog compare-and-set updates under the coordinator's state lock. Stale leaders are rejected with `HtapError::Fenced` without altering catalog state.
5. **Deterministic placement planner (`plan_placement`):** Computes pure deterministic, colocation-free replica placements across canonicalized candidate nodes and tablets, greedily assigning additions to the least loaded nodes, breaking ties by lowest `NodeId`, and allocating monotonic `ReplicaId`s.
6. **Local activation simulation (`stage_placement_addition`, `activate_placement_addition`):** Stages new replicas as non-leader and unhealthy (`is_leader = false`, `healthy = false`) via fenced CAS, performs logical snapshot cloning and package verification (`HTAPMNF1`), and marks replicas healthy via fenced CAS.

### Explicit boundaries and deferred capabilities

- **Direct CatalogStore CAS and old movement repair bypass fence:** Direct calls to `CatalogStore::compare_and_set` and older movement repair functions (`htap_movement::repair_replica`) bypass coordinator fencing. Fencing is strictly enforced when callers route mutations through `fenced_catalog_compare_and_set` or `activate_placement_addition`.
- **Intra-process mutex only:** `LocalCoordinator` serializes mutations using an internal mutex (`parking_lot::Mutex`); cross-process exclusion and file locking are unsupported.
- **No distributed consensus or remote transport:** No Raft (`openraft`) or ZooKeeper backends, watches, locks, KV store semantics, remote physical movement, leader handoff, ongoing replication, capacity/rack placement, or live rebalance are implemented.

### Consequences

- R6 stale-leader split-brain protection is structurally verified and crash-safe in integration tests (`crates/htap-coord/tests/local_coordinator.rs`, `placement_movement.rs`).
- The object-safe `Coordinator` trait allows future pluggable consensus backends (`openraft`, ZooKeeper) without altering catalog or placement consumers.
- Concurrency guarantees are confined to a single process; multi-node distributed coordination remains deferred.

### How to reverse it

Implement distributed `openraft` or ZooKeeper coordination backends conforming to the `Coordinator` trait and replace `LocalCoordinator` in the cluster topology runtime.

---

## ADR-010: Local microbenchmark suite and synchronous embedded client façade for Phase 7 local evidence MVP

`Status: Accepted`
`Date: 2026-09-10`

### Context

Phase 7 requires performance and operational hardening, benchmark validation, and operational documentation. A production HTAP system typically requires distributed TPC-C/TPC-H benchmarks, daemon processes (`htapd`), network wire protocols, and container packaging. Per the autonomy contract, the local MVP prioritizes correctness, runnable verified vertical slices, and explicit boundaries over stubbed production claims.

### Options considered

- **(a)** Attempt partial TPC-C/TPC-H harness scripts requiring distributed transactions, SQL analytical query engines, and background daemons.
- **(b)** Deliver an in-process Criterion microbenchmark suite (`htap-bench`) covering the six core engine layers (rowstore point lookups, columnar zone-map scans, partition conversion, CSV movement, placement planning, and fenced CAS) with deterministic correctness assertions, accompanied by a synchronous in-process embedded client (`htap-client`, `EmbeddedClient`), while keeping CI benchmark verification compile-only.

### Decision

Option **(b)**.

1. **Criterion microbenchmark harness (`crates/htap-bench/benches/local_mvp.rs`):**
   - Implements isolated microbenchmarks: `rowstore/point_get_stable_snapshot`, `colstore/equality_zone_map_scan` (validating 99% block skip), `convert/local_converter_row_to_column`, `movement/fixed_csv_import`, `coord/placement_planning`, and `coord/leadership_fenced_cas`.
   - Each benchmark verifies deterministic functional correctness before entering the timing loop.
   - Modest execution settings (sample size 10, measurement 1s, warmup 500ms).
2. **Compile-only CI benchmark stage:**
   - Modified `ci.sh` to run `cargo bench --workspace --no-run` as a fifth stage following formatting, clippy, build, and test. Timings are never executed in CI, avoiding non-deterministic hardware timing failures in virtualized environments.
3. **Embedded client façade (`htap-client`):**
   - Synchronous, direct in-process façade (`EmbeddedClient`) over `LocalServer` executing single-partition `CREATE TABLE`, literal `INSERT`, PK `DELETE`, complete-PK `SELECT`, and narrow analytical scans (`AnalyticSelect` / `Route::OlapScan`) with structured error mapping and recovery across reopen.
4. **Operational documentation:**
   - Created root `README.md`, `docs/BENCHMARKS.md`, and `docs/OPERATIONS.md` documenting filesystem layouts (`catalog`, `rowstore`, `txn.journal`, `movement`, `COORDINATOR`), recovery boundaries, and explicit non-features at the time (no daemon, no MySQL wire protocol, no network sockets, no Docker/Compose, no TPC-C/TPC-H compliance). Update (Phase 8, ADR-016): the daemon, MySQL wire protocol, and network sockets have since been implemented (`htapd`, `htap-wire`); Docker/Compose and TPC-C/TPC-H compliance remain non-features.

### Consequences

- All benchmark targets and client tests compile cleanly and pass verification locally.
- Explicit non-features prevent scope creep and false claims of production DBMS completeness.
- Clean separation between storage/server crates, benchmark harness, and embedded client.

### How to reverse it

Extend the benchmark harness into multi-process client/server benchmarks when network transports and analytical SQL engines are implemented. Update (Phase 8): a network transport now exists (`htap-wire`/`htapd`), but `htap-bench` has not yet been extended to a client/server benchmark mode; the Criterion suite remains in-process only.

---

## ADR-011: SQL Parser Boundary for Partition DDL and Catalog Partition Model Alignment

`Status: Superseded by ADR-013`
`Date: 2026-09-11 (Updated: 2026-09-15)`

### Context

The database catalog (`htap-catalog`) features a validated partition model with finite typed range and list partitioning (`PartitioningDescriptor`, `PartitioningMethod::Range`, `PartitioningMethod::List`, `RangeBound`, `TableDescriptor::route_partition_value`, `CatalogSnapshot::route_partition_value`), utilized in controlled and admin test fixtures.

However, upstream `sqlparser 0.62` with `MySqlDialect` did not retain MySQL partition DDL syntax (`CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...`) in its AST, rejecting such statements during SQL DDL. Attempting to support MySQL partition DDL via ad-hoc string parsing or lossy mapping of unrelated AST fields would compromise architectural boundaries and data integrity.

### Options considered

- **(a)** Implement ad-hoc regex or string pre-parsing to extract MySQL partition clauses before invoking `sqlparser`.
- **(b)** Coerce unrelated `CreateTable.partition_by` expressions through lossy reinterpretation into internal partition descriptors.
- **(c)** Enforce a strict parser and binder boundary until an AST-retaining parser is adopted: verify that MySQL partition DDL is rejected with `HtapError::InvalidArgument` from `parse_one` under upstream `sqlparser 0.62` / `MySqlDialect`, and ensure manually constructed AST partition fields are rejected by the binder (`HtapError::Unsupported`). Preserve the internal catalog's validated finite range/list metadata for controlled/admin fixtures, keep SQL-created `LocalServer` tables unpartitioned (default single partition), and defer multi-partition execution through SQL.

### Decision

Option **(c)** (initially adopted; superseded by ADR-013).

1. **Strict parser-level rejection (superseded by ADR-013):** Upstream MySQL `CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...` syntax initially failed during `parse_one`, returning `HtapError::InvalidArgument`. ADR-013 replaced upstream `sqlparser` with a vendored copy adding typed AST support.
2. **Strict binder rejection & no lossy AST reinterpretation:** Generic/unrelated `partition_by` AST clauses continue to be rejected by `htap-sql::bind` with `HtapError::Unsupported`. Unsupported partition options, subpartitioning, expressions, and multi-column COLUMNS remain strictly rejected (with partition lifecycle DDL subsequently supported for empty sources in ADR-014).
3. **Preservation of catalog metadata:** The catalog maintains its validated finite range/list descriptors and routing helpers.
4. **SQL-created tables:** Unpartitioned SQL DDL creates a default single partition `p0`; partitioned SQL DDL creates validated range/list topologies as defined in ADR-013.
5. **Deferred capabilities:** Hash buckets / tablet sharding, partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION` — historical context; subsequently implemented on empty sources in ADR-014), cross-partition UPDATE row movement, and distributed multi-partition execution remain deferred.

### Consequences

- Historical record of the initial parser boundary prior to vendored AST support.
- Clear separation between validated finite partition AST structures and lossy dialect workarounds.

### How to reverse it

Superseded by ADR-013.

---

## ADR-012: Native Partitioned Table Execution and Multi-Partition DML/OLAP Execution in LocalServer

`Status: Accepted`
`Date: 2026-09-11`

### Context

While MySQL partition DDL was initially rejected at the parser level due to upstream `sqlparser 0.62` AST limitations (ADR-011), the database engine required verified partition execution to demonstrate multi-partition DML and OLAP routing without waiting for parser changes or introducing lossy dialect workarounds.

Partitioned tables require unambiguous topology specifications (finite range and list definitions), strict catalog validation against primary keys and data types, atomic catalog publication, single-transaction multi-partition mutation semantics, and point lookup preservation of the rowstore fast path.

### Options considered

- **(a)** Defer all partition execution until an upstream SQL parser upgrade allows SQL DDL partition creation.
- **(b)** Implement custom regex / text pre-processing before SQL parsing to fake SQL partition DDL.
- **(c)** Provide a typed, native non-SQL admin API (`LocalServer::create_partitioned_table`) taking explicit `PartitionedTableDefinition` with finite `PartitionTopology::Range` and `PartitionTopology::List`, execute DML/OLAP statements against partitioned tables, and keep SQL MySQL partition DDL strictly rejected at the parser level.

### Decision

Option **(c)**.

1. **Native non-SQL admin API (`LocalServer::create_partitioned_table`):**
   - Tables are defined via `PartitionedTableDefinition` with either `PartitionTopology::Range` (ordered half-open `[lower, upper)` intervals where `lower < upper`) or `PartitionTopology::List` (disjoint sets of explicit values).
   - The table, partition descriptors, initial tablets, and replicas are published to the catalog in a single atomic `compare_and_set` operation.
2. **Catalog validation rules:**
   - The partition key column must be non-null and part of the table's primary key (`primary_key.contains(&key_column)`).
   - Range bounds require `lower < upper`, non-overlapping intervals, and exact type alignment with the key column.
   - List partitions require non-empty disjoint value lists without duplicate entries across or within partitions.
   - Rejects empty partition definitions, duplicate partition names, and type mismatches.
3. **Local topology invariants:**
   - Each partition is initialized with `StorageDescriptor::Row`, exactly one bucket-0 row tablet (`tablets.len() == 1`, `bucket = 0`), and one healthy local leader replica on node 1 (`NodeId(1)`).
   - No hash buckets, sub-partitioning, or physical sharding across distributed nodes is implemented.
4. **Transactional multi-partition DML routing:**
   - Multi-row `INSERT` routes each row by evaluating its partition key against catalog partition metadata (`route_partition_value`), validates partition storage format, and commits all mutations across all touched partitions in a single atomic transaction payload and single monotonically advanced version.
   - Complete-PK `DELETE` resolves the partition key from the primary key, routes to the matching partition, and performs a rowstore point delete.
   - Complete-PK `SELECT` resolves the partition key from the primary key, routes directly to the matching partition, and executes `Route::RowstorePointRead` via `Engine::get`, strictly bypassing analytical execution and format conversion.
5. **Multi-partition analytic SELECT (`Route::OlapScan`):**
   - Evaluates queries across partitions belonging to the table at a single visible transaction snapshot (using logical rowstore scans or compact columnar base-plus-delta scans per partition).
   - Combines rows across partitions and evaluates global or grouped projections, filters, aggregates (`COUNT`, `SUM`, `MIN`, `MAX`, `GROUP BY`), and orderings.
   - Conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented for narrow local OLAP; distributed fanout, disk spilling, query cancellation, and resource quotas remain deferred.
6. **SQL boundary alignment (updated by ADR-013):**
   - Tables created via standard SQL DDL without partitioning clauses remain unpartitioned with a default single partition `p0`. With ADR-013, MySQL `PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` DDL is parsed via vendored `sqlparser` and creates partitioned topologies through unified catalog publication.
7. **Single-partition format conversion guard:**
   - `LocalServer::convert_table` explicitly verifies that the target table has exactly one partition and rejects multi-partition tables with `HtapError::Unsupported`. (Historical context: `convert_table` remains single-partition, while table-wide multi-partition conversion and demotion are subsequently introduced in ADR-015 via `convert_table_to_column` and `convert_table_to_row`.)
8. **Deferred capabilities:**
   - Partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION` — historical context; subsequently implemented on empty sources in ADR-014), partition split/merge/drop, multi-partition conversion (subsequently implemented via table-wide conversion reports in ADR-015) and movement, hash tablets, distributed/remote partition serving across network nodes, replica failover, and an inter-node distributed-serving network protocol remain deferred. (Historical context: the client-facing MySQL wire protocol was subsequently implemented in Phase 8/ADR-016; this item refers to inter-node/distributed partition serving, which remains deferred.)

### Consequences

- Multi-partition execution is proven and reliable in-process without relying on unvalidated SQL parser extensions.
- Storage engine invariants (atomic versioned commits, rowstore point lookup fast path) remain intact.
- Both native admin API and SQL DDL create identical validated partition models.

### Test Evidence

- `crates/htap-server/tests/local_server.rs`:
  - `test_sql_range_partitioning_ddl_and_maxvalue_routing`
  - `test_sql_list_partitioning_ddl_and_routing`
  - `test_partitioned_native_range_topology_catalog_reopen_continuation`
  - `test_partitioned_native_list_topology_catalog_reopen_continuation`
  - `test_partitioned_boundary_unmatched_null_type_errors`
  - `test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`
  - `test_partitioned_composite_pk_partition_key_not_first`
  - `test_partitioned_olap_across_partitions_and_empty_aggregate`
  - `test_partition_pruning_range_and_list_and_conservative_cases`
  - `test_scan_worker_count_equivalence`
  - `test_multi_partition_order_by_directions_nulls_and_tie_breaking`
  - `test_convert_table_multi_partition_guard`
  - `test_partitioned_empty_topology_rejection_no_catalog_mutation`
- `crates/htap-catalog/tests/catalog_recovery.rs`:
  - `test_partitioning_legacy_decode_and_reopen`
  - `test_range_partitioning_routing_and_boundaries`
  - `test_list_partitioning_routing`
  - `test_partitioning_duplicate_violations`
  - `test_range_overlap_and_order_violations`
  - `test_partitioning_type_and_null_violations`
  - `test_partitioning_ownership_and_method_consistency`
  - `test_partitioning_cas_and_reopen_lifecycle`
- `crates/htap-sql/tests/parse_bind.rs`:
  - `test_mysql_partition_ddl_parsed_and_bound`
  - `test_mysql_partition_ddl_negative_parser_and_binder`

---

## ADR-013: Vendored Apache-2.0 `sqlparser` with Typed MySQL Partitioning Grammar

`Status: Accepted`
`Date: 2026-09-15`

### Context

ADR-011 recorded the limitation that upstream `sqlparser 0.62` did not retain MySQL `PARTITION BY RANGE/LIST` syntax in its AST, rejecting such statements during SQL DDL.
To support typed grammar-backed MySQL `CREATE TABLE ... PARTITION BY RANGE/LIST` without lossy regex or suffix slicing, a minimal patch was needed.

### Decision

1. Vendor Apache-2.0-licensed `sqlparser 0.62.0` under workspace directory `vendor/sqlparser`.
2. Extend `CreateTable` AST and parser grammar with `MysqlPartitionBy`, `MysqlPartitionDef`, `MysqlPartitionValues`, and `MysqlLessThanBound` (supporting `MAXVALUE`, `VALUES LESS THAN`, `VALUES IN`, and `COLUMNS (...)`), preserving all existing public parser APIs.
3. Extend catalog `RangeBound` to support optional endpoints (`None` for unbounded / `MAXVALUE`), while retaining serde backwards-compatibility with existing non-optional representation.
4. Unify table creation in `LocalServer` across SQL partitioned, SQL unpartitioned (default `p0`), and native administrative creation through a single lock-safe helper `create_table_internal`.
5. Strict binding in `htap-sql` validates partition keys against PK and non-null constraints, verifies increasing order for RANGE bounds, and checks disjointness of LIST values.
6. Provenance and Apache-2.0 licensing recorded in `ATTRIBUTION.md`.

### Consequences

- Typed MySQL `CREATE TABLE ... PARTITION BY RANGE [COLUMNS] (...)` and `PARTITION BY LIST [COLUMNS] (...)` (including `VALUES LESS THAN MAXVALUE`) are supported directly via SQL DDL.
- Unsupported partition forms (partition options such as `ENGINE`/`COMMENT`/`TABLESPACE`, `SUBPARTITION`, `LIST DEFAULT`, expressions in partition key, multi-column `COLUMNS`, non-final `MAXVALUE`) are strictly rejected with clear errors.
- Partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION` — historical context; subsequently implemented on empty sources in ADR-014), partition split/merge/drop, cross-partition row movement on UPDATE, hash tablets, distributed serving, and replica failover remain deferred.

### Test Evidence

- `crates/htap-sql/tests/parse_bind.rs`:
  - `test_mysql_partition_ddl_parsed_and_bound`
  - `test_mysql_partition_ddl_negative_parser_and_binder`
  - `test_negative_create_table`
- `crates/htap-server/tests/local_server.rs`:
  - `test_sql_range_partitioning_ddl_and_maxvalue_routing`
  - `test_sql_list_partitioning_ddl_and_routing`
- `crates/htap-catalog/tests/catalog_recovery.rs`:
  - `test_partitioning_legacy_decode_and_reopen`
  - `test_range_partitioning_routing_and_boundaries`
  - `test_list_partitioning_routing`

### How to reverse it

When an upstream SQL parser AST natively supports MySQL partition DDL, replace the vendored crate with upstream dependency and align AST mapping.

---

## ADR-014: SQL ALTER Partition Lifecycle Support

`Status: Accepted`
`Date: 2026-09-16`

### Context

ADR-013 introduced vendored `sqlparser` with typed MySQL partition DDL for `CREATE TABLE`.
However, partition lifecycle mutations (`ALTER TABLE ... ADD PARTITION`, `DROP PARTITION`, `REORGANIZE PARTITION`) were previously only accessible via programmatic catalog/server APIs (`LocalServer::alter_partitions`).
To enable declarative partition management through standard SQL while preventing data loss and upholding strict structural guarantees, the SQL front-end and server must support typed MySQL ALTER partition statements without ad-hoc regex parsing.

### Decision

1. Extend vendored `sqlparser` AST (`AlterTableOperation`) with typed variants `AddPartition`, `DropPartition`, and `ReorganizePartition`, and extend the MySQL grammar in `parse_alter_table_operation` to parse:
   - `ALTER TABLE t ADD PARTITION (PARTITION p VALUES LESS THAN (literal|MAXVALUE))` or LIST `VALUES IN (literals)`
   - `ALTER TABLE t DROP PARTITION p[, ...]`
   - `ALTER TABLE t REORGANIZE PARTITION p[, ...] INTO (PARTITION ... definitions...)`
2. Enforce strict rejection of `IF EXISTS` / `IF NOT EXISTS`, partition options (`ENGINE`/`COMMENT`/`TABLESPACE`/`DATA DIRECTORY`), subpartitioning, `HASH`/`KEY`, expressions in bounds, multi-column definitions, and unrelated ALTER operations.
3. Bind typed ALTER statements in `htap-sql` into `BoundStatement::AlterPartitions` by validating against catalog metadata and converting into catalog `PartitionAlteration` types (`Add`, `Drop`, `Reorganize`).
4. Route `BoundStatement::AlterPartitions` as `CatalogDdl` across all storage formats.
5. In `LocalServer`, dispatch `BoundStatement::AlterPartitions` through safe `alter_partitions_internal`, preserving empty-source rowstore collapse guards for `Drop` and `Reorganize` and atomic catalog CAS generation updates.

### Consequences

- Standard MySQL ALTER partition commands are supported end-to-end via SQL interfaces (`LocalServer::execute` and `EmbeddedClient::execute`).
- The native `LocalServer::alter_partitions` API remains available alongside SQL ALTER, providing candidate catalog validation, atomic catalog CAS, empty-source safety, and checked ID allocation without ID burn.
- Populated partitions cannot be dropped or reorganized via SQL or native APIs, preventing accidental data loss without explicit data migration.
- Data migration for populated partition reorganization, physical storage reclamation (space of dropped partitions), automatic split/merge, hash partitions, and distributed lifecycle coordination remain deferred.

### Test Evidence

- `vendor/sqlparser/src/parser/mod.rs`:
  - `test_mysql_alter_partition`
- `crates/htap-sql/tests/parse_bind.rs`:
  - `test_mysql_alter_partition_parsed_and_bound`
  - `test_mysql_alter_partition_negative`
- `crates/htap-sql/tests/route.rs`:
  - `test_route_classification` (verifies `AlterPartitions` routes to `CatalogDdl`)
- `crates/htap-server/tests/local_server.rs`:
  - `test_server_sql_alter_partition_lifecycle`
  - `test_server_alter_partitions_drop_empty_and_populated_guard`
  - `test_server_alter_partitions_reorganize_empty_and_populated_guard`
- `crates/htap-catalog/tests/catalog_recovery.rs`:
  - `test_partition_alteration_add_range_and_list`
  - `test_partition_alteration_drop_range_and_list`
  - `test_partition_alteration_reorganize_contiguous`
  - `test_partition_alteration_cas_and_reopen`
  - `test_partition_alteration_negative_rules`
  - `test_partition_alteration_overflow_rejections`

---

## ADR-015: Table-Wide Conversion Reports, Column-to-Row Metadata Demotion, and Synchronous Policy Ticks

`Status: Accepted`
`Date: 2026-09-16`

### Context

Phase 4 introduced partition-scoped row-to-column conversion via `LocalConverter`. However, operating on multi-partition tables required coordinated table-wide operations, safe reversal when needed, explicit execution policy control, and fail-closed validation on startup to detect catalog and disk manifest divergence.

### Options considered

- **(a) Autonomous background conversion daemon:** Spawn a background worker thread that monitors conversion policies and executes conversions continuously. Rejected to maintain strict local determinism, test repeatability, and avoid background concurrency/scheduling complexity in the local MVP.
- **(b) Full physical reverse data transcoding and file deletion on demotion:** Transcode columnar data back to rowstore format and physically delete `.seg` files on demotion. Rejected as redundant and risky: the rowstore was authoritative for all writes throughout, so rowstore data is already complete; physically deleting columnar files introduces unnecessary I/O and potential data-loss bugs.
- **(c) Synchronous explicit policy ticks, metadata-only Column-to-Row demotion, and fail-closed open validation:** Provide deterministic table conversion reports (`TableConversionReport`), metadata-only demotion via catalog CAS (clearing `column_manifest` while keeping rowstore and column files intact), explicit policy-driven `conversion_tick` / `tick` APIs, and fail-closed storage validation on `LocalServer::open`.

### Decision

1. **Table-wide conversion reports:** Implement `TableConversionReport` and `PartitionConversionReport` providing deterministic per-partition action, error, and manifest results. Expose `convert_table_to_column(table_name)` to convert all partitions of a table.
2. **Metadata demotion (`convert_table_to_row` / `demote_partition_to_row`):** Demote columnar partitions back to `StorageDescriptor::Row` via a single atomic catalog CAS that clears the tablet's `column_manifest` reference. Retain all rowstore data (which remained authoritative throughout) and leave existing columnar files on disk without physical deletion or reverse transcoding. Block demotion if a partition has an active in-flight conversion.
3. **Explicit synchronous policy ticks:** Provide `conversion_tick(policy)` and `tick()` to evaluate explicit target policies (`ConversionTarget::Table`, `ConversionTarget::Partition`) and resume in-flight `Converting` jobs (`ConversionPolicy::manual()`). No autonomous background scheduler is introduced; `tick` resumes persisted jobs only.
4. **Fail-closed startup storage validation:** In `LocalServer::open`, inspect all partitions: verify that `Column` and `Converting` partitions have valid matching manifests and segment files in `<root>/colstore`, and that `Row` partitions have no active catalog manifest. Reject inconsistent states with `HtapError::Corruption` or `HtapError::Io` depending on the cause.

### Consequences

- Multi-partition tables can be converted to columnar format or demoted back to row storage deterministically.
- Demotion is fast and non-destructive: rowstore data is already complete, and columnar files remain on disk but unreachable from the catalog.
- Fail-closed validation prevents silent divergence or corruption between catalog metadata and disk state.
- Physical reverse data transcoding, physical storage reclamation (space of demoted column files or purged rows), delete vectors, background compaction, autonomous background scheduling, and distributed conversion remain deferred.

### Test Evidence

- `crates/htap-convert/tests/materialization.rs`:
  - `test_demote_partition_to_row_clearing_manifest_and_retained_data`
  - `test_demote_partition_rejections_active_converting_and_missing_and_corrupt`
  - `test_conversion_tick_resumes_snapshot_pinned`
- `crates/htap-server/tests/local_server.rs`:
  - `test_server_convert_table_multi_partition_reports_and_demotion_equivalence`
  - `test_server_conversion_tick_idempotent_and_resume_snapshot_pinned`
  - `test_server_open_fail_closed_missing_or_corrupt_manifest`

---

## ADR-016: Hand-written synchronous MySQL text protocol server (thread-per-connection, no TLS, native password only, loopback default)

`Status: Accepted`
`Date: 2026-09-16`

### Context

Phase 8 needed a way to reach `LocalServer` from outside the host process, ideally with existing MySQL
client tooling (`mysql` CLI, drivers). The engine's execution model is already synchronous and
self-serializing (`LocalServer::execution_lock`), and the local MVP explicitly excludes sessions,
distributed transport, and TLS elsewhere, so the network layer needed to match that scope rather than
introduce new concurrency or dependency surface.

### Options considered

- **(a) tokio/async server.** Rejected: `LocalServer` is synchronous and already serializes execution under
  a single lock, so an async runtime buys no concurrency the engine can use, while adding a large dependency
  and a second concurrency model (async cancellation, `Send`/`'static` bounds) to reason about for no
  measurable benefit in a thread-per-connection, lock-serialized design.
- **(b) Third-party MySQL protocol crate (server side).** Rejected: available crates target client-side
  connections (driving a real MySQL server), not implementing a server; adopting one would still require
  hand-writing the server half of the protocol while inheriting an external parsing/dependency surface for
  the parts that do exist. A dev-dependency on the `mysql` crate (client side, `minimal-rust` feature) is
  used only for an interop test, never in the shipped server or client.
- **(c) Custom binary RPC protocol.** Rejected: loses compatibility with the `mysql` CLI and every existing
  MySQL driver, which was the whole point of exposing a network endpoint; the engine's own SQL surface is
  MySQL-dialect already (`sqlparser::dialect::MySqlDialect`), so a MySQL-compatible wire protocol is the
  natural fit.
- **(d) Hand-written synchronous MySQL text protocol, thread-per-connection.** One accept thread plus one
  thread per connection over `std::net`, `Arc<LocalServer>` shared across connections, statements serialized
  by the server's own `execution_lock` so the wire layer holds no additional global lock. Handshake v10 with
  `mysql_native_password` only; no TLS; default bind loopback-only; text protocol only (no prepared
  statements/binary protocol); no session state (every statement auto-commits).

### Decision

Option **(d)**. Implemented in `crates/htap-wire` (`WireServer`, `WireServerConfig`) and exposed as a binary
via `crates/htapd`, with `htap-client::RemoteClient` as the Rust-side client.

Two protocol pitfalls were found and fixed by checking behavior against the `mysql` crate v28 (a real
driver) rather than trusting the MySQL manual text alone:

1. **Result-set terminator header.** The MySQL manual describes `CLIENT_DEPRECATE_EOF` as replacing the
   legacy EOF packet with an OK packet, which reads as "header `0x00`". In practice, and as required by the
   `mysql` crate's packet reader, the terminator must keep header `0xFE` even in deprecated-EOF mode (an
   "OK-shaped" packet, not a literal OK packet); only the body layout changes. `build_resultset_terminator`
   always emits `0xFE`, verified by `terminator_header_is_always_0xfe_legacy_and_deprecated` and
   `test_resultset_packets_modern_vs_legacy`.
2. **OK-packet `info` field encoding.** The manual documents `info` as `string<EOF>` (read-to-end-of-packet).
   Real MySQL servers, and the `mysql` crate's parser, actually send and expect it length-encoded
   (`string<lenenc>`). `build_command_ok` writes `info` length-encoded when non-empty, verified by
   `command_ok_header_is_always_0x00_and_never_in_trans` (`ok[7] == 9, "info must be length-encoded"`) and
   exercised end-to-end by `test_mysql_crate_driver_interop`.

### Consequences

- The server works with the real `mysql` CLI and MySQL drivers over the text protocol, verified by a dev-only
  interop test against the `mysql` crate (`test_mysql_crate_driver_interop`); no such dependency ships in the
  server or client binaries.
- Because the wire layer adds no locking of its own, its concurrency ceiling is exactly `LocalServer`'s: one
  statement executing at a time regardless of connection count. This matches the existing single-lock
  execution model and does not regress it.
- Scope is intentionally narrow: no TLS, no prepared statements/binary protocol, no sessions or explicit
  transactions, no multi-statements/multi-results, no compression, and payloads ≥16 MB close the connection.
  Binding a non-loopback address without a tunnel exposes query text and result rows in cleartext.
- The handshake scramble uses a non-cryptographic xorshift RNG (seeded from the clock and a counter), which
  is acceptable for a challenge that is single-use per connection but is not a general-purpose cryptographic
  primitive; a future hardening pass could swap in a CSPRNG without changing the wire format.

### How to reverse it

Replace `WireServer`'s std::net accept/connection loop with an async runtime, or swap the hand-written codec
for a protocol crate, without changing `htap-client::RemoteClient`'s public API, since both are internal to
`htap-wire`.

### Test Evidence

- `crates/htap-wire/src/*.rs` (unit tests): `sha1_empty_string`, `sha1_abc`, `sha1_multi_block`,
  `native_password_scramble_matches_test_vector`, `packet_round_trip`, `oversize_packet_rejected`,
  `lenenc_int_round_trip`, `lenenc_str_and_nul_str_round_trip`, `seq_counter_wraps_and_resets`,
  `read_fully_handles_partial_reads_and_stop_at_boundary_only`, `handshake_v10_round_trip`,
  `handshake_response41_round_trip_with_and_without_db`, `auth_switch_round_trip`,
  `error_map_all_htap_variants`, `err_packet_round_trip`, `shim_set_returns_ok`,
  `shim_version_comment_single_row`, `shim_multi_sysvar_with_aliases`, `shim_use_db`,
  `shim_passthrough_for_normal_sql`, `column_def_round_trip_all_types`,
  `text_row_null_and_bytes_are_raw_not_hex`, `text_row_numeric_and_timestamp_round_trip`,
  `datetime_text_conversions`, `command_ok_header_is_always_0x00_and_never_in_trans`,
  `terminator_header_is_always_0xfe_legacy_and_deprecated`, `config_defaults_are_loopback_only`,
  `command_info_convention`.
- `crates/htap-wire/tests/wire_server.rs` (23 tests): `test_handshake_empty_password_ok`,
  `test_handshake_wrong_password_rejected_1045`, `test_handshake_correct_password_ok`,
  `test_auth_switch_to_native_password`, `test_ssl_request_rejected_and_pre41_rejected`,
  `test_ddl_insert_point_select_round_trip`, `test_analytic_select_round_trip`,
  `test_typed_values_null_bytes_float_timestamp_round_trip`, `test_syntax_error_maps_to_1064`,
  `test_missing_table_maps_to_1146`, `test_too_many_connections_returns_1040`,
  `test_concurrent_connections_dense_versions`, `test_com_ping`,
  `test_com_init_db_known_and_unknown_db`, `test_shim_set_and_version_comment`,
  `test_shutdown_joins_and_frees_port`, `test_unknown_command_returns_1047`,
  `test_prepared_statement_command_rejected_cleanly`, `test_oversized_packet_closes_connection`,
  `test_legacy_eof_terminator_used_when_client_does_not_negotiate_deprecate_eof`,
  `test_resultset_packets_modern_vs_legacy`, `test_dml_versions_are_reported`,
  `test_mysql_crate_driver_interop`.
- `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`.
- `crates/htap-client/src/remote.rs::decode_command_convention`.

---

## ADR-017: General query executor over materialized logical rows with one snapshot per statement; UPDATE as versioned Put; DROP TABLE metadata-only with persisted identifier high-water mark (catalog format v2)

`Status: Accepted`
`Date: 2026-09-16`

### Context

Phases 3-8 deliberately kept SQL to a narrow single-table slice (`PointSelect`/`AnalyticSelect`) so R5 (point
lookups structurally bypass the analytical engine) could be enforced by construction rather than by a
runtime cost heuristic. Phase 9 needed to add the SQL breadth users actually need on an HTAP engine — joins
across storage engines, expressions, subqueries, set operations, `UPDATE`, `DROP TABLE`, `SHOW` — without
weakening that guarantee, without a new query planner/optimizer investment out of scope for a local MVP, and
without regressing durability invariants (one MVCC version domain, rowstore-authoritative writes, atomic
catalog publication; see ADR-004/008/009).

### Options considered

**Execution model for joins/general queries:**
- **(a) Vectorized / pipelined execution engine (Arrow-style operators over `RecordBatch`).** Rejected for
  this phase: `htap-colstore` already has a vectorized scan primitive (`SegmentReader::scan`), but building a
  vectorized join/aggregate/sort operator pipeline on top of it is a multi-week investment disproportionate
  to a local MVP, and none of the existing SQL paths (including the narrow `AnalyticSelect` path) are
  vectorized above the scan primitive either — introducing it only for joins would create two execution
  models to maintain.
- **(b) Unify the narrow `AnalyticSelect` path into the general executor (single code path for all SELECTs).**
  Rejected: it would remove the purely structural separation R5 depends on — a single binder/executor for
  every `SELECT` shape makes "point lookups never touch analytical code" a runtime property (which shape did
  this statement take?) instead of a compile-time one (this route variant never calls that function). Keeping
  two binders and two routes, gated by a syntactic pre-check, preserves the stronger guarantee.
- **(c) General query executor over materialized logical rows (`Vec<Row>` in memory), reusing the existing
  per-partition storage path (`scan_partition_compact`) for every base table side, hash-joining and
  evaluating expressions/aggregates/ordering in memory, gated in front of the narrow binders by a purely
  syntactic shape test.** Chosen.

**UPDATE implementation:**
- **(d) Delete-then-insert (represent UPDATE as a `Mutation::Delete` plus a `Mutation::Put`).** Rejected: it
  would create a visible gap in the MVCC version chain — a reader whose snapshot lands between the two
  mutations would see the row disappear and reappear, which correctness-sensitive callers should never
  observe for a value that only changed, not disappeared. It also complicates the "no resurrection" tombstone
  invariant the rowstore already guarantees for real deletes.
- **(e) A new `Mutation::Update` variant with in-place value patching in the rowstore.** Rejected: it would
  touch the rowstore's core mutation representation and WAL format for a single SQL-layer feature, when the
  same effect is already expressible as a versioned `Put` under the existing key — no new on-disk format or
  WAL record type needed.
- **(f) A single new-version `Mutation::Put` under the existing key, computed by reading the current row at
  the statement's snapshot and applying assignments in the SQL layer.** Chosen: reuses the existing MVCC
  version chain, WAL record, and 2PC path unchanged; a reader's snapshot sees either the old value or the new
  value, never a gap.
- **(g) Chunk a large scan-form `UPDATE` across multiple transactions to avoid the 2PC payload cap.**
  Rejected for this phase: chunking would mean a scan-form `UPDATE` is no longer atomic (a crash mid-chunk
  leaves some rows updated and others not), which is a correctness regression relative to today's INSERT/
  DELETE, which always commit as one transaction. Keeping `UPDATE` as one transaction bounded by the existing
  16 MiB payload cap (documented as a limitation, not silently chunked) preserves atomicity; a future streaming/
  chunked-with-explicit-multi-statement-semantics design is deferred.

**DROP TABLE and identifier reuse:**
- **(h) Physical reclamation on DROP TABLE (delete rowstore SSTs/WAL entries and columnar segment files for
  the dropped tablets).** Rejected for this phase: physical reclamation of rowstore history and columnar
  segments is already deferred project-wide (compaction, demotion file cleanup — see ADR-015); scoping it
  narrowly to DROP TABLE would be inconsistent with that existing deferral and adds nontrivial risk (deleting
  live files referenced by a catalog CAS that could still be rolled back by a concurrent reader of the old
  snapshot) for a local MVP.
- **(i) Metadata-only DROP TABLE (single catalog CAS removing table/partition/tablet/replica records) with no
  identifier reuse guarantee.** Rejected alone: without a reuse guarantee, a new table created after a drop
  could be assigned a recycled tablet id whose rowstore/columnar files still exist on disk from the dropped
  table, aliasing unrelated historical data into the new table's storage paths.
- **(j) Metadata-only DROP TABLE plus a persisted identifier high-water mark (`IdHighWater`) so every
  allocator draws from `max(persisted, live max) + 1`, closing the reuse hazard without doing physical I/O.**
  Chosen.
- **(k) Stay at catalog format version 1 and add the high-water counters as additive/optional fields without
  bumping the version.** Rejected: CLAUDE.md's durability rule is explicit — a layout change bumps the format
  version and keeps or explicitly rejects old versions with a recovery test; treating a new persisted field
  as "free" because `serde(default)` makes old files parse would hide the compatibility decision (should a
  reader silently default the counters, and is that safe?) instead of recording it. Bumping to version 2,
  defining the version-1 fallback explicitly (fall back to the live maximum id), and adding a recovery test
  makes the compatibility contract visible and testable.

### Decision

1. **General query executor (`htap-sql::{query, expr, binder_query}`, `htap-server::query_exec`).** Every
   statement shape the narrow binders don't handle binds to `BoundStatement::Query(BoundQuery)` and routes to
   the new `Route::Query`. Execution materializes every base table side of a join through the *same*
   `scan_partition_compact` storage path the narrow `Route::OlapScan` executor already uses, at **one** MVCC
   `Snapshot` per statement, then evaluates joins (hash join on equi-conjuncts, nested loop for residual `ON`
   predicates), `WHERE`, `GROUP BY`/aggregation, projection, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT`/
   `OFFSET`, and `UNION` as sequential in-memory stages over `Vec<Row>`. Per-slot partition pruning and
   single-leaf predicate pushdown are derived exactly as on the narrow path, except on the null-supplying
   side of an outer join, where a would-be-pushed conjunct is kept as a residual filter instead.
2. **R5 preserved structurally by a syntactic pre-check.** `bind_select` calls `is_narrow_select_shape`
   (`crates/htap-sql/src/binder.rs`) — a check over the raw AST, with no catalog lookups or type checking —
   *before* any deep binding. A statement matching the narrow shape (one unaliased table, no joins/CTEs/
   subqueries/set operations, no `LIMIT`/`HAVING`/`DISTINCT`, a plain-column or single-aggregate projection,
   an AND-only filter of `column op literal`/`IS [NOT] NULL` leaves, plain unqualified `GROUP BY`/`ORDER BY`)
   still binds through the strict `PointSelect`/`AnalyticSelect` binders unchanged; everything else binds
   through the general query binder. A narrow-shaped statement that fails deep binding (e.g. an unknown
   column) reports that binder's error rather than silently falling through to the general path, so the
   error surface for the narrow shapes is unchanged too.
3. **`UPDATE` as a single new-version `Mutation::Put` under the existing key.** Both the point form
   (complete-PK `WHERE`) and the scan form (general `WHERE` or none) read the current row(s) at the
   statement's snapshot, apply assignments left to right in the SQL layer, and commit the result as
   `Mutation::Put`(s) through the existing `TransactionManager`/`RowstoreParticipant` 2PC path — no new
   mutation kind, WAL record, or on-disk format. The scan form commits all rewritten rows in **one**
   transaction, bounded by the existing 16 MiB 2PC payload cap with no chunking.
4. **`DROP TABLE` is metadata-only, with a persisted identifier high-water mark closing the reuse hazard.**
   `CatalogSnapshot.id_high_water: IdHighWater { table, partition, tablet, replica }`
   (`crates/htap-catalog/src/model.rs`) is persisted, and every allocator (`CREATE TABLE`, `ALTER TABLE ADD/
   REORGANIZE PARTITION`) draws the next id from `max(persisted, live max) + 1`. The catalog envelope
   (`HTAPCAT1`) `FORMAT_VERSION` bumps 1 -> 2 (`crates/htap-catalog/src/local.rs`); a version-1 catalog still
   decodes (`LEGACY_FORMAT_VERSION = 1`), with counters defaulting to zero and `id_high_water()` falling back
   to the live maximum id present in the snapshot, and is rewritten as version 2 on the next CAS. A
   version-1-only binary refuses a version-2 (or any other unrecognized) catalog rather than misinterpreting
   it; conversely, a version-2 payload that omits the `id_high_water` key is rejected as corruption, so a
   version-2 file always carries the field explicitly. Physical reclamation of the dropped table's
   rowstore/columnar data remains deferred, consistent with the rest of the project's deferred-reclamation
   scope (see ADR-015).
5. **Closing three follow-on gaps found by storage review before this ADR's diff checkpoint, all fixed and
   covered by tests before merge:**
   - **Legacy tablet directories are invisible to the live-max fallback.** A version-1 catalog could have
     removed empty partitions via `ALTER TABLE ... DROP PARTITION` (a Phase 3 feature, pre-dating this ADR),
     whose `colstore/tablet-*` directories can remain on disk with no partition left in the catalog
     referencing them — the live in-catalog maximum tablet id cannot see them, so the version-1 fallback
     described in decision 4 was not enough on its own to prevent a later allocation from reissuing one of
     those ids. `LocalServer::open` now calls `migrate_legacy_id_high_water`
     (`crates/htap-server/src/lib.rs`) after storage validation, while still holding the root `ProcessLock`:
     if the persisted mark is exactly all zeros (a genuine unmigrated version-1 catalog), it raises the
     tablet counter to the highest `colstore/tablet-N`/`tablet_N` directory found on disk, merges it with the
     live maximum, and persists the result via one catalog CAS (a one-time write, bumping the generation and
     rewriting the file as version 2, on first open of a legacy root). Partition ids do not need this
     treatment, because rowstore data of a partition dropped under version 1 is always logically empty
     (`DROP PARTITION` requires it). This migration seeds only the tablet counter, not replica ids removed
     along with those tablets — see the remaining gap noted under decision 4's cross-reference in
     `docs/LIMITATIONS.md` and `docs/ARCHITECTURE.md`; it was scoped out as narrow (requires reusing both a
     replica id and a job id) rather than fixed in this ADR.
   - **Nothing prevented a future code path from regressing the mark.** A successor snapshot built with
     `CatalogSnapshot::new` (bypassing the high-water-aware constructors) could omit or lower
     `id_high_water`, silently reopening the reuse hazard on the next CAS. `LocalCatalogStore::compare_and_set`
     (`crates/htap-catalog/src/local.rs`) now rejects, with `HtapError::InvalidArgument` and the file left
     untouched, any successor whose effective `id_high_water()` is lower, component-wise, than the current
     file's.
   - **Replica ids were still allocated from the live maximum, not the persisted mark.** A `ReplicaId` names
     a movement snapshot package directory on disk (see "Sharding and placement" in
     `docs/ARCHITECTURE.md`), so it has the same reuse hazard as a tablet id. `plan_placement`
     (`crates/htap-coord/src/placement.rs`) now allocates new replica ids from
     `snapshot.id_high_water().replica + 1`, and `stage_placement_addition` raises the persisted mark in the
     snapshot it stages, so a removed-and-recreated replica id is never reissued either.

### Consequences

- Joins, expressions, aggregates, subqueries, and set operations work across `Row`, `Column`, and
  `Converting` storage formats in one query, at one consistent snapshot, without weakening R5: the point-
  lookup and narrow-scan code paths are exactly as isolated as before, verified by a pin test that checks
  near-miss shapes (PK predicate plus `LIMIT`/alias/`OR`/join) fail the gate and route through `Route::Query`
  instead of silently keeping — or silently losing — the extra clause.
- `UPDATE`'s correctness rides entirely on rowstore MVCC semantics that already exist; no new durability
  surface was introduced, but `UPDATE`'s two-step read-then-write shape is the first SQL path to make visible
  a pre-existing gap: `LocalServerDataMover`'s methods (`import`, `repair_tablet`) do not take
  `execution_lock`, so a caller sharing one `LocalServer` across threads can race an `UPDATE` against a
  concurrent import/repair on the same table. This is documented as a known limitation, not fixed in this
  phase.
- `DROP TABLE` is fast (one catalog CAS) and safe against identifier aliasing, but does not reclaim disk
  space; operators must account for that when sizing storage (see `docs/OPERATIONS.md`).
- The general query executor has no cost-based planning, no spilling, and no worker-pool parallelism above
  the per-slot scan (each slot's own partition scan still uses the narrow path's scan workers internally);
  it is sized for correctness and breadth on a local MVP, not for large analytical workloads.
- Catalog readers/writers must handle two format versions going forward; the fail-loud version-2-refusal
  behavior in a version-1-only binary is a deliberate compatibility boundary, not a bug, per the format-
  version rule in `CLAUDE.md` (bump + explicit old-version handling + recovery test).

### How to reverse it

The general query executor is additive: removing `Route::Query`/`query_exec` and reverting `bind_select` to
always use the narrow binders would restore the Phase 3-8 SQL surface without touching the rowstore, colstore,
or catalog formats (the catalog format bump is not reversible without another format-version bump, since
`IdHighWater` is now persisted). `UPDATE` and `DROP TABLE` could be removed independently of the general query
executor, since `UPDATE`'s scan form is the only part that depends on `query_exec` (via `scan_base_table`).

### Test Evidence

- `crates/htap-sql/src/expr.rs` unit tests: `three_valued_logic_tables`, `numeric_promotion_and_overflow`,
  `like_in_between_case_cast`, `scalar_functions`, `subquery_and_aggregate_context`, `expr_type_inference`.
- `crates/htap-sql/tests/query_bind.rs`: `test_join_binding_kinds_aliases_and_wildcards`,
  `test_join_binding_errors`, `test_expressions_functions_and_type_checks`,
  `test_aggregates_group_by_having_and_grouping_rules`, `test_order_by_limit_distinct`,
  `test_subqueries_ctes_derived_tables_and_union`, `test_update_drop_show_binding`,
  `test_bound_predicate_evaluation_with_joined_rows`.
- `crates/htap-sql/tests/route.rs`: `test_route_classification`,
  `test_point_read_fast_path_pinned_against_general_query_path` (R5 pin test).
- `crates/htap-sql/tests/parse_bind.rs`: `test_negative_select_and_delete`,
  `test_bind_analytic_select_negative`.
- `crates/htap-server/tests/query_exec.rs`: `test_joins_across_row_column_and_converting_tables`,
  `test_outer_joins_null_padding_residual_on_and_null_keys`,
  `test_expressions_aggregates_having_order_limit_distinct`,
  `test_union_derived_tables_ctes_and_subqueries`, `test_partition_pruning_and_pushdown_through_general_path`,
  `test_single_snapshot_across_engines_and_freshness`, `test_general_query_over_reopened_server`,
  `test_update_by_primary_key_and_reopen_recovery`,
  `test_update_by_filter_across_partitions_and_storage_formats_with_reopen`,
  `test_update_by_primary_key_on_column_and_converting_partitions` (point `UPDATE` against a `Column`
  partition plus reopen, and against a partition mid-conversion, verifying the delta survives a subsequent
  `conversion_tick`), `test_show_tables_databases_columns_and_describe`,
  `test_drop_table_reopen_and_no_id_reuse`,
  `test_legacy_catalog_seeds_tablet_high_water_from_colstore_inventory` (decision 5, legacy migration).
- `crates/htap-server/tests/local_server.rs`: `test_analytic_unsupported_clauses`,
  `test_storage_descriptors_dml_and_point_reads_and_unsupported_non_point`.
- `crates/htap-catalog/tests/catalog_recovery.rs`:
  `test_catalog_v1_envelope_decodes_and_counters_fall_back_to_live_max` (format-version fallback),
  `test_catalog_id_high_water_prevents_reuse_after_removal` (no-reuse guarantee),
  `test_catalog_cas_rejects_regressing_id_high_water` (decision 5, CAS regression guard),
  `test_catalog_v2_payload_without_id_high_water_is_rejected` (decision 4, fail-loud missing field),
  `test_corruption_and_truncation` (updated to use format version 3 as the unsupported/future version,
  proving the fail-loud refusal path).
- `crates/htap-coord/tests/placement_movement.rs::test_plan_placement_allocates_above_id_high_water`
  (decision 5, replica id allocation from the persisted mark).
- `crates/htap-client/tests/embedded_client.rs::test_embedded_client_unsupported_sql_preserves_error_categories`,
  `crates/htap-client/tests/remote_client.rs::test_remote_client_matches_embedded_client_ddl_dml_select`.
- `crates/htap-wire/tests/wire_server.rs::test_general_sql_over_wire` (`LEFT JOIN` + `GROUP BY` + `LIMIT`,
  `UPDATE`, `SHOW TABLES`, `DESCRIBE`, `DROP TABLE` reachable unchanged over the MySQL wire protocol, since
  `htap-wire` passes every non-shim statement through to `LocalServer::execute` — see ADR-016).

Cross-references: ADR-004 (unified MVCC version domain, rowstore-authoritative writes — preserved: `UPDATE`
still commits exclusively through `RowstoreParticipant`), ADR-008 (partition-scoped conversion state machine
— unaffected: the general executor reads converted partitions through the same compact-read path, never
mutates conversion state), ADR-009 (durable synchronous local coordinator — unaffected: `DROP TABLE` and
`UPDATE` go through the same unfenced `CatalogStore::compare_and_set` / `TransactionManager` paths as other
non-coordinator-mediated local operations), and ADR-001 (structural R5 separation, extended rather than
weakened by the syntactic shape gate in decision 2 above).
