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
- Specifically, MySQL partition DDL (`PARTITION BY RANGE`, `PARTITION BY LIST`)
  is not retained in AST by `sqlparser 0.62` under `MySqlDialect` and is rejected
  at parse time with `HtapError::InvalidArgument`, avoiding lossy reinterpretation (see ADR-011).

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

`Status: Accepted`
`Date: 2026-09-11`

### Context

The database catalog (`htap-catalog`) features a validated partition model with finite typed range and list partitioning (`PartitioningDescriptor`, `PartitioningMethod::Range`, `PartitioningMethod::List`, `RangeBound`, `TableDescriptor::route_partition_value`, `CatalogSnapshot::route_partition_value`), utilized in controlled and admin test fixtures.

However, the SQL layer uses `sqlparser 0.62` with `MySqlDialect`. MySQL partition DDL syntax (`CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...`) is not retained in the AST by `sqlparser 0.62`, which only models generic dialect-specific partition expressions (such as BigQuery or ClickHouse `CreateTable.partition_by`). Attempting to support MySQL partition DDL via ad-hoc string parsing or lossy mapping of unrelated AST fields would compromise architectural boundaries and data integrity.

### Options considered

- **(a)** Implement ad-hoc regex or string pre-parsing to extract MySQL partition clauses before invoking `sqlparser`.
- **(b)** Coerce unrelated `CreateTable.partition_by` expressions through lossy reinterpretation into internal partition descriptors.
- **(c)** Enforce a strict parser and binder boundary: verify that MySQL partition DDL is rejected with `HtapError::InvalidArgument` from `parse_one` under `sqlparser 0.62` / `MySqlDialect`, and ensure manually constructed AST partition fields are rejected by the binder (`HtapError::Unsupported`). Preserve the internal catalog's validated finite range/list metadata for controlled/admin fixtures, keep SQL-created `LocalServer` tables unpartitioned (default single partition), and defer multi-partition execution through SQL.

### Decision

Option **(c)**.

1. **Strict parser-level rejection:** MySQL `CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...` syntax fails during `parse_one` under `sqlparser 0.62` with `MySqlDialect`, returning `HtapError::InvalidArgument`.
2. **Strict binder rejection & no lossy AST reinterpretation:** If an AST with `partition_by` or other partitioning clauses is constructed manually, `htap-sql::bind` rejects it with `HtapError::Unsupported`. No lossy reinterpretation of unrelated `CreateTable.partition_by` AST is made.
3. **Preservation of catalog metadata:** The catalog maintains its validated finite range/list descriptors and routing helpers for controlled/admin fixtures.
4. **SQL-created tables remain single-partition:** Tables created via SQL DDL in `LocalServer` remain unpartitioned with a default single partition; multi-partition execution is not yet exposed through SQL.
5. **Deferred capabilities:** Hash buckets / tablet sharding, partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION`), and distributed multi-partition execution remain deferred.
6. **Future upgrade constraints:** If a future parser upgrade or custom AST is pursued, finite typed range and list partition definitions must be explicitly mapped to the catalog model; `MAXVALUE` and partition options remain unsupported until the catalog model itself changes.

### Consequences

- Production Rust logic remains clean without ad-hoc regex parsers or lossy AST hacks.
- The boundary between SQL DDL capabilities and internal catalog capabilities is explicitly documented and tested.
- Future parser integrations have a clear specification for mapping finite typed range/list definitions.

### How to reverse it

Upgrade `sqlparser-rs` to a version that retains MySQL partition definitions, or adopt a custom parser AST that explicitly extracts finite range and list partitions and maps them to the catalog model.
