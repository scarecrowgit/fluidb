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
the analytical evaluator and converter, remaining separate and unchanged. Within this
narrow `AnalyticSelect`/`Route::OlapScan` path itself, compound `AND` pushdown beyond one
leaf, `!=` pushdown, joins, CTEs, and windows remain deferred — those are instead handled,
system-wide, by the separate general query executor `htap-sql::{query, expr, binder_query}`/
`htap-server::query_exec` (`Route::Query`, ADR-017, extended by ADR-022), which added joins/
CTEs/expressions/`ORDER BY`/`LIMIT`/`HAVING`/`OR`/`AVG`/`DISTINCT` (Phase 9) and window
functions/correlated subqueries/`FULL OUTER`/`NATURAL`/`USING` joins/recursive CTEs (Phase
13) without touching this ADR's narrow-path/DataFusion proposal. Multi-tablet/distributed
scans, quotas, spill, cancellation, cost-based optimization, and DataFusion/Arrow analytical
integration remain deferred future work on every path.

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
  together; a selectable single-role mode and Docker/Compose packaging remain deferred future work. Update
  (Phase 21, ADR-035): Docker/Compose packaging is now `implemented (local MVP)` (a narrow local slice); the selectable
  single-role mode stays deferred. The
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
  truth for point operations (`Route::RowstoreWrite` for `INSERT`, `Route::RowstoreDelete` for
  `DELETE` since Phase 13, and `Route::RowstorePointRead` for point reads), executing
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
  reverse transcoding is not implemented. This narrow `Route::OlapScan` analytical-scan path itself does
  not support joins/CTEs/windows (those are handled system-wide by the separate general query executor,
  `Route::Query` — see ADR-017/ADR-022). Columnar bitmap delete vectors, delta-to-base background compaction
  (folding rowstore deltas into new columnar segments), autonomous background conversion scheduling,
  vectorized aggregation, vectorized operator pipelines, and distributed multi-tablet conversion are
  explicitly deferred on every path (the rowstore's own generic LSM compaction and `DROP TABLE` artifact
  reclamation are implemented as of Phase 15 — see ADR-024 — but do not touch the columnar side).

### Consequences

- Zero downtime or blocking for online point reads and writes throughout conversion.
- Crash-safe and resumable: any crash during conversion resumes from the persisted phase and
  pinned snapshot without duplicate manifest generation or orphaned segment leaks.
- Storage footprint temporarily retains rowstore data post-conversion because delta-to-base background
  compaction (folding accumulated rowstore deltas forward into new columnar segments) is deferred; as of
  Phase 15 (ADR-024), the rowstore's own generic LSM compaction does collapse superseded MVCC versions and
  reclaim disk space in the rowstore, including for converted tables' historical row versions, but it never
  folds deltas into the columnar base or advances a conversion's own snapshot version.
- Physical reverse transcoding is unsupported (metadata demotion back to Row is supported via ADR-015).

### Decimal column type in the columnar segment (`HTAPCOL1` v1 -> v2, Phase 17 / A6b task 2)

Phase 17 adds `DataType::Decimal { precision, scale }` as an eighth column type end to end, including
persistence. This is recorded here, as a consequence of this ADR rather than as a new ADR, per the storage
review that gated it (see `docs/PROGRESS.md`'s Phase 17 row and CLAUDE.md's format-bump rule): the change is
additive to the columnar segment's existing byte layout and does not introduce a new conversion-state-machine
decision, so it does not warrant its own ADR number the way ADR-017/021/023/024's format bumps did (those each
bundled a genuinely new design, not a mechanical type addition).

1. **Version bump, floor, and readable range.** `htap-colstore/src/segment.rs`'s `FORMAT_VERSION` goes 1 -> 2.
   `MIN_DECODABLE_VERSION` stays `1` (the legacy floor), so `SegmentReader::open` accepts the inclusive range
   `MIN_DECODABLE_VERSION..=FORMAT_VERSION` (`1..=2`), widened from the previous strict-equality check.
2. **Version 1 stays readable — verified by inspection, not by test.** Decimal reuses the identical 8-byte
   little-endian layout already used for `Int64`/`Timestamp` blocks (an unscaled `i64`, precision and scale
   supplied by the footer's schema, never stored per value or per block); no existing column type's plain
   encoding, dictionary encoding, zone-map layout, block framing, or footer shape changed. **This claim is
   verified by inspection of `encoding.rs`/`segment.rs`'s encode/decode paths, not proven by a test**, because
   no genuine version-1 segment fixture is checked into this repository — the closest available test,
   `segment::tests::legacy_format_still_readable`, hand-patches a version-2-written, non-decimal segment's
   footer `format_version` field down to `1` (recomputing the footer checksum) and asserts it still opens and
   scans correctly; it demonstrates that a footer tagged `1` still decodes, not that every possible historical
   version-1 byte layout is unchanged, which is why the underlying claim about byte layout is stated as an
   inspection result here rather than attributed to that test.
3. **A legacy-tagged segment declaring a decimal column is rejected, keyed to the decimal-introduction
   version, not the legacy floor.** A version-1 segment can never genuinely contain a decimal column, because
   the pre-Phase-17 writer had no decimal branch at all — but nothing in a reader that merely widens the
   version check to a range would otherwise stop a hand-crafted or corrupted file from pairing an old version
   number with a schema that declares one, since a decimal block's bytes are indistinguishable in shape from
   an existing 64-bit type's. `SegmentReader::open` therefore checks, immediately after decoding the footer's
   schema: if `format_version < DECIMAL_INTRODUCTION_VERSION` (`DECIMAL_INTRODUCTION_VERSION = 2`, deliberately
   the version decimal support was introduced in, not `MIN_DECODABLE_VERSION`, so the guard still applies
   correctly even if `MIN_DECODABLE_VERSION` is ever raised past 1 later) and the schema contains any decimal
   column, it fails with `HtapError::Corruption` rather than decoding numerically plausible but unversioned
   data. Covered by `segment::tests::legacy_version_with_decimal_rejected`.
4. **Why every other durable envelope stays unbumped, per envelope.** `HTAPCAT1` (catalog), `HTAPSST1`
   (rowstore SST), `HTAPMNF1` (movement tablet manifest), the bare WAL/journal frames, and the rest of the
   whole-file envelopes in the compatibility table above are untouched by this phase's byte output for any
   existing row: each already carries its payload as a generic `serde`-derived structure (JSON for the
   envelopes above; the columnar segment's own footer schema is also JSON) rather than a positional binary
   layout, and `Value`/`DataType`'s decimal variant was already added to those shared enums back in the
   query-layer half of this phase with no reaction needed then, since nothing could construct one on a
   persistence path yet. The safety property this project's format-version checks exist to guarantee — an old
   binary must refuse to decode bytes it does not understand, rather than silently misinterpreting them (see
   this document's account of the catalog's v1->v2 bump above) — already holds for these envelopes without a
   version bump, through a different, equally fail-loud mechanism: `serde`'s own strict, closed-set enum-tag
   matching. An old binary's `Value`/`DataType` enum simply has no `Decimal` variant to match, so deserializing
   a payload that contains one returns a clean deserialization error (`HtapError::Corruption` after this
   project's envelope decode wrapping), never a silently wrong value, a panic, or a misread as some other
   variant. `htap-catalog/tests/catalog_recovery.rs::test_catalog_invalid_decimal_type_tag_is_corruption`
   exercises exactly this failure mode directly (an unrecognized type tag in a decimal variant's position is
   rejected as `HtapError::Corruption`, not misread). **This depends on those enums keeping their current,
   plain derived `serde` serialization** — no `#[serde(untagged)]`, no catch-all "other variant" arm that
   would swallow an unrecognized tag instead of failing, and no numeric discriminant encoding that could
   silently alias one variant's tag onto another's. Any future change to how `Value`/`DataType`/the catalog's
   other tagged enums serialize must re-examine this argument before relying on "no bump needed" again for the
   next additive variant.

Storage-review sign-off (Phase 17, per the plan's binding validator edit requiring either an `architect`
tie-break or an explicit reviewer sign-off on this exact claim — the `architect` route was exhausted after
three failed attempts across two failure modes): the reviewer confirmed reusing the existing 8-byte layout
with precision/scale held only in the schema is sound because the footer and its block data are written
atomically in the same file, in the same write call — a segment's own footer is always the ground truth for
its own blocks, so schema/data disagreement cannot arise from any code path that writes a segment, only from
a hand-edited or corrupted file, which item 3's guard and the footer's own CRC both already cover; that
bumping only `HTAPCOL1` while leaving the `serde_json`-based envelopes unbumped is defensible under CLAUDE.md's
"a layout change bumps the format version" rule specifically because those envelopes' *byte output for
existing data* is unchanged and their fail-loud behavior for new data is mechanically different but
equally reliable (item 4); that no new ADR is warranted, for the reasons given above; and that the residual
risk is confined to a hand-crafted or corrupted file, already guarded against by item 3's version-keyed check
and the footer checksum.

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
   - Created root `README.md`, `docs/BENCHMARKS.md`, and `docs/OPERATIONS.md` documenting filesystem layouts (`catalog`, `rowstore`, `txn.journal`, `movement`, `COORDINATOR`), recovery boundaries, and explicit non-features at the time (no daemon, no MySQL wire protocol, no network sockets, no Docker/Compose, no TPC-C/TPC-H compliance). Update (Phase 8, ADR-016): the daemon, MySQL wire protocol, and network sockets have since been implemented (`htapd`, `htap-wire`); Docker/Compose and TPC-C/TPC-H compliance remain non-features. Update (Phase 21, ADR-035): a Docker image and Compose deployment have since been implemented as a narrow local slice (`Dockerfile`, `docker-compose.yml`); TPC-C/TPC-H compliance remains a non-feature.

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
introduce new concurrency or dependency surface. (Updated by ADR-018: Phase 10 added a real
`htap_server::Session` per connection on top of this same thread-per-connection design; the "no session
state" framing below describes this ADR's scope at the time it was made, not the current wire contract.
Updated by ADR-019: Phase 11 added the binary protocol/prepared statements, `COM_RESET_CONNECTION`/
`COM_CHANGE_USER`, ≥16 MiB message reassembly with a real `max_allowed_packet`, a CSPRNG handshake scramble,
negotiated `CLIENT_MULTI_STATEMENTS`, and shutdown force-close on top of the same thread-per-connection
framing decided here; the "text protocol only"/"no prepared statements" and "non-cryptographic scramble RNG"
framing below describes this ADR's scope at the time it was made, not the current wire contract. Updated by
ADR-020: Phase 12 added TLS (rustls 0.23, `ring` provider, `htap_wire::tls::Conn`) and MySQL compressed-packet
framing (zlib/zstd, `htap_wire::compression::CompressedStream`) layered under `ServerStream` on top of the
same thread-per-connection design, and by ADR-021: Phase 12 replaced the single shared password with
catalog-backed per-user accounts and privileges; the "no TLS"/"no compression"/"one implicit user, single
shared credential" framing below describes this ADR's scope at the time it was made, not the current wire
contract.)

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
- Scope is intentionally narrow: no TLS, no prepared statements/binary protocol, no multi-statements/
  multi-results, no compression, and payloads ≥16 MB close the connection. Binding a non-loopback address
  without a tunnel exposes query text and result rows in cleartext. (Sessions and explicit transactions were
  added on top of this design in Phase 10/ADR-018, without changing the protocol framing decided here.)
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
  2PC payload cap (documented as a limitation, not silently chunked — see ADR-018's second fix pass for the
  actual ~4 MiB effective bound) preserves atomicity; a future streaming/
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
   transaction, bounded by the existing 2PC payload cap with no chunking (nominally `MAX_PAYLOAD_SIZE`, 16
   MiB; ADR-018's second fix pass found the journal `Intent` frame's own encoding makes the actual effective
   bound about 4 MiB — see ADR-018 and `docs/LIMITATIONS.md`).
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
  `test_corruption_and_truncation` (proving the fail-loud refusal path; as of ADR-021's Phase 12 catalog
  format v3 bump, this test uses format version 4 as the unsupported/future version — it used version 3 for
  that role at the time this ADR was written, before version 3 became the current format).
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

**Superseded in part by ADR-024 (Phase 15).** This ADR's "DROP TABLE metadata-only" title and its decision
text describing `DROP TABLE` as a single catalog CAS with no physical reclamation are historically accurate
for Phase 9 but no longer describe the current contract: as of Phase 15, that same CAS also marks the dropped
tablets `pending_reclaim`, and `LocalServer::reclaim_tick`/`compaction_tick` physically reclaim their
rowstore, columnar, and movement artifacts over as many maintenance calls as it takes. The catalog format
bump this ADR introduced (v1 -> v2, `id_high_water`) and the identifier-reuse guarantee it establishes are
unchanged and remain load-bearing: reclamation is eventual, not instant, so the reuse guarantee still matters
for the window between `DROP TABLE` and full reclaim. See ADR-024 for the compaction/reclaim design.

---

## ADR-018: Session-buffered uncommitted writes for explicit transactions; prepare-time conflict detection; manager-wide recovery latch

`Status: Accepted`
`Date: 2026-09-17`

### Context

The local MVP had no session concept: every statement auto-committed, in-process (`LocalServer::execute`) and
over the wire (`htap-wire`, ADR-016) alike. Phase 10 needed `BEGIN`/`START TRANSACTION`/`COMMIT`/`ROLLBACK`,
`autocommit`, and `@user`/`@@system` session variables, without a unified cross-format WAL (still deferred,
see ADR-004) and without weakening any durability invariant in CLAUDE.md (one MVCC version domain, rowstore
authoritative, irrevocable-once-durable 2PC commit decisions).

### Options considered

**Where an open transaction's uncommitted writes live:**
- **(a) Engine-side uncommitted MVCC**, i.e. let `htap-rowstore` itself hold a write's value at a version
  visible only to the writer's own future reads, becoming visible to everyone else only on a later commit
  signal. Rejected: the rowstore's WAL and version domain assume a mutation is either not durable at all or
  committed-and-visible per the existing 2PC contract; teaching it a third state (durable-but-private, or
  buffered-but-crash-safe) is exactly the "unified WAL across formats" work ADR-004 explicitly defers, and
  doing it only for the rowstore side would still leave a converted `Column` partition with no analogous
  mechanism, reintroducing the base-plus-delta overlay's assumptions from a different angle.
- **(b) A pinned-snapshot registry**, tracking every session's open transaction snapshot so conversion (or
  any base-republishing operation) could either wait for older open transactions to finish or refuse to
  publish a new base while one is pinned below it. Rejected: there is no idle-transaction timeout or reaping
  in this MVC (documented as a limitation, not fixed here), so a single forgotten `BEGIN` with no
  following `COMMIT`/`ROLLBACK` would block conversion indefinitely — a starvation hazard traded for a
  correctness guarantee (avoiding the stale-snapshot conflict below) that a poisoning `Conflict` already
  provides without blocking anyone.
- **(c) Session-buffered writes**: uncommitted mutations live only in the session's own in-memory `WriteSet`
  (`crates/htap-server/src/session.rs`), keyed by `(partition_id, encoded primary key)`; reads inside the
  transaction overlay this write set below relational operators (read-your-own-writes); `COMMIT` builds one
  `TransactionRequest` from the accumulated write set and runs it through the *existing* 2PC path exactly
  once, against the transaction's own pinned snapshot. Chosen: no new durable state, no new on-disk format,
  and a crash before `COMMIT` is exactly an implicit `ROLLBACK` (nothing durable to undo), matching the
  isolation statement below.

**How a stale snapshot vs. a mid-transaction columnar conversion is handled:**
- Given (b) was rejected, a transaction that reads a base version published by a conversion that committed
  *after* the transaction's own snapshot was pinned needs a defined outcome. Rejected alternative: silently
  serve the pre-conversion (rowstore) view for the rest of the transaction, ignoring the new base — this
  would let the transaction commit against data logically superseded by conversion, and correctness would
  depend on the caller happening to notice. Chosen: `scan_partition_compact` returns `HtapError::Conflict`
  the moment `snapshot.version < cat_manifest.base_version` is detected (never fires in autocommit, since an
  autocommit statement always reads the current snapshot), which poisons the transaction exactly like a
  write-write conflict does — a read-time conflict, not a write-time one, but the same terminal state.

**DDL inside an open transaction:**
- **Implicit commit**, i.e. let a DDL statement inside an open transaction silently commit it first (the way
  `BEGIN` while already open does) and then run the DDL outside any transaction. Rejected: the surface is
  large and every edge case needs its own answer (does the implicit commit's own `DurablePending` outcome
  block the DDL? does a DDL validation failure leave the previously-open transaction's writes durably
  committed with no way back?) for a feature (mixing DDL into an explicit transaction) most SQL engines don't
  actually support transactionally either.
- **Reject DDL inside any open transaction, explicit or implicit, without poisoning it.** Chosen: `is_ddl`
  (`crates/htap-server/src/session.rs`) rejects `CreateTable`/`DropTable`/`AlterPartitions` with
  `HtapError::Unsupported` before dispatch when a transaction is open; the transaction's buffered writes and
  poison state are untouched, so the caller can simply `COMMIT`/`ROLLBACK` first and retry the DDL.

**Isolation level offered:**
- Snapshot isolation, first-writer-wins, write skew permitted, reported to clients as `REPEATABLE READ`
  (`htap_sql::variables::validate_isolation_level` accepts only that request, case/separator-insensitive, and
  rejects `READ COMMITTED`/`READ UNCOMMITTED`/`SERIALIZABLE` with `HtapError::Unsupported` rather than
  silently downgrading or upgrading a request this engine cannot actually honor).

  > **Superseded in part by ADR-033 (Phase 19, 2026-10-01).** `SERIALIZABLE` is now an accepted, opt-in level
  > (commit-time read-footprint validation, `implemented (local MVP)`); `REPEATABLE READ` (snapshot isolation) stays
  > the default, and `READ COMMITTED`, `READ UNCOMMITTED` and `SNAPSHOT` are still rejected with
  > `HtapError::Unsupported`. `validate_isolation_level` is now a wrapper over `parse_isolation_level`. The
  > statements above and in decision 2 that only `REPEATABLE READ` is accepted are historical for Phase 10.

### Decision

1. **Session-buffered write model (option (c)):** `Session::commit` builds `Transaction::new(next_txn_id,
   open_txn.snapshot.version)`, `set_request(request)`, and calls `TransactionManager::commit` directly —
   never `TransactionManager::commit_request`, whose own `begin()` would instead pin `read_version` at the
   *current* visible version, defeating the prepare-time first-writer-wins check's ability to detect that
   this session's snapshot is stale (plan amendment A3; pinned by
   `test_commit_write_write_conflict_returns_clean_conflict_and_poisons_session` and
   `test_conflict_detected_before_journal_commit_not_after`, both of which fail if `commit_request` is used
   instead).
2. **Stale-snapshot-vs-conversion conflict, DDL rejection, and the isolation statement:** as described under
   "Options considered" above — no pinned-snapshot registry, DDL rejected without poisoning, and only
   `REPEATABLE READ` accepted.
3. **Prepare-time conflict detection.** The first-writer-wins check (`Engine::check_first_writer_wins`) is
   now run inside `Engine::prepare`, before `Engine::prepare` returns a `PreparedTransaction` and therefore
   before `TransactionManager::commit` journals any `Intent`/`Commit` record for this transaction, not only
   inside `apply_prepared_locked` after the `Commit` record was already fsynced as it did before this phase.
   This was latent-but-correct before Phase 10 only because `LocalServer::execution_lock` serialized whole
   statements end to end; a session's snapshot can now be pinned at `BEGIN` and outlive many other
   statements before `COMMIT`, widening the staleness window far past one statement, so detecting the
   conflict only after the commit decision is already durable would misreport an ordinary stale-write
   conflict as `DurablePending` (an unrecoverable-looking ambiguity) instead of a clean, retryable `Conflict`.
   The apply-time check remains as defense in depth, documented as unreachable for the 2PC path (the
   manager's `decision_lock` excludes an interleaving writer between `prepare` and apply) and still relevant
   only for the legacy direct `Engine::commit` path, which `Engine::commit`'s own doc comment now states
   session/transaction-manager code must never call.
4. **Autocommit snapshot fix.** An autocommit statement's own write now commits against the snapshot that
   statement itself read at, not a new snapshot `TransactionManager::commit_request`'s `begin()` would
   otherwise assign. Previously, a read-then-write autocommit statement racing a concurrent `copy_from_*`/
   import commit landing between the read and the commit decision could silently lose the import's write
   (the fresh snapshot from `commit_request` would not see it as a conflict); reading and committing against
   the same snapshot the statement observed makes that a first-writer-wins `Conflict` instead of a lost
   update.
5. **`TransactionManager` recovery latch.** `TransactionManager` now latches "recovery required"
   (`RecoveryLatch`, storing the earliest unresolved transaction's id/version/reason) the instant any
   `commit` call returns `DurablePending`, and rejects every later `commit` — from any session, on any
   thread — until `TransactionManager::recover()` conclusively resolves the latched transaction (found in
   `recover()`'s committed or unresolved set) or the manager is reopened from a fresh journal replay. Without
   this, a later transaction could allocate and attempt to commit the next MVCC version while the pending
   transaction's own version was still unresolved; `apply_external`/`publish` would then reject the later
   transaction out of order, journaling a second ambiguous attempt that `recover()` would have to make sense
   of on top of the first. **Corrected by a second fix pass (below): the rejected caller gets a distinct
   `HtapError::RecoveryRequired`, not the blocking transaction's own `DurablePending`.**

### Second fix pass: `RecoveryRequired` vs `DurablePending`, `RecoveryCause`, `recover()` hardening, and the effective payload cap

A storage review of the decision above, run in parallel with this ADR's initial landing, found that decision
5 as first implemented reused `HtapError::DurablePending` for the *rejected* caller too — a session or
autocommit statement whose `commit` call never got past the latch check, and therefore did no work and
certainly did not durably commit anything, would see the same error as the transaction whose own outcome is
actually ambiguous. It also found that `recover()` could clear the latch in-process even when the underlying
failure was in the journal write itself, not just a participant's `apply`/`publish` step, and that the
nominal 16 MiB transaction payload cap (`MAX_PAYLOAD_SIZE`) was not actually the binding limit at the journal
layer. This section documents the fixes, landed as one follow-up pass on top of the same ADR.

**A new error for the rejected caller, not `DurablePending`:**
- **(a) Keep reusing `DurablePending` for the rejected caller**, distinguishing it from the blocking
  transaction's own outcome only by message text. Rejected: message-text-only distinctions are not a stable
  contract for wire error mapping or for any future caller that matches on the error variant rather than the
  string, and conflating "this transaction is definitely fine, just queued behind someone else's" with "this
  transaction's own outcome is ambiguous" is exactly the kind of distinction CLAUDE.md's durability rules
  exist to keep precise.
- **(b) A new `HtapError::RecoveryRequired { blocking_txn, reason }` variant.** Chosen: `Self::commit`'s
  latch check (and `abort()`'s `RecoveryCause::JournalIo` check) now returns this instead of `DurablePending`.
  Mapped on the wire to the same code as `DurablePending` (1105/`HY000`, never 1213/`40001` — a client must
  not retry a write-write conflict style either, since the caller's transaction is queued, not rolled back)
  but as a distinct Rust type callers can match on
  (`recovery_required_never_maps_to_the_retryable_conflict_code`).

**Whether `recover()` may clear the latch in-process:**
- **(c) Always allow `recover()` to clear the latch in-process**, as originally implemented. Rejected: if the
  original failure was the journal append or sync itself failing (`RecoveryCause::JournalIo`, e.g. a
  `commit_sync_hook` test fault or a real disk error at the decision boundary), an in-process `recover()` call
  afterward — even one that fsyncs the journal first — proves nothing about whether the original write was
  durable; fsync succeeding after an earlier fsync failed does not retroactively make that earlier write
  durable. Clearing the latch on that basis could resume issuing commits against a journal whose true state
  was never actually re-established from a real file open.
- **(d) Record which kind of failure caused the latch (`RecoveryCause::JournalIo` vs
  `RecoveryCause::ParticipantIo`) and only allow in-process `recover()` to clear a `ParticipantIo` latch; a
  `JournalIo` latch clears only on a fresh reopen (a brand-new `TransactionManager` from a brand-new file
  open and scan).** Chosen: a `ParticipantIo` latch's commit record was already proven durable before the
  failure, so replaying it in-process is exactly as trustworthy as the reopen path; a `JournalIo` latch is
  not, and only a real reopen re-establishes trust in the journal's own state.
  (`test_latch_via_participant_apply_failure_clears_in_process_recover`,
  `test_latch_via_commit_sync_hook_survives_in_process_recover_until_reopen`.) `abort()` on an unrelated,
  still-open transaction is likewise rejected with `RecoveryRequired` while the cause is `JournalIo` (an
  `Abort` record is itself a journal append, and the journal's own state may not be trustworthy yet) but
  still permitted while the cause is `ParticipantIo`.

**`recover()` hardening:**
- `recover()` now calls `Journal::sync()` on the journal it just read from, before replaying any `Commit`
  record into a participant. Reading a record back via an ordinary file read says nothing about whether it
  was ever actually synced to disk; a `Commit` record that only exists in the OS page cache when the process
  crashed should not be trusted as durable just because `recover_records()` can see it.
- `recover()` now cross-checks every registered participant's own durable `committed_version()` (for
  `RowstoreParticipant`, `Engine::committed_version()`) against the journal's own replayed `max_version` after
  replay completes, failing with `HtapError::Corruption` on any mismatch in either direction — a participant
  ahead of the journal means a durable commit record was lost from the journal; a participant behind it means
  replay did not fully apply what the journal records. Rejected alternative: trust the participant's own
  state and skip the check, which would let this exact "lost commit record" corruption go undetected forever
  (`test_recover_detects_engine_ahead_of_journal_as_corruption`). This check is documented (on
  `TxnParticipant::committed_version` itself) as sound only under a contract: every commit registered against
  this journal must touch every participant that overrides `committed_version` to return `Some`, or that
  participant will fall behind and be flagged as corruption even though nothing is wrong.
- `next_txn_id` is now restored via `AtomicU64::fetch_max` instead of a plain store, because `begin()`'s id
  allocation is a lock-free CAS loop that does not take `decision_lock` and can therefore race a concurrent
  `recover()` (unlike `next_version`/`visible_version`, which are only ever touched under `decision_lock`). A
  plain store could move the counter backwards below an id already handed to a live in-flight transaction;
  `fetch_max` guarantees recovery only ever raises it.
- A failed `Journal::append_nosync` (used for the `Commit` frame, which is not synced until immediately
  after) now truncates the file back to the pre-append offset on failure, so a partial write left behind by a
  crashed or short `write_all` cannot make the journal look like it has unrecoverable middle-of-log
  corruption the next time it is opened, rather than a clean torn-tail boundary
  (`test_append_nosync_failure_truncates_partial_write_and_journal_stays_openable`).

**The effective transaction payload cap is not the nominal 16 MiB:**
The durable `Intent` journal frame JSON-encodes each participant's payload as a `Vec<u8>` via `serde_json`'s
default encoding — a JSON array of decimal byte values (`[145,10,...]`), not a compact byte string — which
is roughly 3-4x larger than the raw payload bytes, all inside a journal frame itself bounded by
`DEFAULT_MAX_FRAME_SIZE` (16 MiB). A write set that stays within the nominal `MAX_PAYLOAD_SIZE` (16 MiB of
raw mutation JSON, checked in `TransactionRequest::validate`) could therefore still produce an `Intent` frame
larger than the journal can hold, previously discovered only when `encode_frame` itself failed *after*
`Engine::prepare` had already run. `TransactionManager::commit` now computes a conservative,
never-underestimating closed-form bound (`intent_frame_size_bound`, `crates/htap-txn/src/journal.rs`) on the
resulting frame size from the total payload byte count alone, and rejects with `HtapError::InvalidArgument`
before any prepare or journal work if the bound exceeds the journal's configured `max_frame_size`. Working
the bound backward (for a single-participant transaction; see the third fix pass below for the
multi-participant scaling this section's original derivation did not account for), the effective limit on raw
mutation payload bytes was about 4 MiB (4,194,175 bytes at the time of this fix pass, now 4,194,143 bytes —
see the third fix pass), not 16 MiB — see `docs/LIMITATIONS.md`. `Session::commit` runs the same check before
removing the transaction
from its open-transaction slot, so a rejected `COMMIT` leaves the transaction open rather than discarding it
(`test_commit_of_write_set_exceeding_intent_frame_is_rejected_and_txn_stays_open`,
`test_autocommit_oversize_insert_rejected_cleanly_before_any_journal_write`).

**Applied-external-transactions ledger capacity moved to prepare-time (defense-in-depth reordering, not a new
check):** `Engine::apply_prepared_locked`/`apply_external` already rejected a full ledger
(`MAX_APPLIED_EXTERNAL_TXNS`) before this fix pass, but only when a 2PC transaction reached apply — after its
Intent and Commit records were already durably journaled, turning a full ledger into a stuck, unresolvable
commit rather than a clean rejection. `Engine::prepare` now runs the same capacity check (for a real,
non-`u64::MAX`-sentinel prepare) before returning a `PreparedTransaction`, rejecting with
`HtapError::InvalidArgument` before any journal record exists for this transaction
(`test_ledger_full_commit_rejected_at_prepare_before_journal_growth`). The apply-time check remains as
defense in depth (this same `Engine::prepare` call also backs the legacy, test-only `Engine::commit`, which
cannot be distinguished from a real 2PC prepare, so a full ledger can also reject a legacy-path caller here;
minor, since `Engine::commit` is documented as test/non-transactional-only).

### Third fix pass: journal poisoning, `recover()` refusing outright, and Intent/Abort I/O also latching

A follow-up storage review of the second fix pass found four remaining gaps, landed as one more follow-up
pass on top of the same ADR:

1. **A journal handle can keep accepting appends after a write/sync failure it should not trust itself
   after.** The second fix pass truncated a failed `Commit`-frame `append_nosync` back to its pre-append
   offset, but did not stop the same `Journal` handle from being appended to again — including a failed
   `fsync` itself, after which the kernel's page-cache state for whatever was just written is unknowable even
   if a *later* `fsync` on the same fd would report success. `Journal` now tracks its own `poisoned:
   Option<String>` state, set when: an `append`'s or `sync`'s `fsync` fails (poisoned unconditionally,
   regardless of whether the best-effort truncate that follows also succeeds); the best-effort truncate after
   a failed `write_all` itself fails (the file may then hold stale bytes past `valid_end`); or an append wrote
   its bytes but then failed to fsync (truncated best-effort first, then poisoned unconditionally either way).
   While poisoned, every `append`/`append_nosync`/`sync` is rejected immediately (`Journal::is_poisoned`,
   `Journal::poison_reason`); only a fresh `Journal::open`/`open_with_options` — a brand-new file handle and
   scan, mechanically what a real process restart does — clears it
   (`test_truncate_failure_poisons_journal`,
   `test_append_sync_failure_then_shorter_frame_reopen_succeeds_or_is_rejected`). A failed `write_all` also
   truncates back to the pre-append offset even outside the poisoning paths. Note that clearing this
   in-process flag is a different question from whether the disk state it was protecting is actually durable:
   if poisoning was caused by a failed `fsync`, a fresh file descriptor from a process restart cannot see the
   earlier descriptor's error and may still be reading back a page that was never flushed — see the
   reboot-vs-restart operator guidance in `docs/LIMITATIONS.md` and `docs/OPERATIONS.md`.
2. **`recover()` could still be called (and would still attempt a replay) while latched `JournalIo` or while
   the journal itself was poisoned**, even though the second fix pass's own reasoning ("an in-process resync
   proves nothing about an earlier failed write on the same fd") applies just as much to calling `recover()`
   itself in that state. `recover()` now checks this first, before touching anything else: if the manager is
   already latched `JournalIo`, or the underlying `Journal` reports `is_poisoned()`, it returns
   `HtapError::RecoveryRequired` immediately and applies nothing — never attempting a replay. If the journal
   is poisoned but the manager was not already latched, `recover()` latches defensively with the journal's own
   poison reason first. This also means `recover()`'s own journal-sync failure (already documented in the
   second fix pass as making `recover()` fail) now additionally latches the manager as `JournalIo`, so a
   caller cannot retry `recover()` in a loop expecting a different outcome without reopening
   (`test_in_process_recover_under_journal_io_latch_is_rejected_and_applies_nothing`).
3. **Only `Commit`-boundary journal I/O failures latched the manager as `JournalIo`; `Intent` and `Abort`
   append/sync failures did not**, even though a failed `Intent` or `Abort` write is exactly the same kind of
   "this journal handle's own state may no longer be trustworthy" event the `Commit`-boundary latch exists to
   guard against — it just happens to also produce a clean, definite abort for *that* transaction (unlike a
   `Commit` failure, whose own outcome becomes ambiguous). `commit`'s `Intent` append/sync and `abort`'s
   `Abort` append/sync now latch the manager as `RecoveryCause::JournalIo` too, but only when the error is a
   real `HtapError::Io`: a pure validation/oversize-frame rejection (e.g. `encode_frame` refusing a frame
   bigger than `max_frame_size`) is not a journal-I/O problem — only that one oversize transaction is
   rejected, and a smaller retry must still work normally
   (`test_intent_append_failure_latches_manager_as_journal_io`,
   `test_abort_append_failure_latches_manager_as_journal_io`).
4. **`intent_frame_size_bound`'s fixed overhead assumed a single participant.** A transaction with many
   small-payload participants can produce a frame whose total per-participant JSON punctuation
   (`{"participant_id":...,"payload":[...]},` per participant) exceeds what one fixed constant conservatively
   covers. The bound now also scales by `num_participants *
   INTENT_PER_PARTICIPANT_OVERHEAD_BYTES` (128 bytes per participant, itself well above one such object's
   actual encoded overhead), on top of the existing fixed 512-byte overhead. `TransactionManager::commit`
   passes the real participant count through. A new `TransactionManager::max_frame_size()` accessor lets
   `WriteSet::try_merge`/`Session::commit` bound their own estimate against this manager's journal's actual
   configured `max_frame_size`, rather than assuming `DEFAULT_MAX_FRAME_SIZE`. Reworking the single-participant
   backward-derivation from the second fix pass with the new per-participant term: the effective cap on raw
   mutation payload bytes for a single-participant transaction (the only shape any current 2PC transaction
   uses — `RowstoreParticipant` is the sole registered participant) is now 4,194,143 bytes, not 4,194,175 —
   see `docs/LIMITATIONS.md`.

### Consequences

- Uncommitted transactional state is purely in-memory and per-process: it is never replicated to disk, never
  survives a crash, and is invisible to every other session, `EmbeddedClient::execute`/`LocalServer::execute`
  autocommit calls, and any concurrent `RemoteClient` connection until `COMMIT` — this is the same visibility
  boundary read-your-own-writes normally implies, made explicit here because there is no durable staging area
  behind it.
- There is no idle-transaction timeout or reaping yet (option (b) above shows why that matters): a session
  that opens a transaction and is never driven to `COMMIT`/`ROLLBACK` (or dropped) holds its pinned snapshot
  indefinitely, and a write it buffers against a partition can still poison another transaction's commit via
  the ordinary write-write conflict path even though nothing about it is durable. This is documented in
  `docs/LIMITATIONS.md`, not fixed in this phase.
- The recovery latch means a single ambiguous commit outcome now stops *every* later
  `TransactionManager::commit` call on the server — every session's `COMMIT`, and every plain autocommit
  `INSERT`/`DELETE`/`UPDATE` via `LocalServer::execute` with no session at all — not just the one that hit
  it, with each rejected caller seeing `HtapError::RecoveryRequired` (not the blocking transaction's own
  `DurablePending`), until either `TransactionManager::recover()` conclusively resolves it in-process (only
  possible when the latch's `RecoveryCause` is `ParticipantIo`) or the process is restarted
  (`LocalServer::open` always runs `TransactionManager::recover()` against a fresh journal replay, which
  clears the latch regardless of `RecoveryCause`) — a deliberate availability trade for correctness,
  consistent with how `DurablePending` was already handled per-transaction before this ADR.
- The effective per-transaction payload limit for the `intent_frame_size_bound` check is about 4 MiB of raw
  mutation payload for a single-participant transaction (4,194,143 bytes as of the third fix pass), not the
  nominal 16 MiB `MAX_PAYLOAD_SIZE` — see "Second fix pass"/"Third fix pass" above and `docs/LIMITATIONS.md`.
  This affects `INSERT`, `UPDATE`'s scan form, and any single autocommit statement or explicit-transaction
  `COMMIT` whose combined mutation payload exceeds it; there is no chunking.
- A `RecoveryCause::JournalIo` latch can now also come from a failed `Intent` or `Abort` journal write, not
  only a `Commit`-boundary one, and the underlying `Journal` can independently poison itself and reject every
  further append/sync until reopened. `TransactionManager::recover()` refuses outright (applies nothing)
  while either condition holds, rather than attempting — and potentially failing partway through — a replay;
  see "Third fix pass" above.
- One `htap-wire` connection is one `Session` (ADR-016's thread-per-connection design is unchanged); a
  connection's own transaction is rolled back on every disconnect path, and the wire-layer `SET`/`@@sysvar`
  shim shrank to only the two statement forms `vendor/sqlparser` cannot parse at all
  (`SET CHARACTER SET`/`SET CHARSET`) plus `USE`/`SELECT 1`/`VERSION()`/`DATABASE()`/`SCHEMA()`, since everything else
  now has a real session-backed answer.

### How to reverse it

The session layer is additive on top of `LocalServer::dispatch_bound` (Phase 9's execution path unchanged
underneath it): removing `LocalServer::open_session`/`EmbeddedClient::open_session`, reverting `htap-wire` to
one auto-committing `LocalServer::execute` call per connection, and restoring the old blanket `SET`/`@@sysvar`
shim would return to the Phase 9 surface without touching the rowstore, catalog, or txn-journal formats. The
prepare-time conflict check, the `TransactionManager` recovery latch (including the second fix pass's
`RecoveryRequired`/`RecoveryCause` split and `recover()` hardening), the effective payload cap check, and the
ledger-capacity-at-prepare check are correctness fixes independent of sessions (all apply equally to a bare
`LocalServer::execute` autocommit statement) and should not be reverted even if the session layer were
removed.

### Test Evidence

- `crates/htap-rowstore/tests/engine.rs::test_first_writer_wins_conflict` (unchanged pre-existing coverage of
  the check now also run in `prepare`).
- `crates/htap-txn/tests/two_phase_commit.rs`: `test_conflict_detected_before_journal_commit_not_after`,
  `test_stale_snapshot_non_conflicting_key_still_commits`,
  `test_commit_after_durable_pending_is_rejected_until_recovery`,
  `test_latch_via_commit_sync_hook_survives_in_process_recover_until_reopen`,
  `test_latch_via_participant_apply_failure_clears_in_process_recover`,
  `test_ledger_full_commit_rejected_at_prepare_before_journal_growth`,
  `test_recover_detects_engine_ahead_of_journal_as_corruption` (second fix pass;
  `test_commit_after_durable_pending_is_rejected_until_recovery` and
  `test_latch_via_commit_sync_hook_survives_in_process_recover_until_reopen` were updated for the third fix
  pass's "in-process recover rejected, resolved only after reopen" semantics);
  `test_in_process_recover_under_journal_io_latch_is_rejected_and_applies_nothing` (third fix pass).
- `crates/htap-txn/tests/journal.rs::test_append_nosync_failure_truncates_partial_write_and_journal_stays_openable`
  (second fix pass); `test_append_sync_failure_then_shorter_frame_reopen_succeeds_or_is_rejected`,
  `test_truncate_failure_poisons_journal`, `test_intent_append_failure_latches_manager_as_journal_io`,
  `test_abort_append_failure_latches_manager_as_journal_io` (third fix pass). The following existing
  `crates/htap-txn/tests/journal.rs` tests were updated for the third fix pass's "in-process recover rejected,
  resolved only after reopen" semantics: `test_commit_append_failure_at_decision_boundary`,
  `test_commit_append_torn_tail_failure_and_recovery_semantics`, `test_commit_sync_failure_at_decision_boundary`.
- `crates/htap-wire/src/error_map.rs::recovery_required_never_maps_to_the_retryable_conflict_code` (second fix
  pass).
- `crates/htap-server/tests/session.rs`:
  `test_commit_while_manager_latched_by_other_session_keeps_txn_open`,
  `test_commit_of_write_set_exceeding_intent_frame_is_rejected_and_txn_stays_open`,
  `test_autocommit_oversize_insert_rejected_cleanly_before_any_journal_write` (second fix pass).
- `crates/htap-sql/tests/query_bind.rs`: `test_user_and_system_variable_binding`,
  `test_global_scope_rejected`.
- `crates/htap-sql/tests/route.rs::test_narrow_shape_gate_excludes_variables`.
- `crates/htap-sql/src/variables.rs` unit tests: `constant_variables_and_aliases_resolve`,
  `dynamic_variables_read_session_state`, `classify_set_target_dynamic_readonly_unknown_and_global`,
  `isolation_level_validation`, `autocommit_value_parsing`.
- `crates/htap-server/tests/session.rs` (session lifecycle, write-set payload cap, read-your-own-writes
  across `Row`/`Column`/`Converting` partitions and the general query executor, poisoning and commit-time
  catalog revalidation, `DurablePending` quarantine, autocommit/`BEGIN`/`SET` state machine, and connector
  start-up `SET` compatibility — see `docs/PROGRESS.md` for the full test name list).
- `crates/htap-server/tests/session_concurrency.rs`:
  `test_concurrent_sessions_write_write_conflict_first_committer_wins`,
  `test_r5_point_read_still_bypasses_analytics_inside_and_outside_transaction`,
  `test_autocommit_update_conflicts_with_concurrent_import_instead_of_losing_it` (decision 4).
- `crates/htap-server/tests/session_recovery.rs`: `test_uncommitted_writes_never_visible_after_reopen`,
  `test_committed_transaction_visible_after_reopen`,
  `test_reopen_after_session_commit_durable_pending_resolves_outcome` (decision 5).
- `crates/htap-client/tests/session.rs`: `test_embedded_session_begin_commit_rollback`,
  `test_embedded_autocommit_execute_unaffected_by_open_session_on_same_server`.
- `crates/htap-wire/tests/wire_server.rs`: `test_wire_begin_commit_rollback_round_trip`,
  `test_wire_rollback_on_disconnect`, `test_wire_concurrent_sessions_conflict_returns_1213`,
  `test_wire_set_autocommit_and_user_variable_round_trip`, `test_wire_sysvar_reads_now_reflect_session_state`.
- `crates/htap-wire/src/error_map.rs::durable_pending_never_maps_to_the_retryable_conflict_code`.
- `crates/htap-wire/src/shim.rs` unit tests: `shim_charset_set_forms_return_ok`,
  `shim_no_longer_answers_plain_set_or_sysvar_reads`.

Cross-references: ADR-004 (one shared MVCC version domain and no unified cross-format WAL — preserved:
`Session::commit` still commits exclusively through the existing `RowstoreParticipant`/`TransactionManager`
path, decision 1 above rejects giving the rowstore a new uncommitted-but-durable state), ADR-008
(partition-scoped conversion state machine — decision 2's stale-snapshot conflict is the session layer's
answer to a conversion publishing a new base mid-transaction), ADR-016 (hand-written synchronous wire
protocol, thread-per-connection — unchanged framing; one connection is now one `Session` on top of it), and
ADR-017 (general query executor — the read-your-own-writes overlay reuses `scan_partition_compact`, the same
per-partition storage path `Route::Query` and `Route::OlapScan` already use).

---

## ADR-019: MySQL binary protocol via AST-level placeholder substitution; best-effort PREPARE metadata; real `max_allowed_packet`; unsigned-64 rejection; SEND_LONG_DATA poisoning; reset/change-user under quarantine; multi-statement stop-on-error

`Status: Accepted`
`Date: 2026-09-17`

### Context

ADR-016 explicitly deferred the binary protocol, prepared statements, `COM_RESET_CONNECTION`,
`COM_CHANGE_USER`, ≥16 MiB messages, a cryptographic handshake scramble, and multi-statements. The user
explicitly lifted these deferred items for Phase 11 (TLS, compression, and per-user ACL remain Phase 12
scope, with seams left for them: a capability struct, a `Read`/`Write`-generic packet layer, and a
`verify_credentials` auth hook). None of this touches the rowstore, catalog, or on-disk envelope formats;
CLAUDE.md's durability invariants (one MVCC version domain, `CommitOutcomePending` quarantine, R5) had to be
preserved unchanged. Consulted a panel (`reasoner`+`gemini`) on the five hardest design questions, then had
`architect` approve decisions 1-4 below with refinements before implementation began. (Updated by ADR-020:
Phase 12 replaced the `verify_credentials`-only seam this ADR left for TLS/compression with a real `Conn`
abstraction (`htap_wire::tls::Conn`) and a `CompressedStream` transport layer, and by ADR-021: Phase 12
replaced `verify_credentials` itself with `LocalServer::authenticate_session` against catalog-backed accounts;
"TLS, compression, and per-user ACL remain Phase 12 scope" below is this ADR's framing at the time it was
made, not the current
contract.)

### Options considered

**Parameterization (how a bound `?` becomes part of the executed statement):**
- **(a) Re-render the statement to SQL text with parameters substituted, then reparse.** Rejected: a `BLOB`
  parameter would have to be re-encoded as a string literal and reparsed back into bytes (doubling encode/
  decode work and risking a lossy round trip for arbitrary bytes), and edge values (`i64::MIN`, non-finite
  floats) do not have a lossless canonical text spelling that survives a second parse in every position a
  literal can appear.
- **(b) A native `Value`-carrying `Param` AST node threaded through the binder**, so the binder resolves a
  parameter's type from context the same way it resolves a literal's type today, without ever materializing a
  substituted AST. Rejected: it would mean touching every literal-acceptance site in `htap-sql::binder` and
  `binder_query` to also accept `Param`, doubling the surface area of the binder's literal-handling code for a
  benefit (avoiding one clone-and-mutate pass per `EXECUTE`) that does not matter at this engine's scale;
  `vendor/sqlparser`'s own `Visit`/`VisitMut` traits also do not guarantee source-text order across all node
  kinds this binder needs to walk (join `ON`, `LIMIT`/`OFFSET`, CTEs), so a visitor-based placeholder walk
  could not be trusted to agree with a raw tokenizer's left-to-right count without writing the same
  hand-written walk anyway.
- **(c) Hand-written AST-level substitution**: a single recursive walk over the exact statement/query shapes
  the binder accepts (shared by counting and substitution, so they cannot disagree with each other), replacing
  each placeholder `Expr` node in place with a literal `Expr` built directly from the bound `Value`, with no
  intermediate text form. Chosen.

**PREPARE response metadata (result-set column definitions before any parameter value is known):**
- **(a) Always report `num_columns = 0`.** Rejected: several real clients (including `libmysqlclient`-based
  ones) use `COM_STMT_PREPARE_OK`'s column count to decide how to allocate bind buffers before the first
  `EXECUTE`; reporting zero columns for a plain `SELECT` with no parameter-typed projection would break them
  unnecessarily.
- **(b) Require the caller to supply parameter types at PREPARE time (as some MySQL C API extensions do) and
  bind eagerly against them.** Rejected: the wire protocol's `COM_STMT_PREPARE` request carries no parameter
  type information at all — only `COM_STMT_EXECUTE` does — so this would require a second round trip this
  protocol does not have, or inventing a private extension.
- **(c) Best-effort: infer each placeholder's type from purely local context (assignment target, comparison
  operand), and only when *every* placeholder's type is inferable, probe the real, unmodified binder with
  representative non-NULL values of those types to get the exact output schema it would produce; otherwise
  report `num_columns = 0` and generic parameter definitions.** Chosen, with the `architect` refinement that
  the probe must go through the same binder every real `EXECUTE` uses (not a schema-only shortcut), so the
  reported schema can never drift from what execution actually returns.

**`max_allowed_packet`:**
- **(a) Leave the limit as a fixed, non-configurable constant.** Rejected: MySQL operators expect
  `max_allowed_packet` to be a real, settable server parameter, and a fixed value forces recompilation to
  raise or lower it.
- **(b) Make it configurable, defaulting to MySQL's own default of 64 MiB.** Chosen: matches operator
  expectations for a MySQL-compatible server and gives `htapd --max-allowed-packet`/`HTAPD_MAX_ALLOWED_PACKET`
  a familiar unit and default.

**Unsigned 64-bit parameters above `i64::MAX`:**
- **(a) Add a `UInt64` variant to the engine's `Value` type to represent them exactly.** Rejected: this is a
  storage/type-system change reaching well outside the wire layer (every column type, comparison, and
  arithmetic rule in `htap-sql`/`htap-common` would need a new numeric case) for a narrow protocol edge case;
  out of scope for a wire-layer phase per CLAUDE.md's scope rule.
- **(b) Silently wrap or reinterpret as a negative `i64`.** Rejected: silently reinterpreting `18446744073709551615` as `-1` is exactly the kind of silent data corruption CLAUDE.md's evidence and correctness rules exist to prevent.
- **(c) Reject cleanly with a message naming the value and the limit.** Chosen; documented as a first-class,
  permanent limitation (not a "not yet implemented" gap) until the engine gains a genuine unsigned 64-bit
  value type.

**`COM_STMT_SEND_LONG_DATA` error reporting:**
- **(a) Invent a private response packet for this command.** Rejected: the protocol defines
  `COM_STMT_SEND_LONG_DATA` as having no response at all (success or failure); a private response would break
  every real client, which does not read one after sending this command.
- **(b) Poison the statement and surface the stored error at the next `EXECUTE`.** Chosen: this is the only
  option that reports the failure at all without violating the protocol's "no response" contract; the
  statement remains unusable until `COM_STMT_RESET` clears the poison.

**`COM_RESET_CONNECTION`/`COM_CHANGE_USER` vs. the ADR-018 `CommitOutcomePending` quarantine:**
- **(a) Let RESET/CHANGE_USER unconditionally clear session state, including a quarantined session.**
  Rejected: this would let a client silently discard a `DurablePending`/`RecoveryRequired` outcome whose real
  resolution is still unknown — exactly the ambiguity ADR-018's quarantine exists to keep visible until a
  process restart resolves it, not to let a client mask by resetting the connection.
- **(b) Route both through `Session::reset()`, which already returns the stored outcome-pending error without
  mutating anything when quarantined, and otherwise performs an ordinary rollback/variable-clear.** Chosen:
  reuses the exact ADR-018 gate with no new special-casing in the wire layer.

**Multi-statement batch semantics:**
- **(a) Execute every statement in the batch regardless of an earlier failure, reporting all outcomes.**
  Rejected: MySQL's own multi-statement semantics stop at the first error, and continuing past a
  `DurablePending`/`RecoveryRequired` outcome specifically would mean issuing further statements against a
  transaction manager already in an ambiguous, latched state — exactly what ADR-018's recovery latch exists to
  prevent for *any* caller, not just the one that hit it.
- **(b) Stop at the first error, including a `DurablePending`/`RecoveryRequired` outcome.** Chosen; matches
  both real MySQL client expectations and the existing single-statement quarantine/latch semantics.

### Decision

1. **Parameterization: AST-level substitution (`htap_sql::prepare`), option (c) above.** A single recursive
   walk (`count_placeholders`/`substitute_placeholders`, sharing one traversal so they can never disagree)
   covers every literal-bearing position the binder accepts: INSERT VALUES rows, UPDATE SET/WHERE, DELETE
   WHERE, SELECT projection/WHERE/HAVING/GROUP BY/ORDER BY/JOIN ON, subquery bodies (scalar/IN/EXISTS),
   derived tables, CTE bodies, UNION branches, and `LIMIT`/`OFFSET` — the plan's original task list named only
   the top-level clauses; a main-session amendment (A1) added the nested positions, since MySQL clients
   routinely prepare `SELECT ... LIMIT ?` and similar shapes. A `?` in a position the walk does not recognize
   (an identifier, a DDL default, a `SET` target) makes `checked_placeholder_count`'s raw-tokenizer count
   disagree with the walk's count, and the statement is rejected as `HtapError::Unsupported` naming the
   mismatch, never silently under-substituted. Only `INSERT`/`UPDATE`/`DELETE`/`SELECT` can be prepared.
2. **PREPARE metadata: best-effort, probed through the real binder, option (c) above.**
   `resolve_prepare_output_schema` infers each placeholder's type from purely local context and, only when
   every placeholder's type is known, probes the unmodified binder with representative non-NULL values;
   otherwise `num_columns = 0` and parameter definitions are generic. `INSERT`/`UPDATE`/`DELETE` always report
   `num_columns = 0`.
3. **`max_allowed_packet`: real and configurable, default 64 MiB, option (b) above.**
   `WireServerConfig::max_allowed_packet` is enforced by `codec::read_message_with_stop`/`write_message` (used
   at every call site that may exceed 16 MiB), configurable via `htapd --max-allowed-packet`/
   `HTAPD_MAX_ALLOWED_PACKET`, and reported dynamically as `@@max_allowed_packet`. An oversize message is
   rejected with `ER_NET_PACKET_TOO_LARGE`/1153 (best effort) and the connection closed.
4. **Unsigned 64-bit values above `i64::MAX`: rejected cleanly, option (c) above**, documented as a
   first-class, permanent limitation in `docs/LIMITATIONS.md` rather than a deferred feature.
5. **`COM_STMT_SEND_LONG_DATA`: poison-and-surface-later, option (b) above.** A long-data error (an
   out-of-range parameter index, or exceeding the connection's byte cap) is stored on the statement and
   returned verbatim by the next `EXECUTE`; `COM_STMT_RESET` clears it.
6. **`COM_RESET_CONNECTION`/`COM_CHANGE_USER`: route through `Session::reset()`, option (b) above.** Both
   clear the prepared-statement registry on success; a quarantined session's outcome-pending error is
   returned unchanged, with the registry left intact, exactly as ADR-018 already specifies for any other
   rejected operation on a quarantined session. `COM_CHANGE_USER` additionally re-authenticates via a new
   `verify_credentials(scramble, configured_password, auth_response) -> bool` seam — the same function
   `htapd`'s handshake path now calls, and the seam Phase 12 per-user ACL will extend — closing the
   connection with `ER_ACCESS_DENIED` on failure without touching session state.
7. **Multi-statement batches: stop at the first error, option (b) above**, including a
   `DurablePending`/`RecoveryRequired` outcome. `CLIENT_MULTI_STATEMENTS`/`CLIENT_MULTI_RESULTS` are
   advertised but only honored when the connecting client actually negotiated them; a negotiated batch runs
   sequentially through `Session::execute_statement` with `SERVER_MORE_RESULTS_EXISTS` set on every result but
   the last. `COM_STMT_PREPARE` rejects multi-statement text even when the capability is negotiated.
8. **CSPRNG handshake scramble.** `getrandom::fill` replaces the seeded-xorshift generator ADR-016 shipped,
   with no fallback: a failed OS RNG call fails the handshake rather than falling back to a weaker source.
9. **≥16 MiB messages: real, bounded reassembly/splitting**, not an unsupported case that closes the
   connection. `read_message_with_stop` continues reassembling while a chunk's length is exactly 0xFFFFFF,
   checking the running total against `max_allowed_packet` before allocating further; `write_message` splits
   an outbound message the same way, with a trailing empty packet on an exact multiple.
10. **Shutdown force-close.** A `live_connections` registry of `try_clone`d streams (RAII-unregistered on
    every connection-thread exit, including a panic) lets `WireServer::shutdown()` call `Shutdown::Both` on
    every live connection before joining connection threads, so a connection blocked mid-packet is torn down
    immediately instead of waiting indefinitely for the rest of a packet that may never arrive.

### Post-acceptance fix pass

A follow-up hardening pass on this phase's implementation surfaced two findings worth recording against
this ADR's decisions (the rest of that pass — pre-authentication read bounds, `COM_CHANGE_USER` scramble
persistence, panic-safe connection-count accounting, `ORDER BY`/`LIMIT` placeholder-hint coverage,
`DATE`/`DATETIME`/`TIMESTAMP` calendar-range validation, checked-arithmetic decoders plus a decode fuzz
harness, and prepared-statement id-wraparound handling — are implementation-correctness fixes to code this
ADR already covers, not new design decisions; see the "Network layer" and "Prepared statements and binary
protocol" sections of `docs/ARCHITECTURE.md` for their contract and evidence).

- **Finding 2 (decision 1, parameterization): `DECIMAL`/`NEWDECIMAL` parameters were substituted through a
  lossy path.** The binary decoder already kept a `DECIMAL` parameter as validated text
  (`ParamValue::DecimalText`), but the function that turned it into a substituted AST literal went through
  an `f64`/`i64` round trip first — exactly the kind of precision loss decision 1's option (a) (re-render to
  text and reparse) was rejected for, reintroduced one layer down. Fixed by adding
  `htap_sql::ParamLiteral::NumericText` and `substitute_placeholders_ext`, an extension of the existing
  `ParamLiteral::Value` substitution path that builds the literal `Expr` directly from the validated numeric
  text (`numeric_text_to_expr`), never through a lossy intermediate type. This makes the wire-to-AST step
  itself lossless; what happens after that is unchanged and correct per decision 1 — the binder coerces the
  literal into the target `Int64`/`Float64` column exactly as it would the identical literal typed directly
  into SQL text, and precision beyond what those two types hold is lost there, not before it, because the
  engine has no arbitrary-precision `DECIMAL` type (documented in `docs/LIMITATIONS.md`). Verified by
  `crates/htap-sql/tests/prepare.rs::test_numeric_text_decimal_round_trips_exactly_into_bigint_column`,
  `test_numeric_text_negative_decimal_with_fraction_round_trips`, `test_numeric_text_validates_strictly`, and
  `crates/htap-wire/tests/wire_server.rs::test_wire_prepared_decimal_param_round_trips_exactly_into_bigint_column`.
- **Finding 8 (new, not covered by decisions 1-10): OK/EOF status flags were a hardcoded constant.**
  `build_command_ok`/`build_resultset_terminator` always reported `SERVER_STATUS_AUTOCOMMIT` and never set
  `SERVER_STATUS_IN_TRANS`, regardless of the connection's actual `autocommit` setting or whether a
  transaction was open — a pre-existing gap from ADR-016 this ADR's decisions did not touch. Fixed by
  threading the session's real status bitmask (`server::session_status_flags`) into both packet builders as
  an explicit `status: u16` parameter, composed with `SERVER_MORE_RESULTS_EXISTS` (decision 7) rather than
  replacing it. Chosen over leaving `SERVER_STATUS_IN_TRANS` permanently unset (the ADR-016-era rationale,
  since transactions were the OLTP-only Phase 8/9 kind): Phase 10 sessions gave this server a real,
  observable "transaction open" state that some MySQL clients and proxies key behavior off of, and
  hardcoding autocommit-only made `SET autocommit = 0` and `BEGIN` invisible on the wire even though they
  were enforced correctly. Verified by
  `crates/htap-wire/tests/wire_server.rs::test_wire_status_flags_reflect_autocommit_and_transaction_state`
  and `crates/htap-wire/src/result_codec.rs` unit tests (`command_ok_reports_the_status_it_is_given`,
  `more_results_flag_sets_server_more_results_exists`).

### Consequences

- No on-disk, WAL, catalog, or MVCC format changed; this phase is wire-protocol- and SQL-front-end-only
  (`htap-wire`, `htap-sql::prepare`, `htap-client`). The `CommitOutcomePending` gate lives in exactly one
  place (`Session::execute_statement`/`Session::reset`); every new command (`COM_STMT_EXECUTE`,
  `COM_RESET_CONNECTION`, `COM_CHANGE_USER`, a multi-statement batch) reaches it through that same function,
  never a wire-layer bypass, per the plan's hard requirement.
- `COM_STMT_EXECUTE` and multi-statement batches go through `Session::execute_statement` under the same
  per-statement `execution_lock` discipline Phase 10 already uses; no new wire-level lock is held across
  statement execution (amendment A4).
- A `VARCHAR`/`VAR_STRING`-family bound parameter's raw bytes cannot be told apart from a bound `Vec<u8>` on
  the wire (both arrive as the same type code); resolved by decoding as `Value::String` when valid UTF-8 and
  `Value::Bytes` otherwise, and teaching the binder a narrow, literal-only coercion (a string literal into a
  `BYTES` column; the integer literals `0`/`1` into a `BOOL` column) so `INSERT`/`UPDATE`/`WHERE` all accept
  what the wire layer can actually produce (amendment A3) — a real gap the plan asked to be resolved or
  documented; resolved here rather than left as a limitation.
- `htapd`'s per-user ACL, TLS, and compression remain Phase 12 scope; this phase leaves the seams named in
  the plan (a capability struct generalized beyond `CLIENT_MULTI_STATEMENTS`/`CLIENT_MULTI_RESULTS`, a
  `Read`/`Write`-generic packet layer, and `verify_credentials`) for that work rather than pre-building it.
- `COM_STMT_FETCH`, exact `DECIMAL` (kept as validated text, not arbitrary precision), `TIME`-typed
  parameters, and unsigned 64-bit values above `i64::MAX` remain permanent or near-term limitations,
  documented in `docs/LIMITATIONS.md` rather than silently mishandled.

### How to reverse it

Each piece is independently revertible without touching the others or any on-disk format: dropping
`htap-wire::{binary_codec, prepared}` and the new command handlers in `server.rs` returns to ADR-016's
text-protocol-only surface; reverting `max_allowed_packet` to a fixed constant, the scramble to a seeded
generator, or multi-statements to "advertise but never honor" are each single-file changes; `htap_sql::prepare`
is additive and unused by any other execution path if removed.

### Test Evidence

- `crates/htap-sql/tests/prepare.rs` (placeholder count/tokenizer agreement across statement shapes including
  subqueries/derived tables/CTEs/UNION/`LIMIT`, substitution identical to literal-SQL binding, output-schema
  resolution positive/`None` cases, `i64::MIN`/non-finite-float/string/bytes edge cases,
  `test_no_supported_shape_ever_mismatches_tokenizer_count`,
  `test_placeholder_in_unsupported_position_returns_unsupported_error`).
- `crates/htap-wire/src/binary_codec.rs` unit tests (full parameter type matrix round trip, NULL-bitmap
  offsets 0/2, `new_params_bound_flag` caching, unsigned `LONGLONG` boundary, `TIME`/unknown-type rejection,
  invalid `DECIMAL` text, `VAR_STRING` UTF-8/non-UTF-8 decoding, binary row round trip).
- `crates/htap-wire/src/prepared.rs` unit tests (registry insert/close/reset, statement cap, long-data
  accumulation/clearing/poisoning).
- `crates/htap-wire/src/codec.rs` unit tests (`test_message_reassembly_at_exact_boundary_with_trailing_empty_packet`,
  `test_message_reassembly_rejects_over_max_allowed_packet_before_full_read`,
  `test_message_reassembly_rejects_sequence_id_mismatch_across_chunks`,
  `test_write_message_splits_exact_multiple_and_non_multiple`,
  `write_message_and_read_message_sequence_ids_wrap_at_256`).
- `crates/htap-wire/src/handshake.rs::scramble_is_printable_and_varies`.
- `crates/htap-wire/src/shim.rs::shim_never_matches_multi_statement_text`.
- `crates/htap-sql/src/variables.rs::max_allowed_packet_reads_dynamically_but_set_stays_a_no_op`.
- `crates/htap-wire/tests/wire_server.rs`: `test_prepared_statement_unsupported_kinds_rejected`,
  `test_prepared_statement_unknown_id_and_close_and_reset`, `test_prepared_statement_send_long_data`,
  `test_prepared_statement_param_type_cache_new_params_bound_zero`,
  `test_prepared_statement_in_transaction_and_commit_outcome_pending`,
  `test_prepared_statements_mysql_crate_interop_all_types`, `test_prepare_placeholder_in_limit_and_subquery`,
  `test_wire_reset_connection_clears_state_and_prepared_statements`,
  `test_wire_reset_connection_while_commit_outcome_pending_stays_quarantined`,
  `test_wire_quit_allowed_while_commit_outcome_pending`, `test_wire_change_user_reauth_and_reset`,
  `test_wire_change_user_wrong_password_closes_connection`,
  `test_wire_change_user_while_commit_outcome_pending_stays_quarantined`,
  `test_wire_large_payload_over_16mb_round_trip`,
  `test_wire_max_allowed_packet_rejects_oversize_query_and_closes_connection`,
  `test_max_allowed_packet_variable_reflects_wire_config`,
  `test_wire_multi_statements_sequential_execution_and_more_results_flag`,
  `test_wire_multi_statements_stops_on_first_error`, `test_wire_multi_statements_stops_on_durable_pending`,
  `test_wire_multi_statements_rejected_without_capability`,
  `test_wire_prepare_rejects_multi_statement_text_even_when_negotiated`,
  `test_shutdown_force_closes_connection_blocked_mid_packet`,
  `test_shutdown_force_close_rolls_back_open_transaction`, `test_wire_mysql_connector_startup_still_works`.
- `crates/htap-client/tests/prepared.rs`: `test_remote_prepared_statement_matches_embedded_literal_execution`,
  `test_remote_prepared_statement_close_then_execute_errors`.
- Post-acceptance fix pass (findings 2 and 8 above): `crates/htap-sql/tests/prepare.rs`
  (`test_numeric_text_decimal_round_trips_exactly_into_bigint_column`,
  `test_numeric_text_negative_decimal_with_fraction_round_trips`, `test_numeric_text_validates_strictly`);
  `crates/htap-wire/tests/wire_server.rs`
  (`test_wire_prepared_decimal_param_round_trips_exactly_into_bigint_column`,
  `test_wire_status_flags_reflect_autocommit_and_transaction_state`); `crates/htap-wire/src/result_codec.rs`
  unit tests (`command_ok_reports_the_status_it_is_given`, `more_results_flag_sets_server_more_results_exists`).
- `crates/htap-server/tests/session.rs`: `test_reset_clears_state`,
  `test_reset_while_outcome_pending_is_rejected_without_mutation`,
  `test_execute_and_execute_statement_are_equivalent`,
  `test_execute_statement_respects_commit_outcome_pending`.
- `crates/htap-sql/tests/query_bind.rs::test_where_bytes_column_compared_against_string_literal_coerces`
  (amendment A3, WHERE-clause coercion).

Cross-references: ADR-016 (hand-written synchronous wire protocol — this ADR extends it, decisions 8-10
directly amend that ADR's "no prepared statements"/"non-cryptographic scramble"/text-only framing), ADR-018
(`CommitOutcomePending` quarantine and the `RecoveryLatch` — decisions 5-7 above are new callers of exactly
that existing gate, not new gates), and ADR-017 (general query executor — `Session::execute_statement` is the
same single entry point Phase 9/10 already funnel every statement kind through).

---

## ADR-020: rustls TLS and MySQL compressed-packet framing on a shared `Conn`/`ServerStream` transport layer

`Status: Accepted`
`Date: 2026-09-18`

### Context

ADR-016 and ADR-019 both deferred TLS and protocol compression to Phase 12, leaving `htap-wire`'s connection
functions typed concretely as `&mut TcpStream`. The user explicitly lifted this deferred item. Two independent
requirements had to be satisfied without touching the rowstore/catalog/MVCC invariants in CLAUDE.md: (1) a real
TLS upgrade negotiated via the MySQL protocol's `SSLRequest`/`CLIENT_SSL` handshake, transparent to every
existing command handler; (2) MySQL compressed-packet framing (`CLIENT_COMPRESS`/
`CLIENT_ZSTD_COMPRESSION_ALGORITHM`) layered *underneath* the existing packet codec, transparent to it in the
same way. Consulted a panel (`reasoner` + `gemini`) and `architect` per the phase plan; both flagged that a
naive byte-level compression adapter is unsafe (a crafted frame's declared uncompressed length must never be
trusted for allocation) and that TLS/`aws-lc-rs` needs `cmake`, which this environment does not have.

### Options considered

1. **`rustls` 0.23 with the `ring` crypto provider, explicit `default-features = false, features = ["ring"]`.**
   Verified in a scratch build that `ring` compiles here with only `gcc`; `aws-lc-rs` (rustls's other provider,
   and the `mysql` crate's `rustls-tls` feature) needs `cmake`, confirmed absent in this environment. Enabling
   both providers at once panics at runtime ("no process-level `CryptoProvider`"), so the workspace dependency
   pins `ring` only. **Chosen.**
2. **A native-TLS wrapper (`native-tls`/`openssl`) instead of `rustls`.** Rejected: pulls in a system OpenSSL
   dependency, which is exactly the kind of environment-fragile build requirement `rustls`+`ring` avoids, and
   gives up `rustls`'s pure-Rust, `unsafe`-free (from this crate's point of view) posture.
3. **A single `Conn` enum (`Plain(TcpStream)` / `Tls(StreamOwned<ServerConnection, TcpStream>)`) implementing
   `Read`/`Write`, with every `server.rs` function signature converted from `&mut TcpStream` to `&mut Conn`.**
   **Chosen** — a pure refactor (task A2) that had to compile and pass every existing test unchanged before any
   TLS behavior was added, isolating "plumbing changed" from "behavior changed" as two separate, separately
   verifiable steps.
4. **A stateful `CompressedStream<S: Read + Write>` wrapper (task B3) sitting directly beneath the packet codec
   (`TCP -> Conn -> CompressedStream<Conn> -> codec.rs`), rather than a naive pass-through adapter.** Reads are
   framed per the MySQL 7-byte compressed-packet header (3-byte compressed length, 1-byte independent sequence
   id, 3-byte uncompressed length) and decompressed with three independent bounds, not just the declared
   header length: originally, a hard byte-count cap of `max_allowed_packet + 1` read via `.take()`; a
   follow-up fix pass tightened this to the frame's own declared `uncompressed_length + 1` instead, so
   decoding stops the instant one byte past *that* frame's declared length would be produced, not just once
   `max_allowed_packet` is crossed (see "Fix pass" below); an exact-length check (decoded length must equal
   the declared `uncompressed_length`, not merely fit under it); and a codec-level window-size cap
   (`zstd::stream::read::Decoder::window_log_max(24)`, 16 MiB, independent of `max_allowed_packet`) so a
   crafted frame cannot force an oversized decode window before the byte-count cap even applies. **Chosen**
   over trusting the header (rejected outright by the panel) and over per-packet (rather than per-frame,
   multi-packet-capable) compression (rejected: MySQL packs multiple ordinary packets into one compressed
   frame and splits one packet across frames; a naive 1:1 mapping cannot express either).
5. **`mysql_native_password` only for authentication over TLS; no `caching_sha2_password`.** Same call as
   ADR-019's decision to defer it; `architect` reconfirmed for Phase 12. Compression and TLS activate only
   *after* the authentication OK packet (never during the handshake, `SSLRequest`, an `AuthSwitchRequest`
   round trip, or `COM_CHANGE_USER`'s re-authentication, which does not renegotiate either), matching real
   MySQL and keeping the auth-plugin question orthogonal to this ADR.
6. **TLS cert hot-reload via `ResolvesServerCert` over a `parking_lot::RwLock<Arc<CertifiedKey>>`
   (`ReloadableCertResolver`), not `ArcSwap` (no new dependency needed for this).** `WireServer::reload_tls_certs()`
   loads and validates a new cert/key pair (including that the key matches the certificate,
   `CertifiedKey::keys_match()`) before swapping; a failed reload leaves the previously active certificate
   serving both existing and new connections. There is no `SIGHUP` or other automatic trigger — reload is an
   explicit `WireServer` method call only, cutting the last item DECISIONS-NEEDED-#6 flagged as optionally
   cuttable down to what the plan actually needed rather than adding a signal-handling surface nothing asked
   for.

### Decision

Implemented options 1, 3, 4, 5, and 6 as described above. `SERVER_CAPABILITIES` is no longer a bare constant on
the connection path: `CLIENT_SSL` is advertised only when `WireServerConfig::tls` is configured, computed
per-connection from `Shared`. An `SSLRequest` is decoded strictly as a fixed 32-byte payload
(`decode_ssl_request`, rejecting any other length) — never as a truncated `HandshakeResponse41` — and after the
TLS handshake completes, a full second `HandshakeResponse41` is read over the new TLS stream and *its*
capability flags are used for everything downstream; the pre-TLS `SSLRequest`'s flags are never trusted beyond
the `CLIENT_SSL` bit itself. `require_secure_transport` rejects a plaintext login before `verify_credentials`/
`authenticate_session` ever runs. Compression negotiates zstd (`CLIENT_ZSTD_COMPRESSION_ALGORITHM =
0x0400_0000`, verified against `mysql_common`'s constant, not the value in an earlier phase-plan draft) over
zlib (`CLIENT_COMPRESS = 0x0000_0020`) when the client offers both and the server has `compression_enabled`;
the server clamps a client-requested zstd level to `1..=3` (`response.zstd_compression_level.unwrap_or(3).clamp(1, 3)`)
rather than passing through the full MySQL 1-5 range. Payloads at or below 50 bytes (`MIN_COMPRESS_LENGTH`,
MySQL's own default) are sent uncompressed (`uncompressed_length = 0`) rather than paying compression overhead
for no gain. `WireClient`/`RemoteClient` gained a symmetric client-side `TlsMode` (`Disabled` / `Required {
ca_cert: PathBuf, server_name: Option<String> }` / `InsecureSkipVerifyDoNotUseInProduction`, explicitly named
per panel feedback — `Required` always performs the upgrade and fails closed, there is no silent
"try TLS, fall back to plaintext" mode) and `CompressionMode` (`Disabled` / `Zlib` / `Zstd { level: u8 }`).

### Consequences

- No on-disk, WAL, catalog, or MVCC format changed; this ADR is transport-layer-only (`htap-wire::{tls,
  compression}`, `crates/htapd`, `htap-client`/`htap-wire::client`).
- TLS's raw-socket handshake loop (`perform_tls_handshake`, driving `rustls::ServerConnection::complete_io`
  directly against the accepted `TcpStream` before a `Conn` exists) is a bespoke polling loop parallel to, not
  reusing, the existing stop-flag-polling `server::read()` helper — flagged in the plan as needing extra
  review because a bug here could hang shutdown or busy-loop; covered by
  `test_wire_shutdown_force_closes_idle_tls_connection`.
- A decompression bomb is bounded by three independent limits (byte-count cap, exact-length check, codec
  window cap), not by trusting any single header field, per the panel's explicit rejection of allocating off
  a declared length.
- `htapd --tls-cert`/`--tls-key` must be supplied together; `--require-secure-transport` without `--tls-cert`/
  `--tls-key` (or the `HTAPD_TLS_CERT`/`HTAPD_TLS_KEY` env equivalents) fails `WireServer::start` outright
  rather than silently serving plaintext.
- No `SIGHUP`-triggered reload, no `caching_sha2_password`-over-TLS story beyond what ADR-019's
  `AuthSwitchRequest` machinery already provides, and the zstd level clamp (`1..=3`, not MySQL's full `1..=5`)
  are documented as current gaps in `docs/LIMITATIONS.md`, not silently under-implemented.

### How to reverse it

TLS and compression are each independently revertible without touching the other or any on-disk format:
removing `crates/htap-wire/src/tls.rs`'s `ReloadableCertResolver`/`perform_tls_handshake` and the `Conn::Tls`
variant collapses `Conn` back to a thin `TcpStream` wrapper (ADR-016's original shape); removing
`crates/htap-wire/src/compression.rs` and the `ServerStream::Compressed` variant returns to always-plaintext
framing. Neither touches `htap-sql`, `htap-catalog`, `htap-rowstore`, or `htap-txn`.

### Test Evidence

- `crates/htap-wire/tests/tls.rs` (11 tests): `test_wire_tls_handshake_round_trip`,
  `test_wire_require_secure_transport_rejects_plaintext_login`,
  `test_wire_client_tls_required_against_non_tls_server_fails`, `test_wire_tls_ca_mismatch_rejected`,
  `test_mysql_crate_driver_interop_over_tls`, `test_wire_shutdown_force_closes_idle_tls_connection`,
  `test_wire_tls_invalid_cert_path_fails_start`, `test_wire_tls_start_with_mismatched_cert_and_key_fails`,
  `test_wire_tls_cert_reload_serves_new_cert_to_new_connections`,
  `test_wire_tls_cert_reload_failure_keeps_old_cert`,
  `test_wire_tls_cert_reload_mismatched_key_rejected_keeps_old_cert`.
- `crates/htap-wire/tests/compression.rs` (10 tests): `test_wire_compression_zlib_round_trip`,
  `test_wire_compression_zstd_round_trip`, `test_wire_compression_zstd_level_22_round_trip`,
  `test_wire_compression_large_payload_over_16mb_round_trip`,
  `test_wire_compression_decompression_bomb_closes_connection`,
  `test_wire_compression_disabled_by_server_config`,
  `test_compression_request_to_disabled_server_uses_uncompressed_connection`,
  `test_change_user_over_compressed_connection`, `test_mysql_crate_driver_interop_with_compression`,
  `test_mysql_crate_driver_interop_with_tls_and_compression`.
- `crates/htap-wire/src/compression.rs` unit tests: `test_below_threshold_round_trip`,
  `test_zlib_above_threshold_round_trip`, `test_zstd_above_threshold_round_trip`, and further round-trip/
  resumable-read/oversize-rejection cases in the same module.
- `crates/htap-wire/src/handshake.rs::decode_ssl_request` unit tests (exact-32-byte acceptance, 31/33-byte
  rejection).
- `crates/htap-wire/src/server.rs` unit tests covering `advertised_capabilities`/`CLIENT_SSL` toggling with
  TLS configuration.

### Fix pass: 16 MiB zstd window, stricter framing invariants

A follow-up fix pass corrected the zstd decoder window and hardened the compressed-stream framing beyond what
the original decision implemented:

- **Window cap corrected to 16 MiB.** The zstd window cap is `window_log_max(24)` (16 MiB), not
  `window_log_max(27)` (128 MiB) as originally implemented and originally documented above — a frame whose
  window log exceeds 16 MiB is rejected (`test_zstd_window_larger_than_16mb_is_rejected`).
- **A framing error latches the stream permanently failed.** A bad sequence id, a truncated or corrupt frame,
  or an empty raw frame now sets a `failed` flag on the `CompressedStream`; every subsequent `read` call fails
  immediately rather than attempting to resynchronize on framing that is no longer trustworthy. A read timeout
  (`io::ErrorKind::TimedOut`) from the underlying transport does **not** latch the stream — it resumes on the
  next call, including mid-header and mid-payload for both zlib and zstd
  (`test_zstd_read_resumes_after_timeout_in_header`, `test_zstd_read_resumes_after_timeout_in_payload`).
- **The sequence counter advances only on a match.** A mismatched compressed-packet sequence id is rejected
  outright rather than resynchronized to the value actually received (`test_sequence_id_mismatch_rejected`).
- **Zero-length raw frames are rejected.** A raw (`uncompressed_length == 0`) frame with an empty payload is
  now an error instead of silently producing zero decoded bytes (`test_empty_raw_frame_rejected`).
- **Decompression output is capped at the frame's own declared length, not a fixed bound.** The `.take()` cap
  during decode is `uncompressed_length + 1` (the frame's own declared length), so decoding stops the instant
  one byte past *that* frame's declared length would be produced — in addition to the pre-existing upfront
  rejection of a declared length exceeding the connection's configured maximum, and the exact-length check
  after decode.

`docs/ARCHITECTURE.md`'s "TLS and compression (Phase 12)" section and `docs/PROGRESS.md`'s Phase 12 row are
corrected to match; both previously stated `window_log_max(27)`/128 MiB.

Cross-references: ADR-016 (hand-written synchronous wire protocol — TLS/compression layer under the same
thread-per-connection design, per-connection `Conn`/`ServerStream` replacing the bare `TcpStream`), ADR-019
(the `verify_credentials`/capability-struct seams this ADR consumes), ADR-021 (per-user accounts — compression
and TLS both activate strictly after the authentication this ADR hands off to).

---

## ADR-021: Catalog-backed per-user accounts and privileges (`HTAPCAT1` format v3), `accounts_initialized` bootstrap latch, existence-masking privilege errors

`Status: Accepted`
`Date: 2026-09-18`

### Context

ADR-016 through ADR-019 all deferred per-user ACL, leaving `htapd` with one shared `--password` and an
unchecked username. The user explicitly lifted this deferred item for Phase 12. The design had to add a
credential/privilege model without weakening any durability invariant in CLAUDE.md (one MVCC version domain,
`DurablePending`/R5 unchanged) and without letting an authorization check leak table existence through an
error-message oracle. Consulted a panel (`reasoner` + `gemini`) and `architect` per the phase plan; the panel
flagged five load-bearing corrections to the original draft (folding accounts into the existing catalog CAS,
a scramble/response-based `authenticate_session` signature rather than a plaintext-password one, a stable
`AccountId` rather than raw-username-keyed grants, an `accounts_initialized` one-shot latch rather than an
`accounts.is_empty()` check, and existence-masking privilege errors placed inside `check_privileges` itself
rather than threading `Principal` into account-agnostic `htap_sql::bind()`).

### Options considered

1. **Fold `accounts: Vec<Account>`, `grants: Vec<Grant>`, and `accounts_initialized: bool` into the existing
   `CatalogSnapshot` and its single CAS, bumping `HTAPCAT1` to format version 3, rather than a separate
   envelope/file.** **Chosen** — both panel members and `architect` converged on this: `DROP TABLE`'s grant
   cleanup (removing every `Grant{scope: Table(dropped_id), ..}` row) must be atomic with the table removal
   itself, which a separate envelope with its own CAS could not guarantee without a second, unrelated
   consistency mechanism. Precedent: the v1-to-v2 `id_high_water` bump (ADR-017) already established the
   pattern (old files decode with defaults via `#[serde(default)]`, an old binary refuses a newer file). Unlike
   that bump, this build's `LEGACY_FORMAT_VERSION` stayed at `1` rather than advancing to `2` — a v1, v2, or v3
   payload all decode under `FORMAT_VERSION = 3` (`(LEGACY_FORMAT_VERSION..=FORMAT_VERSION).contains(&version)`),
   because unlike the v1 payload's genuinely different `id_high_water`-omission handling, a v1 or v2 payload
   omitting `accounts`/`grants`/`accounts_initialized` needs no special-cased fallback logic beyond
   `#[serde(default)]` — there is no legacy-omission case to guard against the way there was for
   `id_high_water`, so widening the accepted window costs nothing extra in decode-path complexity.
2. **A stable `AccountId(u64)` (same newtype pattern as `TableId`), with `Grant::account: AccountId`, not a raw
   username string.** **Chosen** per the panel's finding (c): prevents a dropped-then-recreated username from
   silently resurrecting a stale grant that was never explicitly re-issued to the new account.
3. **`accounts_initialized: bool` one-shot latch, checked and set exactly once by `bootstrap_root_account` at
   `WireServer::start`, never re-checked against `accounts.is_empty()`.** **Chosen** per the panel's finding
   (d): an `is_empty()` check cannot distinguish "fresh install" from "every account was deliberately dropped"
   from "mid-migration," and would resurrect `root` after an administrator intentionally removed every account.
   The catalog CAS (`compare_and_set`) additionally rejects any successor snapshot where `accounts_initialized`
   would regress from `true` to `false`, and any successor whose account id high-water mark would regress —
   the same regression-guard pattern ADR-017 established for `id_high_water`. Recovery from a full lockout is
   exclusively the embedded `LocalServer::execute` API (already an unchecked implicit superuser, no wire
   exposure), analogous to `--skip-grant-tables`; there is no wire-reachable break-glass path, by design.
4. **`authenticate_session(username, scramble, auth_response)` — the challenge and response, never a plaintext
   password.** **Chosen**, correcting the original draft per the panel's finding (a): `mysql_native_password`
   never gives the server a plaintext password to compare, only a client-side hash of one. Verification is
   `verify_native_password_hash(scramble, stored_hash, auth_response)` against the stored
   `SHA1(SHA1(password))` double-hash, never needing the plaintext at verify time. On any failure — unknown
   user, locked account, or wrong password — the caller sees exactly one `HtapError::PermissionDenied("access
   denied")`, never a variant that would let a client enumerate valid usernames.
5. **No delegation: `CREATE/ALTER/DROP USER`, `GRANT`, `REVOKE` all require `Principal::Superuser` outright;
   `WITH GRANT OPTION` parses (a disclosed vendor patch to `sqlparser`) but is rejected at bind time as
   `HtapError::Unsupported` rather than persisted as an inert bit; only the `%` host is accepted, a
   `'user'@'host'` with `host != "%"` is a bind-time `Unsupported` rejection.** **Chosen** per the panel's
   finding (b), which flagged the original draft's self-contradiction between "account management is
   superuser-only" and "`WITH GRANT OPTION` governs delegation" — resolved by cutting delegation entirely
   rather than half-implementing it.
6. **Existence-masking privilege errors, applied uniformly inside `check_privileges` for every statement
   kind.** A principal holding *zero* privileges on a table (not just missing the one a statement needs) sees
   `HtapError::NotFound` — the identical error a genuinely absent table would produce — for every statement
   kind including `DROP TABLE`/`ALTER TABLE`, not just reads; a principal holding *some* privilege but not the
   required one sees the new `HtapError::PermissionDenied` (MySQL 1142, uniformly, not 1227). **Chosen** over
   the panel's alternative (finding (f): threading `Principal` into `htap_sql::bind()` itself so a table could
   report `NotFound` before ever being resolved) because that would add a new dependency edge from `htap-sql`
   to account/grant types for no benefit `check_privileges`-side masking doesn't already give, at the cost of
   `DROP TABLE`/`ALTER TABLE` on a real-but-invisible table also reporting "doesn't exist" — matching real
   MySQL's own object-visibility behavior, not a new problem this ADR introduces.
7. **`mysql_native_password` only, no `caching_sha2_password`.** Same call as ADR-019 reconfirmed the third
   time by `architect`: MySQL 8.4 clients still support the plugin opt-in and this server's existing
   `AuthSwitchRequest` machinery (ADR-016/019) is exactly the fallback path a modern client needs; MySQL 9.x
   client tooling that removed the plugin entirely cannot connect, documented as a limitation rather than
   silently broken.
8. **Password hashes redacted from `Debug` output; catalog file mode `0600` on Unix.** `Account` has a manual
   `Debug` impl printing `password_hash: "<redacted>"` so a hash never reaches logs via `{:?}`; `architect`
   flagged that the catalog file now carries offline-crackable `SHA1(SHA1(password))` material for the first
   time, so `atomic_publish`'s temp file is created with `OpenOptionsExt::mode(0o600)` before the rename that
   publishes it (Unix only; no equivalent primitive exists cross-platform).

### Decision

Implemented options 1-8. `PrivilegeSet` is a hand-rolled `u16` bitflag type (`SELECT, INSERT, UPDATE, DELETE,
CREATE, DROP, ALTER` — no `GRANT_OPTION` bit, since option 5 rejects grant delegation outright rather than
persisting an inert one) with no new `bitflags` crate dependency. `GRANT`/`REVOKE` scope binds `ON *.*` and
`ON htap.*` (the single database this server reports) to `GrantScope::Global`; any other named database is
`NotFound`; `ON tbl`/`ON htap.tbl` binds to `GrantScope::Table`, resolved against the catalog at bind time (a
non-existent table is `NotFound` — table-existence disclosure to a superuser-only statement is acceptable,
since only a superuser reaches `execute_grant`). A `Global` grant is treated as "every table" when
`check_privileges` unions an account's applicable grants (`GRANT SELECT ON *.*` behaves like real MySQL).
`CREATE TABLE` by a non-superuser holding a global `CREATE` grant does **not** auto-grant privileges on the
newly created table (matches MySQL; documented explicitly, not an oversight) — the creator needs a subsequent
`GRANT` or a `Global` grant to see the table it just made. Enforcement (`htap-server::privilege::check_privileges`)
runs in two passes under the same `execution_lock`-held catalog snapshot as bind+dispatch: a pre-bind
visibility check (masking schema-probing errors before the AST is even bound) and a full privilege check after
bind, both re-resolved from the just-loaded snapshot on every statement — authorization state is never cached
across statements, so a mid-session `REVOKE` or account lock takes effect on the very next statement. `SHOW
TABLES` filters its result set to tables the caller holds at least one privilege on; `PREPARE` applies the same
existence-masking visibility check via an AST table walker (following CTE definition order so a CTE cannot
mask a real base-table reference) before resolving output-schema metadata, and `EXECUTE` re-checks privileges
independently since a `REVOKE` may have landed between `PREPARE` and `EXECUTE`; a multi-statement batch and
`COM_STMT_EXECUTE` both inherit enforcement because both still funnel through `Session::execute_statement`
(ADR-018/019's existing single entry point — no new bypass). `LocalServer::execute` (no session) does not call
`check_privileges` at all — it is the pre-existing, wire-unreachable implicit-superuser embedded path, and
this ADR does not change that. `execute_drop_user`/`execute_drop_table` both remove every `Grant` row scoped
to the dropped account/table in the same CAS as the drop; dropping the last remaining superuser account is
rejected (`HtapError::InvalidArgument`) as a cheap extra guard against self-inflicted total lockout (the
embedded break-glass path still exists regardless). `--password`/`HTAPD_PASSWORD` becomes a bootstrap seed
only: `WireServer::start` calls `bootstrap_root_account` once, before accepting connections; if the catalog
already has `accounts_initialized == true`, it adopts the existing `root` account (or logs a warning,
`BootstrapReport::config_password_matches_root == Some(false)`, if a still-configured password no longer
matches the stored hash) rather than resurrecting or overwriting it.

### Consequences

- `HTAPCAT1` format version bumped 2 -> 3 (`crates/htap-catalog/src/local.rs`); `LEGACY_FORMAT_VERSION`
  remains `1`, so this build decodes v1, v2, and v3 catalogs, all with `#[serde(default)]` accounts/grants/
  `accounts_initialized` for anything older than v3. A v2-only (or v1-only) binary still refuses a v3 file,
  per the existing `version != FORMAT_VERSION && version != LEGACY_FORMAT_VERSION`-style fail-loud check.
- Password hashes are new security-sensitive material in the catalog file for the first time; mitigated by
  `0600` file permissions (Unix) and `Debug` redaction, but the hash itself is a plain `SHA1(SHA1(password))`
  with no per-account salt or KDF work factor — offline-crackable if the catalog file leaks. (A follow-up fix
  pass below made the comparison itself constant-time; the missing salt/KDF is unchanged and still
  documented in `docs/LIMITATIONS.md`, not silently accepted as equivalent to a modern KDF.)
- No roles, no delegated administration, no host matching beyond `%`, no `ACCOUNT LOCK`/`UNLOCK` SQL syntax
  (an account's `locked` field exists in the model and is enforced at authentication, but nothing sets it via
  SQL yet), and no `caching_sha2_password` remain documented gaps, not silent omissions.
- `check_privileges`'s existence-masking rule is a deliberate, user-visible error-message change from "a
  superuser always sees the real error" (the only principal that existed before this ADR): a non-superuser's
  `DROP TABLE`/`ALTER TABLE` on a real-but-invisible table now also reports `NotFound`. No existing test
  asserted different semantics here (there was no non-superuser principal before this ADR), so this is a new
  contract, not a regression.

### How to reverse it

The account/privilege model is additive and layered strictly above the pre-existing always-superuser
`LocalServer::execute`/`Session` execution paths: removing `crates/htap-server/src/privilege.rs`'s call site in
`Session::execute_statement`, the six new `BoundStatement` variants, and the `accounts`/`grants`/
`accounts_initialized` catalog fields would return to ADR-016's single-shared-password model without touching
the rowstore, colstore, or 2PC transaction paths. The `HTAPCAT1` v3 format bump is additive
(`#[serde(default)]`) and does not need reverting for old files to keep decoding.

### Test Evidence

- `crates/htap-catalog/tests/catalog_recovery.rs`: `test_catalog_v3_round_trip_with_accounts_and_grants`,
  `test_catalog_v2_envelope_decodes_with_empty_accounts`, `test_catalog_v2_envelope_without_account_fields_decodes`,
  `test_catalog_v3_rejects_dangling_grant`, `test_catalog_v3_rejects_duplicate_username`,
  `test_catalog_cas_rejects_accounts_initialized_regression`,
  `test_catalog_cas_rejects_regressing_account_high_water`, `test_catalog_future_version_rejected`,
  `test_catalog_file_permissions_restricted_after_publish`, `test_partition_alterations_preserve_account_state`.
- `crates/htap-catalog/src/model.rs` unit tests: `test_account_and_grant_validation_failures`,
  `test_allocate_account_and_account_debug_redaction`.
- `vendor/sqlparser/src/parser/mod.rs::tests::test_user_management_statements` (the disclosed vendor patch:
  `IDENTIFIED BY`, `'user'@'host'` in `DROP USER`, and `SHOW GRANTS`).
- `crates/htap-sql/tests/parse_bind.rs`: `test_bind_account_management_statements`,
  `test_bind_account_management_rejections`, `test_grant_table_vs_global_grantee_binding`,
  `test_grant_grantee_parsing_regression`.
- `crates/htap-server/tests/accounts.rs` (16 tests): `test_bootstrap_creates_root_when_uninitialized`,
  `test_bootstrap_is_idempotent`, `test_bootstrap_is_noop_after_root_dropped`,
  `test_bootstrap_reports_config_password_mismatch`, `test_cannot_drop_last_superuser`,
  `test_create_alter_drop_user_persists_across_reopen`, `test_create_user_duplicate_conflict_and_if_not_exists`,
  `test_create_user_sql_creates_an_account`, `test_drop_table_removes_table_grants_and_catalog_still_valid_after_reopen`,
  `test_drop_user_removes_its_grants`, `test_empty_password_accounts`,
  `test_grant_revoke_merge_and_remove_rows_and_show_grants_format`,
  `test_partition_alterations_preserve_accounts_and_grants`, `test_revoke_never_granted_is_noop`,
  `test_table_scoped_grant_after_create_user`, `test_account_debug_output_redacts_password_hash`.
- `crates/htap-server/tests/bootstrap.rs::bootstrap_adopts_existing_root_account`.
- `crates/htap-server/tests/privileges.rs` (20 tests): `test_account_ddl_requires_superuser`,
  `test_account_without_grants_gets_identical_error_to_missing_table`,
  `test_check_statement_visible_allows_placeholders`, `test_check_statement_visible_masks_invisible_table`,
  `test_create_table_requires_global_create_and_creator_gets_no_implicit_privileges`,
  `test_delete_requires_delete`, `test_global_grant_applies_to_all_tables`,
  `test_join_with_ungranted_table_is_masked`, `test_locked_or_dropped_account_mid_session_fails_closed`,
  `test_prebind_visibility_masks_alter_table_targets`,
  `test_prebind_visibility_masks_schema_errors_and_cte_self_references`,
  `test_prepared_style_execute_statement_rechecks_privileges`,
  `test_revoke_takes_effect_on_next_statement_in_same_session`,
  `test_select_grant_allows_point_analytic_and_general_query_but_not_insert`,
  `test_show_grants_self_allowed_other_denied`, `test_show_grants_without_for_uses_calling_account`,
  `test_show_tables_filtered_for_account`, `test_subquery_and_cte_referencing_ungranted_table_masked`,
  `test_superuser_session_unrestricted`, `test_update_requires_update_and_select_when_where_present`.
- `crates/htap-server/tests/session.rs`: `test_authenticate_session_accepts_account_without_password`,
  `test_authenticate_session_locked_account_rejected`, `test_authenticate_session_rejects_unknown_account`,
  `test_authenticate_session_rejects_wrong_password`, `test_authenticate_session_returns_account_principal`,
  `test_authenticate_session_unknown_user_same_error_as_wrong_password`,
  `test_change_user_can_switch_between_authenticated_accounts`,
  `test_change_user_failure_preserves_existing_principal_and_state`,
  `test_change_user_resets_session_state_before_switching_principal`,
  `test_change_user_switches_to_authenticated_account`, `test_account_ddl_rejected_inside_open_transaction`.
- `crates/htap-wire/tests/accounts.rs` (10 tests): `test_mysql_crate_login_as_catalog_account`,
  `test_wire_change_user_to_different_account_switches_privileges`, `test_wire_execute_after_revoke_denied`,
  `test_wire_login_uses_catalog_account_not_shared_password`, `test_wire_prepare_masks_invisible_table`,
  `test_wire_prepare_with_placeholders_works_for_granted_table`,
  `test_wire_privileges_enforced_in_multi_statement_batch`, `test_wire_privileges_enforced_over_text_protocol`,
  `test_wire_root_login_with_bootstrap_password`, `test_wire_unknown_user_and_wrong_password_same_error`.
- `crates/htap-common/src/error.rs`/`crates/htap-wire/src/error_map.rs` unit tests covering
  `HtapError::PermissionDenied` -> MySQL 1142/`42000` and the reverse client-side mapping.

### Fix pass: constant-time comparison, timing-safe failure paths, earlier account-statement rejection, stricter v3 decode, and one consistent `PREPARE` snapshot

A follow-up fix pass closed several gaps left open by the original decision above:

- **Constant-time hash comparison.** `htap_common::password::constant_time_eq_20` (XOR-and-OR over all 20
  bytes, no early exit) replaces the plain `==` byte comparison in `verify_native_password_hash` and in the
  empty-password acceptance check. The hash itself is still an unsalted `SHA1(SHA1(password))` with no KDF
  work factor and remains offline-crackable if the catalog file leaks (unchanged from "Consequences" above) —
  only the comparison step is now constant-time.
- **Timing-safe authentication failure paths.** `authenticate_principal` now runs the same dummy
  `verify_native_password_hash` call (against a fixed dummy hash) on the unknown-user, locked-account, and
  no-password-account/non-empty-response paths that a genuine wrong-password attempt runs, so the time taken
  by a failed login does not itself reveal whether the attempted username exists.
- **Earlier rejection of account statements.** `check_statement_visible` now rejects `CREATE/ALTER/DROP USER`,
  `GRANT`, and `REVOKE` from a non-superuser with `PermissionDenied` before the statement is ever bound,
  closing a gap where binding first (and only checking superuser status in `check_privileges` afterward) could
  let `GRANT`/`REVOKE` double as a schema-probing oracle. The same pre-bind visibility walker
  (`referenced_table_names`) was extended to cover `ALTER TABLE` and `DROP TABLE` targets, not just
  `SELECT`/`INSERT`/`UPDATE`/`DELETE`
  (`crates/htap-server/tests/privileges.rs::test_prebind_visibility_masks_alter_table_targets`, and
  `test_account_ddl_requires_superuser` extended to assert `GRANT`/`REVOKE` against a missing table and an
  existing-but-invisible table produce identical errors).
- **Strict v3 catalog decode.** A payload tagged format version 3 must actually contain `accounts`, `grants`,
  `accounts_initialized`, and `id_high_water.account`; a v3-tagged payload missing any of them is now rejected
  as `HtapError::Corruption` rather than silently decoding with empty defaults. v1 and v2 payloads are
  unaffected and still decode via `#[serde(default)]`, since they were never expected to carry these fields
  (`crates/htap-catalog/tests/catalog_recovery.rs::test_catalog_v3_payload_missing_security_fields_is_rejected`).
- **`PREPARE` uses one catalog snapshot for both the visibility check and schema resolution.**
  `Session::check_statement_visible` now returns the `CatalogSnapshot` it checked against, and
  `htap-wire::server`'s `COM_STMT_PREPARE` handler reuses that same snapshot for
  `htap_sql::resolve_prepare_output_schema` instead of loading the catalog a second time, so a concurrent
  `GRANT`/`DROP TABLE` cannot land between the visibility check and schema resolution within one `PREPARE`
  call.
- **`SET @x = expr` visibility check now runs under `execution_lock`.** `Session::eval_scalar_expr` takes the
  server's execution lock before loading the catalog and calling `check_statement_visible`, matching every
  other statement path instead of checking visibility outside the lock other statements hold.
- **Vendored `sqlparser` prints `IDENTIFIED BY` passwords as escaped, quoted literals.** The `Display` impl
  for `CREATE/ALTER USER ... IDENTIFIED BY '<password>'` now round-trips a password containing a single quote
  or backslash instead of emitting an unescaped literal
  (`vendor/sqlparser/src/ast/mod.rs::tests::test_user_management_password_display_escaping`).

`docs/LIMITATIONS.md` and `docs/PROGRESS.md`'s Phase 12 rows are corrected to remove the "non-constant-time
hash comparison" limitation; the missing-salt/KDF gap is unchanged and remains documented.

Cross-references: ADR-017 (the `id_high_water`/format-version-bump precedent this ADR's v2-to-v3 bump follows),
ADR-018 (`Session::execute_statement` as the single enforcement point, reused unchanged as the privilege-check
call site), ADR-020 (TLS/compression activate strictly after the authentication this ADR replaces
`verify_credentials` with).

---

## ADR-022: Depth-1 correlated subqueries with a bounded execution callback; a purely structural `JoinTree` with bind-time offset rebasing, flat-lowering, and a mandatory flat/tree differential test

`Status: Accepted`
`Date: 2026-09-20`

### Context

Phase 9 (ADR-017) deliberately deferred window functions, correlated subqueries, `FULL OUTER`/`NATURAL`/
`USING` joins, parenthesized nested join trees, recursive CTEs, `EXCEPT`/`INTERSECT`, `GROUP BY`/`ORDER BY`
ordinals, filtered `DELETE`, `TRUNCATE`, `INSERT ... SELECT`, and integer `DIV`. The user explicitly lifted
all of these for Phase 13, on top of the same general query executor ADR-017 built
(`htap-sql::{query, expr, binder_query}`, `htap-server::query_exec`, `Route::Query`), without weakening R5
(point lookups structurally bypass the analytical engine), without a new on-disk format or WAL record, and
without a cost-based join optimizer (explicitly out of scope — see CLAUDE.md's scope rule). Of the items
lifted, two required genuinely new execution machinery and are the subject of this ADR: correlated subqueries
(a new htap-sql -> htap-server callback boundary) and arbitrary parenthesized/outer join trees (a second join
executor alongside the existing flat one). The remaining Phase 13 items (`GROUP BY`/`ORDER BY` ordinals,
integer `DIV`, `EXCEPT`/`INTERSECT`, `DELETE` by filter, `TRUNCATE`, `INSERT ... SELECT`, `NATURAL`/`USING`
coalescing, `WITH RECURSIVE`, window functions) are additive extensions of ADR-017's existing binder/executor
stages and did not require a separate design decision beyond what is recorded in `docs/PROGRESS.md`.

Two consultation rounds (`reasoner`, `cx/gpt-5.6-sol`) plus one `architect` ADR-level check were run
specifically because the main session overrode the researcher's initial, more conservative recommendation to
narrow `NATURAL`/`USING` coalescing and general nested join trees; both are recorded below as the panel's
corrections to the first draft, not as the first draft itself.

### Options considered — correlated subqueries

1. **Unbounded-depth correlation, resolving a name against any ancestor scope.** Rejected: matching MySQL's
   own de facto behavior here means a reference that *should* be a bind error (skipping past an intermediate
   scope) can instead silently resolve against a grandparent, which is exactly the "silent mis-resolution"
   class of bug CLAUDE.md's evidence rule exists to prevent. It also means every subquery-binding function has
   to thread an ever-growing, flattened list of ancestor slots, compounding the risk.
2. **Depth-1 correlation: a child subquery's binder sees only its immediate parent's own slots, never a
   concatenated ancestor chain; a name that would need to skip a level is a specific bind error
   ("correlated subqueries may only reference the immediately enclosing query"), not a generic unknown-column
   error.** **Chosen.** This is sound by construction — each binding level only ever threads "my immediate
   parent's own slots," so a subquery nested inside an otherwise-uncorrelated subquery of a grandparent still
   binds correctly against *its own* immediate parent, without ever being able to reach past it. Confirmed by
   both panel rounds to cover every cited TPC-H correlated shape (Q2, Q4, Q17, Q20-Q22) used to motivate lifting
   this deferral.
3. **A new `Expr::CorrelatedColumnRef` variant, distinct from `Expr::ColumnRef`, carrying an offset into the
   immediate parent's own row layout.** **Chosen** over reusing `ColumnRef` with a sentinel: keeping the two
   variants distinct in traversal/type-inference/column-reference-collection means a correlated reference can
   never be silently treated as an ordinary local column (e.g., during predicate pushdown or hash-key
   extraction), and the compiler's exhaustive-match requirement forces every such site to make an explicit
   choice about how to handle it.
4. **Execution: a `SubqueryRunner` callback trait in `htap-sql::expr` (subquery index + outer-row slice only,
   no storage type in the signature), implemented in `htap-server::query_exec` by recursively calling the same
   query executor against the referenced subquery, reusing the statement's single pinned `Snapshot`/write-set
   overlay.** **Chosen** over threading a storage handle into `htap-sql` (would create a `htap-sql -> htap-
   server` dependency the crate boundaries in `docs/ARCHITECTURE.md` forbid) or precomputing every subquery
   once up front the way an *uncorrelated* subquery already is (impossible for a correlated one, since its
   result depends on the current outer row). The existing precomputed-once path for uncorrelated subqueries is
   left byte-for-byte unchanged; only a subquery flagged `correlated` at bind time takes the callback path.
5. **A per-statement `SubqueryBudget` (total invocation cap and re-entrant nesting-depth cap), independent of
   the recursive-CTE cap, erroring the same way (`HtapError::InvalidArgument`) once exceeded.** **Chosen**:
   without an independent bound, a pathological query (a correlated subquery whose own body re-triggers
   another correlated evaluation) has no structural reason to terminate the way the recursive-CTE loop's own
   iteration counter does; `crates/htap-server/src/query_exec.rs` constructs the runtime budget as
   `SubqueryBudget::new(10_000, 20)` (10,000 total invocations, 20 re-entrant nesting levels), and
   `crates/htap-sql/src/expr.rs` unit-tests the budget type in isolation with a fake `SubqueryRunner` that
   always recurses, proving it hits a bounded error rather than a stack overflow or hang.
6. **In an aggregate query, a correlated subquery in `HAVING`/the projection may only correlate on the outer
   query's own `GROUP BY` keys.** **Chosen**, closing a trap both panel rounds flagged: without this check, a
   correlated subquery inside an aggregate context could silently read an arbitrary representative row of a
   group instead of a value that is actually constant across the group. The existing grouped-expression
   validator (which already treats a subquery reference as an opaque leaf) now looks up the referenced
   subquery's recorded outer-correlation list and requires every one of those outer-side references to itself
   be a `GROUP BY` key, rejecting with a message naming the offending column and clause otherwise.

### Options considered — general join trees (`JoinTree`)

1. **Extend the existing flat `Vec<JoinSpec>` representation to carry an explicit nesting marker instead of
   adding a new type.** Rejected: `FULL OUTER`/`NATURAL`/`USING` coalescing needs each join node to carry a
   *visible schema* that is itself a merge of its two children's visible schemas (not "accumulated flat slots
   so far vs. one new physical slot," which is all the flat representation can express), and an arbitrarily
   parenthesized tree (e.g. `a LEFT JOIN (b JOIN c ON ...) ON ...`) has no faithful flat encoding at all.
2. **A cost-based join-tree IR shared with a future join-reordering optimizer.** Rejected for this phase, per
   both panel rounds and `architect`: cost-based optimization is explicitly out of scope (CLAUDE.md), and nothing
   about *this* tree's contract (built once, directly and always, 1:1 with the SQL's own parenthesization, never
   rewritten/reordered/commuted/cost-estimated) is compatible with what a reordering optimizer would need to do
   to it later. `architect`'s explicit condition for accepting this design was that it stay out of that
   territory permanently — Phase 14, if it ever adds join reordering, needs its own IR, not a repurposing of
   this one.
3. **A small, purely structural `JoinTree` (`Slot(usize)` | `Join{kind, left, right, on}`), built directly and
   always from the parsed `FROM` clause (comma-separated items fold in as unconditional-cross nodes in
   encounter order), used as the single binding source of truth for every query — with a total, pure
   `lower_to_flat` function computing, at bind time, whether the tree is purely left-deep (every join node's
   right child a bare slot leaf; the join **kind** at each step unrestricted) and, when it is, populating
   today's existing flat fields exactly as before.** **Chosen.** Both panel rounds explicitly warned against
   maintaining two divergent binding code paths for "flat" and "nested" shapes — building the tree unconditionally
   and *deriving* the flat form from it (rather than binding flat-or-tree depending on shape) means there is only
   ever one source of truth for what the query's join structure actually is.
4. **Offset scheme for a tree node's own `ON` condition: (a) a runtime offset-bias parameter threaded through
   `Expr::eval`, (b) a stable-identity/layout-descriptor scheme decoupling logical columns from physical
   offsets entirely, or (c) a bind-time-only, regenerate-every-time rebased copy of the `ON` expression (offsets
   shifted down by the subtree's own base offset, computable from already-known slot widths), with
   `Expr::ColumnRef.offset` itself and `Expr::eval`/`EvalContext` completely unchanged.** **(c) chosen.** Option
   (a) would touch the expression evaluator itself for a bind-time-derivable fact; option (b) was independently
   flagged by both panel rounds as "architecturally cleaner but bigger" and deliberately deferred to a future
   phase rather than taken on here. `architect`'s binding condition on accepting (c): the rebased copy must
   never be persisted or cached as a second source of truth — it is documented as regenerable fresh, every time
   it is needed, purely from the node's own slot range, so it structurally cannot drift from the canonical
   globally-offset tree. Top-level `WHERE`/`GROUP BY`/projection/`HAVING` keep using the original globally-offset
   expressions unchanged throughout, since by the time the whole tree has joined up to the full select body,
   every surviving row is already the full global-width concatenation exactly as before this ADR.
5. **`NATURAL`/`USING` column coalescing: (a) a single flat `COALESCE` over all physical positions sharing a
   name, or (b) a per-join-node recursive merge rule where the visible/merged expression at the node
   introducing a shared column is `INNER`/`LEFT` -> the left operand's own (possibly already-merged) expression,
   `RIGHT` -> the right operand's, `FULL` -> `COALESCE` of both.** **(b) chosen**, correcting the researcher's
   original draft ((a)) after the panel showed it does not compose correctly through a chained
   `a FULL JOIN b USING(id) FULL JOIN c USING(id)`: the second predicate must compare against the already-merged
   `COALESCE(a.id, b.id)`, not a raw physical column, and the final visible value is
   `COALESCE(a.id, b.id, c.id)`. No new `Expr` variant was needed; the existing `ScalarFn::Coalesce` is reused
   unchanged. Unqualified name resolution searches the *whole* visible schema (physical + merged) and requires
   a *unique* match rather than preferring a coalesced column — the panel showed a "coalesced-preferred, else
   fall back" rule would hide a real ambiguity. Merged-column nullability is derived per-operand, from each
   side's own nullability *as of just before this join's own null-extension* (not generically re-derived from
   post-padding physical nullability, which the panel showed can be needlessly conservative — e.g. a `FULL`
   join of two `NOT NULL` keys has a non-NULL merged key even though both physical positions individually
   become nullable after that join's own padding). `NATURAL`'s empty-intersection case degrades to `CROSS` only
   for `INNER`; `LEFT`/`RIGHT`/`FULL` keep their own kind with an always-true condition, because literal `CROSS`
   would silently change empty-right-side null-preservation behavior.
6. **A mandatory, permanent differential test proving the flat executor and the new recursive `JoinTree`
   evaluator produce byte-identical output (rows and ordering) for every documented lowerable shape, shipped as
   ordinary `cargo test` coverage, not a one-time manual migration check.** **Chosen** per `architect`'s
   explicit, non-optional condition — both panel rounds independently flagged silent divergence between the two
   executors as the single biggest risk this design introduces, since a future edit to one path has no
   structural reason to also update the other.
7. **Generalizing `join_rows` (the existing two-input hash-join-with-nested-loop-residual primitive) to take
   explicit left/right widths as parameters, derived from the actual input row sets, rather than assuming the
   right side is exactly one physical slot.** **Chosen**, required for the same primitive to serve both the
   existing flat one-physical-slot-at-a-time case and the new recursive evaluator's arbitrary-width intermediate
   relations; done in the same pass as fixing the pre-existing `Int32`/`Int64`/`Timestamp` -> `Float64` hash-key
   widening, which could collide two distinct integers above `2^53` — exact-integer values now keep an
   integer-keyed representation and only widen to `Float64` when one side of the same key position is actually
   `Float64`.

### Decision

1. **`Expr::CorrelatedColumnRef` + `SubqueryRunner` + `SubqueryBudget`.** A correlated subquery binds one level
   deep only; a name requiring a grandparent lookup is the specific bind error "correlated subqueries may only
   reference the immediately enclosing query" (`crates/htap-sql/src/binder_query.rs`). Execution threads a
   `SubqueryRunner` callback and the current outer row through every per-row evaluation site (`WHERE` filter,
   projection, `HAVING`) via the shared `EvalContext`, reusing the statement's one pinned `Snapshot`/write-set
   overlay — never re-pinning or advancing it — so a correlated subquery reads exactly the same committed-plus-
   buffered view as the rest of the statement (the snapshot/self-write invariant is unchanged from ADR-018). A
   per-statement `SubqueryBudget` (`crates/htap-server/src/query_exec.rs`: `SubqueryBudget::new(10_000, 20)`)
   bounds total invocations and re-entrant nesting depth, erroring `HtapError::InvalidArgument` and naming
   which cap fired, mirroring the recursive-CTE cap's error convention (decision 3 below) without sharing its
   counter. A correlated subquery inside an aggregate `HAVING`/projection may only correlate on the outer
   query's `GROUP BY` keys.
2. **`query::JoinTree` as the single binding source of truth, with a total/pure `lower_to_flat` deriving the
   existing flat form when eligible.** `bind_table_with_joins`/`bind_table_factor` always build a `JoinTree`
   mirroring the parsed `FROM` clause 1:1; `TableFactor::NestedJoin` recurses into a tree node; `JoinOperator::
   FullOuter` maps to a new `JoinKind::Full`. Nullability marking generalizes to "every physical slot covered by
   the relevant subtree" (sound because every tree node covers a contiguous, declaration-order-preserving slot
   range). Each node's `ON` condition gets a derived, bind-time-only rebased copy (offsets shifted by the
   subtree's own base offset) that is regenerated fresh whenever needed and never cached; `Expr::ColumnRef.
   offset` and `Expr::eval`/`EvalContext` are untouched. `lower_to_flat` returns `Some(Vec<JoinSpec>)` exactly
   when every join node's right child is a bare slot leaf (any join kind), in which case the executor's existing
   flat path runs byte-for-byte unchanged for every pre-Phase-13 query and every plain non-nested Phase-13
   query; a genuinely nested/parenthesized shape uses the new recursive evaluator instead.
3. **`NATURAL`/`USING` real column coalescing, per-join-node, reusing `ScalarFn::Coalesce`.** Built bottom-up
   on the same `JoinTree`, each node's visible schema merges its children's per O1's rule (see options above);
   unqualified resolution requires a unique match over the whole visible schema; `SELECT *` expands merged/
   common columns first (left-input order), then each side's remaining physical columns; qualified `t.col`/
   `t.*` are unaffected, always resolving through the existing per-slot physical path.
4. **Recursive `JoinTree` evaluator plus the mandatory flat/tree differential test.** A leaf materializes
   exactly as today; an internal node recursively evaluates its children into locally-compact row sets,
   evaluates its regenerated-fresh rebased `ON` condition via the generalized `join_rows`, and returns its own
   compact row set — top-level `WHERE`/`GROUP BY`/projection/`HAVING` continue over the final, full-width row
   set unchanged. `run_select` picks this path only when `SelectBody`'s flat fields are empty.
   `crates/htap-server/tests/query_exec.rs::test_flat_and_tree_join_evaluators_match_for_lowerable_queries` is
   the permanent, mandatory regression gate proving the two executors agree, run as ordinary `cargo test -p
   htap-server` coverage.
5. **`WITH RECURSIVE` caps, for symmetry with decision 1's subquery budget:** a `QueryBody::RecursiveQueryBody`
   with a working-table placeholder slot (`TableSlot::WorkingTableSlot`, resolved by the executor to the
   previous iteration's rows, never a real table lookup) executes a fixed-point loop bounded by three
   independent, explicitly named caps in `crates/htap-server/src/query_exec.rs`: `MAX_RECURSIVE_ITERATIONS =
   1_000`, `MAX_RECURSIVE_ROWS = 1_000_000`, `MAX_RECURSIVE_BYTES = 256 * 1024 * 1024` (approximate), each
   erroring `HtapError::InvalidArgument` naming exactly which cap fired — the same error convention decision 1
   uses for the independent correlated-subquery budget, deliberately not sharing a counter with it.

### Consequences

- Correlated subqueries and general join trees both cross a boundary that did not exist before Phase 13 (a
  htap-sql -> htap-server execution callback, and a second join executor), so both were storage-reviewed per
  CLAUDE.md's trigger list even though neither touches an on-disk format, WAL record, or catalog envelope.
- The `SubqueryRunner`/`VariableLookup` pattern now has two structurally similar callback traits on
  `EvalContext`; a third such trait should trigger consolidating them into one seam rather than growing the
  field list further (documented on the evaluation context itself).
- `JoinTree` genuinely reopens part of ADR-017's original "no join-tree IR" framing, but is categorically
  different from a cost-based optimizer's join-tree: it mirrors the SQL's own parenthesization 1:1, never
  reorders/commutes/estimates cost, and `lower_to_flat`'s eligibility predicate is exhaustively enumerated
  (purely left-deep in shape, any join kind), not inferred heuristically — Phase 14, if it adds join
  reordering, must treat this predicate as a per-candidate, re-callable check rather than a one-shot bind-time
  decision baked into `SelectBody`, per `architect`'s note (not actionable in this phase).
- Two join executors now exist permanently; the differential test in decision 4 is not a one-time migration
  check — it must keep passing for every future join-related change, and a future edit to one executor without
  the other should fail it immediately.
- Memory growth is unchanged in kind from ADR-017 (everything in-memory, unbounded except where explicitly
  capped): window partitions, recursive-CTE working tables, join hash tables, and now the recursive `JoinTree`
  evaluator's per-node materialized row sets are all fully in-memory; only the recursive-CTE and
  correlated-subquery paths have hard caps (decisions 1 and 5).
- `INSERT ... SELECT`, `DELETE` by filter, and `TRUNCATE` (implemented as an unfiltered `DELETE`, additive to
  this ADR's scope, see `docs/PROGRESS.md`) all read the statement's single pinned snapshot and build every
  mutation before the one `commit_or_buffer` call, preserving the snapshot/self-write (Halloween-problem)
  invariant unchanged from ADR-017/018 — verified for a self-referencing `INSERT INTO t SELECT ... FROM t` by
  `crates/htap-server/tests/query_exec.rs::test_insert_select_halloween`.

### How to reverse it

Correlated subqueries can be reverted independently of the join-tree work: removing `SubqueryRunner`/
`SubqueryBudget`/`Expr::CorrelatedColumnRef` and rejecting a subquery flagged `correlated` at bind time restores
Phase 9's uncorrelated-only behavior without touching joins, recursion, or windows. The join-tree work is
additive at the type level (`JoinTree`/`VisibleColumn` are new types alongside the unchanged flat `JoinSpec`
representation) but `lower_to_flat` and the tree-construction change in `bind_table_with_joins` are the single
source of truth for every join binding going forward; reverting to flat-only binding would mean re-adding the
old rejection for `NestedJoin`/`FullOuter`/`NATURAL`/`USING`, which Phase 13 removed. Neither touches the
rowstore, colstore, or catalog on-disk formats.

### Test Evidence

- Correlated subqueries — binder: `crates/htap-sql/tests/query_bind.rs::test_correlated_subquery_binding_and_depth_limit`,
  `::test_correlated_subquery_grouped_context`, `::test_correlated_subquery_binding_is_case_insensitive`.
- Correlated subqueries — callback/caps unit tests: `crates/htap-sql/src/expr.rs::subquery_runner_trait_and_correlation_eval`,
  `::subquery_invocation_and_nesting_caps`.
- Correlated subqueries — execution: `crates/htap-server/tests/query_exec.rs::test_correlated_exists_in_where`,
  `::test_correlated_in_subquery`, `::test_correlated_subquery_nested_two_levels`,
  `::test_correlated_subquery_caps_fire_during_execution`, `::test_correlated_subquery_column_outer_pruning_and_having`,
  `::test_correlated_scalar_subquery_in_select`, `::test_correlated_subquery_in_having_grouped`,
  `::test_correlated_subquery_self_reference`, `::test_correlated_subquery_across_row_column_converting_and_partitions`.
- Correlated subqueries — sessions/transactions: `crates/htap-server/tests/session.rs::test_correlated_subquery_sees_uncommitted_session_writes`.
- Join tree — binder: `crates/htap-sql/tests/query_bind.rs::test_join_tree_lowering_and_nested_groups`,
  `::test_natural_using_coalescing_all_join_kinds`, `::test_natural_using_ambiguity_and_errors`.
- Join tree — execution: `crates/htap-server/tests/query_exec.rs::test_nested_join_groups_and_full_join_execute`,
  `::test_nested_join_groups_using_qualified_access_derived_tables_and_subqueries`,
  `::test_flat_and_tree_join_evaluators_match_for_lowerable_queries` (the mandatory differential harness),
  `::test_using_and_natural_joins_merge_columns`, `::test_hash_join_preserves_large_integer_keys` (decision on
  option 7, the hash-key widening fix).
- Join tree — sessions: `crates/htap-server/tests/session.rs::test_read_your_own_writes_nested_join_group`.
- Recursive CTE caps: `crates/htap-server/tests/query_exec.rs::test_recursive_cte_iteration_and_row_cap_bounded_time`,
  `::test_recursive_cte_large_working_set`, `::test_recursive_cte_counting_and_hierarchy_traversal`,
  `::test_recursive_cte_union_distinct_vs_all_semantics`.

Cross-references: ADR-017 (the general query executor and R5's syntactic shape gate this ADR extends without
weakening), ADR-018 (the snapshot-pinning/self-write-visibility invariant every new write and re-execution
path in this ADR preserves), ADR-004/008/009 (durability invariants — unaffected: no on-disk envelope, WAL
record, or catalog format changed anywhere in this ADR).

---

## Note: Stage R — shared durability primitives module in `htap-common` (not a numbered ADR)

`Status: Accepted`
`Date: 2026-09-20`

This is recorded as a short note rather than a numbered ADR: stage R is a pure code-organization refactor.
It introduces no new on-disk format, no new format version, no new accepted-version set, no CRC-scope change,
and no new durability or recovery semantics anywhere — the only in-scope question was how to remove
duplication, not what the system's behavior should be. `docs/PROBLEMS.md` P1 already recorded the "why" (the
same crash-safety code, hand-written seven times, has to be right seven times for the CLAUDE.md durability
invariants to hold); this note records the "how," for a cold reader who needs to check the diff against a
contract rather than re-derive it.

**What moved.** One shared module, `htap-common::{fs, envelope, bytecursor}`:
- `fs::{sync_dir, write_new_tmp_file, remove_file_if_exists, fsync_file, atomic_publish}` — a three-tier API
  (bare `sync_dir`; a `write_new_tmp_file` write-and-fsync leaf; an `atomic_publish` convenience composing
  write, rename, and directory sync for the common "one buffer, one dir sync" shape), deliberately **not** one
  universal "publish a file" function with a growing options struct, because several call sites (the movement
  tablet package's DATA/MANIFEST pair, `htap-convert`'s two-directory-sync `write_atomic`) have branch-specific
  error typing and side effects (`mover.fail_job`) that a single generic wrapper would blur or erase. Those
  sites use only the `write_new_tmp_file` leaf and keep their own rename/cleanup/dir-sync control flow.
- `envelope::{encode_envelope, decode_envelope, encode_bare_frame, EnvelopeError, SizeCheckMode}` — one
  magic+version+length+CRC32C codec for the six whole-file envelopes, returning a structured `EnvelopeError`
  enum (not a formatted string) so each of the six call sites can still produce its own historical message
  text and none of the existing message-asserting tests had to change. `SizeCheckMode` has no default: a new
  envelope's author must pick `TruncatedThenTrailing` or `ExactMatch` explicitly rather than silently
  inheriting one.
- `bytecursor::ByteReader` — a bounds-checked little-endian cursor replacing hand-rolled
  `if cursor+N > len { Corruption } else { from_le_bytes(...) }` sites in `htap-colstore` and
  `htap-rowstore/src/sst.rs`, returning a structured error and leaving the reader's position unchanged on a
  failed read instead of relying on `unwrap()`.

**What deliberately did not move.**
- `htap-rowstore/src/wal.rs::fsync_dir`: a sixth `sync_dir`-shaped function found during the migration audit
  (`docs/PROBLEMS.md` P1 previously counted five). Unlike the five migrated copies, it is not `cfg(unix)`-gated
  — it runs unconditionally on every platform. Migrating it onto the shared `sync_dir` (Unix-only real fsync,
  silent no-op elsewhere) or unifying the other five onto its unconditional behavior would each change
  non-Unix behavior that no test in this repository exercises. Approved by the validator as an explicit,
  documented non-migration rather than a silent choice; a comment at its definition says so.
  **Superseded (Phase 20, ADR-034):** this function no longer exists. The power-loss audit this note anticipated is
  Phase 20; it migrated the WAL's directory syncs onto the shared shim in commit `4891f13` (`refactor(rowstore)`),
  so `htap-rowstore/src/wal.rs` has no private `fsync_dir`. The cost is the one this note foresaw: directory sync is now
  a no-op off Unix (`docs/LIMITATIONS.md`, unix-only durability).
- `HTAPMAN1`'s single exact-size check (`SizeCheckMode::ExactMatch`): the other five envelopes report a
  truncated and a trailing-bytes input as two distinct messages; the rowstore manifest reports both as one
  "size mismatch" message. `HTAPMAN1` gates WAL/SST manifest recovery, the single most crash-critical file in
  the repository, so this stage preserved its narrower check exactly rather than unifying it onto the more
  permissive two-message shape.
- Stale-temp-file removal before encoding (catalog only), per-branch `HtapError`-vs-raw-`io::Error` typing
  feeding `mover.fail_job` (movement tablet manifest), and which directory syncs are propagated versus
  swallowed (`let _ = ...`) at each site: all are load-bearing, call-site-specific choices about what counts
  as a hard failure on that path, not incidental duplication, so they were left untouched rather than folded
  into a shared default.

**One accepted message-text change.** `Manifest::read_from_file`'s trailing-probe error text changed, because
delegating its bounded-read logic to the shared `read_file_exact_bounded` (which already existed and was the
one real near-duplicate found in this audit) produces slightly different wording on that one untested
corruption path. No test asserted the old text; a new test (`test_manifest_read_from_file_trailing_content_rejected`)
pins the new one so it cannot silently drift again.

**Consequences.** Seven crates now share one implementation of each migrated durability primitive. (As written on
2026-09-20 this sentence excepted the deliberately retained `htap-rowstore/src/wal.rs::fsync_dir`; that function was
migrated in Phase 20, commit `4891f13`, see the superseded note above, so there is again one directory-sync
implementation.) A future format
(Phase 15 journal compaction, Phase 16 multiprocess ownership) gets `atomic_publish`/`decode_envelope`/
`ByteReader` for free instead of a new hand-rolled copy. The power-loss safety audit this note anticipated had one
implementation per primitive to audit instead of five to seven, and it is Phase 20 (ADR-034). No on-disk format changed, so this note does
not update the storage-format compatibility table's "versions accepted"/"CRC scope" columns — see the table in
`docs/ARCHITECTURE.md` ("Dual-format storage") for the format facts themselves, and `docs/PROGRESS.md`'s
Stage R row for the full test list.

**How to reverse it.** Inline each shared function back into its call sites; every call site's own control
flow (error typing, cleanup policy, dir-sync propagation) was preserved unchanged, so reversal is a pure
mechanical inlining with no format or behavior consequence.

---

## ADR-023: Inline catalog statistics with a format bump; an `htap-sql`-hosted cost-based optimizer stage; non-durable spill scratch; keeping two binder entry points (Option B) for P2

`Status: Accepted`
`Date: 2026-09-21`

### Context

`docs/LIMITATIONS.md` and ADR-017's Consequences section listed "no cost-based planning, no spilling, and no
worker-pool parallelism" as current, intentional gaps on the general query path (`Route::Query`), deferred
again at Phase 13 (ADR-022). The task explicitly lifted exactly these three items for the general executor
only — `Route::RowstorePointRead` and `Route::OlapScan` stay byte-for-byte unchanged — plus a partial pass at
`docs/PROBLEMS.md` P2 (parallel paths through the query layer), without a new on-disk WAL/rowstore format and
without weakening R5.

### Options considered — where statistics live

1. **A separate sidecar file per table (e.g. `<root>/catalog/stats/<table_id>`).** Rejected: it would add a
   second durable artifact that has to be kept consistent with the catalog CAS (a table rename/drop racing an
   in-flight `ANALYZE` publish would need its own two-file consistency protocol), duplicating machinery the
   catalog's own CAS already provides for free.
2. **Inline `stats: Option<TableStats>` on `TableDescriptor`, published by the existing single-CAS `HTAPCAT1`
   envelope, bumping `FORMAT_VERSION` 3 -> 4.** **Chosen.** One CAS already atomically publishes every other
   piece of table metadata (schema, partitioning, accounts/grants since ADR-021); statistics are just another
   additive, `#[serde(default)]` field following the same `id_high_water`/`partitioning` precedent — a v3
   payload decodes with `stats: None` on every table with no new decode-time branching, and a v4 payload with a
   corrupted CRC is rejected the same way every other envelope fault is (`EnvelopeError::ChecksumMismatch` ->
   `HtapError::Corruption`).
3. **Refresh policy: explicit-only (`ANALYZE TABLE`), no automatic trigger, default-heuristic fallback when
   absent.** **Chosen** over auto-refresh-on-write or a background analyze job: either would need its own
   scheduling/backoff/staleness-tracking machinery disproportionate to a local MVP, and CLAUDE.md's scope rule
   already excludes autonomous background scheduling elsewhere (conversion ticks). `estimate_row_count`/
   `estimate_equality_selectivity`/`estimate_range_selectivity` each report whether the number came from real
   statistics or a hard-coded default (`EstimateSource::{Stats,Default}`), surfaced through `EXPLAIN`, so an
   operator can always tell which one fired rather than the optimizer silently pretending a default is a
   measurement. Statistics never expire automatically and are never checked for staleness against subsequent
   writes — a documented gap, not fixed here (see `docs/LIMITATIONS.md`).
4. **Statistics are table-level only (aggregated across every partition in one `ANALYZE TABLE`), not
   per-partition.** **Chosen**: a per-partition breakdown would roughly multiply the exact-distinct-count
   memory cost by partition count for no benefit the optimizer currently uses (partition pruning is still
   purely bound/range-based, unaffected by statistics — see `docs/PARTITIONS.md`), and would require deciding
   how to merge per-partition distinct sets at the cap boundary, a genuinely harder problem than exact
   distinct counting itself.
5. **Distinct count: exact via `HashSet<Value>`/`BTreeSet<Value>`, capped at a configurable limit
   (`LocalServer::with_analyze_distinct_limit`, default 200,000); past the cap, drop the accumulated set and
   report `distinct_count: None` rather than an approximation.** **Chosen** over an approximate sketch
   (HyperLogLog or similar): an approximate structure is a second statistics format to design, encode, and
   keep correct, and an *exact* count that stops being available past a cap (rather than becoming silently
   wrong) matches the project's existing "fail loud, not silently approximate" posture (e.g. `EnvelopeError`
   over guessed recovery). `min`/`max`/`null_count` are always collected unconditionally regardless of the
   cap, since they cost O(1) additional state per row, not O(distinct values).

### Options considered — the optimizer stage

1. **Host the optimizer in `htap-server` alongside `query_exec`.** Rejected: cost estimation, predicate-atom
   classification, and join-tree reordering operate purely on `htap-sql::query::{BoundQuery, SelectBody,
   JoinTree}` types and need no storage access beyond a small `StatsLookup` trait object — putting it in
   `htap-server` would create a needless `htap-server -> htap-sql -> htap-server` type round-trip and blur the
   crate boundary ADR-022's `SubqueryRunner` design explicitly kept clean (`htap-sql` must never depend on
   `htap-server`).
2. **A storage-agnostic `htap-sql::optimize` stage: a `StatsLookup` trait supplied by the caller (implemented
   as a thin `CatalogStats` wrapper in `htap-server`), cost estimators reporting `EstimateSource`, a
   predicate-atom inventory with `origin`/`mobility` (not a "lowest common ancestor slot" heuristic — see
   below), subset-DP join reordering up to 8 relations per connected `Inner`/`Cross` component with a greedy
   cheapest-extension fallback above that, and a `PhysicalQuery` entry point (`optimize::optimize`) that is
   total (never panics) and never changes the result set.** **Chosen.**
3. **Predicate placement model: "lowest common slot" (attach a predicate to the lowest join node whose subtree
   covers every slot the predicate references) vs. predicate atoms carrying explicit provenance
   (`origin: On(join_id) | Where | Using | Natural`, `mobility: FreelyMovableWithinComponent |
   PinnedToJoin(join_id) | PostJoinOnly`).** **Chosen the provenance model.** "Lowest common slot" alone gets
   the two classic outer-join traps wrong: a `WHERE`-clause conjunct referencing a null-supplying slot under an
   outer join, or a conjunct inside a `LEFT JOIN ... ON` that touches the null-supplying side, both change the
   query's result set if freely relocated, even though a slot-coverage rule alone would place them the same
   way it places a safe inner-join conjunct. `PredicateAtom.mobility` is computed once per atom from its
   origin and the join structure it sits under, and every reorder respects it.
4. **Always-on predicate-conservation validator, not a debug-only assertion.** **Chosen**: a silently dropped
   or duplicated predicate during reordering is a wrong-result bug, the single most dangerous failure class
   for this feature (see the researcher's original plan's "Risks" section). `validate_predicate_conservation`
   runs unconditionally after every optimization attempt and asserts the multiset of original predicate ids
   equals exactly the attached-plus-retained set; on failure, `optimize_select` falls back to the identity
   `PhysicalQuery` (`HtapError::Internal` is never raised to the caller as a query failure — a suboptimal but
   correct plan is always preferred over erroring out or risking a wrong result).
5. **Enable the optimizer by default for every `Route::Query` execution** (`ExecContext::optimization_mode`
   defaults to `OptimizationMode::Enabled`, both `dispatch_bound` call sites in `htap-server/src/lib.rs`
   construct it enabled), keeping an internal `Disabled` mode reachable only for the differential test's
   identity-baseline comparison. **Chosen** over an opt-in flag: the optimizer only ever produces a
   conservation-validated, differentially-tested plan or falls back to the untouched original, so there is no
   "unsafe by default" argument for gating it behind a setting the way, say, `--require-secure-transport` is
   gated for a genuinely behavior-changing security posture.

### Options considered — spill files

1. **Reuse `htap_common::envelope` (the shared six-envelope magic+version+CRC32C format) for spill files.**
   Rejected: that contract exists specifically for durable, recoverable files that must be correctly readable
   after a crash and across format versions — every property spill scratch explicitly does not need. Reusing
   it would either weaken the envelope's own guarantees by special-casing an exemption, or force spill files to
   pay for CRC computation and a version-range decision they get no benefit from.
2. **A minimal, explicitly non-durable spill header** (`crates/htap-server/src/spill.rs`: magic `HTAPSPIL`,
   kind tag, statement id, operator kind, format tag; no CRC, no fsync, no accepted-version-range contract) **+
   a `SpillDir` that tracks every file it creates and removes them all, best-effort, on `Drop`.** **Chosen.**
   Framing still goes through `htap_common::bytecursor::ByteReader` for the length-prefixed row records (so a
   truncated or adversarially large length field fails cleanly with a structured error rather than an
   unchecked allocation or panic), but the file as a whole carries no durability contract, matching decision 8
   below.
3. **Location and lifecycle: `<data-root>/spill/<statement-id>/`, removed on `SpillDir::drop` at the end of
   the statement, and swept in full (`remove_dir_all("<data-root>/spill")`) on every `LocalServer::open`, after
   the root `ProcessLock` is acquired.** **Chosen** over leaving abandoned spill directories from a crashed
   process for a future statement to stumble over, or requiring an operator to clean them manually; sweeping
   under the exclusive root lock is safe because no other statement can be mid-spill while the lock is held
   exclusively at open time.
4. **Spill scratch is never counted toward any durability guarantee, and is disclosed as such, not silently
   assumed safe.** **Chosen** (restating decision 2 as an explicit consequence): a process kill mid-spill
   leaves partial spill files that the next `LocalServer::open`'s sweep simply deletes; no in-flight query's
   result was ever considered committed or durable while spilling, so this has no bearing on the CLAUDE.md
   durability invariants (WAL/rowstore/catalog/manifest envelopes), which are entirely unrelated files.

### Options considered — P2 binder convergence

1. **Option A: unify `htap_sql::binder::bind` and `htap_sql::binder_query::bind_select_body` into one binder
   entry point with a post-bind classifier deciding the route.** Rejected, per the coordinator's explicit
   instruction and verified against ADR-017's own text before being accepted: ADR-017 deliberately rejected
   exactly this shape ("it would remove the purely structural separation R5 depends on... a single
   binder/executor for every `SELECT` shape makes... a runtime property... instead of a compile-time one").
   Option A would restore R5's guarantee only via the same kind of runtime check ADR-017 already considered
   and declined.
2. **Option B: keep both binder entry points, converge only the leaf-level sub-problems they solve
   independently (literal binding, cast-target mapping, type-compatibility/comparability checks, scalar-
   function signature checking, schema column lookup), plus executor-side unifications that do not touch
   binding at all (one `EvalContext` constructor, one join evaluator).** **Chosen**, for the reason above: a
   complete-PK point read is still decided by `is_narrow_select_shape` before any general-binder code runs at
   all, exactly as before this ADR — the guarantee stays in the code's shape, not a runtime branch, at the
   admitted cost of a less complete cleanup than Option A would have given.

**What this diff actually delivered under Option B, stated exactly so `docs/PROBLEMS.md` is not overclaimed.**
Delivered: an `EvalServices`/`EvalContext::new` (`htap_sql::eval_context!`) constructor replacing every
hand-built `EvalContext { .. }` literal across `crates/htap-server/src/{query_exec,lib,session}.rs`; one join
evaluator (`SelectBody.join_tree` is now always populated at bind time — `left_deep_join_tree` synthesizes a
tree from the flat `Vec<JoinSpec>` form when the binder builds one — and the separate flat-loop branch inside
`run_select` plus the old `SelectBody.joins`/`tree_only` fields were removed, leaving `evaluate_join_tree` as
the only join execution path); and the `#[allow(clippy::too_many_arguments)]` count in
`crates/htap-server/src/query_exec.rs` dropped to zero via context structs. **Not delivered in this diff: the
shared binder leaf-helper module** (literal binding, cast-target mapping, comparability checks, scalar-
function signature checks, schema column lookup) that decision 2 above scoped as Option B's actual P2
deliverable. `htap_sql::binder.rs` and `binder_query.rs` still each maintain their own copy of that
leaf-level logic; no `bind_helpers`-shaped module exists. This is recorded here as an open item, not folded
silently into "P2 partially fixed" without saying which part is still open — see `docs/PROBLEMS.md`'s Phase 14
update for the precise remaining scope.

### Consequences

- `HTAPCAT1` `FORMAT_VERSION` is 4; a version-3-or-earlier catalog still opens (`stats: None` on every table),
  and a version-4 payload with a bad CRC is rejected the same way every other envelope fault is. The
  storage-format compatibility table in `docs/ARCHITECTURE.md` reflects the new accepted range.
- `Route::RowstorePointRead` and `Route::OlapScan` never call `htap_sql::optimize::optimize` — verified
  directly (not just by matching output) in `crates/htap-server/tests/explain.rs`'s two R5-bypass tests.
- `ANALYZE TABLE` binds to `BoundStatement::AnalyzeTable`, classified as `Route::CatalogDdl` like every other
  DDL statement, and **is** included in `Session::is_ddl`'s match list: it is rejected by the "DDL inside an
  open transaction" gate exactly like `CREATE TABLE`/`DROP TABLE`/user-management DDL, the rejection does not
  poison the transaction, and `EXPLAIN ANALYZE` follows the transaction rules of the statement it actually
  executes (plain `EXPLAIN`, which only plans, remains permitted). Proven by
  `crates/htap-server/tests/session.rs::{test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives, test_explain_analyze_wrapping_ddl_rejected_inside_open_transaction, test_explain_analyze_wrapping_insert_rejected_inside_read_only_transaction}`
  and `crates/htap-server/tests/explain.rs::test_plain_explain_select_permitted_inside_open_transaction`. This
  was originally shipped as a disclosed gap (see the superseded wording this bullet replaces, still visible in
  git history) and was closed by the storage review described below, not by this ADR's original diff.
- Hash-join spilling is not structurally restricted to `INNER`/`CROSS` the way the *parallel* hash join is
  (`join_rows_inner`'s `allow_spill` path runs for any join kind whose `ON` clause yields at least one usable
  equi-key); only `INNER`-join spilling is exercised by a test in `crates/htap-server/tests/spill.rs`. This is
  recorded as an evidence gap, not a claim that outer-join spilling is proven correct.
- P2 (`docs/PROBLEMS.md`) moves from "fix in Phase 14" to "partially fixed in Phase 14": the executor-side
  duplication (hand-built `EvalContext`, two join executors, long parameter lists in `query_exec.rs`) is
  resolved; the binder-side leaf-helper duplication decision 2 above scoped as the actual P2 deliverable is
  not, and remains open for a future phase.
- **A storage review of this ADR's original diff found 13 defects, all fixed and covered by tests before this
  wording was written.** Most were narrower correctness/evidence gaps (including the `ANALYZE TABLE`
  transaction-gating gap closed above, and the spill scratch location and reader-buffering issues described in
  `docs/PROGRESS.md`'s Phase 14 row). Two are worth naming here as lessons for whoever next touches this code,
  not just a fix log entry: (1) the optimizer's predicate-atom reordering could move a predicate across an
  outer-join boundary and silently change the result set — exactly the class of bug decision 3 under "the
  optimizer stage" above was designed to prevent, so the conservation validator alone was not sufficient
  without also fixing the mobility classification itself; fixed and now covered by
  `crates/htap-sql/src/optimize.rs`'s pinning/conservation unit tests and
  `crates/htap-server/tests/differential.rs`'s two outer-join differential tests (see `docs/PROGRESS.md`). (2)
  The parallel `GROUP BY`/hash-join operators were unreachable dead code — the parallelism gate's precondition
  could never be satisfied in practice, so every "parallel" test was actually comparing the serial path against
  itself under a different worker-count setting; fixed by reworking the gate to inspect the expressions workers
  evaluate instead of requiring services to be absent, and now covered by
  `crates/htap-server/tests/parallel.rs` (including the new `test_group_by_with_variable_does_not_parallelize`
  negative case) using the test-telemetry `LocalServer::last_query_parallel_workers()` accessor to assert more
  than one worker actually ran.

### External-review round: scope clarification, per-operator telemetry, partition caps, and a second brick-risk close

A second, external-review pass over this ADR's diff found the memory budget's scope was under-specified and
two more brick-risk-shaped gaps, all fixed and covered by tests before this section was written:

- **Memory budget scope.** The budget and every spill path apply to `Route::Query` only. A single-table
  `SELECT` with `ORDER BY`, `GROUP BY`, or a plain aggregate and no join routes to `Route::OlapScan` instead,
  which has no memory budget and never spills — by design, since that narrow path predates this ADR and was
  never in scope for it. Several early spill tests were silently exercising the unbudgeted `Route::OlapScan`
  path (their `SELECT` had no join) and were rewritten to use a shape that actually reaches `Route::Query`.
  The budget itself only bounds each operator's own working memory (hash tables, sort runs, aggregate state,
  partition buffers); it does not bound the rows a non-pipelined executor materializes between operators — a
  disclosed, not hidden, limit of "memory budget," not "memory bound."
- **Spill telemetry became per-operator.** The original diff's spill telemetry was coarse enough that an
  `ORDER BY` spill test could pass because its `JOIN` spilled instead of its sort. `LocalServer` now exposes
  one accessor per operator kind (`last_query_hash_join_spilled`, `last_query_group_by_spilled`,
  `last_query_sort_spilled`, `last_query_distinct_spilled`, `last_query_set_operation_spilled`,
  `last_query_window_spilled`), and every spill test in `crates/htap-server/tests/spill.rs` now asserts its own
  named operator actually spilled.
- **Partition counts and depth, made explicit and bounded.** Hash-join spill partition count is sized from the
  input and the remaining budget, capped at 128 (windows share this cap, to bound open file descriptors and the
  writer buffers the budget doesn't count); `GROUP BY` and the set operators each spill into their own
  fixed 16 partitions (a separate constant from the hash-join cap, not shared machinery, and — unlike the
  hash-join/window cap — not scaled by input size or the remaining budget: an input much larger than roughly
  16x the budget fails with the memory-budget error instead of spilling successfully; budget-sized
  partitioning for `GROUP BY`/the set operators is a deferred improvement); window spilling
  hash-partitions by the `PARTITION BY` key and evaluates one window partition at a time (no `PARTITION BY` is
  a single partition). In every case, a partition that still doesn't fit — skew, or the partition-count cap —
  fails with the memory-budget error rather than recursing into a second spill level; an agent's attempt at
  recursive re-spill during this round caused a stack overflow and was reverted, which is why "one level, then
  fail" is now stated as a hard design constraint here, not an implementation detail that might change.
  `GROUP BY` spill additionally reserves each partition's rows as they are read back and releases that
  reservation before in-memory aggregation runs, to avoid double-charging the same bytes on the way in — but
  the rows themselves are still resident while the released charge is re-applied to the aggregate state, so
  peak memory during one partition's aggregation can approach about twice the budget. This is disclosed as an
  imprecision in the budget's accounting, not a correctness defect: the query still completes with the right
  answer, or fails cleanly if truly out of memory.
- **`EXPLAIN` on DDL, and catalog statistics get their own validation.** `EXPLAIN`/`EXPLAIN ANALYZE` on
  `CREATE TABLE` (and other DDL) now works, rather than being an unhandled or silently-wrong case. Catalog
  statistics (`TableStats`) are now structurally validated on every publish — column count, null count,
  distinct count, min/max type agreement, `min <= max`, and finiteness — with no `HTAPCAT1` format change.
- **Float overflow now errors instead of silently going non-finite.** `+`/`-`/`*`/`/` and `SUM`/`AVG` overflow
  return `"DOUBLE value is out of range"` (MySQL-compatible); division by zero is unchanged and still yields
  `NULL`. The motivating risk was concrete, not theoretical: `serde_json`, used for both row mutation payloads
  and catalog statistics, encodes a non-finite float as JSON `null`, which then fails to decode back into a
  non-`Option` float field. On the row path this was already caught before this round — confusingly, but
  safely — by `RowstoreParticipant::decode_payload` running inside 2PC `prepare`, i.e. before commit, so an
  undecodable mutation was refused rather than corrupting anything; the catalog statistics path had no
  equivalent incidental protection and was the real brick risk, now closed by rejecting non-finite bounds
  during `ANALYZE TABLE` and by the `TableStats` structural validation above. The narrow analytic scan path's
  own `SUM` accumulator (`crates/htap-server/src/olap.rs`) had no equivalent overflow check as of this
  writing; this was recorded as an unverified-beyond-code-inspection gap, not claimed as fixed — closed in
  the second external-review round below.
- **Two cost-quality-only weaknesses, confirmed not to affect correctness.** An external reviewer specifically
  checked whether the optimizer's leaf-cost and outer-join cardinality estimation weaknesses could change a
  result, not just a plan choice, and confirmed they cannot: outer joins are never reordered by the DP/greedy
  join-reordering step, and null-padding of an outer join's unmatched rows is independent of which side was
  chosen as the hash build side. Left open, and disclosed rather than fixed in this round: window evaluation
  carries every materialized column of the joined input into its spill partitions, not just the columns the
  query needs (measured 8 columns / ~424 B per row where 3 are needed) — a column-trimming improvement, not
  attempted here.

### Second external-review round: a float-decode precaution, non-finite coverage completed, and two disclosed limits

A second, independent storage review of this ADR's diff found one theoretical `serde_json` risk (not
demonstrated), confirmed the analytic path's `SUM` gap noted above was real, and flagged two more
non-blocking limitations:

- **`serde_json`'s `float_roundtrip` feature, enabled workspace-wide, is a precaution, not a bug fix.** The
  review's claim was that `serde_json`'s default ("best-effort precision") float parser could decode a stored
  `DOUBLE` one ULP off from the value that was encoded. Tested directly with the feature switched off, the
  default parser round-tripped 50,000 random finite bit patterns plus adjacent doubles, subnormals, the
  extremes, and `-0.0` bit-identically on this `serde_json` version — no drift was demonstrated. The feature
  is kept anyway because it guarantees exact round-trips regardless of `serde_json` version, at some cost in
  parse speed; there is no on-disk format change, only a parsing-library behavior. Covered by
  `crates/htap-common/src/types.rs::types::tests::test_serde_json_float64_roundtrips_bit_identically` (the
  committed bit-identity check, run with the feature enabled), the strengthened
  `crates/htap-server/tests/session_recovery.rs::test_double_values_round_trip_bit_identically_after_reopen`
  (bit-identity across a `LocalServer` close/reopen), and
  `crates/htap-server/tests/spill.rs::test_spilled_set_operations_and_distinct_preserve_awkward_doubles`
  (bit-identity through a spilled `DISTINCT`/set-operation round-trip).
- **Spilled set operations and `SELECT DISTINCT` emit rows by source index, not by re-decoding.** Both now
  look up the original row by the index recorded when it was written to the spill file, instead of keying a
  map by the decoded row. This makes their correctness independent of JSON round-trip exactness — hardening
  in response to the review's concern, not a fix for a demonstrated bug (the encode/decode path is exact per
  the point above regardless).
- **Spill partition cap tightened to 128 for hash join and window.** The cap introduced in the first
  external-review round (above) was 256; this round lowered it to 128 to bound open file descriptors and the
  writer buffers the memory budget does not itself count. `GROUP BY` and the set operators keep their
  separate, fixed-16-partition constant (see the partition-counts bullet above), which is now recorded there
  as a disclosed, not-yet-fixed limitation for large inputs.
- **Non-finite float coverage completed: `CAST` from a string, non-finite literals, and the analytic path's
  `SUM` all now return `"DOUBLE value is out of range"`.** `CAST('nan'/'inf'/'1e999' AS DOUBLE)` and
  out-of-range float literals (e.g. `1e400`) are rejected the same way `+`/`-`/`*`/`/`/`SUM`/`AVG` overflow
  already were; the analytic scan path's own `SUM` accumulator (flagged as an open, unverified gap in the
  first external-review round above) is now checked identically to the general executor's `SUM`. The
  transaction layer's pre-commit payload decode remains the confirmed reason a non-finite value could never
  become durable even before any of these checks existed. Covered by
  `crates/htap-server/tests/query_exec.rs::{test_non_finite_double_casts_and_literals_are_rejected,
  test_non_finite_cast_update_fails_at_statement_and_leaves_value_unchanged,
  test_analytic_float_sum_overflow_returns_out_of_range_error, test_float_sum_overflow_returns_out_of_range_error}`.
- **`ANALYZE`'s min/max bound reservation now reserves before swapping.** `AnalyzeAccumulator::replace_bound`
  reserves the replacement value's memory first and only then drops the old reservation and installs the new
  bound, so a failed reservation leaves the previous bound and its accounting exact instead of leaving a
  bound with no matching reservation. Covered by
  `crates/htap-server/src/analyze.rs::analyze::tests::replace_bound_preserves_existing_reservation_when_budget_is_exhausted`.
- **Disclosed, not fixed.** `ANALYZE TABLE` fully materializes each partition before reserving its memory, so
  usage can overshoot before the check runs, and a partition whose estimate lands above the budget fails
  `ANALYZE` outright. The abandoned-spill sweep at `LocalServer::open` only logs a failure to remove the
  directory rather than returning an error; a stale file left behind by a failed sweep can collide once with
  a statement id reused after a process restart (`SpillWriter::create` opens with `create_new(true)`), and
  that one statement fails with an `AlreadyExists` I/O error that heals on its own once that id has been
  consumed.

### Test Evidence

See `docs/PROGRESS.md`'s Phase 14 row for the full, verified test list (statistics, optimizer, `EXPLAIN`,
spilling, and parallelism, each cross-checked against `cargo test -p <crate> -- --list`). Cross-references:
ADR-017 (the general query executor and R5's syntactic shape gate this ADR extends without weakening), ADR-022
(the `JoinTree`/flat-lowering design this ADR's "one join evaluator" change supersedes — flat-lowering is now
always applied at bind time rather than being a separate fast path the executor chooses between), ADR-004/008/
009 (durability invariants — the only on-disk change in this ADR is the additive `HTAPCAT1` v3 -> v4 bump;
spill files are explicitly outside the durability contract by decision 2 above).

---

## ADR-024: Contiguous-run rowstore compaction with in-place splice; manifest watermarks that refuse to regress; lease-before-engine ordering; a checkpoint latch on any rewrite error; and DROP TABLE artifact reclamation via the same leases

`Status: Accepted`
`Date: 2026-09-22`

### Context

Three things were named `deferred` since Phase 4/ADR-004: `DROP TABLE` never physically reclaimed rowstore or
columnar bytes (`docs/LIMITATIONS.md`, `docs/ARCHITECTURE.md:185,192,1076`), the rowstore LSM never compacted
its SSTs, and `txn.journal` only ever grew, eventually blocking `LocalServer::open` once
`max_journal_size` is exceeded (`docs/LIMITATIONS.md`'s "no compaction" bullets). Phase 15 delivers a narrow
local slice of all three: `Engine::compact_once` (tier-selected, entry-count-bounded SST merging with an
MVCC-safe tombstone rule), a per-tablet reclaim/movement lease set that gates both compaction and `DROP
TABLE` artifact deletion, a `pending_reclaim` catalog entry (`HTAPCAT1` v5) that drives colstore/movement
cleanup and rowstore purge confirmation to completion across ticks, and `TransactionManager::checkpoint()`
(a new `HTAPTXC1` envelope) that compacts `txn.journal` in place. None of this is StarRocks's design translated
into Rust; StarRocks is read-only reference material never copied per `ATTRIBUTION.md`, and this ADR
re-derives each mechanism from this codebase's own existing invariants — the LSM's newest-layer-wins read
order, the single shared MVCC version domain (ADR-004), and the manager-wide decision lock 2PC already uses
(ADR-018).

An initial implementation was reviewed by a storage-review panel (`cx/gpt-5.6-terra-review` + `reasoner`) and
found one critical bug and several high/medium ones before any of this shipped; the decisions below are the
post-review design, not the first draft, and the bug is recorded here because it shapes decision 1.

### Decision 1 — Compaction selects and replaces only a *contiguous* run of the manifest's SST list, spliced in place

**The bug the panel found.** The first implementation selected a compaction tier by entry-count regardless of
manifest position, then published the merged output by *prepending* it to `read_state.ssts` (index 0) and to
the manifest. `Engine::get`/`scan_partition` resolve a key by returning from the first SST layer (searched
newest-first) that holds any version at or below the snapshot. Manifest order is the newest-first read order
by construction (`Engine`'s doc comment: "an ordered (newest-first) list of immutable on-disk `SstReader`s").
Prepending a merged run built from arbitrary, non-adjacent SSTs to index 0 makes it look newer than every SST
above it in the *old* order that were not selected — so a newer, unselected `Put` or `Delete` for the same key
sitting in one of those un-selected-but-now-apparently-older SSTs would silently lose to the compacted output.
This is a live-data resurrection bug, not a cosmetic one: it can un-delete a row.

**Decision.** `select_compaction_candidates` (and the new `explicit_sst_ids` path, decision 5 below) must pick
only SST ids that form one contiguous run in the manifest's current order — verified structurally in
`compact_once` by checking `commit_guard.manifest.ssts[input_start_index..input_end_index]`'s ids exactly equal
`input_sst_ids` before merging, and again against `read_state.ssts` at the same indices before swapping,
returning `HtapError::Corruption` if either check fails rather than silently proceeding. The merged output is
spliced into that same run's original start index in both the manifest's SST list and `read_state.ssts` — never
prepended — so every SST that was newer than the run stays newer, and every SST that was older stays older.
Forced dropped-partition selection (used by `DROP TABLE` reclaim) may pick a run of length 1 (a single isolated
SST); the normal tier-size minimum does not apply to it, since correctness (removing dropped-partition rows)
does not depend on batching. When a candidate set has gaps — a protected partition, an excluded SST, or an
explicit id list with holes — the caller compacts one contiguous sub-run at a time, over as many
`compact_once` calls as it takes; this is exactly decision 5's rationale.

**Why not the rejected alternative (max-across-layers merge).** `reasoner` and `cx/gpt-5.5`, consulted as a
panel, considered making the read path resolve ties by explicit per-version comparison across every layer
instead of first-hit-wins by position. Rejected: it would have to be threaded into every read path that
currently exits early on the first hit (`Engine::get`, `scan_partition`, the first-writer-wins prepare check),
turning an O(1) fast-path exit into an O(layers) scan for every read, permanently, to fix a compaction-only
bug. Keeping the invariant "manifest position is read priority, unconditionally" and fixing compaction to
respect it is strictly cheaper and does not touch the hot read path at all.

**Consequence.** A dropped partition scattered across several non-adjacent SSTs interleaved with live SSTs
purges over several passes, one contiguous sub-run (or singleton) per pass — see decision 5's regression test
naming this explicitly. A movement- or reclaim-leased tablet that happens to sit in the middle of an otherwise
compactable tier breaks that tier into two sub-runs around it, and only the sub-runs outside the lease make
progress that tick (decision 3's "shared keyspace" limitation, disclosed in `docs/LIMITATIONS.md`).

### Decision 2 — `HTAPMAN1` bumps to format v3 to carry two watermarks that only ever rise, refusing to publish a regression

**Committed-version high-water (`committed_version_high_water`).** Every manifest publish — flush or
compaction — sets it to `max(existing value, read_state.committed_version)` under `commit_lock`, in the same
atomic-publish call that changes the SST set. `Engine::open`'s recovered committed version is
`max(manifest high-water, every SST's max_version, every replayed WAL commit)` — the manifest term is added
without dropping the other two, so a manifest that is stale relative to the WAL (a crash between WAL commit
and the next flush) still recovers correctly; the manifest term exists specifically to survive the case a WAL
segment holding the last commit for an already-compacted, already-dropped partition gets garbage collected
(decision 4) before a flush would otherwise have recorded that version. **Gc low-water (`gc_low_water`).**
Every *actually publishing* compaction call sets it to `max(existing value, effective_gc_horizon)`, where
`effective_gc_horizon = input.gc_horizon.min(visible_version)` is the same clamped horizon used to decide
which versions collapse (an X-batch review fix: an earlier draft raised `gc_low_water` to the unclamped
`input.gc_horizon` but exempted the `u64::MAX` "collapse everything" sentinel from raising it at all, so a
`u64::MAX`-horizon compaction could collapse versions without the read floor ever rising to cover them — see
`docs/PROGRESS.md`'s Phase 15 row, X2 — and a follow-up Y-batch review found the X-batch fix itself clamped to
`committed_version`, not `visible_version`, which could raise `gc_low_water` above `visible_version` whenever
`apply_external` had committed ahead of what was published, rejecting every fresh snapshot; `committed_version`
still feeds `committed_version_high_water` above, unaffected) — a no-op pass (`compacted: false`) or a pure
preview never advances it.
`Engine::get`, `scan_partition`, and `prepare`'s
first-writer-wins check (whenever the snapshot carries a real version, not the `u64::MAX` sentinel used by
`apply_external`'s re-prepare and txn recovery replay) reject a snapshot below `gc_low_water` with a clear
error rather than silently returning collapsed, no-longer-fully-versioned data. **Both watermarks refuse to
regress**, mirroring `checkpoint::publish_checkpoint`'s precedent (ADR-below/decision 4): if a manifest publish
would lower either field from what is currently on disk, `Manifest::atomic_publish` returns an error instead
of writing the file. **Decode:** v1/v2 payloads derive the high-water exactly as `Engine::open` already did
before this phase and default `gc_low_water` to `Version::INITIAL`; a v3 payload cross-checks its external-apply
ledger's max version against `committed_version_high_water` at decode time, rejecting a v3 file where the
ledger is ahead of the watermark as `HtapError::Corruption` — that combination cannot arise from any code path
that writes v3, so seeing it means the file was hand-edited or corrupted in a structured way a raw CRC failure
would not catch. v1..=v3 all still decode; an unknown version is still hard-rejected.

**Why in the existing envelope, not a sibling file (architect consult).** A second `<rowstore>/GC` file would
let the SST-set edit and the watermark edit publish non-atomically — exactly the two-file consistency hazard
ADR-below/decision 4 rejected for `txn.checkpoint` vs. `txn.journal`, and the one the architect explicitly
flagged when consulted on this format choice. `HTAPMAN1` already publishes the whole SST set atomically via
`Manifest::atomic_publish`; adding two more `u64`-sized fields to the same payload costs nothing structurally
and keeps the "one manifest publish = one consistent view" invariant Phase 1 established.

**Sequencing invariant this decision depends on, stated explicitly:** a manifest publish that raises
`committed_version_high_water` to cover some version *V* must land before any WAL garbage collection that could
remove the last WAL evidence of *V*. This already held for ordinary flushes (`flush_locked` publishes the
manifest, then nothing GCs the WAL until a later, separate call); decision 3's `flush_roll_and_gc` preserves it
by construction (one method, one `commit_lock` acquisition, flush-then-roll-then-GC in that order, never two
public calls a caller could reorder or interleave).

### Decision 3 — Movement/reclaim leases are acquired before any engine call, all-or-nothing for `DROP TABLE`'s forced set and best-effort for the ordinary tiered pass; `flush_roll_and_gc` is one atomic engine method, not two

**The ordering rule.** `LocalDataMover`'s per-tablet lease set (`active_tablet_leases`, split into a movement
side and a `reclaim_tablet_leases` marker) is always acquired by the caller — `compaction_tick`, or a
movement job's `copy_from_csv_reader`/`clone_tablet`/etc. — *before* that caller touches rowstore or colstore
state for the tablets in question, and released only after that state has been touched and (for compaction)
published. Movement acquires its lease before tablet I/O and before its own commit lock (documented directly
in `LocalDataMover`'s doc comment); `compaction_tick` acquires reclaim leases (all-or-nothing for the
`pending_reclaim`-forced set via `try_acquire_reclaim_lease`, best-effort for the ordinary tier via
`acquire_reclaim_leases_best_effort`) before calling `Engine::preview_compaction_candidates`/`compact_once`,
never the reverse. This closes a real race two external reviewers (`reasoner`, `cx/gpt-5.5`) converged on
independently: a lease acquired *after* selecting candidate SSTs could still let a movement job start reading
a tablet mid-compaction, since selection and rewrite are not instantaneous. Lock ordering has no cycle to
prove safe here — the movement-lease mutex is never held while acquiring `commit_lock` or `execution_lock`,
and `compaction_tick` always acquires the lease first — but it is worth stating as an explicit invariant
(mirroring `engine.rs`'s own documented `commit_lock`-before-`read_state` rule) precisely because it is easy to
get backwards by accident in a future change.

**Why leases are non-durable (accepted, with a stated precondition).** The lease set lives only in
`LocalDataMover`'s in-process mutex; a crash loses every lease. This is sound for exactly one reason, stated
here because it stops being true the moment a future phase changes it: **a protection mechanism only needs to
outlive the reader it protects, and today, nothing in this codebase can survive the crash that would drop a
lease and still be reading the tablet that lease protected** — a movement job always re-resolves a fresh
snapshot on resume (`tablet.rs`/`export.rs`'s `options.pinned_version.map(Snapshot::new).unwrap_or_else(||
engine.snapshot())` pattern) rather than continuing a stale in-flight read across a restart. If a future phase
makes a movement job resumable against a *fixed, historical* pinned snapshot across a process restart, this
precondition breaks and the lease set must become durable (e.g. persisted in the job record) at that point —
this is a standing precondition to re-check before that feature ships, not merely a note.

**One atomic `flush_roll_and_gc`, not two composed public calls.** Purge confirmation (draining the WAL of a
dropped partition's last replayable rows before declaring `rowstore_purge_confirmed`) needs a flush, a WAL
segment roll, and a WAL GC to all happen under one `commit_lock` acquisition, in that order, so decision 2's
sequencing invariant cannot be violated by a caller flushing, releasing the lock, and rolling/GC-ing later
(during which window a concurrent commit could advance state the roll/GC would then act on inconsistently).
`Wal::gc` already refuses to remove the active segment and `Wal::roll_segment` was already private; this phase
adds one crate-private forced-roll entry point and one new public `Engine::flush_roll_and_gc()` that takes
`commit_lock` once and performs all three steps, rather than exposing `flush()` and a hypothetical
`roll_and_gc()` as two separate public methods a caller could call out of order or with an intervening commit.
A first implementation rolled unconditionally on every call and collided with an existing segment file on the
second call in the same tick (`AlreadyExists`); the fix picks a fresh segment id every call, keeping the
method idempotent under repeated invocation within one tick.

**Consequence, disclosed as low-medium severity, not fixed here:** because the WAL roll/GC step only runs when
purge confirmation actually calls it, a row belonging to an already-dropped, already-compacted-out partition
can in principle still be replayed from an un-rolled WAL segment after a crash, landing back in a memtable
under a partition id that is provably never reused (`IdHighWater`) and therefore unreachable by any live
query — a disk leak, not visible corruption. Decision 2's version-counter floor and this WAL-roll mechanism are
complementary, not redundant: one bounds what a *snapshot* can see, the other bounds what the *WAL replay on
restart* can resurrect.

### Decision 4 — `TransactionManager::checkpoint()` latches `RecoveryRequired` on *any* error from the journal rewrite step, unconditionally

**The problem.** Checkpointing publishes a new `txn.checkpoint` baseline (`HTAPTXC1`, magic already reserved in
`CLAUDE.md`), then atomically rewrites `txn.journal` to contain only the still-unresolved records, then reopens
the live `Journal` handle. Between "the rewrite's `atomic_publish` call returns" and "the handle is confirmed
reopened," several distinct failures are possible (the rename succeeds but the directory fsync fails; the
rename itself fails; the file is fine but reopening a fresh handle against it fails). A first pass only latched
`RecoveryRequired` on some of these paths and let the journal-open step happen with `?`, so an error partway
through could return `Err` to the caller while quietly leaving the manager holding a stale in-memory `Journal`
handle referencing a file that had already been replaced or partially replaced on disk — “unknown state”
returned to the caller as if it were an ordinary retryable error.

**Decision.** Any error from `atomic_publish`'s rewrite call, from that point's `#[cfg(test)]` fault-injection
hook, or from the subsequent `Journal::open_with_options` reopen call latches
`RecoveryCause::JournalIo` **before** attempting any cleanup, unconditionally — including the case where the
rewrite itself failed but a defensive re-open of the *old* handle (for FD safety only, never for trust) happens
to succeed. The rationale, stated in the code's own comment: "any rewrite error leaves the durability of the
replacement unknown, regardless of whether reopening the resulting path succeeds." Once latched, every other
commit is refused with `HtapError::RecoveryRequired` (the same manager-wide latch ADR-018 introduced for 2PC),
and only `recover()` (in-process, for a `JournalIo` cause specifically) or a full manager reopen clears it. A
directory-sync failure injected right after the rename step is the concrete regression test for this: the
manager must refuse further commits with `RecoveryRequired` from that point, and only a clean `recover()` (or
reopen) restores normal operation — never a bare retry of `checkpoint()` on the same, now-untrusted handle.

**Additional guards adopted alongside this fix (T2):** `checkpoint()` and `finalize_open()` both refuse
(`compacted: false`, not `Err`) before `recover()` has ever run — mirroring `recover()`'s own poisoned/latched
guard — since folding un-recovered records against a checkpoint baseline would be meaningless. The record-fold
step now rejects a `Commit` record with no matching prior `Intent` as `HtapError::Corruption` rather than
silently accepting it, closing a hole where a hand-corrupted or partially-truncated journal could be folded
into a checkpoint without detection. Before dropping any record as resolved, `checkpoint()` cross-checks that
every participant whose `committed_version()` reports a concrete value equals the fold's effective maximum
version exactly, refusing without touching disk on any mismatch — the same kind of defense-in-depth
cross-check `recover()` already performs.

### Decision 5 — `DROP TABLE` marks artifacts `pending_reclaim` in the same CAS that removes the table; reclamation completes only once both column-store/movement and rowstore sides confirm, across as many ticks as needed; the movement reclaim callback also deletes that tablet's job records (S2)

**Catalog: `HTAPCAT1` v4 -> v5.** `execute_drop_table` appends one `PendingReclaim{ table_id, table_name,
catalog_generation, dropped: Vec<DroppedPartitionArtifact{partition_id, tablet_id}>, created_at_unix_ms,
colstore_and_movement_reclaimed: false, rowstore_purge_confirmed: false }` onto `CatalogSnapshot.pending_reclaim`
in the exact same CAS that removes the table/partition/tablet/replica rows — never a follow-up CAS, so there is
no window where a table is gone from the catalog but its reclaim intent is not yet durable. `validate()`
rejects a `(partition_id, tablet_id)` pair that overlaps any still-live partition/tablet row or is duplicated
across `pending_reclaim` entries; it does not itself remove an entry — that is caller logic (`reclaim_tick`/
`compaction_tick`), once both flags are true. A v5 payload missing the `pending_reclaim` key is
`HtapError::Corruption`; v1-v4 payloads decode it as an empty vec via `#[serde(default)]`; this is structurally
independent of Phase 14's per-table `stats` field, since a dropped table's row (and its `stats`) is removed
from `next.tables` in the same CAS that adds the `PendingReclaim` entry — there is no orphaned-stats case.

**Two independent completion flags, not one.** Column-store and movement artifact deletion (an entire
`colstore/tablet-<id>/` directory, plus any movement package/job directories referencing that tablet — decision
below) is fast and can complete synchronously inside `execute_drop_table` itself (best-effort, swallowing
errors — a slow or leased tablet just leaves the flag false for a retry on the next tick). Rowstore purge is
tier-driven and can take several `compaction_tick` calls. Making the fast path wait for the slow one for no
correctness reason would tie unrelated cleanup timelines together; the catalog entry itself is only removed
once both flags are true.

**S2 — the reclaim callback also deletes that tablet's movement job records, not just its package directory.**
The initial implementation only removed `<movement>/tablets/<tablet_id>/`, leaving behind
`<movement>/jobs/<job_id>/` directories for any job that had ever targeted the now-dropped tablet — a
leftover-but-harmless disk leak that the review round flagged as incomplete reclamation, not a correctness bug
(a job record referencing a dropped tablet id can never collide with a future one, since tablet ids are never
reissued). `LocalDataMover::delete_tablet_movement_artifacts` now also scans `<movement>/jobs/`, reads each job
record, and removes any job directory whose `tablet_id` matches, alongside the package directory — both under
the same reclaim-lease precondition (`HtapError::Conflict` if called without holding the lease), and both
idempotent on retry (a missing directory is not an error).

**Why this belongs in `compact_once`'s lease set rather than a separate mechanism.** A dropped table's tablet
ids need the *same* mutual exclusion against a still-Running movement job that the ordinary tiered compaction
path needs against a busy tablet (decision 3) — reusing one lease set for both, rather than inventing a
second "drop-pending" lock, means there is exactly one place a future reader of movement state has to check
for correctness, not two.

### Consequences (cross-cutting)

- **Compaction is tier-driven and explicit-tick-only, not instant or automatic**, matching `conversion_tick`'s
  existing precedent (determinism, test repeatability) — `compaction_tick()` has no background thread and
  blocks all SQL for its duration (accepted trade-off, same as `conversion_tick`), bounded by
  `max_compaction_input_ssts`/`max_compaction_input_entries`.
- **The shared rowstore keyspace means SST-level, not row-level, protection**: one SST holds rows from every
  partition written in the same flush window, so a movement-leased tablet blocks compaction of *every* SST
  that contains or spans it, not just that tablet's own rows — disclosed in `docs/LIMITATIONS.md`, not solved
  here (a per-partition physical SST layout would remove this, at a cost this phase does not spend).
- **Tombstones are never elided**, even below `gc_low_water` — per key, every version above the GC horizon is
  kept unconditionally, and among versions at or below it, the single newest survives whether it is a `Put`
  or a `Delete`, so a partial compaction schedule can never resurrect an older value hidden behind a
  tombstone that a *different*, not-yet-compacted SST still holds above it in read order (decision 1's fix is
  what makes this true; the rule itself was unchanged from the original design and reconfirmed against the
  ordering fix by the same review panel).
- **`gc_low_water` is a hard floor for reads, not a soft hint** — an explicit transaction (or movement job)
  pinned below it via `LocalServer.pinned_snapshots`/a conversion's `snapshot_version` cannot silently receive
  collapsed data; it gets a clear error instead. The GC horizon itself is computed once per tick as
  `min(TransactionManager::visible_version(), every open session's pinned snapshot, every in-flight
  conversion's snapshot_version)`, minus a configurable, default-zero `gc_horizon_retention_slack` — never a
  `base_version`/movement-`pinned_version` source, both of which an earlier draft of this design relied on and
  which independent review found unsound (a movement job's default-`None` pinned_version is never written
  back to its durable job record, so a horizon source reading it would silently miss the common case).

### Post-review fixes (storage-review "V batch")

A second storage-review pass, after the fixes in decisions 1-5 above had already landed, found three further
issues — one correctness/liveness hazard in the read path and two liveness-only convergence/isolation gaps —
none of which reopens any decision above:

- **`gc_low_water` mirrored into `read_state` so readers never take `commit_lock`.** `Engine::get`/
  `scan_partition` originally read `gc_low_water` via `self.commit_lock.lock().manifest.gc_low_water` — a
  lock-order hazard for the hot read path, since the engine's own documented rule is `commit_lock` before
  `read_state`, never the reverse, and a pure reader taking `commit_lock` at all could block behind a
  concurrent writer holding it for an unrelated flush or compaction. `ReadState` now carries its own
  `gc_low_water` field, mirrored from `commit_guard.manifest.gc_low_water` under `commit_lock` at the exact
  points `read_state.ssts` is already swapped (`Engine::open`, `flush_locked`, `compact_once`), so `get`/
  `scan_partition` only ever take `read_state.read()`. Verified by a real concurrent-thread test:
  `crates/htap-rowstore/tests/concurrent_reads.rs::test_concurrent_reads_do_not_deadlock_with_flush_and_compaction`.
- **`compaction_tick`'s protection-convergence loop has a bounded, safe fallback.** The loop that adds every
  lease-denied tablet's partitions to `protected_partition_ids` and re-previews (bounded at 8 iterations) is
  now confirmed to protect every denied partition on every pass; if it still has not stabilized when the cap
  is reached, `compaction_tick` skips the `compact_once` rewrite for that tick alone — never an unstable
  candidate set whose protection status could change mid-rewrite — but still runs `flush_roll_and_gc`, purge
  confirmation, the removal CAS, and `reclaim_tick_locked`, reporting `ran: true` with an explicit
  non-convergence reason. Verified by
  `crates/htap-server/tests/compaction_convergence.rs::leased_dropped_tablets_are_all_protected_in_one_tick`.
- **Reclaim no longer fails on an undecodable job file belonging to a different tablet.**
  `LocalDataMover::delete_tablet_movement_artifacts`'s job-directory scan used to propagate any decode error
  it hit while looking for the target tablet's own job records, so one corrupt, unrelated tablet's `JOB` file
  could block reclamation of a different, healthy tablet. It now skips an undecodable entry instead of
  failing the whole scan (a real limitation, not silently hidden: the corrupt directory itself is never
  deleted or reported — see `docs/LIMITATIONS.md`). Verified by
  `crates/htap-movement/tests/corrupt_job_isolation.rs::corrupt_unrelated_job_does_not_block_tablet_artifact_reclamation`.

Two further liveness-only gaps were disclosed rather than fixed in this round (both fail safely, neither loses
data): a `compact_once` error occurring after its manifest publish but before the in-memory manifest is
updated leaves the in-memory copy stale until the engine reopens, refusing every subsequent flush in the
meantime; and a flush whose new SST reader fails to open *after* the manifest already lists that SST leaves a
later compaction touching it failing safely with `Corruption` until reopen. Both are documented in
`docs/LIMITATIONS.md`'s "Rowstore compaction, garbage collection, and DROP TABLE reclaim scope and deferred
features".

### Post-review fixes (storage-review "X batch")

A third, whole-diff storage-review pass found and fixed five further issues — two correctness/liveness
hazards in the write and export paths, one durability-hardening fix, one accounting fix, and one recovery
availability fix — none of which reopens any decision above:

- **Exports now hold their tablet lease for the whole scan-and-write (X1).** COPY TO CSV/JSONL and file export
  (`crates/htap-movement/src/export.rs`) previously did not consistently hold their per-tablet movement lease
  across the entire scan-and-write on every code path, leaving a window where a reclaim lease — and therefore
  `compact_once`/`DROP TABLE` reclamation — could be acquired against a tablet an export was still reading.
  The lease is now held for the export's full duration, so the two are mutually exclusive in either direction:
  an export attempted while a reclaim lease is held fails with `HtapError::Conflict` rather than racing it.
  Verified by `crates/htap-movement/tests/export_leasing.rs::exports_hold_tablet_leases_against_reclaim`.
- **`compact_once`'s GC horizon clamp has no write-side exemption (X2; corrected to clamp against
  `visible_version` rather than `committed_version` in the "Y batch" follow-up below).** `effective_gc_horizon`
  is now the one value used both to decide which versions collapse and to advance `gc_low_water`, on every
  publishing pass, including the `u64::MAX` "collapse everything" sentinel. An earlier draft exempted that
  sentinel from raising `gc_low_water` at all, reasoning it was a write-side-only signal — but the
  corresponding collapse still happened, so a real snapshot at an older, now-collapsed version could silently
  read stale data instead of being rejected with the "below GC low-water" error decision 2 relies on. Verified
  by `crates/htap-rowstore/tests/horizon_clamp.rs::test_infinite_gc_horizon_is_clamped_to_committed_version`.
- **Tablet artifact deletion fsyncs its parent directories (X3).**
  `LocalDataMover::delete_tablet_movement_artifacts` now calls `sync_dir` on the tablets directory, the jobs
  directory, and the movement root after removing a tablet's package directory and its referencing job
  directories, so a crash immediately after deletion cannot leave those directory entries resurrectable from
  stale directory metadata on reopen — the same durability contract (ADR-008/009) every other owned-state
  deletion in this workspace already follows.
- **`checkpoint()` reads the journal through `max(configured_max_journal_size, RECOVERY_BOOTSTRAP_MAX_BYTES)`,
  not the configured limit (X4; corrected below in the "Y batch" follow-up to not be a fixed 2 GiB cap).** An
  earlier draft's `checkpoint()` read the live journal through the manager's normal handle, already reopened
  at the *configured* `max_journal_size` by the time `checkpoint()` runs — so a journal a single oversized
  commit had pushed past that limit could never be checkpointed at all, and the opportunistic post-commit
  checkpoint (decision 4's own trigger) failed on every subsequent commit instead of shrinking the file.
  `checkpoint()` now reads through the larger of the configured limit and the 2 GiB bootstrap ceiling
  `TransactionManager::open`/`recover()` already use, so a journal within that ceiling but over the configured
  limit can still be folded and rewritten back under it. Verified by
  `manager::tests::test_checkpoint_compacts_journal_that_exceeds_configured_limit`
  and `manager::tests::test_finalize_open_restores_configured_journal_limit`, both in
  `crates/htap-txn/src/manager.rs`.
- **`compaction_tick`'s `entries_purged` counts only confirmed CAS successes (X5).** It previously counted every
  `pending_reclaim` entry the tick attempted to mark `rowstore_purge_confirmed`, regardless of whether the
  catalog CAS marking them actually landed; a lost race against a concurrent catalog writer (`HtapError::
  Conflict`, silently retried on the next tick) could make the report overstate how many entries were durably
  confirmed on that call. `entries_purged` is now assigned only inside the CAS's `Ok(())` branch.

**Two latent API hazards disclosed, not fixed, by this round (neither is reachable today).**
`TransactionManager::new` does not itself load the durable checkpoint baseline (`checkpoint_baseline` defaults
to `CheckpointBaseline::default()`); its only caller, `open_with_options`, immediately overwrites it with the
loaded baseline right after, so no code path today can observe the gap. And the checkpoint file name
(`txn.checkpoint`, `CHECKPOINT_FILE` in `crates/htap-txn/src/checkpoint.rs`) is fixed per directory — two
`Journal`s opened against the same directory would silently share one baseline file — but `LocalServer` always
gives each root exactly one `txn.journal`, so this cannot arise through any path this workspace exercises.
Both are recorded in `docs/LIMITATIONS.md`'s "Transaction journal checkpoint scope and deferred features" as
standing preconditions to re-check before either constraint changes (a future multi-journal-per-directory or
manually-constructed-`TransactionManager` feature would need to address them first).

### Post-review fixes (storage-review "Y batch")

A fourth storage re-review pass corrected two of the "X batch" fixes above and found three further issues:

- **X2 corrected: clamp to `visible_version`, not `committed_version`.** The "X batch" fix computed
  `effective_gc_horizon = input.gc_horizon.min(committed_version)`. This is unsound: `apply_external` can
  advance `committed_version` before the corresponding `publish` call advances `visible_version` (a 2PC or
  external-apply commit is durable and counted in `committed_version` before it is made visible), so clamping
  to `committed_version` could raise `gc_low_water` above `visible_version` and reject every fresh snapshot —
  no snapshot is ever bounded by anything but `visible_version`, so this was strictly worse than the bug X2
  fixed. The clamp is now `effective_gc_horizon = input.gc_horizon.min(read_state.visible_version)`;
  `committed_version_high_water` (decision 2) is unaffected, since it is still fed from `committed_version`
  directly, not from the clamped horizon. Verified by the existing
  `test_infinite_gc_horizon_is_clamped_to_committed_version` (whose committed and visible versions happen to
  coincide) plus a new test, `crates/htap-rowstore/tests/horizon_clamp.rs::test_infinite_gc_horizon_does_not_exceed_visible_version`,
  which commits via `apply_external` without publishing to construct a committed-ahead-of-visible state and
  proves the horizon (and therefore `gc_low_water`) never exceeds `visible_version`.
- **X4 corrected: the read ceiling is `max(configured_max_journal_size, RECOVERY_BOOTSTRAP_MAX_BYTES)`, not a
  fixed 2 GiB cap.** A fixed 2 GiB ceiling would itself refuse to read a journal larger than 2 GiB but smaller
  than a *larger-than-2-GiB* configured `max_journal_size` — the exact failure mode X4 was fixing, just moved
  to a different threshold. `checkpoint()`'s formula was in fact already `max(...)`, not a fixed constant; this
  correction is to the ADR's/docs' description of it, not to the code. No test covers the more-than-2-GiB case,
  since it would require constructing a journal over 2 GiB.
- **Y3 — `delete_tablet_movement_artifacts` tolerates a missing `jobs/` directory.** An earlier draft
  propagated the `NotFound` error from opening `movement/jobs/` for the job-record scan, so a tablet reclaimed
  on a root that had never run a movement job (and therefore never created that directory) failed reclamation
  outright instead of treating "no jobs ever existed" as "no jobs reference this tablet." A missing `jobs/`
  directory is now treated as empty, and the parent-directory `sync_dir` calls (X3) still run. Verified by
  `crates/htap-movement/tests/missing_jobs_dir.rs::reclaim_succeeds_when_jobs_directory_is_missing`.
- **Y4 — the colstore reclaim step fsyncs `<root>/colstore` after deleting a tablet's colstore directory.**
  `LocalServer::reclaim_tick_locked`'s colstore-deletion callback now calls `sync_dir` on the colstore root
  immediately after `std::fs::remove_dir_all` on the tablet's colstore directory, before
  `delete_tablet_movement_artifacts` runs and before the catalog CAS that marks
  `colstore_and_movement_reclaimed`, so a crash right after removal cannot leave the removed directory
  resurrectable from stale directory metadata — closing the same class of gap X3/Y3 close on the movement
  side, on the colstore side. No dedicated test: an fsync's durability effect is not observable without crash
  injection, which this pass did not add for this specific call site.

**F5 disclosed, not fixed: `LocalDataMover`'s global lease mutex is held across the whole reclaim-deletion
critical section.** `delete_tablet_movement_artifacts` acquires the same single mutex every lease
acquire/release path uses and holds it for its entire body: the tablets-directory and job-record deletions,
the job-directory scan, and all three `sync_dir` calls from X3. Every lease operation for *any* tablet — not
just the one being reclaimed — stalls behind one tablet's reclaim deletion for that duration. This is a
performance/liveness cost, not a correctness one: no data race results, only reduced concurrency during
reclaim. Recorded in `docs/LIMITATIONS.md`'s rowstore-compaction "Completed local MVP" list.

**Also corrected in this pass: a pre-existing test-count error in `docs/PROGRESS.md`'s Phase 15 row.**
`crates/htap-txn/src/checkpoint.rs` has 9 unit tests, not 7 — `checkpoint_with_trailing_bytes_is_rejected` and
`truncated_checkpoint_is_rejected` were omitted from the named list in an earlier pass. Both are now named.

### Test evidence

See `docs/PROGRESS.md`'s Phase 15 row for the full, verified test list. Named regression tests worth calling
out here because they pin exactly the bugs this ADR describes:
`crates/htap-rowstore/tests/compaction_ordering.rs::{test_non_newest_compaction_run_keeps_newer_ssts_authoritative,
test_selected_tombstone_never_resurrects_older_unselected_value, test_sandwiched_partition_becomes_exactly_absent_in_one_pass,
test_scattered_dropped_partition_is_purged_over_contiguous_passes}` (decision 1),
`crates/htap-rowstore/tests/{manifest_v3.rs,gc_low_water.rs,purge_reopen.rs}` (decision 2),
`crates/htap-server/tests/{tier_shift_protection.rs,compaction_tick.rs}` (decision 3),
`crates/htap-txn/src/manager.rs`'s `test_checkpoint_crash_*`/`test_checkpoint_refused_before_recovery`/
`test_checkpoint_rejects_participant_version_mismatch` unit tests (decision 4), and
`crates/htap-server/tests/{reclaim.rs,movement_artifacts_reclaim.rs,sandwiched_purge.rs,purge_no_resurrection.rs}`
plus `crates/htap-catalog/tests/catalog_recovery.rs`'s v5 tests (decision 5); and, for the post-review "V
batch" fixes above, `crates/htap-rowstore/tests/concurrent_reads.rs`,
`crates/htap-server/tests/compaction_convergence.rs`, and `crates/htap-movement/tests/corrupt_job_isolation.rs`.
For the "X batch" fixes above: `crates/htap-movement/tests/export_leasing.rs` (X1),
`crates/htap-rowstore/tests/horizon_clamp.rs` (X2), `crates/htap-txn/src/manager.rs`'s
`test_checkpoint_compacts_journal_that_exceeds_configured_limit`/`test_finalize_open_restores_configured_journal_limit`
(X4), and `crates/htap-server/tests/compaction_tick.rs::dropped_table_is_purged_from_rowstore_before_pending_entry_is_removed`
(X5, the existing `entries_purged` assertion; X3 has no dedicated new test — see `docs/PROGRESS.md`'s Phase 15
row). For the "Y batch" fixes above: `crates/htap-rowstore/tests/horizon_clamp.rs::test_infinite_gc_horizon_does_not_exceed_visible_version`
(X2's correction) and `crates/htap-movement/tests/missing_jobs_dir.rs` (Y3; Y4 and F5 have no dedicated test —
see `docs/PROGRESS.md`'s Phase 15 row). Cross-references: ADR-004 (the
single shared MVCC version domain both watermarks and the GC horizon build on), ADR-008/009 (durability
invariants — every new envelope here follows the same temp-write/fsync/rename/sync-dir + magic/version/CRC32C
contract), ADR-018 (the manager-wide recovery latch decision 4 reuses verbatim).

---

## ADR-025: Owner plus IPC for concurrent multiprocess use

`Status: Accepted`
`Date: 2026-09-23`

### Context

Since `1083fbd` (see `docs/LIMITATIONS.md`, `docs/ARCHITECTURE.md`), `LocalServer::open` has enforced
single-process exclusive ownership of a root directory via a non-blocking advisory `flock` at `<root>/LOCK`: a
second process opening the same root got `HtapError::Conflict` and nothing else. That is safe — it protects
the one shared WAL and one shared MVCC version domain (ADR-004) from two independent writers — but it means a
second local tool, a second embedded-client process, or a restarted `htapd` racing its own predecessor during
a deploy could never usefully touch an already-open root at all. Phase 16 removes that limitation for the
narrow local-machine case without touching the durability model: no new on-disk format, no change to
ADR-004/008/009's one-WAL/one-MVCC-domain/atomic-publish invariants, and no distributed consensus.

### Options considered

1. **Shared-root multi-writer.** Let two processes each open their own `Engine`/`LocalCatalogStore`/
   `TransactionManager` against the same root directory, coordinating through file locks or optimistic CAS
   retries at a finer grain than the current root lock. Rejected: the rowstore's WAL, manifest, and commit
   path assume one in-process `TransactionManager` decides commit order under one lock (ADR-018); making that
   safe across processes would mean either a cross-process commit protocol (which is what option 2 already is,
   just implemented badly) or memory-mapped shared state with process-crash-safe locking primitives this
   workspace has no dependency on and could not add without new `unsafe` code. It would also multiply the
   surface that needs power-loss/crash-safety proof (`docs/OPERATIONS.md` section 5) by the number of writer
   processes.
2. **Distributed consensus (Raft/ZooKeeper).** Already `deferred` in `docs/ARCHITECTURE.md`/
   `docs/LIMITATIONS.md` for cluster coordination; using it just to arbitrate two processes on one machine
   sharing one filesystem is disproportionate — it solves cross-node leader election and log replication, a
   different problem, at a much higher implementation and operational cost, for a same-host, same-OS-user
   scenario that has no network partition to reason about.
3. **Owner plus IPC (chosen).** Whichever process wins the existing root-lock race keeps being the one real
   storage owner, unchanged; every other process that would have gotten `Conflict` instead becomes a client
   that forwards SQL and session calls to the owner over a local Unix domain socket at `<root>/htap.sock`
   (mode `0600`, Unix-only). This adds a transport, not a second storage or commit path: the owner still has
   exactly one `TransactionManager`, one WAL, one MVCC version domain, and every rule in ADR-004/008/009
   applies unchanged inside that one process. A version-skewed pair (owner and client built from different
   commits) is possible only because both binaries can be up at once, and is handled by fast, clean failure at
   decode time (see the wire protocol below), not by a schema-compatibility scheme.

### Decision

`LocalServer::open` still tries `ProcessLock::acquire` first. On success, the process becomes the owner: it
builds the storage core (`OwnedServer`, formerly the whole of `LocalServer`'s body) and starts a background
IPC listener (`ipc::owner::start`) bound to `<root>/htap.sock`, handed a clone of the storage-core handle
directly — never a handle to the outer `LocalServer` wrapper, since the listener only ever needed direct
storage access, not the wrapper's own mutable-configuration surface. A bind failure (a socket path too long
for this kind of socket, a permission error, or a non-socket file already at that path) is non-fatal: the
owner falls back to lock-only mode, exactly like before this phase, and `LocalServer::is_listener_up()`
reports `false` so a caller can distinguish a healthy owner from a degraded one. `ProcessLock::acquire`
failing with `Conflict` now triggers `ipc::client::IpcClient::connect`, which retries only the two transient
connect errors that mean "the owner is still starting" (a missing socket, a refused connection), bounded to a
low single-digit-second backoff ceiling; a permission error or exhausted retries still return the original
`Conflict`, with a message that distinguishes "locked but unreachable" from ordinary lock contention. The
forwarding surface is exactly the SQL/session surface reachable through `LocalServer`/`Session`: `execute`,
`bootstrap_root_account`, `authenticate_session`, `open_session`, and every session method reachable from an
open session (including `begin`, present in the public surface even though no in-tree caller uses it
directly). Every administrative, data-mover, conversion, compaction, and reclaim method (partitioning DDL,
table conversion, `compaction_tick`/`reclaim_tick`, copy/import/export, tablet clone/verify/repair, job
load/resume) returns `HtapError::Unsupported` in client mode — those stay owner-only, unchanged from today's
"standalone subsystem opens are unsafe for concurrent use" boundary, just now also unreachable from a client
handle by construction rather than merely undocumented.

Because `htap-wire`'s server module and `htap-client` call only `open_session`, `execute`, and the session
methods it returns, and because `LocalServer::open`'s signature and success-path return type are unchanged
(still a plain value, not a reference-counted one), `htapd`, `htap-wire`, and `htap-client` needed zero source
changes for batches A-C: a second `htapd` (or embedded client) on an already-owned root transparently becomes
an IPC front end. This stopped being true once `open_session` itself became fallible (batch E's E1, below):
`htap-wire`'s `authenticate` and `htap-client::EmbeddedClient::open_session` both needed a small source change
to handle the new `Result`, since a client-mode `open_session`/`authenticate_session` call can now return an
error instead of panicking.

**Wire protocol.** Length-prefixed frames (a four-byte big-endian length, then a JSON body) in both
directions, symmetrically capped at 16 MiB before any allocation on either side — an oversize declared length
is rejected before the body buffer is allocated, for both a request frame on the owner side and a response
frame on the client side. JSON, not a byte-oriented binary encoding, because the payload already needs to
carry the parsed statement AST (see below) and the existing catalog envelope's own serialization already goes
through `serde`; reusing that removes a second encoding scheme from this phase's scope. A hand-bumped
`IPC_PROTOCOL_VERSION` constant (currently `1`), checked at handshake alongside the canonicalized root path, is
the actual compatibility gate — not a derived schema hash — because a decode failure on either side, at any
point in the protocol, already closes the connection cleanly with a diagnostic rather than panicking or
misinterpreting bytes; a schema hash would only add process, not safety, for this MVP's single-repository,
single-binary deployment model. Every session-scoped response, success or failure alike, carries a small
session-status snapshot (`autocommit`, `in_transaction`, `principal`) — an error can still change session
state (a poisoned commit still clears the transaction), so the client-side session resyncs its cached status
even on a failed call rather than only on success.

**Why forward the statement AST directly instead of a text round trip.** `Session::execute_statement` takes an
already-parsed statement, not raw SQL text, and is the entry point `htap-wire`'s prepared-statement path uses
after its placeholder-substitution helper has already turned every `?` into a literal value node — including
binary blobs (ADR-019) and non-finite-float rejection concerns already present on the row-mutation path.
Rendering that already-substituted AST back to SQL text on the client and re-parsing it on the owner would be
exactly the lossy, dialect-fragile bridge CLAUDE.md's durability caution warns against: a binary literal with
embedded zero bytes has no safe unambiguous SQL-text spelling in general, and a round trip through text is one
more place a corner case (non-finite floats, deeply nested expressions, unusual escaping) could silently
diverge between what the client meant and what the owner executes. The vendored parser fork's statement type,
and every fork-added typed-partition-DDL type in its DDL module, already carry a conditional `serde` derive
behind a cargo feature the fork's manifest already declares (unused by any workspace member before this
phase); turning it on needed only two additive one-line feature-list entries in `htap-sql`'s and
`htap-server`'s own manifests, not a change to any file inside the vendored fork's source tree, so CLAUDE.md's
"change the vendored fork only when unavoidable" rule does not apply. `IpcRequest::ExecuteBound` therefore
carries `sqlparser::ast::Statement` directly, and `IpcRequest::VisibilityCheck` carries a statement AST the
same way; the plain-text `IpcRequest::Execute` path — the common case for the embedded client, a remote
client's ordinary query, and `htap-wire`'s ordinary `COM_QUERY` handling — still forwards as SQL text, since
that path already works and has no substituted-literal problem to solve. This does tie the wire format to the
exact AST shape of whichever build produced it (see Consequences), which is why the manually bumped protocol
version, not the AST's own shape, is the actual compatibility gate.

**A new `Ambiguous` error variant, distinct from `Conflict` and `DurablePending`.** A non-read-only IPC
request (anything except the statically-read-only catalog-snapshot and visibility-check requests) can fail
after some or all of its request bytes have already reached the owner — the owner may have applied it, or may
not have, and the client cannot tell which from a write or read failure alone, because the standard
all-or-nothing write call's error contract does not expose "how many bytes actually reached the peer." The
classification rule (`ipc::client::classify_transport_failure`) is: zero bytes of the request handed to the OS
before the failure, or the request being one of the two statically-read-only kinds, is safely retryable
(`Conflict`); anything else is `Ambiguous`, and never auto-retried. `Ambiguous` was consulted on with the
architect (heavy tier) specifically, after two earlier consultation attempts on this same decision hit a
transient service error before one succeeded; architect recommended the same choice this design already
favored — a new sibling variant, not a reuse of an existing one:
- **Not `Conflict`.** `Conflict` already means "safe to retry, nothing changed" throughout the codebase (lock
  contention, first-writer-wins); reusing it for an outcome that might have mutated durable state would imply
  a false safety guarantee to every caller that already treats `Conflict` as retryable.
- **Not `DurablePending`.** `DurablePending`'s two identifying fields (`txn_id`, `version`) are load-bearing
  and pattern-matched verbatim at several call sites as "this specific transaction's outcome must be resolved
  by inspecting durable state at this version." An ambiguous IPC failure usually has no such identifying values
  at all — the request might have been a plain autocommit statement, a bootstrap call, or a commit with no
  transaction identifier the client ever learned — so retrofitting it onto `DurablePending` would either
  fabricate values that do not exist or silently break every exhaustive match that already relies on those
  two fields meaning something specific.
- **Not the generic `Internal` outcome.** That would defeat the actual point of this feature: a session
  quarantine that survives exactly as long as the existing `DurablePending`/`RecoveryRequired` quarantine does,
  never silently cleared, never transparently retried.

Architect also flagged and this plan addressed: exhaustiveness (the shared error enum stays closed, not marked
open-ended for future growth, since the workspace's existing lint gate already forces every match site to
handle a new variant — a separate dispatch-predicate abstraction was judged unnecessary on top of that); the
wire-mapping risk (`docs/DECISIONS.md`'s own error-mapping test, extended in Task A2, pins `Ambiguous`'s mapped
MySQL code away from any driver-auto-retry code, the same protection `DurablePending` already has); and
reversibility (judged moderately reversible now, since it is purely additive, but costly to unwind once
client and session code depends on it — not treated as a reason to defer, since retrofitting `DurablePending`
instead was judged strictly worse to reverse later).

An `Ambiguous` outcome quarantines a session exactly like `DurablePending` does today: every call site that
already re-raises a stored `DurablePending` error also re-raises a stored `Ambiguous` one (including the
`reset` method's own quarantine gate), and neither state is ever exited by silently reconnecting — a session
whose connection is confirmed dead before a call even starts instead enters a second, distinct terminal
`RemoteDisconnected` state carrying the ordinary `Conflict` error, since "this session's connection is gone,
open a new one" is a different, safe-to-retry-with-a-fresh-session fact from "this specific outcome is
unknown."

**Ownership graph and teardown order.** Two problems made "add a socket field to the existing struct" unsound
rather than merely inelegant, both found during design review (see Consequences below for how they were
found): first, `open_session` requires a reference-counted handle to the outer server value that only the
*caller* constructs after `open` returns, so the IPC listener — which must mint one real session per
connection the same way — cannot go through `open_session` without either changing `open`'s return type
(touching roughly 354 existing call sites, several of which use `LocalServer`'s exclusive-reference
configuration setters that stop being callable once shared through a reference-counted wrapper) or being given
a narrower way to mint sessions that does not depend on the caller's wrapper at all. Second, an optional
socket field bolted onto the existing session/server structs would leave invalid state combinations reachable
(a "local" session with a live socket, a "client" server that still thinks it has direct storage access) and
would not solve the first problem regardless.

The fix is an explicit ownership graph, not a field: `OwnedServer` is the renamed storage core (unchanged
body); `ServerMode` is a two-case enum (`Owner(OwnerRuntime)` or, Unix-only, `Client { client: IpcClient }`)
that `LocalServer` wraps; `OwnerRuntime` pairs a reference-counted `OwnedServer` handle with the optional
running `IpcListener`, by value, not behind its own extra reference count, since exactly one `OwnerRuntime`
exists per owning process — the caller's own wrapper (`Arc<LocalServer>` in `htapd`, `EmbeddedClient`, every
test) is the only sharing layer needed above it. The IPC listener is started by handing it a direct clone of
the `OwnedServer` handle, bypassing `open_session` and the outer wrapper entirely, which sidesteps the
chicken-and-egg problem: the listener never needed anything but direct storage access. `Session`'s own private
storage-access field became the same kind of two-case split (`SessionBackend::Owner(Arc<OwnedServer>)` or,
Unix-only, `SessionBackend::Remote { connection, status }`), so an owner-side session (used both by
`open_session` and by each IPC connection's server-side handler) and a client-side session are structurally
distinct, not one struct with an optional field.

Teardown order is the second half of the same problem: dropping the caller-facing `LocalServer` handle must
not release `<root>/LOCK` while the listener's own accept thread or a connection-handler thread might still be
touching storage through its own clone of the `OwnedServer` handle — Rust's ordinary field-declaration drop
order cannot express "shut the listener down, *then* release the lock" across a reference count, because
`OwnedServer`'s own `Drop` (which releases `ProcessLock`) only runs once the *last* clone of it is dropped, and
a listener thread's clone is not necessarily the caller's. `OwnerRuntime`'s `Drop` is hand-written, not
derived, specifically to sequence this: it takes and drops its `IpcListener` first, whose own `Drop`
(`ipc::owner::IpcListener`) sets a stop flag, force-closes every live connection (interrupting any blocked
read), then joins the accept thread and every connection thread with a real, unbounded join — not a bounded
wait that gives up and proceeds anyway, matching `htap-wire`'s own existing shutdown precedent, at the accepted
cost that one stuck in-flight statement can delay shutdown. Only once that join has returned — guaranteeing
every listener-held clone of the `OwnedServer` handle is gone — does `OwnerRuntime`'s own `OwnedServer` handle
clone drop normally, which is now deterministically the last one, releasing `<root>/LOCK` only after the
socket is already gone. This ordering is exercised by
`crates/htap-server/tests/ipc_owner_shutdown_bounded.rs::dropping_owner_with_idle_client_is_bounded_and_releases_lock`
and the disconnect-mid-transaction rollback test
(`crates/htap-server/tests/ipc_owner_disconnect_mid_txn.rs::disconnect_rolls_back_open_transaction`), which
also confirms a connection whose session still has an open transaction gets that transaction rolled back
before the session is dropped, matching `htap-wire`'s own dropped-connection behavior.

### Consequences

- **Positive.** A second local process on an already-owned root now gets a working, if narrower, connection
  instead of an unconditional failure; `htapd`, `htap-wire`, and `htap-client` needed no source changes to gain
  this for batches A-C (batch E's `open_session` fallibility fix later required a small change to each of
  `htap-wire` and `htap-client`, see above). No new on-disk format, no change to the durability invariants in
  ADR-004/008/009. The trust model
  matches the existing root-lock file exactly: same-OS-user, not a security boundary (see Decision #6 in the
  Phase 16 plan). The socket is published mode `0600`; as of batch E, it is created inside a private,
  owner-only (`0700`) directory and tightened there before being atomically renamed into place, so there is no
  window during creation where a different local user could reach it (see "Post-review fixes (batch E)"
  below — the original bind-then-chmod sequence directly at the published path did have such a window, found
  by a second storage re-review). That private directory is itself removed immediately after the rename
  publishes the socket, and any left behind by a crashed prior owner are swept up at startup (batch F, F2
  below) — the directory never accumulates on a normal run, only ever a real crash leaves one to clean up.
- **Negative / disclosed gaps.** A client-mode session cannot change its authenticated user: `change_user`
  returns `Unsupported` in client mode, a narrower surface than the owner side, disclosed in
  `docs/LIMITATIONS.md`. A client-mode `LocalServer`'s configuration setters — both the `with_*` builder forms
  and their mutable `set_*` counterparts (scan workers, query parallelism, query memory budget, the
  `ANALYZE TABLE` distinct-value limit, GC horizon retention slack) — are accepted no-ops, and their getters
  report the compiled-in defaults rather than the owner's actual
  configuration, because that configuration lives entirely in the owner process's `OwnedServer` and is not
  itself forwarded over the wire protocol (found during the batch D review as one symptom of the same
  `Deref`-panic defect, fixed as a safe no-op rather than as forwarding, since none of `htapd`/`htap-wire`/
  `htap-client` ever call these setters on anything but the one process that opens the root). Every
  administrative/data-mover/conversion/compaction/reclaim method is owner-only. A client session that is
  confirmed disconnected is terminal — it never
  transparently reconnects and continues the same transaction, matching the review finding that a broken
  session silently resuming would be unsafe. A socket path too long for a Unix domain socket, or a non-socket
  file already occupying `<root>/htap.sock`, leaves the owner in the same lock-only mode this workspace has
  always had as its fallback: a second opener in that case still gets the ordinary `Conflict`, not IPC
  forwarding. The wire format is tied to the exact AST shape of the build that produced it; a version-skewed
  owner/client pair (e.g. an unrestarted process after an in-place binary upgrade) fails cleanly at decode
  time rather than silently misinterpreting bytes, but there is no schema-compatibility scheme beyond the
  hand-bumped protocol-version constant. Standalone subsystem opens that bypass `LocalServer`
  (`Engine::open`, `LocalCatalogStore::open`, `LocalDataMover::new`) remain unsafe for concurrent use, exactly
  as before this phase — this decision does not touch that gap.
- **Design review.** An initial draft of the owner/client split assumed a single shared "run one statement"
  function and an optional socket field bolted onto the existing session struct; a review panel (`reasoner` and
  `cx/gpt-5.5`) found both unsound — the shared function would have silently reordered session-specific
  decisions (implicit-transaction timing relative to a bind/privilege failure, DDL-in-transaction rejection,
  poison/read-only checks), and the optional field left invalid state combinations reachable while not
  actually solving the `open_session`/reference-counting problem described above. The narrower
  prepare-then-dispatch split (a new `prepare_statement` helper shared by `LocalServer::execute` and
  `Session::execute_statement`, fixing `docs/PROBLEMS.md` P3 as a side effect) and the explicit ownership-graph
  types described above are the adopted fixes.

### Post-review fixes (batch D)

A storage review of the Batch A-C2 diff (verdict: fix-first) plus an external review found 13 defects the
passing test suite had not caught, two of them serious enough to undermine the design's own stated
guarantees: prepared statements were completely broken for every client-mode session, because the owner's
`VisibilityCheck` handler discarded the `CatalogSnapshot` the client needed and returned an empty one instead
(D4); and roughly 30 owner-only `LocalServer` methods, plus `Debug` formatting and the five mutable
configuration setters, panicked the whole process on a client-mode handle, because they reached the storage
core through a blanket `Deref` impl that assumed local storage access unconditionally (D3). Neither had a
dedicated test before this batch — the existing suite only ever exercised these paths in owner mode. Twelve of
the thirteen (D1-D12) were fixed, each verified against the code before landing; the last (D13) was left
unfixed and is instead recorded as a limitation:

- **D1 (durable-pending downgrade).** A `DurablePending` error received over IPC is now latched into
  `SessionState::CommitOutcomePending` at the point `Session::remote_request` receives it, before it is ever
  returned to the caller, so a later disconnect can never downgrade it to a retryable `Conflict` — the same
  "never silently downgraded" invariant the local-backend path already had.
  Test: `crates/htap-server/tests/ipc_client_session_durable_pending_latch.rs::remote_session_latches_durable_pending_commit_outcome`.
- **D2 (stale status on error).** `IpcConnection::request` now returns the response's `SessionStatus`
  alongside both the success and the error case, instead of discarding it on error, so `Session::in_transaction()`
  cannot report a stale "still in a transaction" answer after a transaction-ending failure.
  Test: `ipc_client_session_status_after_error.rs::remote_session_applies_status_from_failed_commit_response`.
- **D3 (client-mode panics).** The blanket `Deref` to the storage core is gone. Every owner-only method
  (`compaction_tick`, `reclaim_tick`, `data_mover`, `txn_manager`, `colstore_dir`, `convert_table`,
  `convert_table_to_column`, `convert_table_to_row`, `conversion_tick`, `tick`, `load_job`, `resume_job`, and
  the rest of the administrative surface) returns `HtapError::Unsupported` instead of panicking on a
  client-mode `LocalServer`; `Debug` formatting and the five configuration knobs (scan workers, query
  parallelism, query memory budget, the `ANALYZE TABLE` distinct-value limit, and GC horizon retention
  slack) — both their `with_*` builder forms and their mutable `set_*` counterparts — are safe no-ops in
  client mode, with their getters reporting the compiled-in defaults rather than reading through to storage
  that does not exist in this process. Recorded
  as a limitation, not further fixed: this makes client-mode configuration cosmetic, not forwarded to the
  owner (see Consequences below).
  Test: `ipc_client_mode_unsupported_methods.rs::ipc_client_mode_owner_only_methods_return_unsupported_without_panicking`.
- **D4 (broken prepared statements).** The owner's `VisibilityCheck` handler now returns the real
  `CatalogSnapshot` it just checked against instead of an empty one, fixing prepared statements for
  client-mode sessions end to end.
  Test: `ipc_client_session_visibility_check.rs::remote_session_forwards_statement_visibility_check`.
- **D5 (reclaim/listener race).** `LocalServer::open` now runs `reclaim_tick_locked(true)` under
  `execution_lock` and only starts the IPC listener afterward, so a client can never execute a statement
  while startup reclaim is deleting a dropped table's tablet directories. Verified by read-through of
  `crates/htap-server/src/lib.rs::open` (the reclaim call is now sequenced, still under the lock, strictly
  before `ipc::start`); no new dedicated regression test, since the existing reclaim suite staying green after
  the reorder was the actual check.
- **D6 (silent close on an undecodable frame).** A fully received but undecodable request frame on the owner
  side now gets a definite `InvalidArgument` error reply before the connection is closed, instead of a bare
  disconnect, so a client never reports an unknown outcome for a statement that provably never ran.
  Test: `ipc_owner_undecodable_frame.rs::undecodable_frame_returns_error_then_closes`.
- **D7/D8 (local failure misclassified as terminal).** A purely local, pre-send failure — the concrete case
  is an oversize request rejected by `MAX_FRAME_SIZE` before any byte is written to the socket — is now
  classified as `HtapError::InvalidArgument` ahead of the write-boundary rule in `classify_transport_failure`,
  rather than falling through to the ordinary `Conflict`/`Ambiguous` path and latching the session terminal;
  the session stays usable for the next statement. The `set_max_allowed_packet` remote path was fixed the
  same way in passing: it now checks the terminal-state (quarantine/disconnected) gate up front like every
  other remote call, and no longer swallows a transport error.
  Test: `ipc_client_session_oversize_request.rs::oversized_request_is_rejected_locally_without_poisoning_session`.
- **D9 (unbounded handshake, blocking rollback on drop).** The owner now bounds its handshake read to a fixed
  5-second timeout, so a connection that sends nothing after connecting cannot hold a connection slot forever.
  Dropping a `Session` whose backend is `SessionBackend::Remote` no longer issues a blocking rollback request
  over the socket (`impl Drop for Session` returns immediately for the remote case; only a local-backend
  session's `Drop` still calls `rollback()`), matching the terminal-state contract that a lost or disconnecting
  remote session must not block teardown.
  Test: `ipc_owner_handshake_timeout.rs::silent_handshake_times_out_and_releases_connection_slot`.
- **D10 (fabricated bootstrap report).** A client-mode `bootstrap_root_account` now returns the owner's real
  `BootstrapReport` from the IPC response instead of fabricating one locally.
- **D11 (non-fatal paths treated as fatal).** `ipc::owner::start` now treats a root-canonicalize failure, a
  stale-socket removal failure, and a bind failure identically: log a warning and return `Ok(None)` (lock-only
  fallback), never propagate an error that would fail `LocalServer::open` itself.
- **D12 (string-based disconnect classification).** Disconnect classification is now a structural match on
  the `WireError` enum (`WireError::OwnerUnreachable` vs. `WireError::Ambiguous`, a new dedicated wire-error
  variant) rather than a string-prefix check on an error message.
- **D13 (not fixed — recorded as a limitation).** `Session::change_user` still checks "this is a client-mode
  session" before checking the terminal-state (quarantine/disconnected) gate, so a client-mode session in
  `AmbiguousOutcomePending` or `RemoteDisconnected` gets `HtapError::Unsupported` for a `change_user` call
  instead of the stored terminal error. This is disclosed in `docs/LIMITATIONS.md` rather than fixed in this
  batch, since `change_user` is already `Unsupported` for every client-mode session regardless of state — the
  gate ordering changes which specific error a caller sees, not whether the call succeeds.

`docs/PROGRESS.md`'s Phase 16 row states plainly that both reviews found defects the passing suite had missed;
see it for the complete test-evidence map.

### Post-review fixes (batch E)

A second storage re-review of the batch D diff (verdict: fix-first) found 8 more defects, two of them serious
enough that this ADR's own "Post-review fixes (batch D)" section had overclaimed: D3's "client-mode panics are
fixed" was incomplete (`open_session`/`authenticate_session` still panicked in 3 places D3 never reached), and
this ADR's own trust-model claim about the socket's creation window understated the actual exposure (the root
directory is world-traversable, so the window was a real unauthenticated-superuser path, not merely a
theoretical race). Both are fixed here, along with the six other findings:

- **E1 (client-mode `open_session` still panicked — HIGH).** `open_session` had 3 remaining panic sites in
  client mode, and `authenticate_session` inherited them; a dead owner or the 64-connection cap crashed the
  calling process instead of returning an error, and `htap-wire` hit this once per incoming connection.
  `LocalServer::open_session` is now `Result`-returning end to end (`OwnedServer::open_session` stays
  infallible, since the owner-side path cannot fail this way); `authenticate_session` propagates the error;
  `htap-wire`'s server module reports it to the client (`ER_UNKNOWN`) instead of unwinding. This is a
  behavioral, not merely internal, change: `LocalServer::open_session`'s public signature is now
  `fn open_session(self: &Arc<Self>) -> Result<Session>`, and every one of the roughly 83 existing call sites
  across `crates/htap-server/tests/*.rs` was updated mechanically (`.unwrap()`) to match. No dedicated
  regression test reproduces the dead-owner/connection-cap failure itself at the `LocalServer`/wire-server
  level (unlike D3's own dedicated panic test); the fix is verified by code inspection (no remaining
  `unwrap`/`expect`/`panic` on this path) and by every existing session-opening test continuing to pass against
  the new fallible signature.
- **E2 (a read-side failure bypassed the write-boundary rule — MED).** An undecodable or oversize response used
  to be classified `HtapError::InvalidArgument` ("bad input"), even for a mutation whose request bytes had
  already fully reached the owner — reporting a write that may have been applied as a client-side input error,
  the wrong side of the write-boundary rule entirely. An oversize declared length also left unread bytes on
  the wire, desynchronizing the connection for whatever request came next. Fixed: only a genuinely local,
  pre-write failure (never handed to the OS) still gets `InvalidArgument`; every `read_frame` failure is now
  `Ambiguous` for a non-read-only request or `OwnerUnreachable` for one of the two statically-read-only
  requests, and the connection is marked dead after any framing failure so nothing later reads stale bytes.
  Test: `crates/htap-server/src/ipc/client.rs::ipc::client::tests::malformed_or_oversized_response_marks_connection_dead_and_preserves_outcome_classification`.
- **E3 (socket creation-window is an unauthenticated-superuser path — MED, security).** See "Positive" above
  and the new decision text there: the socket is now bound inside a private `0700` directory, tightened to
  `0600` there, then atomically renamed to `<root>/htap.sock`.
  Test at the time: `ipc::owner::tests::published_socket_is_owner_only` — batch F (F1, below) found this test
  reimplemented the publication sequence itself instead of calling the real startup path and would have passed
  even against a reverted fix; it was deleted and replaced by
  `crates/htap-server/tests/ipc_owner_socket_permissions.rs::startup_publishes_owner_only_socket_and_cleans_up_staging_directory`.
- **E4 (handshake deadline reset on every byte — LOW-MED).** The handshake bound was a per-read socket
  timeout, not an absolute deadline, so a peer trickling one byte at a time could hold a connection thread
  forever; enough such connections made the owner unreachable for everyone. Fixed: a single absolute deadline
  now covers the whole frame read on both the owner's handshake-accept path and the client's connect path.
  Established sessions (past the handshake) are never cut off — a long-running statement or an idle session
  has no deadline at all.
  Test: `crates/htap-server/src/ipc/protocol.rs::ipc::protocol::tests::handshake_frame_deadline_is_absolute_when_peer_trickles`.
- **E5 (non-Unix build broken — LOW, a batch-D/C2 regression).** `Session`'s `remote_request` helper was
  missing `#[cfg(unix)]`, breaking the non-Unix build C2 had otherwise gated correctly. Restored. Verified by
  read-through only, matching C2's own precedent (this workspace's CI runs Linux exclusively).
- **E6 (owner-backed sessions skipped the quarantine gate the remote arms already had — LOW).**
  `catalog_snapshot`, `check_statement_visible`, and `set_max_allowed_packet` checked the
  `AmbiguousOutcomePending`/`CommitOutcomePending`/`RemoteDisconnected` terminal-state gate only on the remote
  branch; an owner-backed (local) session could call them past quarantine. The gate now runs once, ahead of
  the owner/remote split, so both modes agree. (D13's `change_user` gate-ordering quirk is unrelated and
  unchanged — see "Post-review fixes (batch D)" above and `docs/LIMITATIONS.md`.) No dedicated regression test
  was added for this specific symmetry; verified by code read-through (the gate check now precedes the
  `#[cfg(unix)]` remote-branch check in each of the three methods) and by the existing owner-mode quarantine
  tests in `crates/htap-server/tests/session.rs` continuing to pass unchanged.
- **E7 (client-mode getters fabricate values — LOW, documentation).** The `last_query_*` diagnostic getters
  (parallel workers, spill flags, optimizer invocation count) return fixed defaults (`1`/`false`/`0`) in client
  mode rather than reading the owner's actual last-query state, which they cannot see; the configuration
  setters (`with_*`/`set_*`) were already correct as no-ops (configuration is process-global, owner-only), but
  this was previously undocumented. Documented in `docs/LIMITATIONS.md`; no behavior change.
- **E8 (protocol version doesn't cover the payload types it actually gates — LOW).** `IPC_PROTOCOL_VERSION`'s
  doc comment now states plainly which payload types force a manual bump (`IpcRequest`, `IpcResponse`,
  `SessionStatus`, any of their variants, `sqlparser::ast::Statement`, or any nested payload type they carry) —
  this is what makes E2's read-side classification fix meaningful: two builds with a mismatched serde layout
  for one of those types could otherwise complete a handshake (which only checks the numeric constant) and
  then fail at frame decode, which used to surface as a misclassified `InvalidArgument`.

`docs/PROGRESS.md`'s Phase 16 row states plainly that this second review found a security-relevant window and
another panic path, and that batch D's own claim to have removed client-mode panics was incomplete.

### Post-review fixes (batch F)

A third storage re-review of the batch E diff (verdict: fix-first) found 7 more defects, two of them a repeat
of the same pattern the "batch F" heading itself exists to name: a regression test for a real fix that was
written to pass regardless of whether the fix held, and, once one of those was replaced with a test that
actually exercised the real code path, that new test immediately found a further defect the review itself had
only partly identified.

- **F1 (E3's own regression test could not fail — the test-quality defect this batch is named for).**
  `ipc::owner::tests::published_socket_is_owner_only` reimplemented the private-directory-then-rename
  publication sequence itself, inline in the test, instead of calling `LocalServer::open`'s real startup path;
  it would have kept passing even if E3's fix (see above) were reverted back to a bind-then-chmod sequence at
  the published path. It is deleted and replaced by
  `crates/htap-server/tests/ipc_owner_socket_permissions.rs::startup_publishes_owner_only_socket_and_cleans_up_staging_directory`,
  which opens a real `LocalServer` and asserts the actually-published `<root>/htap.sock` is a socket, mode
  `0600`.
- **F2 (the staging-directory leak F1's new test then found).** The private `.htap-ipc-<pid>-<id>` directory
  `ipc::owner::start` creates before binding, tightening, and renaming the socket into place was never removed
  on the successful path — only the socket itself was ever cleaned up. Every server that ever started left one
  of these directories behind in `<root>`, forever, not merely after a crash. Fixed: the directory is now
  removed immediately after the rename publishes the socket, and a new `cleanup_stale_private_directories`
  scans `<root>` for any left by a crashed prior owner and removes them at the start of `ipc::owner::start`,
  before a new private directory is created. This is the specific claim in `docs/OPERATIONS.md`'s and
  `docs/LIMITATIONS.md`'s batch E text that read as though the private directory were only ever a transient
  bind target with no cleanup story of its own — both now describe the successful-path removal and the
  startup sweep explicitly. Test: the same
  `startup_publishes_owner_only_socket_and_cleans_up_staging_directory` above also asserts no `.htap-ipc-*`
  directory remains in `<root>` after a normal, non-crashing `LocalServer::open`/drop cycle.
- **F3 (E1's own no-panic fix had no test either).** E1 made `open_session`/`authenticate_session` fallible
  instead of panicking in client mode, but landed with no test reproducing the dead-owner scenario itself
  (verification at the time was code inspection plus the existing suite passing against the new signature).
  `crates/htap-server/tests/ipc_client_open_session_owner_gone.rs::ipc_client_session_operations_error_when_owner_has_gone_away`
  now proves it directly: with the owner process dropped, both `open_session` and `authenticate_session` on a
  client-mode handle return `Err` within a bounded one-second deadline, never panicking and never hanging.
- **F4 (a post-dispatch response-framing failure was reported as bad input, not as unknown).** If a response
  failed to serialize *after* its statement had already run against the owner's real storage,
  `ipc::owner::write_response`'s fallback reported `HtapError::InvalidArgument` ("bad input") — the wrong side
  of the same write-boundary rule E2 fixed for the read side, and a classification that could have invited a
  caller to retry a mutation that had already happened. It now reports `HtapError::Ambiguous` (unknown
  outcome) whenever the original dispatch had already run, keeping the safe-to-retry `InvalidArgument`
  classification only for a request that failed to decode before anything executed. This is latent today —
  no write path in this phase returns rows through this response frame — but would have gone live the moment
  one did, e.g. an `INSERT ... RETURNING`-shaped future statement. Test:
  `ipc::owner::tests::post_dispatch_serialization_failure_returns_ambiguous_outcome`, which replaces B3's
  original `serialization_failure_is_returned_as_a_normal_error_frame` (that name asserted the now-fixed
  `InvalidArgument` behavior and no longer exists).
- **F5 (a failed write with bytes already on the wire did not mark the connection dead).** `IpcConnection`'s
  write path counted bytes actually handed to the OS (via `CountingWriter`) to classify a write failure as
  `Conflict` or `Ambiguous`, but did not also set the connection's `dead` flag the way the read path already
  does on any framing failure (E2) — a partially written, desynchronizing request left the connection object
  itself still marked usable, so an out-of-tree caller holding it could send a next request onto a connection
  the owner had already given up on. `request` now sets `dead = true` whenever any request bytes were written
  before the failure, matching the read side exactly. Test:
  `crates/htap-server/src/ipc/client.rs::ipc::client::tests::partial_request_write_marks_connection_dead`.
- **F6 (owner-gone login misreported as bad credentials).** With the owner unreachable, `htap-wire`'s
  `authenticate` (initial login) and `respond_change_user` (`COM_CHANGE_USER`'s re-authentication) both fell
  through to their generic `Err(_)` arm and reported `ER_ACCESS_DENIED`/"Access denied for user '...'" —
  indistinguishable from an actual bad password, which would send an operator chasing a credentials problem
  that did not exist. Both call sites now match `HtapError::Conflict` ahead of the generic arm and report the
  real transport failure via `ER_UNKNOWN` instead. Verified by code read-through only, matching the
  read-through-only precedent already set by D5/D10/D11/D12/E1/E5/E6 above; no dedicated wire-level
  integration test names this exact scenario.
- **F7 (an interrupted system call during the handshake failed spuriously).** `read_exact_until` — used only
  for the bounded handshake read on both the owner's accept side and the client's connect side (E4) — treated
  `io::ErrorKind::Interrupted` as an ordinary hard failure and returned it straight to the caller instead of
  retrying the read, so a single `EINTR` arriving mid-syscall (e.g. from an unrelated signal) could fail an
  otherwise-healthy handshake for a reason that has nothing to do with the peer. It now retries the same read
  against the same absolute deadline on `Interrupted`, matching how the ordinary (non-handshake) `read_frame`
  path already behaves via the standard library's own `read_exact` retry semantics. Verified by code
  read-through only; this crate has no seam to inject a real `EINTR` in a test.

`docs/PROGRESS.md`'s Phase 16 row states plainly that this third round found that two earlier fixes (E3's
socket-permission test and E1's no-panic test) had shipped with tests that could not have caught a regression,
and that replacing one of those fake tests with a real one immediately exposed a further defect (the
staging-directory leak, F2) the review itself had only partly identified.

### Reversal path

Every new type here is additive: `ServerMode`, `OwnerRuntime`, `SessionBackend::Remote`, the `ipc` module, and
the `Ambiguous` error variant can all be deleted without touching the durable format, since none of them are
ever persisted — the socket handshake and every frame are transient, in-memory-only protocol state. Reverting
to lock-only behavior means: make `LocalServer::open`'s `Conflict` branch return the error unchanged again
(dropping the `IpcClient::connect` attempt), stop constructing `OwnerRuntime`'s listener, and remove the
`ipc`-only session/error variants — the exhaustive-match discipline the workspace already relies on (Decision
#5 above) means the compiler enumerates every call site that would need to change. No stored data or catalog
format depends on any of this, so a reversal carries no migration.

### Verification

`cargo test --workspace --no-fail-fast` (re-measured after the batch F fixes above): 1535 passed, 0 failed, 1
ignored; `cargo clippy --workspace --all-targets -- -D warnings` clean. See `docs/PROGRESS.md`'s Phase 16 row
for the full test-name evidence map, including the 28 dedicated `ipc_*` integration test files
(`crates/htap-server/tests/ipc_owner_*.rs`, `ipc_client_*.rs`, `ipc_multiprocess.rs`) and the protocol/owner/
client unit tests inside `crates/htap-server/src/ipc/{protocol,owner,client}.rs`. Cross-references: ADR-004
(the single shared MVCC version domain and WAL the owner alone still writes to), ADR-008/009 (durability
invariants, unchanged — the socket carries no durable state), ADR-018 (the `DurablePending`/`RecoveryRequired`
quarantine pattern `Ambiguous` and `RemoteDisconnected` extend), ADR-019 (binary-blob literal substitution,
the concrete case the AST-serialization decision above protects).

---

## ADR-026: Clamp derived `DECIMAL` precision to the supported maximum rather than rejecting the query (Amendment 4)

`Status: Accepted`
`Date: 2026-09-24`

### Context

Phase 17 pins `DECIMAL` as a fixed-point value: a signed 64-bit unscaled integer plus a declared precision and
scale, maximum 18 digits, exact round-half-away-from-zero arithmetic, and a hard error on any *value* that
does not fit its own declared precision. Separately from that, every arithmetic operator (`+ - * / %`) and
aggregate (`SUM`/`AVG`) must derive a result *type* — its own precision and scale — from its operand types,
before any value is known. An earlier decision in this same phase required rejecting a query outright, at bind
time, whenever that derived type's *worst-case* precision exceeded the 18-digit maximum — matching the
already-settled decision to reject silent `Float64` fallback for an inexact result, which this project's user
explicitly ruled out.

That rejection turned out to reject ordinary, realistic queries, not just pathological ones. Textbook
multiplication precision is the sum of the operand precisions (`lp + rp`, or `lp + rp + 1` in some
formulations); `SUM(amount_cents * 0.01)`, with `amount_cents` a `BIGINT` (modeled as 19 integer digits) and
`0.01` binding as `DECIMAL(3,2)`, derives a 23-digit result and was rejected outright, even though every actual
value fits comfortably in 18 digits — a `BIGINT` column holding cents does not actually contain 19-digit
values in any realistic dataset. The money-column shape our TPC-H kit uses, `DECIMAL(15,2) * DECIMAL(15,2)`,
derives 31 digits and fails the same way — `(15,2)` is our own choice (a widely used convention that exceeds
the spec's stated minimum), not something the TPC-H specification requires: Clause 1.3.1 defines its
"Decimal" type only as a range of ±9,999,999,999.99 (a minimum of 12 total digits, 2 fractional), and Clause
1.4's schema definition states no per-column precision or scale anywhere. Four of five A6a-2 checkpoint test
failures traced to this one cause: worst-case precision derivation combined with an 18-digit bound cannot
express the ordinary arithmetic this phase exists to support.

### Options considered

- **(a)** Keep worst-case-derived-precision rejection at bind time (the original decision). Safe — it never
  produces a wrong or silently-narrowed result — but self-defeating for a phase whose purpose is to make money
  arithmetic expressible: it rejects the overwhelming majority of realistic decimal expressions on the strength
  of a bound that describes only a theoretical maximum no real query's values approach.
- **(b)** Fall back to `Float64` when a derived decimal result would not fit. Rejected outright: this is
  exactly the silent-inexactness fallback this project's decimal work exists to avoid, already rejected earlier
  in A6a for the same reason and not reopened here.
- **(c)** Clamp the derived *type's* precision (and, where necessary, its scale) to the 18-digit maximum rather
  than rejecting the query, while leaving the existing per-*value* precision check (`check_decimal_precision`,
  run on the actual computed value at evaluation time) completely unchanged as the only place an actual
  overflow is ever caught.

### Decision

Option **(c)**. This is MySQL's own behavior when a derived `DECIMAL` exceeds its own maximum precision: MySQL
does not reject the query, it narrows the declared type and still errors if an actual value does not fit.
Adopting it here does not touch the settled invariants — the in-memory representation stays a scaled `i64`
with precision and scale, the maximum stays 18 digits, and a value that does not fit its declared precision is
still always a hard error, never a silent truncation or wraparound. Only the derivation of the result *type*
changes, and only in `crates/htap-sql/src/expr.rs::arithmetic_result_type` (arithmetic) and
`crates/htap-sql/src/binder_query.rs` (`SUM`/`AVG`) — see `docs/ARCHITECTURE.md`'s "Derived `DECIMAL`
precision and scale rules" for the exact per-operator formulas and their test citations, which this ADR does
not repeat.

Two consequences of the derivation rules are worth naming explicitly here because they are easy to get wrong
(both did, in this phase, before being caught by tests specifically written to probe them):

- **`DIV`'s scale must come from the dividend plus a fixed increment, not from the divisor's precision.** An
  earlier rule grew the scale with the *divisor's* precision (`max(6, ls + rp + 1)`), which for an integer
  divisor derives a scale of 20 — already above the 18-digit bound, and once clamped would consume every digit
  as fractional, leaving no room for the integer part at all. The corrected rule — scale is the dividend's own
  scale plus four, unconditionally — is both saner and is literally MySQL's `div_precision_increment`.
- **Clamping can force scale above the (already-clamped) precision; when it does, scale is reduced to fit, not
  precision raised past the maximum.** This drops fractional digits from the declared *type* exactly as MySQL
  does. It is not the same thing as losing a fractional digit from a *value* — the value's own scale, and
  therefore its exactness, is set at evaluation time from the actual rescale/round performed, not retroactively
  changed by this clamp; the clamp only affects what a *further* operation downstream declares as its operand
  type. This is called out with an inline comment at the clamp site in `arithmetic_result_type`.

### Consequences

- Ordinary, realistic decimal arithmetic (money math, TPC-H-shaped queries) is now expressible; a query is
  never rejected merely because a theoretical worst case exceeds the maximum.
- This incidentally covers TPC-H's separate, wider "Big Decimal" notion (Clause 1.3.1), which exists because
  an aggregate over millions of rows can exceed a base column's own range: `SUM`/`AVG` here are always clamped
  to the 18-digit maximum regardless of the base column's declared precision, which comfortably holds a
  `SUM(l_extendedprice)`-style aggregate at any scale factor this project's TPC-H kit will realistically run
  (loading cost from fsynced commits becomes the practical ceiling long before an 18-digit aggregate would).
  This is not a claim of exact correctness at scale factors nobody runs.
- A derived type's declared precision can now be narrower than the textbook formula would give for an extreme
  operand combination (e.g. two `DECIMAL(18,0)` operands multiplied derive a *declared* `DECIMAL(18,0)`, not
  `DECIMAL(36,0)`), which is a real, disclosed narrowing of the type system's honesty about extreme cases — but
  it was already true in the sense that the storage layer could never have held more than 18 digits regardless
  of what the type said, so no *value* becomes representable that was not already representable before.
- The one behavior this ADR deliberately does not touch: exactness of a computed *value*. A `DECIMAL(18,0) *
  DECIMAL(1,0)` whose actual product needs 19 digits still fails at evaluation time
  (`crates/htap-sql/src/expr.rs::expr::tests::test_decimal_arithmetic_overflow_and_precision_errors`), exactly
  as it did before this ADR — only the bind-time rejection of the *query* (regardless of the actual values
  involved) is gone.

### How to reverse it

Revert `arithmetic_result_type`'s per-operator precision/scale formulas to reject (return `Err`) whenever the
unclamped derived precision would exceed `MAX_DECIMAL_PRECISION`, and do the same for `SUM`/`AVG` in
`binder_query.rs`. Nothing here is persisted — this is a query-layer, bind-time-only decision with no on-disk
format dependency — so a reversal carries no migration and no format-version change.

### Verification

`crates/htap-sql/src/expr.rs::expr::tests::{test_decimal_required_precision_is_clamped_to_supported_bound, test_decimal_arithmetic_overflow_and_precision_errors, decimal_arithmetic_and_comparison, d7_decimal_division_rounds_non_tie_remainders_correctly, d9_decimal_division_rounding_direction_uses_exact_quotient_sign, test_decimal_rounding_half_away_from_zero_for_rescale_division_and_cast}`,
`crates/htap-server/tests/decimal_aggregation.rs::{test_tpch_style_money_aggregation_uses_exact_decimal_precision, test_decimal_sum_avg_min_max_and_distinct_count, test_decimal_sum_reports_precision_overflow}`, and
`crates/htap-server/tests/decimal_avg_rounding.rs::test_decimal_avg_rounds_half_away_from_zero`. Cross-references:
ADR-008's decimal addendum (the columnar persistence side of the same phase, a separate concern from this
query-layer derivation decision).

## ADR-027: TPC-H query-kit conformance choices — row-limiting `SELECT` syntax for Clause 2.1.2.9; exact rational scale-factor arithmetic

`Status: Accepted`
`Date: 2026-09-26`

### Context

Phase 17 Batch B checkpoint 1 adds `crates/htap-tpch`, a foundation crate carrying the TPC-H schema, all 22
published query texts, and an exact-integer scale-factor helper. Two of that crate's choices are genuine,
hard-to-reverse design decisions rather than incidental implementation details, so both are recorded here
together rather than left to be reconstructed from the code later.

**Row limiting.** Five of the 22 published queries (2, 3, 10, 18, 21) require that a given number of rows be
returned — exactly the first N unless fewer than N qualify. TPC-H Clause 2.1.2.9 permits exactly three
mechanisms for this, and requires that a test sponsor select one and use it consistently for every query that
needs it: (1) vendor-specific control statements supported by the test sponsor's interactive SQL interface
(the clause's own example: `SET ROWCOUNT n`); (2) control statements recognized by the implementation-specific
layer (Clause 6.2.4) used to control a loop that fetches the rows (the clause's own example: `while rowcount <=
n`); or (3) vendor-specific SQL syntax added to the `SELECT` statement itself (the clause's own example:
`SELECT FIRST n`). Of mechanism 3 specifically, the clause states: "This syntax is not classified as a minor
query modification since it completes the functional requirements of the functional query definition and
there is no standardized syntax defined." — i.e. the specification itself treats the limiting mechanism as
completing the query's own functional definition, not as an execution detail external to it, precisely because
no standard SQL syntax for it exists.

**Scale-factor arithmetic.** `scale_factor::scale_factor(text, multiplier)` converts a decimal scale-factor
string and a per-table row-count multiplier into an exact row count. A workload kit's row counts need to be
exactly reproducible for a given scale factor on every platform and every future run — a data generator built
against this helper later in Batch B, and any correctness fixture sized against a specific scale factor, both
depend on that determinism holding forever, not just today.

### Options considered — row limiting

1. **Mechanism 1: a vendor-specific interactive-SQL-interface control statement (e.g. `SET ROWCOUNT n`)
   issued alongside an otherwise-unlimited query.** Rejected for this kit: it moves the row limit outside the
   query text entirely, so a tool that only ever sees the SQL string this crate hands out (`queries::query`,
   `EXPLAIN`, or a later disclosure report reproducing the query text) would show an unlimited query whose
   result is actually limited by a side channel the tool never sees.
2. **Mechanism 2: an implementation-specific fetch-loop control statement (e.g. `while rowcount <= n`, per
   Clause 6.2.4).** Rejected for the same reason as (1): the limit lives in the caller driving the fetch loop,
   not in the query text itself, so `queries::query`'s return value alone would not be the complete
   specification of what actually runs.
3. **Mechanism 3: vendor-specific SQL syntax added to the `SELECT` statement itself (the clause's own example:
   `SELECT FIRST n`; this engine's own syntax is `LIMIT n`).** **Chosen.** The query text this crate returns is
   already the complete, self-contained specification of what runs; a `LIMIT` clause keeps that true for the
   five queries that need a row bound, matches what the clause itself says this mechanism is for (completing
   the functional query definition, since no standard syntax exists), and is the one mechanism of the three
   that this engine's SQL layer already supports natively, so no separate result-truncation code is needed in
   any caller.

### Decision — row limiting

Mechanism 3, applied consistently to all five affected queries and only those five: `queries::query` returns
query 2 with `limit 100`, query 3 with `limit 10`, query 10 with `limit 20`, query 18 with `limit 100`, and
query 21 with `limit 100`. These row counts come from each query's own Functional Query Definition
(Clause 2.4.N.2), not from that query's substitution-parameter defaults (Clause 2.4.N.4, a separate
sub-clause) — the two are easy to conflate but are not the same provenance. No other query in the kit carries
a `LIMIT` that Clause 2.1.2.9 did not ask for.

### Options considered — scale-factor arithmetic

1. **Parse the scale-factor text as `f64`, multiply by the multiplier, round.** Rejected: binary floating
   point cannot exactly represent most decimal fractions, so a scale factor and multiplier that should produce
   an exact integer can drift below or above it before rounding — demonstrated directly by `0.29 * 50` in
   ordinary binary floating point, which lands on `14.499999999999998`, one representable step below the exact
   decimal value `14.5` (`scale_factor::tests::test_scale_factor_exact_vs_float_discrimination`). A row-count
   helper that can silently round a boundary case the wrong way is exactly the kind of drift a workload kit's
   generator must not have.
2. **Parse the scale-factor text into an exact rational number — an `i128` numerator over a power-of-ten
   denominator — multiply and divide with `i128` intermediates, and round half-up in integer arithmetic.**
   **Chosen.** The decimal text is finite by construction (it is not an arbitrary irrational value), so an
   exact rational representation exists and every arithmetic step stays exact until the final, deliberate
   half-up rounding to an integer row count — no intermediate step can introduce drift.

### Decision — scale-factor arithmetic

Option 2. `scale_factor::scale_factor` rejects non-finite/non-positive/malformed input and an `i128` overflow
explicitly (`ScaleFactorError::{InvalidScaleFactor, Overflow}`) rather than wrapping or silently producing a
wrong count, and clamps the minimum result to one row so a very small fractional scale factor still preserves
a caller's structural invariants (e.g. a table that must have at least one row).

### Consequences

- The row-limiting choice commits every future consumer of this crate's query texts (a data generator's
  expected-row-count checks, a disclosure report reproducing the query text, `EXPLAIN` output) to seeing the
  `LIMIT` as part of the query, not as an invisible side channel — this is deliberate, per Clause 2.1.2.9's own
  reasoning, and must stay consistent if a future query is added that also needs a row cap.
- The scale-factor helper's row counts for a given scale factor and multiplier are now a permanent contract:
  anything built against it later in Batch B (a data generator, a correctness fixture sized to a scale factor)
  depends on today's exact counts continuing to hold.
- Neither decision touches any on-disk format or durability invariant; both are query-text/bind-time-adjacent,
  local to `crates/htap-tpch`.

### How to reverse it

Row limiting: replace the `limit n` clause in the affected `queries::query` match arms with plain `Some(query)`
text and implement one of the other two Clause 2.1.2.9-permitted mechanisms at the caller instead (mechanism 1,
a `SET ROWCOUNT`-style interactive-interface control statement, or mechanism 2, an implementation-specific
fetch-loop row-count control per Clause 6.2.4), disclosing the switch as a deviation from the row-limiting
syntax choice recorded here.
Scale-factor arithmetic: replace `scale_factor::scale_factor`'s `i128` rational parse/round with an `f64`
parse-multiply-round; no persisted state or on-disk format depends on it, so a reversal carries no migration,
but every row count it has ever produced for a fractional scale factor could change.

### Verification

`crates/htap-tpch/src/queries.rs` (queries 2, 3, 10, 18, 21 carry `limit 100`/`10`/`20`/`100`/`100`
respectively in their returned text; `queries::tests::test_all_22_queries_bind_and_execute` proves every
query, `LIMIT` included, still binds and executes against the schema) and
`crates/htap-tpch/src/scale_factor.rs::scale_factor::tests::{test_scale_factor_exact, test_scale_factor_fractional, test_scale_factor_rounding_boundary, test_scale_factor_floor, test_scale_factor_overflow, test_scale_factor_invalid_input, test_scale_factor_exact_vs_float_discrimination}`
(7 tests). See `docs/PROGRESS.md`'s Phase 17 (continued) row and `docs/LIMITATIONS.md`'s "TPC-H workload kit
scope and deferred features" for the surrounding contract; the TPC compliance/deviations disclosure document
that formally records the row-limiting choice as a stated (not violating) conformance path is task B9,
[`docs/TPCH-DISCLOSURE.md`](./TPCH-DISCLOSURE.md).

## ADR-028: Statement-scoped hash lookups for eligible correlated subqueries, not a new Semi/Anti JoinKind

`Status: Accepted`
`Date: 2026-09-28`

### Context

Phase 17 task B3's first release-mode TPC-H timing (SF 0.01, all 22 queries) found six queries too slow for a
power/throughput run: Query 4 and Query 21 exceeded a 120-second timeout, Query 19 took about 61 seconds, Query
17 about 22, Query 20 about 20, and Query 22 about 9.7. All six depend on a correlated `EXISTS`/`NOT EXISTS` or
scalar-aggregate subquery; the general query executor (Phase 13, ADR-022) executes a correlated subquery's
whole physical plan once per outer row via the `SubqueryRunner` callback, with no memo keyed on the correlation
value, so a subquery that could be answered from a small keyed relation was instead re-planned-and-scanned once
per outer row. Query 19 additionally nests a join equality inside every branch of a three-way `OR`, which the
optimizer's `conjuncts()` — an AND-only split — never turned into a join key, so it ran as an unbudgeted nested
loop; that is a separate, independent fix (`optimize.rs::factor_or_common_conjuncts`) not covered by this ADR.

The natural database-textbook fix for the correlated-subquery half of this is a semi-join/anti-join rewrite:
turn the correlated `EXISTS`/`NOT EXISTS` into a `Semi`/`Anti` join node in the outer query's own join tree, so
the existing hash-join machinery (build side, equi-key, spill) answers it directly. That would need a new
`JoinKind::Semi`/`JoinKind::Anti` variant threaded through `query::JoinTree`, `JoinSpec`, `evaluate_join_tree`,
its row-width and null-padding invariants (a semi/anti join's output row width differs from an inner/outer
join's — it takes only the probe side's columns, never the build side's), the optimizer's join reordering and
predicate-conservation validator, and the cost model — a genuine operator-hierarchy expansion, not a local
change to subquery preparation.

### Options considered

- **(a) A new `Semi`/`Anti` `JoinKind` in the outer query's own join tree.** The textbook rewrite. Reuses the
  existing hash-join spill/parallelism machinery once built, and would also make an eventual `IN`/`NOT IN`
  rewrite (out of scope here) a smaller step later. Rejected for this task: it touches the row-width invariant
  every join-tree consumer (evaluator, optimizer reordering, predicate-conservation validator, EXPLAIN) already
  relies on, is a materially larger and riskier change than the measured bottleneck requires, and none of the
  panel consulted for this task's plan (`reasoner`/`cx/gpt-6-astra`, `cx/gpt-5.6-sol`, `cx/gpt-5.5`) recommended
  it as the right scope for this fix — see the plan's own "CONSULTED" section.
- **(b) A statement-scoped lookup built once per statement in the subquery-preparation layer, keyed on the
  correlation value(s), probed per outer row.** Narrower: the outer query's `JoinTree`/`JoinSpec`/row-width
  invariants are completely untouched; only `query_exec.rs`'s existing `PreparedCorrelatedQuery` preparation
  step (already run once per statement, not per row, since Phase 14's optimizer invocation) changes, becoming
  an enum of `PerRow`/`ExistsLookup`/`ScalarAggregateLookup`. The lookup is built through the unmodified
  ordinary query-execution path (`run_query_with_budget`) and the existing equi-join key-normalization helpers,
  reusing tested code rather than adding a new join operator.
- **(c) Do nothing structural; only add the `OR`-factoring fix and accept the six queries' running time.**
  Rejected: the measured problem (two outright timeouts, four multi-second queries) is exactly what this task
  was scoped to fix, and the panel's independent design work converged on a build-once/probe-per-row lookup as
  tractable within the same task.

### Decision

Option **(b)**. A correlated `EXISTS`/`NOT EXISTS` with at least one key equality (`CorrelatedColumnRef =
local-only expr`), optionally plus residual correlated conjuncts that are not key equalities, and a correlated
ungrouped scalar-aggregate subquery with at least one key equality and no residual, are answered from a
`HashMap`-backed lookup (`crates/htap-server/src/decorrelate.rs`) built once per statement and probed per outer
row through the same `SubqueryRunner::run` entry point every other correlated subquery already uses. Every
entry in the subquery's `correlated_outer_refs` must be accounted for by a key or residual conjunct (a
correlated reference anywhere else — `ON`, projection, `HAVING`, a nested subquery — is ineligible), `EXISTS`
additionally rejects aggregates/`GROUP BY` (an always-true `EXISTS(SELECT COUNT(*) ...)` must not be treated as
key-filterable), and the scalar-aggregate shape additionally requires exactly one aggregate in a single
projection with no `HAVING`/`DISTINCT`/window/`LIMIT`/`OFFSET`/`ORDER BY`. A key component that is `NULL` on
either the build or probe side never matches, following SQL's own equality semantics; the scalar-aggregate
miss/NULL-key case is answered by evaluating the same ungrouped projection once over a guaranteed-empty input
through the unmodified evaluator, so `COUNT(*)` gives `0` and `SUM`/`AVG` give `NULL` without a per-aggregate
special case. Every ineligible shape, a build-query error, or a failed `MemoryBudget` reservation for the built
rows keeps the pre-existing per-row path unchanged — a slow success is never turned into an error. A new
`SubqueryBudget::enter_cached()` still enforces the nesting-depth cap (a cached lookup can still nest inside
another correlated subquery) but not the 10,000-invocation cap, because a lookup probe is not a fresh subquery
execution against the statement's snapshot; it is a hash lookup against a relation already materialized once.

### Consequences

- No new `JoinKind` variant, so every existing join-tree consumer (evaluator, optimizer reordering and its
  predicate-conservation validator, `EXPLAIN`, the row-width/null-padding invariants) is provably unaffected —
  the outer query's own execution plan is byte-for-byte identical to before this task whether or not any of its
  subqueries decorrelates.
- The fix is confined to subquery preparation and a new module; `htap-sql::query`/`JoinTree`/`JoinSpec` gained
  no new field or variant, and no on-disk format, wire protocol, or durability path changed (`storage-reviewer`:
  n/a for this reason).
- The scope is narrower than a semi-join rewrite would eventually be: a correlated `IN`/`NOT IN` is not
  rewritten at all (no TPC-H query needed it, so it was out of scope), a scalar aggregate with a residual
  correlated predicate still falls back to the per-row path (no known TPC-H query needs it either), and the
  uncorrelated `IN`/`EXISTS` membership scan remains an unrelated, still-deferred O(n)-per-row check. See
  `docs/LIMITATIONS.md`'s "Completed local MVP — correlated-subquery decorrelation and OR factoring (Phase 17,
  task F6)" for the complete eligibility contract.
- A future genuine semi-join rewrite (option (a)) remains open and is not foreclosed by this decision — this
  ADR only records why it was not the right scope for the measured problem, not that it is wrong in general.

### How to reverse it

Delete `crates/htap-server/src/decorrelate.rs`, the `mod decorrelate` declaration, `PreparedCorrelatedQuery`'s
`ExistsLookup`/`ScalarAggregateLookup` variants and their construction in `query_exec.rs` (falling back to
`PerRow` unconditionally restores the exact pre-task-F6 execution path), and `SubqueryBudget::enter_cached()` in
`htap-sql/src/expr.rs` (reverting `subquery_result_rows` to always call `enter()`). Nothing here is persisted,
so a reversal carries no migration or format-version change.

### Verification

`crates/htap-server/tests/query_exec.rs`'s
`{test_key_only_correlated_exists_is_decorrelated_once_per_statement, test_decorrelated_exists_single_and_composite_keys, test_decorrelated_exists_with_q21_shaped_residual, test_decorrelated_exists_residual_null_does_not_match, test_decorrelated_exists_residual_edge_cases, test_decorrelated_exists_without_key_conjunct_falls_back, test_decorrelated_exists_normalizes_numeric_key_types, test_decorrelated_exists_aggregate_and_residual_fall_back, test_decorrelated_exists_build_error_triggers_fallback, test_decorrelated_exists_probed_over_invocation_cap_succeeds, test_decorrelated_exists_outer_between_distinct_inner_columns, test_decorrelated_exists_scalar_subquery_residual_falls_back, test_decorrelated_scalar_aggregate_q17_shape_missing_group, test_decorrelated_scalar_aggregate_miss_values, test_decorrelated_scalar_aggregate_composite_key_missing_group, test_decorrelated_scalar_aggregate_null_composite_key_components_miss, test_decorrelated_scalar_aggregate_normalizes_numeric_keys, test_decorrelated_scalar_aggregate_local_filter_decorates, test_decorrelated_scalar_aggregate_correlated_residual_falls_back, test_q19_shaped_or_predicates_return_hand_computed_revenue, test_reordered_inner_join_predicate_placeholders_are_boolean_true}`,
the pre-existing `test_correlated_subquery_is_optimized_once_per_statement`/
`test_correlated_subquery_caps_fire_during_execution`/`test_correlated_subquery_through_window_spec` passing
unchanged against the new eligibility gate, `crates/htap-sql/src/optimize.rs`'s
`optimize::tests::{factors_shared_equality_from_two_and_three_branch_ors, factoring_collapses_when_a_branch_is_only_the_shared_conjunct, factoring_is_a_no_op_without_a_shared_conjunct, factoring_does_not_move_function_or_subquery_conjuncts, factors_or_nested_inside_and}`,
and `crates/htap-tpch/tests/load.rs::all_22_queries` (release, `#[ignore]`d, `--ignored`), which measured all 22
queries finishing in about 10.3 seconds total after this change (Query 4 timeout → 0.29s, Query 21 timeout →
0.89s, Query 19 61s → 0.28s, Query 17 22s → 0.67s, Query 20 20s → 0.48s, Query 22 9.7s → 0.07s). See
`docs/PROGRESS.md`'s Phase 17 (continued) row and `docs/LIMITATIONS.md`'s "Completed local MVP —
correlated-subquery decorrelation and OR factoring (Phase 17, task F6)" for the surrounding contract.

## ADR-029: TPC-H refresh functions (RF1/RF2) as per-order transactions; RF2 keyed independently of the generated `Dataset`

`Status: Accepted`
`Date: 2026-09-28`

### Context

Phase 17 Batch B task B7 adds `RF1` (new-sales) and `RF2` (old-sales) to `crates/htap-tpch`. Both functions
insert or delete a whole scale-factor-sized batch of orders (`SF * 1500` per stream set) plus each order's 1-7
lineitems. Two questions have no single obviously-correct answer and each commits the crate to a shape that
would be expensive to unwind once callers depend on it, so both are recorded here together.

**Transaction granularity.** TPC-H Clause 2.5.2 states a refresh function "can be decomposed into any number of
database transactions," as long as each is atomic and leaves the database logically consistent, and gives
exactly this project's shape as its own example: adding or deleting an order modifies LINEITEM and ORDERS in
the same transaction. Separately, `htap-server::Session`'s buffered `WriteSet` commits as a single durable
request with an effective payload cap of about 4 MiB (`docs/LIMITATIONS.md`'s "Effective 2PC transaction
payload cap"); a whole RF1 stream at SF 1 (1,500 orders, up to 7 lineitems each) serializes to roughly 4.5-5 MB,
over that cap on its own, independent of the spec question.

**RF2's key source.** RF2 must delete the same order keys RF1's *load-time* generation originally inserted at
the corresponding sparse slot (Clause 4.2.4). Those keys could be recomputed on demand from `(scale_factor,
stream)` alone, using the same sparse-key formula the loader uses, or `rf2_old_sales` could require the
caller to pass in the original loaded `Dataset` (or a `Vec` of its order keys) so RF2 deletes exactly what was
actually inserted, rather than trusting a recomputation to agree with it.

### Options considered — transaction granularity

1. **One transaction per refresh-function call**, covering every order and lineitem in the stream.
   Rejected: it exceeds the ~4 MiB effective payload cap at SF 1 on its own; a fallback that tried the whole
   transaction first and only split on `HtapError::InvalidArgument` was also rejected in panel review, since
   that error variant is too broad — a fallback keyed on it could turn an unrelated real bug (a malformed date,
   a duplicate key) into a silent partial-then-full-retry sequence instead of surfacing the bug.
2. **One transaction per order** (the order row plus all of its lineitems), stopping at the first failing
   order and leaving every earlier order's transaction committed. **Chosen.** Matches Clause 2.5.2's own
   worked example exactly, stays well under the payload cap at any scale factor this crate supports, and keeps
   each order's own atomicity (all lineitems or none) exactly as strong as a single whole-function transaction
   would have given it — only *cross-order* atomicity within one call is given up, and Clause 2.5.2 explicitly
   permits that.
3. **One transaction per lineitem.** Rejected: strictly weaker than option 2 for no cap-driven reason (a single
   order's lineitems fit the payload cap trivially), and it would let a crash leave an order with only some of
   its lineitems committed — the one thing Clause 2.5.2's own example transaction boundary exists to prevent.

### Decision — transaction granularity

Option 2. `apply_one_order_insert`/`apply_one_order_delete` each open, execute, and commit (or roll back) one
order's transaction; `rf1_new_sales`/`rf2_old_sales` loop over their stream's orders calling these, stopping and
returning the first error without attempting the remaining orders.

### Options considered — RF2's key source

1. **Recompute keys from `(scale_factor, stream)` alone**, using the same `sparse_order_key_in_slice` formula
   (slice 0) the loader used, deleting each candidate key via `DELETE ... WHERE o_orderkey = <key>` (and the
   matching lineitem deletes) whether or not a row with that key is actually present. **Chosen.** RF2 needs no
   dependency on a generated `Dataset` at all — a caller that only ever loaded through some other path (or lost
   the in-memory `Dataset`) can still run RF2 correctly, since `Mutation::Delete` is an unconditional tombstone
   and deleting an absent key is a no-op; this was also the unanimous recommendation across the three external
   models consulted at the plan gate.
2. **Require the caller to pass the original `Dataset` (or its order keys)** and delete exactly those rows.
   Rejected: it ties RF2's signature to a specific in-memory representation of the loaded data that a caller
   using the bulk loader once and then discarding the `Dataset` would no longer have, is unnecessary since the
   sparse-key formula is already the single source of truth both the loader and RF1 use, and would silently
   diverge from real TPC-H tooling, where a refresh stream is driven by scale factor and stream number, not by
   a copy of the previously generated rows.

### Decision — RF2's key source

Option 1. `generate_rf2_plan(scale_factor_text, stream)` derives its candidate order keys purely from the scale
factor and stream number via `sparse_order_key_in_slice(_, 0)`, the same formula `generate::orders` uses at load
time; the crate's tests use a `Dataset` only to cross-check that these recomputed keys agree with what the
loader actually produced (`rf2_stream_one_keys_match_the_load_order_key_range`), not as a runtime dependency of
`rf2_old_sales` itself.

### Consequences

- **Atomicity is per order, not per call.** A crash or a mid-stream error during `rf1_new_sales`/`rf2_old_sales`
  leaves a committed prefix of orders and stops there; no order is ever left half-inserted or half-deleted, but
  the caller must treat a failing refresh call as "some orders applied, then it stopped," not as fully
  all-or-nothing. This is exactly what Clause 2.5.2 permits and disclosed in `docs/LIMITATIONS.md`.
- **RF1 is a plain `INSERT`, so it upserts on retry.** The engine's `INSERT` performs no primary-key-existence
  check; running the same RF1 stream a second time (with the same seed) writes byte-identical rows again rather
  than failing or being rejected as a duplicate. Not guarded by this task; disclosed in
  `docs/LIMITATIONS.md`.
- **RF2 never scans a table.** Each order's transaction issues exactly seven full-primary-key point deletes of
  `lineitem` (`l_orderkey`, `l_linenumber` 1..=7 — a missing line number is a harmless no-op) plus one
  full-primary-key point delete of `orders`, so RF2's cost is proportional to orders processed, not table size.
- **Keys follow Clause 4.2.4's asymmetric stream caps, not its full reuse cycle.** RF1 streams 1-1000 write
  into slice 1 (the second eighth of each block of 32 keys, as Clause 4.2.4 specifies); this crate additionally
  accepts RF1 streams 1001-3000, continuing into slices 2 and 3, so up to three refresh cycles of new sales can
  run before a key slice repeats. RF2 streams 1-1000 delete only from slice 0 (the original load's keys).
  Clause 4.2.4.3's full 4,000-pair cycle that reuses all four quarters together is not implemented; a stream
  number outside `1..=3000` (RF1) or `1..=1000` (RF2) returns a typed `RefreshError::StreamOutOfRange` rather
  than a silent wraparound or panic.
- **Refresh-inserted orders do not carry the load-time Query 13 forced-comment phrase.** `generate_rf1_rows`
  reuses ordinary text generation for `O_COMMENT`; the forced phrase the load-time generator adds for Query 13
  is not reproduced for refresh rows.
- **`queries::query`'s panic on an invalid scale factor (tracked as F3) is untouched by this task**;
  closed by task B8 (ADR-030), which changes `queries::query` to return `Result<String, QueryError>`.
- Nothing here touches any on-disk format, WAL, or durability invariant: both functions are ordinary SQL
  executed through the existing public `Session` API (`begin`/`execute`/`commit`/`rollback`), so no format
  version bump and no storage review were needed for this task.

### How to reverse it

Transaction granularity: replace the per-order loop in `rf1_new_sales`/`rf2_old_sales` with a single
`session.begin()`/many `execute()` calls/`session.commit()` spanning the whole stream; this reintroduces the
~4 MiB payload-cap failure at SF 1 and above, so it is not a safe reversal without first raising or chunking
around that cap.
RF2's key source: change `generate_rf2_plan`'s signature to take a `&Dataset` (or `&[i64]`) instead of deriving
keys from `sparse_order_key_in_slice`, and delete exactly the keys passed in; no persisted state depends on the
current signature, so this is a pure API change with no migration.

### Verification

`crates/htap-tpch/src/refresh.rs`'s own unit tests
(`refresh::tests::{rf1_counts_and_generation_are_deterministic, rf1_streams_use_the_expected_sparse_slices, out_of_range_streams_are_rejected, rf2_stream_one_keys_match_the_load_order_key_range}`),
`crates/htap-tpch/tests/refresh.rs`
(`test_rf1_inserts_orders_and_lineitems_with_correct_counts, test_rf2_deletes_orders_and_makes_rows_gone, test_rf1_then_rf2_restores_counts_with_quarter_shift, test_partial_failure_rolls_back_without_committing_half_order, test_out_of_range_streams_are_rejected`),
and (ignored, `--ignored`) `crates/htap-tpch/tests/correctness_generated.rs::test_refresh_rf1_rf2_stream_one_updates_q1_and_q6`,
which mutates a loaded `Dataset` by the same RF1+RF2 stream applied through the engine and checks Query 1 and
Query 6 against the independent oracle over the mutated dataset. See `docs/PROGRESS.md`'s Phase 17 (continued)
row and `docs/LIMITATIONS.md`'s "TPC-H workload kit scope and deferred features" for the surrounding contract.

---

## ADR-030: TPC-H Power/Throughput driver diagnostics — no official metric, fixed validation parameters, caller-supplied refresh key stream, and the `execution_lock` concurrency disclosure

`Status: Accepted`
`Date: 2026-09-28`

### Context

Phase 17 Batch B task B8 adds `power_test`/`throughput_test` to `crates/htap-tpch::drivers`, the last piece of
the query-kit foundation before a TPC compliance/deviations disclosure document (task B9). The TPC-H
specification's Power Test (Clause 5.3.4) and Throughput Test (Clause 5.3.5) define exact procedures — a single
query stream interleaved with one refresh stream for Power, `S` concurrent query streams plus one refresh
stream running `S` refresh-function pairs for Throughput — and exact metric formulas (`QppH@Size`, `QthH@Size`,
composed into `QphH@Size` and `$/QphH@Size`) that only an audited, full-compliance implementation may report
under those names. This project is not audited and is not TPC-H compliant (`docs/LIMITATIONS.md`'s "No TPC-H
compliance or comparability claim"), so four questions had to be settled before any driver code was written,
each one a decision that would be costly to reverse once a caller depended on the resulting API or on a
published number: whether to compute the official metrics at all (even unpublished, for internal use); whether
to implement Clause 2.1.3.3's per-stream random substitution-parameter generation; who supplies a refresh
function's key-stream number; and how to define and disclose "concurrency" given `htap-server`'s existing
concurrency model.

**The `execution_lock` finding.** `LocalServer::execute`, `Session::execute`, and `Session::commit`
(`crates/htap-server/src/lib.rs`, lines 199, 443, 455, 1210) each hold one process-wide `Mutex<()>` for the
entire duration of the call. This was already true before this task (see the safe multi-thread pattern in
`crates/htap-server/tests/session_concurrency.rs`) and is unrelated to this task's own design choices, but it
directly determines what a "throughput" driver can honestly claim: no two statements from different sessions
ever execute inside the engine at the same instant, regardless of how many threads submit them.

### Options considered — official metric

1. **Compute and report `QppH@Size`/`QthH@Size`/`QphH@Size`/`$/QphH@Size`,** clearly labeled as unaudited/
   unofficial (e.g. "unofficial QphH-shaped estimate"). Rejected by all three consulted models: the TPC's own
   policy (Clause 0.3 / the Fair Use policy referenced throughout the specification) restricts these composite
   metric names to results that have gone through the full disclosure and audit process; even a clearly
   "unofficial" label risks the number being quoted out of context later, and nothing about this project's scope
   requires the composite formula to exist at all.
2. **Report no official metric under any name; report only neutral diagnostics in seconds.** **Chosen.**
   `PowerTestReport`/`ThroughputTestReport` carry `Vec<QueryTiming>`/`Vec<RefreshTiming>` (raw durations) plus
   exactly one summary number each: `rounded_interval_geomean_seconds` (Power) and `measurement_interval`
   (Throughput), both plain seconds with no query-per-hour conversion. Two `pub const` labels,
   `POWER_METRIC_LABEL` and `THROUGHPUT_METRIC_LABEL`, exist so a caller printing a report has fixed, reviewed
   text rather than composing its own label that might drift toward a forbidden name; a unit test
   (`test_labels_do_not_contain_official_metrics`) asserts both labels contain none of `qphh`, `qpph`, `qthh`,
   `price`, `performance`, `composite`, or `query-per-hour`, case-insensitive.

### Decision — official metric

Option 2. No `QppH`/`QthH`/`QphH`/`$/QphH` (or any renamed equivalent) is computed anywhere in
`crates/htap-tpch`. `power_test`/`throughput_test` report rounded-interval and measurement-interval diagnostics
in seconds only.

### Options considered — Clause 2.1.3.3 substitution parameters

1. **Implement the specification's random substitution-parameter generation** (a per-stream seeded draw from
   each query's parameter domain, Clause 2.1.3.3), i.e. a minimal QGen-equivalent. Rejected: this is a
   substantial, spec-precise random-generation subsystem in its own right — comparable in scope to task B2's row
   generator — and no part of this task's brief calls for it; building it as a side effect of the driver task
   would blur B8's own scope and duplicate B2's already-disclosed "no PRNG algorithm is specified, so no sequence
   is spec-derivable" reasoning for a different generation problem.
2. **Use the specification's own fixed validation-default parameters (`params::fixed_parameters`) for every
   stream.** **Chosen.** Every query in every stream, power or throughput, binds through the same
   `queries::query(query_number, scale_factor)` the correctness fixtures and the independent oracle already use,
   with the validation defaults task B2's checkpoint-1 work already ships. This is a disclosed deviation from
   Clause 2.1.3.3, not silently substituted: `crates/htap-tpch/src/drivers.rs`'s module doc, `docs/LIMITATIONS.md`,
   and this ADR all state it, and there is no QGen-equivalent parameter generator anywhere in this crate.

### Decision — Clause 2.1.3.3 substitution parameters

Option 2. `power_test`/`throughput_test` never draw per-stream parameters; every query binds with the fixed
validation-default parameters, for every stream, every time.

### Options considered — refresh key-stream number

1. **Compute the refresh key-stream number internally** (e.g. derive it from the driver's own stream index).
   Rejected: it hides the mapping between a query stream and the refresh keys it consumes behind an internal
   convention a caller cannot see or override, and the specification itself treats the refresh key stream as an
   input the test sponsor supplies, not something the driver infers.
2. **Take the refresh key-stream number as an explicit caller-supplied parameter.** **Chosen.** `power_test`
   takes `refresh_key_stream: u32` directly; `throughput_test` takes `first_refresh_key_stream: u32` and assigns
   `first..first + stream_count - 1` to its `stream_count` refresh pairs, validated with checked arithmetic
   against the `1..=1000` RF2 range (ADR-029) before any thread is spawned. The convention used in this crate's
   own tests — power at key stream 1, throughput starting at key stream 2 — is a test-only convention, not part
   of the public contract.

### Decision — refresh key-stream number

Option 2. Both driver entry points require the caller to supply the refresh key-stream number(s) explicitly.

### Timing definitions (recorded here since they have no single unambiguous spec reading)

- **QI (query interval).** Submission-to-next-submission, not call latency: `QueryTiming::submission_interval`
  for query *i* is `submission[i+1] - submission[i]`; the last query in a stream uses its own
  `completion - submission` instead, since there is no next submission to measure against. This matches Clause
  5.3.2's own definition of `QI` as a submission-to-submission gap, not an `execute()` return-time measurement,
  which would instead measure engine-internal latency.
- **Power Test summary.** `rounded_interval_geomean_seconds` is `exp(mean(ln(rounded_seconds)))` over exactly 24
  intervals — RF1's duration, all 22 `QI`s in `query_order(0)`, and RF2's duration — each individually rounded to
  the nearest 0.01s with a 0.01s floor (Clause 5.3.7.5: an interval that rounds to zero is reported as 0.01s, never
  0s, so the geometric mean is always well-defined and positive).
- **Throughput Test summary.** `measurement_interval` runs from the single earliest submission across every
  query stream and the refresh thread to the single latest completion across all of them (Clause 5.3.6.1), rounded
  up rather than to the nearest value (Clause 5.3.6.2's own worked example, 923.741s → 923.75s, is reproduced as a
  test, `test_round_up_centisecond_worked_example`) — a measurement interval must never under-report elapsed wall
  time.
- **Thread lifecycle.** Every spawned thread — every query stream and the refresh thread — is always joined,
  including on the failure path; a panicked thread's join failure becomes `DriverError::WorkerPanicked` rather
  than being ignored or left detached. All query-stream threads and the refresh thread wait on one shared
  `Barrier` before submitting their first statement, so no stream gets a submission-time head start from opening
  its session or computing its query order.
- **No retry on conflict.** Neither driver retries `HtapError::Conflict`. Both require a single-writer
  precondition — no external writer touches `server` while a driver run is active — documented on
  `PowerTestReport`/`ThroughputTestReport` themselves; a conflict is returned to the caller directly, exactly
  once, and never silently retried.

### The `execution_lock` concurrency disclosure

**This is the most consequential fact this ADR records, not a footnote.** `htap-server::LocalServer` serializes
every `execute`/`commit` call through one process-wide `Mutex<()>` held for the call's whole duration. This means
`throughput_test`'s "concurrency" is real only at the *submission* level (multiple threads race to submit their
next statement) and the *session* level (each stream keeps its own `Session`, so cross-stream state never leaks)
— it is never real at the *execution* level: two statements from two different streams never run inside the
engine simultaneously. A conforming TPC-H Throughput Test's whole premise is genuinely concurrent multi-user
execution; this driver cannot honestly claim that, and does not. This is disclosed in three places so it cannot
be read past: the `crates/htap-tpch/src/drivers.rs` module doc, the doc comments on `ThroughputTestReport` and
`StreamReport`, and here. `docs/LIMITATIONS.md`'s "TPC-H workload kit scope and deferred features" restates it
for anyone reading limitations rather than source or ADRs.

### Appendix A reproduction

The 41-row query-order permutation table (`QUERY_ORDER` in `crates/htap-tpch/src/drivers.rs`) is TPC-H
Specification Appendix A, reproduced verbatim; the specification itself attributes that table's origin to F.
Moses and O. Oakford, *Tables of Random Permutations*, 1963, pp. 52-53, and `drivers.rs`'s module doc carries
that attribution alongside the existing TPC copyright/permission notice `queries.rs` already carries for the
published query texts (see `ATTRIBUTION.md`). This is the same category of reproduction as the query texts
already shipped in Batch B checkpoint 1 — a verbatim table the specification itself publishes and permits
copying, under the same TPC permission notice, not a new attribution question. One external model raised a
caution about reproduction permission during the plan-gate panel; the coordinator's reading, on the same basis
as the already-shipped query texts, is that Appendix A is in scope for verbatim reproduction under the
specification's existing copying-by-permission notice.

### Consequences

- No caller of this crate can ever be quoted a `QppH`/`QthH`/`QphH`/`$/QphH` number, official or unofficial,
  because none is ever computed.
- Every stream in every run uses the same fixed query parameters; a caller wanting genuine per-stream parameter
  variation (closer to a real Throughput Test) would need to build a QGen-equivalent generator first — not part
  of this crate today.
- Callers must track their own refresh key-stream bookkeeping; nothing in this crate infers or reuses a key
  stream across calls.
- A driver-reported "throughput" number reflects submission-and-session-level scheduling under one process-wide
  lock, not genuinely parallel query execution; this must be repeated wherever a driver report is discussed, not
  assumed to be self-evident from the function name.
- `DriverError` (`QueryError`/`RefreshError`/`HtapError`/`InvalidStreamCount`/`InvalidRefreshKeyRange`/
  `WorkerPanicked`) is a new public error type with `std::error::Error`/`From` impls for its three wrapped
  variants, following the same shape as `QueryError` and `RefreshError`.
- Nothing here touches any on-disk format, WAL, or durability invariant: both functions issue ordinary SQL and
  session calls through the existing public `Session`/`LocalServer` API, so no format version bump and no
  storage review were needed for this task.

### How to reverse it

Official-metric policy: nothing here is structurally hard to reverse — a caller-facing `QphH`-shaped
computation could be added later as a clearly separate, explicitly unofficial function once the TPC's naming
restrictions are re-reviewed; today's `DriverError`/report types would not need to change to add it. Fixed
validation parameters: replacing them with a real QGen-equivalent generator is additive (a new parameter-draw
function called instead of `params::fixed_parameters`), not a breaking change to `power_test`/`throughput_test`'s
signatures. Refresh key-stream number: could be made optional with an internal default derivation later without
breaking existing callers that already pass one explicitly, since Rust has no optional-parameter overload
ambiguity here — a new `_with_default` entry point would be additive.

### Post-review fixes

An external review of the B8 diff found four more defects, all fixed in `crates/htap-tpch/src/drivers.rs`:

1. **`power_test` validated the scale factor but not the refresh key stream before calling RF1.** An
   out-of-range `refresh_key_stream` would have let RF1 commit its inserted orders/lineitems before any
   validation failed, and there would then be no way to ask RF2 to delete them (RF2 requires the same
   `1..=1000` range). `power_test` now checks `(1..=1_000).contains(&refresh_key_stream)` first, before opening
   either session or calling RF1, so an invalid stream is rejected with zero committed changes. Test:
   `crates/htap-tpch/tests/drivers.rs::power_driver_rejects_invalid_refresh_key_stream_without_changes`
   (`#[ignore]`d, release mode), which asserts the order count is unchanged after rejecting both `0` and
   `1_001`.
2. **`throughput_test` validated `stream_count` and the upper end of the refresh key-stream range, but not a
   zero `first_refresh_key_stream`.** `throughput_test` now also rejects `first_refresh_key_stream == 0` up
   front, alongside the existing checks, before any thread is spawned. Test:
   `drivers::tests::test_throughput_rejects_invalid_inputs_before_workers` (extended, not renamed, to cover
   all three rejection cases: zero stream count, zero first refresh key stream, and an out-of-range key-stream
   window).
3. **A panic during a worker thread's pre-barrier setup could deadlock every other thread.** Each query-stream
   thread and the refresh thread do some work (opening a session, computing a query order) before calling
   `barrier.wait()`; a panic during that setup used to leave the panicking thread never reaching the barrier,
   which would hang every other thread waiting on it forever. That pre-barrier work is now wrapped in
   `std::panic::catch_unwind(std::panic::AssertUnwindSafe(..))` (`crates/htap-tpch/src/drivers.rs` lines 444
   and 488), converting a panic into `Err(DriverError::WorkerPanicked)` that still reaches the barrier normally,
   so every spawned thread calls `barrier.wait()` exactly once regardless of a setup-time panic. Verified by
   code read-through only: this crate has no seam to inject a mid-`open_session` panic, matching the
   read-through-only precedent already set for unseamed fixes elsewhere in this project (see `docs/PROBLEMS.md`'s
   Phase 16 batch D/E/F entries for the same pattern).
4. **The rounding helpers wrapped instead of saturating on an out-of-range duration.** `round_to_nearest_centisecond`
   and `round_up_centisecond` used to cast their `i128` intermediate result to `u64` with `as u64`, which wraps
   silently for a value above `u64::MAX` rather than erroring or saturating. Both now go through
   `u64::try_from(rounded).unwrap_or(u64::MAX)`, so an out-of-range duration saturates at `u64::MAX` instead of
   wrapping to a small, wrong value. Test: `drivers::tests::test_rounding_saturates_at_u64_max`.

None of the four changes any on-disk format, WAL, or durability invariant, so no additional storage review was
needed.

### Verification

`crates/htap-tpch/src/drivers.rs`'s own unit tests (`drivers::tests::{test_rounded_interval_geomean,
test_throughput_rejects_invalid_inputs_before_workers, test_query_order_wraparound,
test_table_11_stream_count_exact_values, test_labels_do_not_contain_official_metrics,
test_rounding_boundaries, test_round_up_centisecond_worked_example, test_rounding_saturates_at_u64_max,
test_permutation_row_coverage, test_every_row_is_permutation}`, 10 tests),
`crates/htap-tpch/tests/drivers.rs` (`power_driver_runs_all_queries_and_refreshes`,
`power_driver_rejects_invalid_refresh_key_stream_without_changes`,
`throughput_driver_runs_query_and_refresh_streams`, 3 tests, all `#[ignore]`d, release mode), and
`crates/htap-tpch/tests/correctness_generated.rs::test_refresh_rf1_rf2_stream_one_all_queries` (`#[ignore]`d,
release mode), which runs all 22 queries in `query_order(0)` against the independent oracle over a mutated
dataset after RF1+RF2. F3's closure is verified by `crates/htap-tpch/src/queries.rs::queries::tests::
test_query_11_and_query_23_error_handling`. See `docs/PROGRESS.md`'s Phase 17 (continued) row (task B8) and
`docs/LIMITATIONS.md`'s "TPC-H workload kit scope and deferred features" for the surrounding contract.

---

## ADR-031: TPC-C population and loading — schema mapping, surrogate `h_id`, decimal scales, timestamps, and generator conformance

`Status: Accepted`
`Date: 2026-09-30`

### Context

Phase 18 adds `crates/htap-tpcc`, a local, unaudited kit derived from the TPC Benchmark C Standard
Specification, Revision 5.11. Batch 1 (commit `2465c2f`) built the schema, the population generator and the bulk
loader. Five points had to be settled before the transaction code (ADR-032) could be written, each of which is
costly to reverse once transactions and tests depend on it: how the specification's nine tables map onto this
engine's DDL; how to give `HISTORY` a primary key, since the engine requires one and the specification's table
has none; which engine types carry the specification's numeric and date columns; how timestamps enter the
database; and how faithfully the generator follows Clause 4.3.3.1. The engine facts that drive these points:
`INSERT` into a `TIMESTAMP` column accepts a date string, a `DATE` literal or an integer, but not a
date-and-time string literal (`crates/htap-sql/src/binder.rs`, the `CommonDataType::Timestamp` branch, lines
1884-1927); `DECIMAL` is a signed 64-bit unscaled integer with at most 18 digits (ADR-026); `INSERT` performs no
primary-key-existence check; and the bulk `COPY` path parses date-and-time strings (`crates/htap-movement/src/codec.rs::parse_timestamp_text`).

### Options considered — table names

1. **Quote the specification's names** (`"ORDER"`, `"ORDER-LINE"`, `"NEW-ORDER"`). Rejected: in this MySQL
   dialect double quotes delimit string literals, not identifiers, and `ORDER` is reserved.
2. **Rename to `orders`, `order_line`, `new_order`.** **Chosen.** Every other table and column keeps its
   specification name in lower case.

### Options considered — `HISTORY` primary key

1. **A natural composite key** (for example the customer key plus `H_DATE`). Rejected: the specification does not
   make any such tuple unique, and two payments by one customer in the same instant would collide silently
   because `INSERT` upserts instead of rejecting a duplicate.
2. **An added surrogate `h_id BIGINT PRIMARY KEY` built from a 15-bit source and a 48-bit sequence
   (`history::build_h_id`).** **Chosen.** Source 0 is the initial population (sequence = row index); runtime
   callers pass a terminal number 1..=32767 and their own sequence, so distinct (source, sequence) pairs never
   collide by construction and the value always fits a non-negative signed `BIGINT`. The driver caps
   `terminal_count` at 32767, and each terminal seeds its sequence from the largest existing `h_id` of its source
   so consecutive runs on one database keep their history rows. Clause 1.4.7 permits added attributes provided
   they do not improve performance (this kit makes no performance claim) and Clause 1.4.10 exempts `HISTORY` from
   the relative-addressing rule.

### Options considered — types

Numeric columns map to `DECIMAL(p, s)` with the specification's own precision and scale (Clause 1.3.1: taxes and
the customer discount at four decimal places, money at two); the specification's date-and-time columns map to
`TIMESTAMP`; keys and counters map to `INTEGER`/`BIGINT`; `O_CARRIER_ID` and `OL_DELIVERY_D` are nullable (they
are `NULL` for undelivered orders). Money is not stored as `Float64`, because the consistency
conditions are exact-equality checks and ADR-026 settled exact decimals for this project.

### Options considered — timestamps

1. **Send date-and-time string literals through `INSERT`.** Not possible: the binder rejects them (see Context).
   Extending the binder was out of scope for a workload kit.
2. **Integer epoch microseconds for everything written by a transaction; the fixed string
   `2000-01-01 00:00:00` for the population, loaded through the `COPY` path, which does parse it.** **Chosen.**
   The transaction request structs carry each timestamp as a caller-supplied `..._micros` field. The consequence is two disclosed departures: transactions do not stamp the current system date and
   time (Clauses 2.4.1.6, 2.7.1.3), and every population row carries one constant timestamp instead of the load
   time (Clause 4.3.3.1) — deterministic, but not what the specification says.

### Options considered — generator

Own SplitMix64 PRNG (the specification names none and permits pregenerated random numbers for the population),
implementing Clause 4.3.3.1 rule by rule: the delivered boundary at order 2100, `O_C_ID` from a random
permutation, the first 1,000 `C_LAST` values iterating 0..999 and the rest NURand(255,0,999), population
`OL_I_ID` uniform, 16-digit `C_PHONE`, `I_DATA`/`S_DATA` of 26-50 characters with `ORIGINAL` in about 10% of
rows, `C_CREDIT = 'BC'` in about 10%, taxes/discounts at four decimal places. One NURand constant (157) serves
the population and every run. One `C` per field shared by all terminals is what Clause 2.1.6 asks, but the `C_LAST`
`C-Delta` (Clause 2.1.6.1, which applies to `C_LAST` only) is then 0 instead of 65 to 119, and `C` is a fixed constant
rather than randomly chosen within [0..A] (both disclosed). `a_string` draws
from a 95-character printable-ASCII alphabet including punctuation, wider than the specification's
alphanumerics; the punctuation was kept because it exercises the SQL-literal escaping that the full-warehouse
driver run found to be broken (ADR-032). Fixes made after the batch 1 external review: the delivered boundary,
`C_PHONE` at 16 digits, `I_DATA`/`S_DATA` lengths.

### Decision

The schema, `history::build_h_id`, the `DECIMAL`/`TIMESTAMP` mapping, the timestamp handling and the generator
choices above, as implemented in `crates/htap-tpcc/src/{schema,history,generate,load}`. The loader mirrors
`crates/htap-tpch/src/load.rs`: a fresh-target check before any DDL, batched independently committed `COPY`
loads, post-load count checks, and no whole-load rollback.

### Consequences

- `HISTORY` keys are unique by construction, so the engine's upsert `INSERT` cannot silently overwrite a history
  row; the price is a key space that a caller must respect (`terminal_count` <= 32767).
- Timestamps in transaction-written rows are only as meaningful as the microsecond values the caller supplies;
  the population's timestamps are identical.
- A failed load leaves created tables and committed batches behind; a retry fails the fresh-target check.
- The kit's departures are listed, with tests, in [`docs/TPCC-DISCLOSURE.md`](./TPCC-DISCLOSURE.md) §3-§4.
- No on-disk format, WAL or durability invariant is touched; the crate uses only the public `LocalServer`/
  `Session` API and the existing CSV movement path, so no storage review was needed.

### How to reverse it

Renaming the three tables or replacing `h_id` with a natural key is a schema change that every transaction
statement and the consistency checker would follow. Sending timestamps as date-and-time strings needs an engine
binder change first; once it exists, the transactions can switch without changing the schema. The generator's
alphabet, `C` constant and timestamps are local to `generate/` and can be tightened independently.

### Verification

`crates/htap-tpcc/tests/schema_ddl.rs::ddl_creates_expected_catalog_schema`; `history::tests::{
distinct_source_sequence_pairs_produce_distinct_ids, rejects_out_of_range_values, accepts_boundary_values}`;
`generate::tests::{population_is_deterministic_and_referentially_closed, order_customer_ids_are_permutations,
taxes_and_discounts_use_four_decimal_precision, customer_bad_credit_rate_is_approximately_ten_percent,
item_and_stock_original_rates_and_cardinalities}`; `generate::text::tests::customer_last_name_uses_required_syllables`;
`crates/htap-tpcc/tests/load.rs::{load_small_subset_with_referential_consistency,
reject_load_when_tables_already_exist, reject_invalid_batch_size_before_ddl}` and (ignored)
`load_with_warehouse_count_1_and_verify_counts_and_values`.

---

## ADR-032: TPC-C transactions, isolation adaptation, and the workload driver — OCC retry, per-district Delivery, escalation, a 23-card deck, and no metrics

`Status: Accepted`
`Date: 2026-09-30`

### Context

Batches 2 and 3 of Phase 18 (commits `fd8d1ff`, `90beb05`) add the five TPC-C transactions, the 12-condition
consistency checker, the Clause 3.4.2 isolation tests and a workload driver to `crates/htap-tpcc`. The engine
provides snapshot reads with first-committer-wins write-write conflict detection at commit; it is optimistic and
never blocks a writer. The specification's isolation requirements (Clause 3.4) assume a locking system in which
a conflicting transaction waits, and its Delivery transaction is defined as deferred, queued work (Clause
2.7.2). Points to settle: how a conflict is handled; how Delivery is executed and retried; what isolation
statement is honest; how the transaction mix is regulated; how a starving transaction is kept live; and whether
any official metric is computed. Engine facts: `execution_lock` (`crates/htap-server/src/lib.rs`) is held per
`execute`/`commit` call, not across a transaction, so transactions from two sessions can interleave statement
by statement but statements never run in parallel; `INSERT` is an upsert; `UPDATE` supports arithmetic `SET` on a
primary-key equality path, and every TPC-C `UPDATE` targets a primary key or a primary-key prefix (Delivery's
`order_line` update uses the three leading columns of the four-column key). No engine prerequisite work was
needed (unlike TPC-H Batch A).

### Options considered — conflicts

1. **Retry inside the engine or hold locks.** Rejected: the engine has no lock manager and this phase adds none.
2. **Surface `TransactionError::Conflict` and let the caller retry the whole transaction from `BEGIN`.**
   **Chosen.** Every transaction function opens, runs and commits (or rolls back) its own database transaction
   through the public `Session` API; `TransactionError` separates `Conflict` (retry) from `ExpectedRollback` (the
   specification's 1% New-Order unused-item rollback), `InvalidInput` and `Htap` (other engine errors). Because
   `INSERT` upserts, new order IDs come from a conflict-protected read-increment of `D_NEXT_O_ID` in the same
   transaction, and two overlapping inserts of the same absent key must yield exactly one commit and one
   `Conflict`; both are tested.

### Options considered — Delivery

1. **Implement Clause 2.7.2 deferred execution** (a queue and a result file). Rejected: it adds a subsystem that
   validates nothing about the engine.
2. **Run Delivery synchronously, one database transaction per district
   (`transactions::delivery_one_district`; `transactions::delivery` loops over it), and let the driver retry only
   the district that conflicted.** **Chosen.** Clause 2.7.4.1's comment permits up to 10 database transactions
   per Delivery; retrying per district means a conflict on one district never redoes committed districts. Skip and
   staleness detection use affected-row counts. The queue and result file are disclosed as not implemented.

### Options considered — isolation statement

The isolation tests of Clause 3.4.2 assume locking; the clause permits different validation techniques for
non-locking schemes if fully disclosed. The nine tests are adapted to check the required outcomes under OCC
(the second transaction never blocks, a conflicting commit fails with `Conflict` and is retried) and must check
those outcomes, not merely accept an abort where a locking test would block. During the Phase 18 plan the wording
of the claim was reviewed with `reasoner` (`cx/gpt-6-astra`), `cx/gpt-5.6-sol` and `cx/gpt-5.5`; all three
recommended the narrow form below, and the phrasing "satisfies the literal P0-P3" is deliberately not used.
Review history: the `architect` model was unreachable at plan time and on four attempts at the batch-4 gate
(HTTP 503 and connection failures), so the final wording was reviewed by the `reasoner` model (`cx/gpt-6-astra`)
instead, and the coordinator verified its points against `crates/htap-tpcc/tests/isolation.rs`.

> **Isolation claim (reviewed).** This is the claim adopted for the kit; the Phase 18 sections of
> `docs/ARCHITECTURE.md`, `docs/LIMITATIONS.md` and `docs/TPCC-DISCLOSURE.md` §3 item 8 condense it and keep every
> qualifier.
>
> This assessment is conditional on all 16 tests in `crates/htap-tpcc/tests/isolation.rs` passing for the build
> under review. The suite contains nine scenarios derived from TPC-C Clause 3.4.2 and seven engine-level tests;
> Clause 3.4.2 permits alternative validation techniques for non-locking schemes provided full details are
> disclosed, and that permission does not by itself establish that each adaptation meets every original test
> objective. For the exercised schedules, the tests demonstrate exclusion of uncommitted reads (tests 1, 2, 4, 6),
> rejection of conflicting commits (first committer wins; tests 3 and 5), rollback non-interference on the asserted
> state (tests 4 and 6), and fixed-snapshot reads (tests 7-9). Deterministic lost-update evidence comes from tests
> 3 (`D_NEXT_O_ID`) and 5 (customer balance); the four barrier-released concurrency tests are smoke tests and do
> not guarantee transaction overlap. Tests 7-9 check snapshot reads, not serializability; test 7 checks explicit
> price reads across a concurrent price update and does not execute a complete New-Order across the change.
> Prevention of the broad P2/P3 phenomena (Berenson et al.) and of A5B write skew is not claimed, nor is ANSI
> Level 3 or serializability; no serializable mode exists. Transactions interleave statement by statement. The
> results rely on atomic commit/rollback, logical-key write-conflict detection including absent-key inserts, and
> retries at the actual transaction boundary (whole transaction for New-Order, Payment, Order-Status and
> Stock-Level; per district for Delivery). INSERT has upsert semantics and does not reject duplicate keys.

### Options considered — transaction mix

1. **Weighted random selection (Clause 5.2.4.1).** Rejected: it does not guarantee the minima over a short run.
2. **A shuffled 23-card deck (Clause 5.2.4.2: 10 New-Order, 10 Payment, 1 each Order-Status, Delivery,
   Stock-Level).** **Chosen.** Each terminal shuffles its own deck. The minima hold per completed deck; a
   transaction limit is a counter shared across terminals and can stop a run mid-deck, so `assert_report` keeps
   1 percentage point of slack, and the observed mix excludes expected rollbacks. The slack lowers the asserted
   floors (42.48% and 3.35%) below the specification's 43.0% and 4.0%, so a full deck is what meets them.

### Options considered — liveness

Without think time, long New-Orders starved under first-committer-wins on the hot warehouse and district rows;
the engine's conflict messages were diagnosed as genuine write-write conflicts, not an engine bug.
1. **Add randomized backoff.** Not chosen: it changes timing without bounding starvation.
2. **After 3 conflicts (`ESCALATION_CONFLICT_THRESHOLD`) run that transaction on the exclusive side of a
   driver-level `RwLock`; after 10 retries (`MAX_CONFLICT_RETRIES`) fail the run.** **Chosen.** A driver liveness
   mechanism, not an engine property; the engine stays optimistic. It is disclosed, and no test asserts
   escalation specifically.

### Options considered — metrics and pacing

No keying time, think time or response-time gating; no `tpmC`, price/performance or any relabeled equivalent
(TPC Policies §8.1.4), in the same spirit as ADR-030. The driver reports only neutral counts (completed,
expected rollbacks, conflict retries, escalations, observed mix, elapsed). `WorkloadReport` carries only numeric
fields and has no label text or `Display` implementation; the six label constants are free `pub const`s, and two
tests scan the constants only for `tpmc`, `tpm-c`, `transactions per minute`, `performance` and `price`.

### Decision

The retry-from-`BEGIN` conflict model, synchronous per-district Delivery, the isolation adaptation with the
claim above, the 23-card deck, the driver-level escalation and the no-metrics policy, as implemented in
`crates/htap-tpcc/src/{transactions,consistency,drivers}` and `tests/`. The MySQL dialect treats a backslash as
an escape, so `transactions::sql_literal` escapes `\` before `'`; the defect was found by the full-warehouse
driver run, and the a-string alphabet of ADR-031 keeps exercising it.

Consistency condition 11 (`count(ORDER) - count(NEW-ORDER) = 2100` per district, Clause 3.3.2.11) holds only for
the initial population, because each Delivery raises the difference by one. This is an observation about the
specification's text, not an engine defect. The checker implements the condition as written. The hand-driven
tests (`tests/consistency.rs::transactions_preserve_derived_condition_11`) assert only `2100 + delivered` for
district 1 and a violation set of exactly {11}; the three workload-driver tests assert the derived invariant
`count(ORDER) - count(NEW-ORDER) = count(ORDER with O_CARRIER_ID)` and that condition 11 is reported if and only
if the difference is not 2100, through `assert_condition_11_and_delivery_invariant` in `tests/drivers.rs`.

### Consequences

- The kit's isolation statement is deliberately weaker than Clause 3.4's Level 3 requirement; nothing here may be
  quoted as serializable or as ACID-compliant. Serializable isolation is not built. (Superseded in part: Phase 19
  built an opt-in SERIALIZABLE level, ADR-033; the default driver mode and every Phase 18 isolation test still run
  under snapshot isolation, and the claim is re-derived in "Update (Phase 19)" below, not widened here.)
- The specification's atomicity (3.2), consistency-test (3.3.3) and durability (3.5) tests are not run as
  specified; what exists is the consistency checker plus the engine's own recovery tests elsewhere.
- Terminal concurrency is submission-level only because of the process-wide `execution_lock` (see ADR-030).
- Full-warehouse tests need about 20.7 GiB (a full load plus the checker) and are `#[ignore]`d; the observed
  full-warehouse driver run (load 60 s, 400 transactions over 2 terminals in 851 s) is an observation, not a
  metric, and engine memory follow-up F7 is pending.
- No on-disk format, WAL or durability invariant is touched; only the public `Session` API is used, so no storage
  review was needed. The validator re-checks that reasoning at the diff gate.

### How to reverse it

Deferred Delivery, a pacing/RTE layer, a serializable mode or an SSI implementation can each be added without
changing transaction signatures; the isolation claim would then be re-derived, not widened in place. The
escalation policy is one constant pair in `drivers.rs`. Replacing the deck with weighted selection is local to
the driver's selection function.

### Update (Phase 19): serializable mode and the re-derived isolation claim

Phase 19 (ADR-033, commits `48b19a0`, `dfd0240`, `d937bb0`, `ff6d026`) built the serializable mode this ADR named as
a follow-on. The mechanism is commit-time read-footprint validation, not SSI. The driver gained an isolation option
(`drivers::DriverIsolation::{SnapshotIsolation, Serializable}` and `drivers::run_with_isolation`; `drivers::run` is
unchanged and defaults to snapshot isolation). Under `Serializable` each terminal runs
`SET SESSION transaction_isolation = 'SERIALIZABLE'` before the start barrier, and serialization failures go through
the existing conflict retry and escalation (they are `HtapError::Conflict`). The reviewed Phase 18 claim above is
unchanged: it still describes the nine adapted Clause 3.4.2 tests and the seven engine-level tests in
`tests/isolation.rs`, which still run under snapshot isolation. The claim below is new for Phase 19. Its wording was
reviewed by the reasoner model (`cx/gpt-6-astra`), and the coordinator verified its points against
`crates/htap-tpcc/tests/serializable.rs`.

> **Isolation claim (Phase 19, reviewed).** This claim is conditional on passing, for the build under review,
> `crates/htap-tpcc/tests/serializable.rs::{customer_balance_pair_write_skew_prevented,
> new_order_queue_cap_phantom_skew_prevented, five_transactions_under_serializable_keep_consistency_conditions}`, the
> engine suites listed in the Phase 19 row of `docs/PROGRESS.md`, and, for the unchanged snapshot-isolation claim, all
> 16 tests in `tests/isolation.rs`. The engine offers opt-in SERIALIZABLE through commit-time read-footprint
> validation against the global commit order (backward OCC, not SSI; ADR-033), and the workload driver supports it.
> Within the supported domain, the guarantee applies only among SERIALIZABLE transactions, with serial order equal to
> commit-version order; it does not extend to mixed-isolation execution. Snapshot isolation, reported as REPEATABLE
> READ, remains the default. Catalog and account state are outside the serializable domain. Primary-key point reads
> track keys; scans track whole partitions, so conservative aborts are expected. Two TPC-C-derived tests stage
> explicit two-transaction schedules: customer-balance write skew and a NEW-ORDER queue-cap phantom skew. Under
> REPEATABLE READ both transactions commit and violate the respective invariant; under SERIALIZABLE the second commit
> fails with a `Conflict` containing "serialization failure", and the invariant holds. A separate run covering all
> five transaction types uses two terminals, 230 transactions, a small hand-built dataset, and one seed. It checks
> consistency conditions, not serializability of the observed history: all conditions other than condition 11 must
> hold, condition 11 must identify exactly the districts whose delivered-order count differs from 2,100, and delivery
> accounting must match completed deliveries in this run. Its additional assertion that at least one validation abort
> occurs is scheduling-dependent; it is not deterministic evidence of enforcement. These TPC-C-derived tests establish
> the named schedule and consistency results, not the general guarantee, whose basis is ADR-033's soundness argument,
> additionally exercised by a seeded brute-force oracle over three-transaction histories on a small key-value table.
> This is a TPC-C-derived, unaudited workload kit: no ANSI Level 3 claim, Clause 3.4.1 Requirement 1 claim, Clause
> 3.4.2 result beyond the Phase 18 claim, TPC-C compliance, or comparability is claimed.

### Verification

`crates/htap-tpcc/tests/isolation.rs::{isolation_test_1_new_order_then_order_status ..
isolation_test_9_order_status_snapshot_has_no_order_phantom, two_concurrent_new_orders_same_district,
concurrent_payments_same_customer, two_concurrent_deliveries_same_warehouse,
concurrent_new_orders_same_stock_decrement, two_inserts_same_absent_pk, insert_over_committed_key_is_upsert,
delivery_one_district_does_not_redeliver_a_completed_district}`; the transaction fixture tests
(`tests/{new_order,payment,order_status,delivery,stock_level}.rs`); `tests/consistency_small.rs` and the ignored
`tests/consistency.rs`; `drivers::tests::{deck_has_required_composition_and_minimum_mix,
generated_default_inputs_obey_ranges_and_rates, report_labels_do_not_contain_official_metrics}`;
`crates/htap-tpcc/tests/drivers.rs::{run_small_consistent_dataset, report_labels_do_not_claim_official_tpcc_metrics}`
and the ignored `consecutive_runs_preserve_history_rows` and `run_full_warehouse_load_ignored`;
`crates/htap-tpcc/tests/payment.rs::bad_credit_data_with_backslashes_and_quotes_round_trips` and
`tests/string_escaping.rs`. See `docs/TPCC-DISCLOSURE.md` for the full deviations list.


## ADR-033: SERIALIZABLE via commit-time read-footprint validation (serializable snapshot validation), not SSI

`Status: Accepted`
`Date: 2026-10-01`

### Context

Until Phase 19 the only isolation level was snapshot isolation (first-writer-wins, write skew permitted, reported
as `REPEATABLE READ`; ADR-018). `docs/LIMITATIONS.md` listed "only `REPEATABLE READ` is offered" as a gap, and
ADR-032 named a serializable mode as a follow-on whose claim must be re-derived rather than widened. Phase 19
(commits `48b19a0` htap-txn foundation, `dfd0240` server/session/SQL wiring, `d937bb0` oracle and recovery tests,
`ff6d026` TPC-C driver option) adds SERIALIZABLE as an opt-in level; snapshot isolation stays the default.

Engine facts that constrain the design (each checked in the code): a session pins a snapshot at `BEGIN` and buffers
writes in a `WriteSet`; `COMMIT` builds one `TransactionRequest` and calls `TransactionManager::commit`, which under
`decision_lock` runs participant prepare (first-writer-wins on written keys), then the journal `Intent`, then the
`Commit` frame, then apply and publish. `execution_lock` is held per statement and per `COMMIT`, but imports
(`htap-movement`) and `TransactionManager::recover()` replays reach the manager without it; only `decision_lock`
orders every commit. Every read goes through two primitives, `LocalServer::read_with_overlay` (point reads, the R5
path, `UPDATE`/`DELETE` by key) and `scan_partition_compact` (everything else). There are no secondary indexes, and a
scan materialises its partition. There is no wire or IPC change available for free: the IPC error enum is
exhaustive and the TPC-C driver retries on `HtapError::Conflict`.

### Options considered

| Option | What it is | Abort behaviour | Cost and fit | Verdict |
| ------ | ---------- | --------------- | ------------ | ------- |
| A. Full SSI (Cahill/Röhm/Fekete) | Track read-write antidependencies of active and recently committed transactions; abort only a transaction that is the pivot of two consecutive rw edges; needs safe-snapshot handling for read-only transactions | Fewest false aborts | SIREAD state for active and committed transactions, retention rules, a larger correctness surface in the commit path | Not chosen now; the refinement path (see "How to reverse it") |
| B. Serializable snapshot validation (SSV, backward read-set validation at commit) | Each serializable read-write transaction records a read footprint; at commit, inside `TransactionManager::commit`, it is validated against an index of writes committed after its snapshot | Aborts on any rw edge to a committed concurrent writer, not only on pivots, so more false aborts than A | Footprint plus a key-to-last-commit-version index pruned to the oldest pinned snapshot; single node with a total commit order, commit-time-only aborts and buffered writes make it a small addition | **Chosen** |
| C. Hold `execution_lock` for the whole transaction | Serial execution | No aborts | No concurrency at all; an idle session blocks every other session and every statement | Rejected |
| D. Promote reads to writes | Treat each read as a write so first-writer-wins covers it | Aborts on every read-write overlap, including read-read with a writer that touches nothing else | Needs every read annotated; scans would need whole-partition write marks; no locking reads exist | Rejected |

`reasoner` (`cx/gpt-6-astra`), `cx/gpt-5.6-sol`, `cx/gpt-5.5` (as a panel) and the `architect` alias all recommended B
as sound and as the right first step. B's substrate (a per-transaction footprint and a recent-writes index) is a
subset of what A needs, so false aborts are reduced by making the footprint more precise or, if measured abort rates
demand it, by evolving toward A, not by weakening validation. The mechanism is named "serializable snapshot
validation" (SSV) and is never called SSI anywhere in the docs or code.

### Decision

**Mechanism.** A serializable read-write transaction pins a ticket (`TransactionManager::pin_serializable`, an RAII
`SerializableTicket` that registers the snapshot in a pinned-snapshot registry and unregisters on drop without ever
taking `decision_lock`) and reads at that snapshot. Reads record a `ReadFootprint` through a `ReadTracker`
(`crates/htap-server/src/lib.rs`):

- point keys, including absent keys and keys served from the session's own write set, recorded in
  `read_with_overlay` and on every primary-key-routed read;
- whole partitions, recorded in `scan_partition_compact` before overlay processing, so empty scans and overlay-only
  results still record;
- per logical table, the sorted partition set resolved before pruning (`resolve_table_and_all_partitions` and the
  primary-key routed paths). The first set seen is kept; a later differing set, or a differing set at commit, is a
  `Conflict` ("catalog changed").

The footprint type also supports half-open key ranges, but the server records no ranges today: every scan is a
whole-partition read. Per-partition point reads promote to a whole-partition read at a cap (100 reads by default).
Primary-key-prefix range narrowing was planned as a precision task and is deferred (see "Consequences").

**Validation point.** Inside `TransactionManager::commit`, under `decision_lock`, after every participant has
prepared and before the next-version check and the journal `Intent`, the footprint is validated against
`RecentWrites`, an in-memory index (`(partition, encoded key) -> last commit version`) of writes committed after
the transaction's snapshot. A failure aborts every prepared participant, sets `TxnState::Aborted`, writes no
journal record and sets no recovery latch. Validation therefore completes before any durable decision exists: an
`Intent` without a `Commit` is aborted by recovery, and a durable `Commit` was validated. After each successful apply,
on both the `commit()` path and the `recover()` replay path, inside `decision_lock`, the transaction's written keys
(`TxnParticipant::written_keys`, implemented by `RowstoreParticipant` from its decoded `Put`/`Delete` mutations) are
recorded at their commit version, but only while at least one ticket is pinned (the pin count is read under
`decision_lock`; with none pinned the index is cleared). SI writers are always recorded, so a serializable
transaction aborts against an SI writer. Lock order is `execution_lock` -> `decision_lock` -> registry mutexes; no
manager callback may take `execution_lock`.

**Serial order** is commit-version order. No on-disk format changes: no new magic, no format version, no recovery
file. Footprints and `RecentWrites` live in memory; an open transaction is not durable, a crash discards sessions, no
dependency crosses a crash, and after reopen the index is empty and every new snapshot is at least the recovered
visible version.

**Read-only and empty-write-set transactions.** A transaction declared `READ ONLY` pins a plain snapshot, records
nothing, is never validated and never aborts. A read-write transaction whose write set is empty at `COMMIT` skips
validation. An autocommit write statement in a SERIALIZABLE session gets a statement-local tracker and ticket and is
validated at its own commit. Since the statement's reads and commit sit under one `execution_lock` acquisition, only an
import (which bypasses that lock) can make the validation fail, and no test provokes that case; ticket release and the
statement result are tested. Autocommit statements that write nothing are unvalidated reads of one statement.

### Soundness argument

Let T be a committed serializable read-write transaction that read at snapshot S_T and commits at version C_T.
Commits are serialized by `decision_lock`, and every commit (session commit, import, `recover()` replay, SI commit)
reaches `TransactionManager`, which records its written keys at its version while a ticket is pinned. Validation
proves that no committed write to any key or partition T read has a version in (S_T, C_T): the only commits with
versions in that interval are those already applied when T validates, and `decision_lock` excludes another commit
between T's validation and its own commit. First-writer-wins at prepare proves no concurrent write to anything T
wrote. T's reads therefore equal what T would have read at C_T, and its writes do not overwrite anything newer, so T
behaves as if it executed atomically at C_T, and ordering committed serializable transactions by commit version gives
a serial order. A transaction with an empty write set is not validated; it observes exactly the prefix of commits up to
its snapshot S and is placed at that position. That placement is consistent because every committed serializable
read-write transaction is equivalent to an atomic execution at its commit version, so the snapshot at S is the state
after exactly the commits up to S; what prevents the Fekete read-only anomaly is that the overlapping read-write
transaction which would complete the cycle fails its own validation. Aborted transactions can have read only snapshot-consistent state, so they never observe
an inconsistent state. Write versions are compared, never transaction ids, and a write at a version equal to the
snapshot is part of the snapshot, not a conflict (`equal_version_is_not_a_conflict`).

The argument depends on three properties that tests pin: every read is recorded (the two primitives, plus
primary-key-routed reads; tests below), every commit is recorded (the commit and `recover()` replay paths), and
the index fails closed when it cannot be complete (next section). It holds only among serializable transactions (see
the mixed-mode caveat).

### Fail-closed floors

Validation never silently skips an incomplete window:

- **Global floor.** If a participant cannot report its written keys (`written_keys` returns `Ok(None)`, the trait
  default, or an `Err`, for example a key that fails to decode), the global floor is raised to that commit version.
  Every serializable transaction whose snapshot is below the floor aborts with "validation window exceeded". The
  version-log budget (10,000 retained entries) is the other trigger: it counts overwritten historical entries too, so
  a hot key written repeatedly under a long-lived pin trips a global-floor clear every 10,000 writes, aborting every
  older ticket (by design, fail-closed).
- **Partition floor.** When more than 10,000 distinct point keys are retained, the oldest are evicted and the
  partition floor is raised to the evicted version; a transaction whose footprint touches that partition with an
  older snapshot aborts the same way.
- **Pin before recovery.** `pin_serializable` returns `HtapError::Conflict` ("requires successful recovery first")
  if recovery has not completed, mirroring `checkpoint()`. `Conflict` is normally retryable; the server never pins
  before recovery, so this cannot be reached from SQL.
- **Ticket identity.** `commit` rejects a serializable `Transaction` whose ticket belongs to another manager's
  registry before any participant is prepared (`forged_ticket_from_other_registry_is_rejected_before_prepare`).

### Mixed-mode caveat

The guarantee holds only among SERIALIZABLE transactions. An SI writer is always recorded, so a serializable
transaction validates against it and aborts if it wrote something the serializable transaction read. But an SI
transaction itself is never validated and can still be part of a cycle: write skew between two SI transactions, or
between a serializable and an SI transaction when the serializable one commits first (it validates clean) and the SI
one then commits its disjoint write without checking its reads. There is no server-wide or forced isolation level in this phase. `LocalServer::execute` (no session) stays
snapshot isolation. Catalog and account state (DDL, privileges) are outside the serializable domain; a concurrent
`DROP`/`ALTER` of a partition the transaction read is caught by the partition-set comparison and fails the commit
with the "catalog changed" prefix. Evidence: `mixed_mode_si_partner_can_still_skew_documented`,
`serializable_aborts_against_si_committed_writer`.

### Error surface

A validation failure is `HtapError::Conflict` (MySQL error 1213 / SQLSTATE 40001) with a stable message prefix,
so the IPC error enum, the wire error map and the TPC-C retry loop need no change and callers can retry the whole
transaction:

- `serialization failure (read-write dependency): ...`: a newer committed write overlaps the footprint;
- `serialization failure (validation window exceeded): ...`: a global or partition floor is above the snapshot;
- `serialization failure (catalog changed): ...`: the table set or partition topology a transaction read changed or
  the table was dropped (raised by `Session::commit_locked`, before the manager is reached).

A new `HtapError::SerializationFailure` variant was rejected (exhaustive IPC enum at `htap-server/src/ipc/protocol.rs`,
the wire `error_map`, and a TPC-C retry keyed on `Conflict`); the `architect` fallback answer agreed. Distinct
counters for dependency and floor aborts are exposed as `SerializableStats { validation_aborts, floor_aborts }`.

### SQL selection semantics and the vendored-parser change

MySQL 8.0 manual, Table 15.10 (transaction-isolation selection forms; verified against
`dev.mysql.com/doc/refman/8.0/en/set-transaction.html`) is the reference. Implemented behaviour
(`crates/htap-sql/src/variables.rs::parse_isolation_level`, `crates/htap-server/src/session.rs`):

- accepted levels are `REPEATABLE READ` (snapshot isolation) and `SERIALIZABLE`, case- and separator-insensitive;
  `READ COMMITTED`, `READ UNCOMMITTED` and `SNAPSHOT` stay `HtapError::Unsupported`, never silently up- or
  downgraded;
- `SET transaction_isolation = ...`, `SET SESSION transaction_isolation = ...`, `tx_isolation`, and
  `SET [SESSION | LOCAL] TRANSACTION ISOLATION LEVEL ...` are persistent for the session;
- unscoped `SET @@transaction_isolation = ...` and `SET TRANSACTION ISOLATION LEVEL ...` apply to the next
  transaction only, as a one-shot override. Every autocommit statement, including a `SELECT`, consumes a pending
  one-shot (as in MySQL), as does an explicit `START TRANSACTION`; applied while a transaction is open it affects
  only the next one;
- `START TRANSACTION ISOLATION LEVEL ...` sets the level of that transaction; `SELECT @@transaction_isolation`
  reports the open transaction's level, else the pending one-shot, else the session default;
- `SET GLOBAL ...` is rejected (`global_isolation_is_rejected`, `set_global_transaction_isolation_is_rejected`).

The vendored `sqlparser` dropped the `SESSION` keyword for MySQL `SET SESSION TRANSACTION ...`: the AST's
`SetTransaction.session` was `false` except for the Postgres `SET SESSION CHARACTERISTICS AS` form. Without a fix,
`SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE` would silently apply to the next transaction only, a fail-open
snapshot-isolation outcome. The change in `vendor/sqlparser/src/parser/mod.rs` (called out in commit `dfd0240`) sets
`session = true` for `SET LOCAL TRANSACTION` and `SET SESSION TRANSACTION`, and rejects `SET GLOBAL TRANSACTION` with
a parse error. `SET ... TRANSACTION READ ONLY | READ WRITE` still applies to the next transaction only; only the
isolation level honours the persistent form.

### Review history

The plan was reviewed by the `reasoner` model, `cx/gpt-5.6-sol` and `cx/gpt-5.5` as a panel, and by the `architect`
alias, which answered through its `cc/claude-sonnet-5` fallback, not Opus. The implementation batches were reviewed as
follows:

- **Batch 1 (`htap-txn`, commit `48b19a0`):** one `storage-reviewer` round with 2 blocking findings, a forged ticket
  and a pin before recovery, both fixed (`forged_ticket_from_other_registry_is_rejected_before_prepare`,
  `pin_before_recover_is_rejected_after_checkpoint_reopen`); then a focused external re-review of the fix delta
  (`cx/gpt-5.6-sol-review`) found no fail-open path.
- **Batch 2 (server, session, SQL; commit `dfd0240`):** two `storage-reviewer` rounds. Round 1 found 4 blocking
  fail-open findings, all fixed: point reads not recording table partition sets, re-reads overwriting the recorded
  set, the scope of `SET GLOBAL`/`LOCAL TRANSACTION` (the vendored-parser change above), and a one-shot isolation
  level ignored by autocommit statements. Round 2 found no blocking findings. A whole-diff external review then found
  no stale-read path.
- **Batches 3 (tests only, `d937bb0`) and 4 (TPC-C driver option, `ff6d026`):** no storage review, as not applicable;
  they had external review and the validator gate.

The MySQL reference in the previous section was checked against
`dev.mysql.com/doc/refman/8.0/en/set-transaction.html`: `SET @@var_name = value` applies to the next transaction only
and `SET var_name = value` is session-scoped. Review input adopted in the design: validate in the commit path under
`decision_lock` before the `Intent`; `recover()` replay must feed the index; a participant that cannot report keys
raises the global floor; record SI writers; keep `HtapError::Conflict` for the error surface. One implementation
defect found during Phase 19 Task 4: the first `RecoveryRequired` handling destroyed the read footprint (a retried `COMMIT`
would have validated an empty footprint, a fail-open); it was fixed to clone the footprint and is pinned by
`recovery_required_keeps_read_footprint` (unreachable from SQL today, because the server only recovers on reopen).

### Consequences

- SERIALIZABLE is `implemented (local MVP)`: opt-in, a narrow local slice, with the limits in `docs/LIMITATIONS.md`.
- False aborts. Every scan records its whole partition, so any committed write into a scanned partition after the
  snapshot aborts the reader if it writes anything; for example two TPC-C Delivery transactions on different districts
  scan the same `new_order` partition. Primary-key-prefix range narrowing is follow-up Task 7 and is not built (no
  test exists for it). A one-directional rw edge aborts the reader-writer
  (`one_directional_rw_edge_aborts_reader_writer_documented_conservative`). Serializable read-write transactions can
  starve; retry with escalation exists only in the TPC-C driver.
- A leaked ticket (a session that opens a transaction and goes idle) holds the retention floor down; there is no
  idle-transaction timeout.
- Follow-up F9 (latency only): eviction at the 10,000-entry bound does an O(n) minimum-version scan per evicted key
  under `decision_lock`; a version-ordered eviction index would fix it.
- `pin_serializable` waits for at most one in-flight commit (including an import) because it takes `decision_lock`.
- Autocommit catalog-check skip: the statement-local catalog check relies on partition-topology changes holding
  `execution_lock`.
- No on-disk format, WAL or journal change, so no format-version bump or recovery-format test was needed; recovery
  behaviour is covered by the tests below.

### How to reverse it

SSV is replaceable by full SSI without changing the SQL surface or the error surface: the footprint and
`RecentWrites` substrate is a subset of what SSI needs. The refinement path is to add SIREAD state for active and
recently committed transactions, mark rw edges in both directions, abort only a pivot, and add safe-snapshot
handling for read-only transactions, keeping validation inside `TransactionManager::commit`. Independently,
precision can be raised by recording primary-key-prefix ranges for provable equality predicates (the footprint
already supports ranges). Removing SERIALIZABLE entirely means restoring `parse_isolation_level` to the single
`REPEATABLE READ` request; no data migration is involved because nothing durable depends on it.

### Verification

`crates/htap-txn/src/serializable.rs::tests::{footprint_promotes_to_partition_at_cap,
recent_writes_point_range_partition_hits, prune_respects_oldest_ticket,
overflow_raises_partition_floor_and_aborts_old_snapshots, equal_version_is_not_a_conflict,
global_floor_aborts_every_older_snapshot, version_log_retention_is_bounded_and_fails_closed,
range_edges_merge_and_ticket_drop}`; `crates/htap-txn/src/manager.rs::tests::forged_ticket_from_other_registry_is_rejected_before_prepare`;
`crates/htap-txn/tests/serializable_validation.rs` (13 tests, listed in `docs/PROGRESS.md`);
`crates/htap-server/tests/{serializable,serializable_session,serializable_history,session_recovery}.rs`;
`crates/htap-tpcc/tests/serializable.rs`; `htap_sql::variables::tests::parse_isolation_level_accepts_serializable_and_repeatable_read_only`.
The full requirement-to-test map is the Phase 19 row of `docs/PROGRESS.md`; the design is described in
"Serializable isolation (Phase 19)" in `docs/ARCHITECTURE.md`; design references are in `docs/RESEARCH.md`.


---

## ADR-034: Power-loss crash consistency under a modelled filesystem (strict POSIX): recording shim and crash-image harness

`Status: Accepted`
`Date: 2026-10-04`

**What this ADR claims, and what it does not.** This is **crash consistency under a stated model, not a power-loss
proof.** Every durable mutation in the engine's local crates now goes through one recording shim, and CI enumerates
simulated crash images of the recorded operation log under a stated strict-POSIX filesystem contract. A green suite means
"no image the harness enumerated violated the oracles" (write and truncate boundaries are sampled, at most eight per log,
unless exhaustive mode is on; Torn uses a fixed number of seeds; equal images are deduplicated); it does not mean the engine survives a real power cut on real
hardware. Device caches, lying drives, `fsync` failure semantics, and the other items in "Explicit non-coverage" below are
outside the model. Real-device and kernel-level proof stays `deferred`.

### Context

Until Phase 20 durability was verified by code inspection and by `kill -9` crash tests (`htap-rowstore/tests/wal_crash.rs`,
`engine_crash.rs`, `htap-coord/tests/local_coordinator.rs`, `htap-server/tests/ipc_multiprocess.rs`). A killed process
leaves the page cache intact, so those tests cannot observe a missing `fsync` or a missing directory sync; the mutation
table in `docs/LIMITATIONS.md` shows the old suites still pass with every `sync_all` stubbed out. The Stage R
note ("shared durability primitives module", above) had collapsed the file-durability helpers onto one implementation per
primitive so that this audit would have one thing to audit.

Phase 20 replaces "no power-loss proof" with CI-gated evidence under an explicit filesystem model. The researcher inventory of
the durable write paths that preceded the harness expected, and the harness then confirmed, that the open paths of nearly every component created directories
without syncing their parents, so a crash could drop a whole component directory together with acknowledged data.
Those findings are listed under "Bugs found" and are why the harness was worth building.

### Options considered

- **(a1) A full virtual filesystem under the engine.** Rejected: every durable crate would need an injected VFS handle,
  an invasive change with its own fidelity questions and no gain over recording the real calls.
- **(a2) A recording shim in `htap_common::fs` plus an offline crash-image materializer.** Chosen. The shim runs the real
  syscall and then appends to a per-test operation log; images are computed from log prefixes after the fact.
- **(b) `dm-flakey` device-mapper fault injection.** Rejected as a gate: needs root and a block device, is not
  enumerable from one recorded workload, and is not runnable from `cargo test` on a developer laptop.
- **(c) A VM that is power-cycled.** Rejected: no hypervisor in CI, not enumerable, slow.
- **(d) A process-global "crash mode" switch.** Rejected: parallel `cargo test` binaries and threads share the process, so a
  global switch would make recorded workloads interfere. The shim is instead keyed by a canonical root prefix, so tests
  run in parallel.
- **(e) LazyFS (a FUSE filesystem that drops unsynced data) as the gating mechanism.** Rejected as a gate: needs
  `cmake` and `libfuse3-dev` (unavailable in this environment), and it is an outside tool whose model we would still have
  to state and trust.
  Kept as an optional non-gating cross-check; see "How to reverse it" and `docs/LIMITATIONS.md` (E4, `deferred`).
- **Relaxed (ext4-like) contract as the gating model.** Rejected: strict-to-relaxed is cheap to loosen later, the reverse
  is expensive (a suite proven only under a relaxed model does not demonstrate the sync was needed). Chaos keeps a
  non-gating relaxed probe.

### Decision

#### Crash contract (strict POSIX)

The model each oracle is checked against:

- `fsync(file)` persists that inode's data and size only. It does **not** persist the file's directory entry.
- `fsync(dir)` persists that directory's entry map (creates, renames, unlinks inside it) only. It does **not** persist the
  directory's own entry in its parent. Creating `a/b/c` therefore needs a parent sync **per level**.
- `rename` is atomic. Production code renames only within one directory; the recorder fails closed on a cross-directory
  rename or a directory rename rather than guessing their persistence rules.
- Writes and file extensions not covered by a later `fsync` of that inode before the crash point may be lost or torn;
  directory operations not covered by a later `fsync` of every affected directory may be lost.
- Sector atomicity is configurable (`CrashPolicy::Torn { sector_size, .. }`; suites use 16, 512, and 4096).

Image classes, per crash point:

- **Strict**: only forced persists survive. Required.
- **Torn(seed, sector)**: metadata as Strict; for each file, starting from the last synced snapshot, a seeded subset of its
  dirty sectors survives and an extended region that never fully persisted is zero-filled. Required. Torn mixes only the
  strict bytes and the final observed bytes of a sector: it has no intermediate versions of a sector (review note R3-N4).
- **Chaos(seed)**: each unforced logged operation is kept or dropped independently, in log order: a namespace operation with
  probability 3/4, and a whole logged `Write` or `SetLen` operation with probability 1/2 (`materialize.rs`). Chaos does not
  mix per sector; only Torn does. This is a **fixed-seed triage probe**, not a required class. One ordinary test uses it in the normal run
  (`tablet_package_rename_order_chaos`, fixed seeds); `chaos_*` model tests pin its semantics.
- **Depth-1 recovery crash**: for suites that exercise recovery writes (WAL tail repair, journal `repair_torn_final`,
  engine orphan removal), the harness also crashes **during** recovery at every boundary of the recovery log. That second
  crash is **always Strict**, even under an outer Torn image. Deeper recursion is not explored.
- **Strict-only suites (follow-up F21).** Six suites enumerate `Strict` images only: `powerloss_wal_open`,
  `powerloss_wal_segment`, `powerloss_wal_tail`, `powerloss_engine_open`, `powerloss_movement_clone` (`Strict` plus the
  `Chaos` probe) and `powerloss_movement_repair`. Torn-sector behavior on those paths is unexercised. The other suites
  enumerate Torn in at least one test (`powerloss_txn` only in its depth-1 recovery test).

Crash points: each file or directory sync contributes both the pre-sync and the post-sync boundary; namespace operations
(create, rename, unlink, mkdir, rmdir) and acknowledgements contribute the post-operation boundary; write boundaries are
sampled, up to eight (`POWERLOSS_EXHAUSTIVE=1` takes every index). Ordinary enumeration deduplicates images by
`(tree hash, acknowledged label set)`; recovery-stage images are keyed by `(tree hash, outer acknowledged labels)`. Dedupe is
turned off whenever exhaustive mode is on: `POWERLOSS_EXHAUSTIVE=1` by itself, or any active skip-sync mutation (witnesses and
controls), which forces exhaustive mode.

#### Harness design

- **Shim (`htap-common/src/fs.rs`, `fs/dur.rs`, `fs/recorder.rs`).** `DurFile` and `DurOpenOptions` wrap `std::fs::File` and
  `OpenOptions`; free functions wrap `rename`, `remove_file`, `create_dir`, `create_dir_all`, `remove_dir`,
  `remove_dir_all` (decomposed into per-entry operations while recording), `sync_dir`, and path-based `fsync`. The
  existing helpers (`sync_dir`, `write_new_tmp_file`, `remove_file_if_exists`, `fsync_file`, `atomic_publish`) are built on
  the shim. The `crashsim` cargo feature on `htap-common` is enabled only through a dev-dependency of the new
  dev-only crate `htap-crashsim` (`publish = false`); with the feature off the shim is an `#[inline]` pass-through, and
  `htapd` refuses to start if `htap_common::fs::CRASHSIM_ENABLED` is true.
- **Recorder.** A registry keyed by canonical root prefix lets parallel tests record independently. The real
  syscall runs first, before the recorder lock is taken; only the model update and the append of the successful operation
  run under the recorder lock: `Mkdir`, `Create`, `Write{id, offset, bytes}`,
  `SetLen`, `FsyncFile`, `FsyncDir`, `Rename`, `Unlink`, `Rmdir`, `Ack(label)`. Files have inode-like ids (rename-over
  tracks the replaced id), and `O_APPEND` offsets come from the model length. The write is logged below any `BufWriter`
  at the real flush. Self-checks: at each `FsyncFile` the reconstructed content must equal the real file, and at workload
  end the shadow tree must equal the real tree; raw `std::fs` mutation under a registered root fails the self-check.
- **Fail-closed rules.** The recorder panics (the test fails) on `..` paths that escape the root, symlinks that escape,
  cross-directory and directory renames, files or names the model has no record of (a raw `std::fs` mutation), and
  recording from more than one thread. Workloads are tiny,
  seeded, and single-threaded (`with_scan_workers(1)`, `with_query_parallelism(1)` in server tests).
- **Ephemeral data roots.** The registered root is itself an ephemeral data root by default (`State::from_disk` registers
  the empty relative path), so `LOCK`, `spill/`, `htap.sock`, and `.htap-ipc-*` entries directly beneath it are ignored. A
  server root nested under the harness root additionally has its own such entries ignored through
  `CrashHarness::with_data_root` (called before `run_workload`; later registration fails closed). These are ephemeral
  runtime state outside the crash model. The other exceptions to "all durable mutation goes through the shim" are the
  `htap-server` spill scratch and IPC socket code, which carry item-scoped lint allows.
- **Materializer (`htap-crashsim`).** Computes a crash image for `(log prefix, policy)` into a temp directory, which the
  suite then opens with plain `std` and the production API.
- **Ack labels.** A workload acknowledges a step only after the API returned `Ok`; `DurablePending` and
  `RecoveryRequired` never acknowledge (CLAUDE.md invariant). An image at crash point `k` must contain every operation
  acknowledged before `k`.
- **Oracles (per suite).** (1) the production API reopens every enumerated Strict and Torn image (Torn only in the suites that enumerate it, F21) with no `Corruption`; (2) every
  acknowledged operation is present with its value and acknowledged deletes are not resurrected; (3) the recovered state is
  some prefix of the serial history with `acked <= len <= issued`; (4) a second reopen is idempotent and the two reopens
  agree; (5) crash-during-recovery at depth 1; (6) a published (non-`.tmp`) envelope never has an invalid CRC.
- **Replay.** A failure prints `POWERLOSS_REPRO=<harness>/<policy>/<seed>/k=<n>` (recovery images add `/rk=<n>`).
  Setting the environment variable replays exactly that image: the repro name must equal the harness name (equal to the
  test name by convention). Success is the reproduced oracle panic; `POWERLOSS_REPLAY_NOT_REPRODUCED` means the image now
  passes (fixed, or workload drift). Other knobs: `POWERLOSS_SEEDS`, `POWERLOSS_EXHAUSTIVE`, `POWERLOSS_SKIP_SYNC`.
- **Static completeness gate.** `ci.sh` runs `CLIPPY_CONF_DIR=ci/clippy-durability cargo clippy --workspace --exclude
  htap-crashsim --lib --bins -- -A clippy::all -D clippy::disallowed_methods` over the whole workspace, so raw `std::fs`
  mutation, `File::sync_all`/`sync_data`/`set_len`/`try_clone`, `OpenOptions::open`, `FileExt::write_at`,
  `set_permissions`, and `symlink` are rejected outside the shim. A second `ci.sh` step requires that the set of source
  files mentioning `disallowed_methods` equals exactly `htap-common/src/fs/dur.rs`, `htap-rowstore/src/sst.rs` (two
  read-only `try_clone` calls), `htap-server/src/spill.rs`, and `htap-server/src/ipc/owner.rs`, and rejects lint-group
  allows (`clippy::all`, `clippy::style`, `warnings`) and a `[lints]` table in any `Cargo.toml`. The server spill and IPC allows are item-scoped with a "ephemeral, not durable"
  comment.

#### Durability conventions

These are the contracts the fixes below establish and the oracles enforce:

- **Open-time REQUIRED syncs (immediate parent only).** The rowstore engine, WAL, journal, catalog, coordinator,
  movement root, and `LocalServer` create their directory with `create_dir_all_durable` (parent sync for every level the
  call creates) and then, **unconditionally and with errors propagated**, sync the directory itself and its **immediate
  parent**. "Unconditionally" matters: a directory created by an earlier process that was killed before its parent sync is
  not trusted to be durable. For the engine, WAL, journal, catalog, and movement root the parent is taken from the path
  text (`parent_or_current_dir`; empty maps to `.`), so a symlinked component or `..` is not resolved.
  `LocalCoordinator::open` and `LocalServer::open` canonicalize the root first and sync the canonical parent.
- **D1: the `LocalServer` root and export destinations split strict from best-effort.** The root's immediate parent (and
  an export destination's parent) is synced strictly. Ancestors above it are synced by
  `sync_ancestors_best_effort`, a lexical walk to the filesystem root (or `.` for a relative path) that tolerates only
  `PermissionDenied`, `InvalidInput`, and `Unsupported` and propagates every other error (`EIO`, `ENOENT`, `ESTALE`,
  `EROFS` on a far ancestor therefore fails the open or export). The contract is "the nearest durable ancestor is the
  anchor": a multi-level fresh root is durable once its levels are created durably and its parent is synced, and
  ancestors that were already volatile before this call are repaired best-effort, not as a strict guarantee. The panel
  that reviewed this was split (an explicit durable-anchor contract versus a best-effort walk); the strict/best-effort
  split was chosen because it makes the fixed cases green without claiming more than the filesystem lets us check.
  The best-effort claim is not part of the crash-image proof (see "Explicit non-coverage").
- **One-level fresh root was already safe.** A fresh server root exactly one level below a durable directory was already
  durable before BUG-PL-5's fix: `Journal::open` syncs the journal's grandparent, which is that root's parent. Only roots two
  or more levels below a durable directory, or volatile ancestors above the immediate parent, were lost. The tests
  therefore use roots of depth 1 (green control) and 3 (the red case); see the BUG-PL-5 entry.
- **Per-operation chain syncs.** A tablet package publish syncs the package directory between the `DATA` and `MANIFEST`
  renames and then the package directory chain (`sync_package_dir_chain`: package dir up through the movement root and its
  parent) before the job is marked `Complete`; an export syncs the destination parent strictly and its ancestors
  best-effort before completion; conversion syncs the colstore root and its parent before the first publish and the tablet
  and generation directories after each segment publish.
- **Every path syncs an adopted WAL active segment before publish.** `Wal::open` syncs the adopted final segment on both
  branches: `wal:open_adopt_sync` when the tail is not torn, and, when a torn tail is truncated, `wal:repair_sync` after
  `set_len`, which is then the barrier for the adopted unsynced valid prefix (the two sites are alternatives, not both run).
  Before BUG-PL-3 only the torn-tail branch synced, so a frame written by a killed process could be replayed, published,
  and then lost.
- **Instrumentation commits never fix bugs; no hot-path syncs.** All added syncs run at open, per conversion, per clone,
  per export, or per job persist, never per commit or per row. No on-disk format changed, so no format-version bump was
  needed.
- **Unix only.** The shim's directory sync is a no-op off Unix (the old `wal.rs::fsync_dir`, which ran on every platform
  and likely failed on Windows, no longer exists). Durability is therefore a Unix-only claim.

#### Bug policy

Fix in-phase only local missing-sync, directory-sync, or ordering defects that need no format change, no framing change,
no acknowledgement-order change, and no hot-path `fsync`. Anything else (the oracle or contract is indicted, two
subsystems are involved, a format or ordering change is needed) stops the batch and is reported. More than five findings
in one subsystem would have triggered a design ADR; that breaker was not tripped. Each batch was intended to land as a
red/green pair: a behavior-preserving migration commit (instrumentation, no fixes), then a commit with the failing suite and
the fix. Red evidence (a failing repro captured on the unfixed code) is recorded in the commit messages for BUG-PL-1, 3, 5, 6
and 7 (BUG-PL-6 is a log-order assertion, not a `POWERLOSS_REPRO` image); BUG-PL-2 had no red test in its commit, and BUG-PL-2
and BUG-PL-4 are guarded by the tests listed in `docs/LIMITATIONS.md`. A pinned `KNOWN_RED` allowlist
that may only shrink was the rule for tests that must stay red across commits; none exists in the tree. BUG-PL-7's two
tests were `#[ignore]`d for exactly one commit (`033c503`) and re-enabled by the fix (`6315e2a`).

#### Mutation-control design

A passing suite proves nothing if the model cannot fail it. The controls:

- **`SkipSync` scopes.** The recorder can omit the *log entry* of a sync (the real syscall still runs) for `File`,
  `Directory`, `All`, or one `Site("<id>")`. A scope is applied with `with_skip_sync`, is thread-local, nests, restores on
  panic, is re-applied to the recorders of recovery stages, and counts how many syncs it suppressed (`scope_skip_hits`). The
  `POWERLOSS_SKIP_SYNC=file|dir|all|site:<id>` environment variable applies a skip to a whole run; any active skip forces
  exhaustive, undeduplicated enumeration.
- **Witnesses, controls, survivors.** Every production sync site carries a string id. `crashsim_witness!(fn, site = "<id>",
  body = <suite test>)` re-runs a suite with that one site skipped and requires a recorded oracle
  failure (`assert_skip_kills`: the harness stores a typed failure record and the panic must be exactly the
  `POWERLOSS_REPRO=...` re-panic), rejecting an unrelated panic, a surviving run, and a zero-hit run (`site never
  exercised`).
  `crashsim_control!` does the same for `File`, `Directory`, or `All`, and every `powerloss_*` suite must contain
  controls (or a reasoned exemption). `crashsim_survivor!` proves the opposite for a site that is redundant by design (the
  suite must still pass, and the site must be exercised).
- **Static site checker.** `htap-crashsim/tests/mutation_controls.rs` parses the production sources with `syn`, extracts
  every site id passed to a shim sync, and requires that each id has a witness in `SYNC_SITE_WITNESSES` or an `ALLOWLIST`
  entry with a **reason from a closed enum**: `SubsumedByLaterSync`, `CoveredByOtherSync`, `IdempotentResurrection`,
  `ErrorPathOnly`, `NonUnixOnly`, `NoProductionCaller`, `OracleGap`. An allowlist entry must carry survivor proof (a
  `crashsim_survivor!` test or a documented data-only argument), `CoveredByOtherSync` must name a witnessed site, and a
  scoped entry must match a real call. Site ids must be unique, no production sync may be untagged, every shared helper id
  (`sync_dir:sync`, `write_new_tmp_file:sync`, `fsync_file:sync`, `atomic_publish:dir_sync`,
  `create_dir_all_durable:parent_sync`) needs a witness or a scoped, reasoned allowlist entry in each calling crate
  (`create_dir_all_durable:parent_sync` and `fsync_file:sync` have no production witness), and `UNATTRIBUTED_WITNESSES`
  lists witnesses that a test fixture, not production code, kills. A further test,
  `every_sync_site_is_listed_in_the_architecture_catalog`, parses only the "Durability-point catalog" table of
  `docs/ARCHITECTURE.md` and fails on a missing, stale, or duplicate id. At the time of this ADR the scanner sees 37
  production site ids: 24 with a production witness and 13 relying only on a reasoned allowlist entry (some witnessed shared helper ids also carry crate-scoped allowlist rows; `wal:repair_sync` is witnessed
  by `witness_wal_repair_sync`). `OracleGap` marks sites no workload observes (catalog, coordinator, and movement `open_dir_sync`, follow-up F15).
  The scanner has documented blind spots (UFCS, aliases, glob imports, macro-wrapped calls); raw `std`/`libc` syncs are
  caught by the clippy gate instead.
- **Witnesses prove existence of a failing image, not breadth.** A witness shows that skipping the site yields at least one
  image that fails an oracle. It does not show that every dependent path is covered. Shared-helper ids are killed
  per (id, workload), not per call site: skipping `sync_dir:sync` suppresses every caller in that workload at once.
- **`SyncFault` is propagation-only.** `SyncFault::Nth(n)` / `SyncFault::Site{site, occurrence}` makes the 1-based nth sync
  attempt in the workload region return an error before the syscall. The `sync_fault_*` sweeps fail each sync ordinal in
  turn and assert that **no injected failure is silently dropped** (the error reaches the caller, or for server `DROP`
  reclaim is logged with the pending entry left retryable); they do not
  materialize crash images and do not model what the filesystem does after a failed `fsync` (`EIO` semantics are out of
  scope). A sweep need not make every sync fail for every API.
- **F12: ambiguous outcome after the rename.** When the directory sync that follows `atomic_publish`'s rename fails, the API
  returns `Err` although the new file is already visible in the running process. The sweeps pin the exact set of
  tolerated trailing ordinals for each API (clone, repair, export, job persist, artifact delete, server `DROP` reclaim) as
  `AMBIGUOUS_AFTER_RENAME`-style tables, and tie each rename row to its sync with a site-targeted companion run. Callers
  must treat such an `Err` as "outcome unknown". The catalog CAS and the coordinator can diverge from disk. This is
  recorded in `docs/LIMITATIONS.md` ("Ambiguous outcome after a post-rename directory-sync failure (F12)"), together with
  the retry fast-path gap F20.
- **Real-source mutations (E3).** Manual mutations of the production sync code (stubbed `DurFile` sync, stubbed
  `sync_dir`, a dropped roll or SST-rename directory sync), run in a scratch git worktree with its own `CARGO_TARGET_DIR`
  against the old kill-9 suites and the new power-loss suites, are a separate evidence step recorded in
  `docs/PROGRESS.md`. This ADR's claims rest on the witnesses and controls above, not on that table.

#### Explicit non-coverage

The model does **not** cover: device write caches and FUA/flush semantics; drives that lie about flush; `fsync` `EIO` and
post-error state (the kernel may drop dirty pages after a failed sync; F10 records the WAL gap); `ENOSPC`; sub-sector
tearing and bit rot; `mmap` and `O_DIRECT` (the engine uses neither); thread interleavings and concurrent crashes (one
thread is recorded); non-Unix platforms; and any real hardware or real kernel. It also does not prove: that Torn images
contain intermediate sector versions (they do not); that the second crash in recovery can be Torn (always Strict); that the
server SQL workload produces Torn-distinct crash trees (measured `POWERLOSS_TORN_DISTINCT=0`, follow-up F13); that the
open-time syncs cover ancestors beyond the immediate parent (best-effort only, and ancestors outside the harness root are
not recorded); that a missing required conversion manifest is classified as `Corruption` (it is `Io`, F14); that
`movement:package_data_dir_sync` has a crash-image witness (no shipped Strict, Torn, or Chaos test kills its skip, including
the `tablet_package_rename_order_chaos` probe; only the op-log order test `clone_publish_has_data_dir_barrier` guards it, F16);
that Torn images are enumerated for the six Strict-only suites (F21); or that every `OracleGap` site is observed.
Open-time `EACCES`/`EINVAL` on an unreadable or unsyncable parent now fails closed, and a checkpoint-reopen `fsync` failure
becomes `recovery-required`. The full list with follow-up ids F10 to F21 is in `docs/LIMITATIONS.md`.

### Bugs found

The harness found seven defects (red evidence is recorded in the commit messages for BUG-PL-1, 3, 5, 6 and 7; BUG-PL-2 and
BUG-PL-4 are guarded by tests; full list with commit and evidence in `docs/PROGRESS.md` and `docs/LIMITATIONS.md`):

- **BUG-PL-1** (`01ca864`): `Engine::open`/`Wal::open` created directories without syncing parents and trusted a directory
  left by an earlier killed process; the whole engine, with acknowledged commits, could vanish.
- **BUG-PL-2** (`01ca864`): `Wal::open` adopted a segment whose directory entry was never synced.
- **BUG-PL-3** (`01ca864`): the adopted active WAL segment was `fsync`ed only after a torn-tail repair, so an unsynced
  commit frame could be replayed and published and then lost, leaving visible newer than recovered (reopen `Corruption`).
- **BUG-PL-4** (`4321287`): a zero payload-length journal header followed by more than one header of bytes was classified
  as middle corruption without probing for a later valid frame, so a zero-filled tail after power loss made the database
  unopenable.
- **BUG-PL-5** (`033c503`, high): a fresh or volatile `LocalServer` root two or more levels below a durable directory was
  lost after an acknowledged open (neither the root nor its ancestors were synced). One-level roots were not affected.
- **BUG-PL-6** (`033c503`, low, log order only): the `colstore/` directory entry was not followed by a root directory sync
  before `open` returned.
- **BUG-PL-7** (`6315e2a`, availability only, no data lost): a crash after the `SegmentsWritten` or `ReadyToPublish`
  catalog step left a manifest-less `Converting` partition whose reads failed until `tick()`; the read path now serves it
  from the rowstore in every phase (the rowstore stays authoritative until publish).

Fixes without a numbered id (journal file entry, state directories, tablet clone chain, `DATA`/`MANIFEST` barrier,
swallowed syncs) are in the commit messages of `4321287` and `02a238d`.

### Consequences

- Phase 20 is `implemented (local MVP)`: simulated power loss under the model above across the rowstore, journal, catalog,
  coordinator, column store, conversion, movement, and `LocalServer` open/conversion/reclaim paths. Real-device, kernel,
  and fault-injection-below-the-filesystem proof is `deferred`.
- New dev-only crate `htap-crashsim`; the workspace has 16 `htap-*` crates plus `htapd` (17 crates, plus `vendor/sqlparser`). It is never linked into production.
- Every new durable file or directory mutation must go through `htap_common::fs` (clippy gate), and every new sync site
  needs a string id and a killing witness or a reasoned allowlist entry (static checker), or CI fails.
- Added test wall time (the server suites dominate; `server_recovery_crash_depth1` is slow in exhaustive mode,
  follow-up F18). Measured runtimes are recorded in `docs/PROGRESS.md`.
- Open-time latency increases by a handful of directory syncs per component open (follow-up F17 records a redundant double
  `fsync` in the journal append-and-sync path; performance only).
- A site's witness proves one failing image exists; coverage per shared helper is per (id, workload).
- No on-disk format, WAL, or journal framing changed; no ack ordering changed.

### How to reverse it

Remove the `crashsim` feature wiring and the `htap-crashsim` crate; the shim is a pass-through with the feature off, so no
production behaviour depends on it. The fixes are plain extra directory and file syncs and can stay. Loosening the model
is a configuration change: gate on Torn only, or demote Strict to a triage run, without touching the shim. To add an
external cross-check, run the existing kill-9 workloads under LazyFS (`deferred` until `cmake` and `libfuse3-dev` are
available), keeping it non-gating; the reopen condition is a CI image that has both.

### Verification

Model and harness semantics: `crates/htap-crashsim/tests/model_semantics.rs`
(`strict_loses_unsynced_write`, `file_fsync_without_dir_fsync_loses_entry`, `dir_fsync_publishes_entry`,
`dir_fsync_does_not_publish_own_entry`, `nested_mkdir_needs_parent_fsync_per_level`,
`rename_without_dir_fsync_keeps_old_name`, `unlink_without_dir_fsync_resurrects`,
`torn_keeps_sector_aligned_subset`, `torn_extension_zero_filled`, `chaos_reaches_rename_before_data_sync`,
`images_deterministic_for_same_seed`, `repro_string_roundtrips`), `dir_generations.rs`, `recovery_baseline.rs`,
`recovery_panic.rs`, `recovery_repro.rs`, `torn_extension.rs`, `chaos_parent_mkdir.rs`, `chaos_type_conflicts.rs`,
`repro_env.rs` (`replay_env_replays_exactly_one_image`, `replay_env_not_reproduced_panics_with_distinct_prefix`),
`ephemeral_roots.rs`, `skip_controls.rs` (`assert_skip_kills_accepts_a_real_kill`, `assert_skip_kills_rejects_survivor`,
`assert_skip_kills_rejects_zero_hits`, `recovery_recorders_inherit_skip`, `witness_scope_forces_exhaustive_no_dedupe`),
`skip_env.rs`, `sync_fault.rs`, and `review_controls.rs`. Ancestor helper units in `htap-common/src/fs.rs`:
`sync_ancestors_best_effort_propagates_not_found`, `sync_ancestors_best_effort_skips_invalid_input_and_continues`,
`sync_ancestors_best_effort_records_ancestors_inside_root` (the last one needs `--features crashsim`).

Crash-image suites (each reopens Strict images and all but the six Strict-only suites of F21 also reopen Torn images; the full per-suite test list is the R7 row of
`docs/PROGRESS.md`): `htap-rowstore/tests/powerloss_{wal,wal_open,wal_segment,wal_tail,engine,engine_open}.rs`
(`wal_acked_commits_survive_strict`, `wal_fresh_dir_entry_durable`, `wal_open_syncs_adopted_unsynced_segment`,
`adopted_unsynced_wal_tail_is_synced_before_publish`, `torn_tail_repair_syncs_adopted_prefix_before_publish`,
`engine_fresh_open_dirs_durable`),
`htap-txn/tests/powerloss_txn.rs` (`txn_fresh_journal_entry_durable`, `txn_repair_torn_final_crash_depth1`),
`htap-catalog/tests/powerloss_catalog.rs`, `htap-coord/tests/powerloss_coord.rs`,
`htap-colstore/tests/powerloss_segment.rs`, `htap-convert/tests/powerloss_convert.rs`,
`htap-movement/tests/powerloss_movement{,_clone,_export,_import,_repair}.rs`
(`clone_survives_fresh_tablet_dirs`, `clone_publish_has_data_dir_barrier`, `export_to_fresh_nested_destination`), and
`htap-server/tests/powerloss_server.rs` (`server_fresh_root_bootstrap_durable`, `server_preexisting_volatile_root_durable`,
`server_reopen_syncs_volatile_root_children`, `server_open_syncs_colstore_entry_before_ack`),
`powerloss_server_recovery.rs` (`server_conversion_and_reclaim_survive`, `server_recovery_crash_depth1`),
`conversion_crash_recovery.rs` (`test_server_open_serves_manifest_less_segments_written_and_ready_to_publish`).

Mutation controls: each `powerloss_*` suite ends with `witness_*`, `control_*`, and `survivor_*` macro tests (run
`cargo test --workspace --tests -- witness_ survivor_ control_`); the static gate is
`crates/htap-crashsim/tests/mutation_controls.rs`
(`every_observed_sync_site_has_a_killing_witness_or_a_reasoned_allowlist_entry`, `allowlist_entries_are_not_stale`,
`every_shared_helper_id_has_a_witness_in_each_calling_crate`,
`every_powerloss_suite_has_file_and_dir_controls_or_a_reasoned_exemption`, `every_witness_row_names_an_existing_macro_and_test`,
`every_production_site_id_is_used_at_one_location`, `every_sync_site_is_listed_in_the_architecture_catalog`,
`scanner_observes_the_workspace`, `no_production_sync_is_untagged`).

Sync-failure propagation (F12 pinned sets): `cargo test -p htap-movement --test sync_fault_clone --test sync_fault_repair
--test sync_fault_export --test sync_fault_jobs` (`clone_sync_failure_is_never_swallowed`,
`repair_sync_failure_is_never_swallowed`, `export_sync_failure_is_never_swallowed`,
`job_persist_sync_failure_is_never_swallowed`, `delete_artifacts_sync_failure_is_never_swallowed`) and
`cargo test -p htap-server --test sync_fault_reclaim` (`reclaim_sync_failure_is_never_swallowed`).

Static gate: `./ci.sh` (the `CLIPPY_CONF_DIR=ci/clippy-durability` step and the `disallowed_methods` file-set guard).
Replay recipe: `POWERLOSS_REPRO=<value> [POWERLOSS_SKIP_SYNC=<spec>] cargo test -p <crate> --test <file> <test_name>`.
The architecture is in "Power-loss simulation harness (Phase 20)" in `docs/ARCHITECTURE.md`, the requirement map in the R7
row and Phase 20 row of `docs/PROGRESS.md`, the gaps in `docs/LIMITATIONS.md`, and the design references in
`docs/RESEARCH.md`.

---

## ADR-035: Container packaging: tini as PID 1 with no in-process signal handler, a handshake health probe, mounted-file secrets, and pinned (not reproducible) inputs

`Status: Accepted`
`Date: 2026-10-05`

**What this ADR claims, and what it does not.** This is packaging: a Docker image and a Compose file for the existing `htapd`
binary. It changes no storage, WAL, MVCC, envelope or sync-site behavior and adds **no durability claim beyond ADR-034**. A
named volume is a local POSIX filesystem directory; NFS, FUSE, Docker Desktop file sharing, volume plugins and the overlay
layer are outside that model. `docker stop` is a crash-equivalent stop, not a graceful shutdown. The image is built from
pinned inputs and is **not** claimed to be bit-reproducible. Tested only on linux/amd64 (nothing in the Dockerfile or compose
enforces a platform).

### Context

ARCHITECTURE, README and OPERATIONS listed Docker/Compose packaging as deferred or "not provided" since Phase 7/8, and the
user asked for it. `htapd` ends in a park loop (`loop { std::thread::park() }`), has no signal handler, and the repository
forbids `unsafe` (no signal crate is in `Cargo.lock`). The kernel does not apply default signal actions to PID 1 of a PID namespace, so as a
container's PID 1 `htapd` would ignore SIGTERM and `docker stop` would wait out the grace period and then SIGKILL. OPERATIONS
section 6 already documents the process-level contract: no drain, crash-safe by construction (ADR-004/008/009), recovery on
the next start, and `kill -9` recovery is tested. `htapd` reads configuration from flags and environment only, has no
`*_FILE` variants and no health-check flag, and validates `HTAPD_TLS_*` environment variables before flags. The first-start
`root` password is bootstrap-only (the `accounts_initialized` latch). Compose 5.4.0 file-backed secrets keep the host
file's ownership and mode (measured), they do not appear root-owned.

### Options considered

- **(D1a) `tini` as PID 1, no code change in `htapd`.** Chosen. `htapd` becomes `tini`'s child, the default SIGTERM
  disposition applies and it exits 143 at once. Same contract as the documented process-level stop.
- **(D1b) A SIGTERM handler in `htapd` for a graceful exit 0.** Rejected for Phase 21. It needs a new signal-handling
  dependency, a new shutdown path, and a bounded drain (`WireServer::shutdown` joins connection threads with no deadline),
  which makes it a storage-reviewer item, not packaging. Recorded as `deferred`, together with `SIGHUP` TLS reload.
- **(D1c) Docker's `--init` / `init: true` instead of an image `ENTRYPOINT`.** Not chosen: it depends on the operator's
  invocation rather than the image; a baked-in entrypoint works for plain `docker run`.
- **(D2a) A shell handshake probe (`docker/healthcheck.sh`).** Chosen. Reads 5 bytes of the MySQL greeting under a `timeout`
  and requires sequence `00` and protocol version `0a` (so it rejects EOF, truncation, an ERR packet and an arbitrary
  listener). `htapd` sends the greeting immediately on accept, before TLS negotiation, so it also works under
  `--require-secure-transport` and logs nothing above debug for the pre-auth drop. Listener and bootstrap readiness only.
- **(D2b) An `htapd --healthcheck <addr>` flag.** Rejected: grows the daemon CLI for something a probe script does. It stays
  the fallback if the shell probe proves flaky, and would be needed for a distroless image.
- **(D3a) Mounted-file secrets with a `0700` host directory and `0444` files; container uid 10001.** Chosen. Because
  file secrets keep host ownership and mode, uid 10001 must be able to read the file; the directory mode keeps other host
  users out. The `entrypoint.sh` shim reads `HTAPD_PASSWORD_FILE`, exports `HTAPD_PASSWORD`, unsets the `_FILE` variable,
  rejects both being set, rejects an unreadable or empty file, and refuses an empty password unless
  `HTAPD_ALLOW_EMPTY_PASSWORD=1`. It is bootstrap-only by nature of the `htapd` latch. TLS uses environment variables only.
- **(D3b) A root entrypoint that fixes ownership and drops privileges.** Rejected for now: it needs `SETUID`/`SETGID`,
  which conflicts with `cap_drop: ALL`. Running the container as the host uid was also rejected (breaks named-volume
  ownership).
- **(D6) Distroless runtime image.** Rejected: the secret shim and probe need a shell and coreutils; the size win is small
  against the debian-slim base.
- **(D6) `cargo-chef` layer caching.** Rejected: BuildKit cache mounts for the registry and target directory are simpler and
  the binary is copied out in the same `RUN`.
- **(D7a) Pinned inputs: base images as `tag@sha256`, a version-pinned `tini`, `cargo build --locked`, a `rustc -V` guard
  against the `rust-toolchain.toml` channel.** Chosen, with no reproducibility claim: apt state, base-image updates and
  timestamps still vary. A Debian snapshot mirror was not adopted (extra maintenance).
- **(D4) Opt-in verification.** Chosen: `./ci.sh --docker` runs `ci/docker-smoke.sh` and is fail-closed (a missing Docker,
  Compose, BuildKit, daemon or registry is a hard failure, never a skip); default `./ci.sh` stays Docker-free and only gains
  hermetic checks (`bash -n`, `ci/check-docker-pins.sh`). The crashsim guard moved into `ci/check-htapd-no-crashsim.sh`,
  shared by `./ci.sh` and the image build.
- **(D5) A one-line `htapd` log fix.** Adopted: the non-loopback "traffic is cleartext" warning is logged whenever secure
  transport is not required, which covers both no TLS and optional TLS, and is silent only with `--require-secure-transport`/
  `HTAPD_REQUIRE_SECURE_TRANSPORT`. The container always binds `0.0.0.0`, so the TLS override (which requires secure transport)
  is the configuration that is silent; merely configuring TLS is not enough.

### Decision

Ship a multi-stage `Dockerfile` (builder and runtime on the same Debian release, uid/gid 10001, `tini` entrypoint, no
`VOLUME`, no secrets in the image, a handshake `HEALTHCHECK`), a `docker-compose.yml` (loopback publish, named volume,
file-backed secret, `read_only`, `cap_drop: ALL`, `no-new-privileges`, `stop_grace_period: 15s` as a safety bound only,
single replica) and a `docker-compose.tls.yml` override. Stop semantics are exit 143, crash-equivalent, with no drain.
Document: bypassing the entrypoint restores the PID-1 hang; the password file is bootstrap-only; the probe is not SQL health;
the volume must be a local POSIX filesystem.

### Consequences

- A container deployment exists with evidence: `ci/docker-smoke.sh` scenarios A/B/C driving
  `crates/htap-client/tests/docker_smoke.rs` (`smoke_write`, `smoke_verify`, `smoke_wrong_password_rejected`, `smoke_tls_ok`,
  `smoke_plaintext_rejected_when_secure_transport_required`, `smoke_tls_bad_server_name_rejected`), opt-in and fail-closed.
- Still `deferred`: graceful in-process drain, `SIGHUP` TLS reload, multi-arch or registry-published images, SBOM and
  signing, Kubernetes/Helm, Docker-based ZooKeeper ensemble testing (ADR-006 is unchanged), roles.
- Certificate rotation and every stop need a restart that is crash-equivalent; operators must wait for health after any
  lifecycle step.
- The rebuild cadence for base-image security updates is an operator decision, not enforced by CI. Image size is a measured
  number on one host (35.6 MB by `docker image inspect`, about 139 MB unpacked by `docker images`). `ci/docker-smoke.sh` does assert
  the `docker image inspect` size against `HTAPD_IMAGE_SIZE_BUDGET` (default 43000000 bytes, the measured 35655375 plus about 20%),
  but only in the opt-in smoke run; the unpacked size is not gated.

### How to reverse it

Delete `Dockerfile`, `.dockerignore`, `docker/`, `docker-compose*.yml`, `ci/docker-smoke.sh`, `ci/check-docker-pins.sh` and the
`--docker` flag and hermetic checks in `./ci.sh`, and `crates/htap-client/tests/docker_smoke.rs`. Nothing in the engine
depends on them; `ci/check-htapd-no-crashsim.sh` and the `htapd` warning fix can stay. Adding a graceful drain later is a
separate `htapd` change (signal handling, a bounded `WireServer::shutdown`, a storage-reviewer pass); the accepted exit code in
`ci/docker-smoke.sh` scenario B would then become 0.

### Verification

`ci/docker-smoke.sh` (opt-in, `./ci.sh --docker`), `crates/htap-client/tests/docker_smoke.rs` (the six tests named above, run
with `--ignored` and the `HTAP_DOCKER_SMOKE_*` environment by the script), `ci/check-docker-pins.sh` and
`ci/check-htapd-no-crashsim.sh` (hermetic, in default `./ci.sh`). The smoke script also asserts the image-size budget and that a
no-password start fails with the entrypoint message on stderr. Operator guide: `docs/OPERATIONS.md` section 7.
