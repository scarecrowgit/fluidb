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

- Future demo can expose unified frontend/backend roles. Note: `htapd` daemon binary, network listeners, and Docker/Compose deployments are deferred future work; current MVP provides in-process `LocalServer` and `EmbeddedClient`.
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
  Reverse `Column -> Row` conversion is not implemented and not claimed. Columnar bitmap delete vectors,
  physical rowstore reclamation, delta-to-base background compaction, vectorized aggregation, vectorized
  operator pipelines, joins/CTEs/windows, and distributed multi-tablet conversion are explicitly deferred.

### Consequences

- Zero downtime or blocking for online point reads and writes throughout conversion.
- Crash-safe and resumable: any crash during conversion resumes from the persisted phase and
  pinned snapshot without duplicate manifest generation or orphaned segment leaks.
- Storage footprint temporarily retains rowstore data post-conversion because physical rowstore
  reclamation is deferred.
- Reverse conversion is unsupported and returns `HtapError::Unsupported`.

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
   - Created root `README.md`, `docs/BENCHMARKS.md`, and `docs/OPERATIONS.md` documenting filesystem layouts (`catalog`, `rowstore`, `txn.journal`, `movement`, `COORDINATOR`), recovery boundaries, and explicit non-features (no daemon, no MySQL wire protocol, no network sockets, no Docker/Compose, no TPC-C/TPC-H compliance).

### Consequences

- All benchmark targets and client tests compile cleanly and pass verification locally.
- Explicit non-features prevent scope creep and false claims of production DBMS completeness.
- Clean separation between storage/server crates, benchmark harness, and embedded client.

### How to reverse it

Extend the benchmark harness into multi-process client/server benchmarks when network transports and analytical SQL engines are implemented.

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
2. **Strict binder rejection & no lossy AST reinterpretation:** Generic/unrelated `partition_by` AST clauses continue to be rejected by `htap-sql::bind` with `HtapError::Unsupported`. Unsupported partition options, subpartitioning, expressions, multi-column COLUMNS, and partition lifecycle DDL remain strictly rejected.
3. **Preservation of catalog metadata:** The catalog maintains its validated finite range/list descriptors and routing helpers.
4. **SQL-created tables:** Unpartitioned SQL DDL creates a default single partition `p0`; partitioned SQL DDL creates validated range/list topologies as defined in ADR-013.
5. **Deferred capabilities:** Hash buckets / tablet sharding, partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION`), cross-partition UPDATE row movement, and distributed multi-partition execution remain deferred.

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
   - `LocalServer::convert_table` explicitly verifies that the target table has exactly one partition and rejects multi-partition tables with `HtapError::Unsupported`.
8. **Deferred capabilities:**
   - Partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION`), partition split/merge/drop, multi-partition conversion and movement, hash tablets, distributed/remote partition serving across network nodes, replica failover, and network wire protocol remain deferred.

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
- Partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION`), partition split/merge/drop, cross-partition row movement on UPDATE, hash tablets, distributed serving, and replica failover remain deferred.

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
- Populated partitions cannot be dropped or reorganized via SQL, preventing accidental data loss without explicit data migration.
- Data migration for populated partition reorganization, automatic split/merge, hash partitions, and distributed lifecycle coordination remain deferred.

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
