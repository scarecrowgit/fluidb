# Architecture

This document describes the intended system. Every component carries a status:

- `implemented` / `implemented (local MVP)` — built and covered by tests.
- `in progress` — partially built.
- `planned` / `deferred` — designed, not yet built in the local slice.

**Current state of the repository.** The cargo workspace skeleton, `htap-common`
(the `Version` MVCC domain, `FencingToken`, and shared error types),
`htap-rowstore` (WAL, memtable, SST writer/reader, and LSM row-store engine), and
`htap-colstore` (immutable encoded/compressed segments, typed zone maps, and vectorized scans)
are `implemented`. Phase 3 has a completed narrow local slice: sqlparser MySQL dialect,
strict binder (with typed `PointSelect` and `AnalyticSelect`), structural route classifier (`htap-sql`), durable catalog with reopen recovery
(`htap-catalog`), and synchronous `LocalServer` (`htap-server`) supporting `CREATE TABLE`, literal
`INSERT`, PK `DELETE`, complete-PK `SELECT` (strictly routing to `RowstorePointRead` and remaining separate and unchanged), and narrow analytical scans over logical rowstore
and base-plus-delta rows using server-root `<root>/colstore` for materialized `Column`/`Converting` partitions with projection-aware compact reads (PK+requested column union, single safe predicate-leaf pushdown into `SegmentReader`, delta suppression/overlay, deterministic PK ordering, and full residual SQL filter/aggregate/group evaluation) with reopen recovery.
Phase 4 has a completed local conversion MVP: partition-scoped row-to-column conversion (`htap-convert`)
with durable tablet columnar manifest envelopes, a four-phase state machine
(`SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`), atomic per-tablet manifest and
catalog publication, rowstore-authoritative base-plus-delta overlay, and online point writes/reads on
converting and columnar storage partitions.
Phase 5 has a completed local movement MVP (`htap-movement`): durable jobs (`HTAPJOB1`), CSV/JSONL
streaming import and full logical partition export materialization (exports materialize full logical partition before writing), and tablet snapshot clone/verify/repair (`HTAPMNF1`).
Phase 6 has a completed local coordination and placement MVP (`htap-coord`): synchronous `Coordinator`
trait, durable `LocalCoordinator` persisting at `COORDINATOR` (`HTAPCRD1`), deterministic sorted membership,
strictly monotonic fencing tokens (`FencingToken`), coordinator-fenced catalog CAS
(`fenced_catalog_compare_and_set`), deterministic placement planner (`plan_placement`), and coordinator-fenced
local replica staging and activation simulation (`stage_placement_addition`, `activate_placement_addition`,
`activate_placement_plan`).
Phase 7 has a completed local evidence MVP: Criterion microbenchmarks (`htap-bench`, `benches/local_mvp.rs`)
evaluating rowstore point get, colstore zone map scans, row-to-column conversion, CSV movement import,
coordinator placement planning, and coordinator leadership fenced CAS; and synchronous in-process embedded
client (`htap-client`, `EmbeddedClient`) providing an ergonomic SQL execution interface over `LocalServer`
with full test coverage in `crates/htap-client/tests/embedded_client.rs`. Root `README.md`, `docs/BENCHMARKS.md`,
and `docs/OPERATIONS.md` define the operational model, and `ci.sh` runs `cargo bench --workspace --no-run`.
Later components described below remain `planned` or `deferred` (explicitly deferred:
direct CatalogStore CAS and older movement repair APIs bypass coordinator fence; no Raft/`openraft`,
ZooKeeper backend, watches/locks/KV semantics, distributed consensus, concurrent shared-root writers / distributed coordination (concurrent shared-root operation remains unsupported),
remote physical movement, leader handoff, ongoing replication, capacity/rack placement, or live rebalance;
reverse `Column -> Row` conversion, delete vectors, physical rowstore reclamation, compaction,
compound AND pushdown beyond one leaf, != pushdown, vectorized aggregation / operator pipelines, joins/CTEs/windows/ORDER/LIMIT/HAVING/OR/expressions/AVG/distinct,
multi-tablet/distributed scans, quotas/spill/cancellation, DataFusion/Arrow integration, multi-partition routing, MySQL wire protocol/`htapd` daemon,
sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`, Docker image/Compose deployment, and broad MySQL compatibility).
See [`PROGRESS.md`](./PROGRESS.md).

---

## Component diagram

### Target / Planned Architecture Diagram (Qualified Intended State)

The following ASCII diagram illustrates the planned multi-node target architecture, including the planned `htapd` daemon listener and planned DataFusion OLAP execution engine. These components are explicitly deferred or planned and are not part of the active SQL execution path in the current local MVP.

```text
                        +---------------------------+
                        |   client (EmbeddedClient  |
                        |   façade; wire planned)   |
                        +-------------+-------------+
                                      |
====================================  |  ==================================
 htapd / LocalServer (LocalServer implemented in htap-server; daemon planned)
======================================================================
                                      |
                        +-------------v-------------+
                        |  htap-sql                 |  implemented (local MVP)
                        |  parse (sqlparser, MySQL) |  (narrow point subset;
                        |  bind / catalog resolve   |   breadth planned)
                        +-------------+-------------+
                                      |
                        +-------------v-------------+
                        |  query router             |  implemented (local MVP)
                        +------+-------------+------+  (rowstore classifier;
                    PK point   |             |  everything else
                    lookup /   |             |         sharded routing planned)
                    short txn  |             |
             +-----------------v--+       +--v---------------------+
             | OLTP fast path     |       | OLAP engine            |
             | index probe -> row |       | (DataFusion, vectorized|
             | fetch, no planner  |       |  plan fragments)       |
             | implemented (MVP)  |       | planned                |
             +---------+----------+       +-----------+------------+
                       |                              |
                       |   (no crate dependency)      |
                       +--------------+---------------+
                                      |
      +-------------------------------v-------------------------------+
      |  htap-txn : MVCC version domain, rowstore participant (MVP);  |
      |  shared multi-format WAL planned / deferred                   |
      +-------------------------------+-------------------------------+
                                      |
              +-----------------------+-----------------------+
              |                                               |
    +----------v-----------+                       +-----------v----------+
    | htap-rowstore (OLTP) |                       | htap-colstore (OLAP) |
    | WAL, memtable,       | rowstore-auth overlay | immutable segments,  |
    | SSTs, PK index,      |<- base+delta (impl) ->| column chunks,       |
    | MVCC snapshot engine |    delete vectors     | per-page zone maps   |
    | IMPLEMENTED          |  (planned/deferred)   | IMPLEMENTED          |
    +----------+-----------+                       +-----------+----------+
              |                                               |
              +-----------------------+-----------------------+
                                      |
      +-------------------------------v-------------------------------+
      | htap-convert : row -> col conversion (R2)  IMPLEMENTED (MVP)   |
      +---------------------------------------------------------------+

   +----------------------+  +----------------------+  +-----------------+
   | htap-catalog         |  | htap-coord (localMVP)|  | htap-movement   |
   | durable local store, |  | local | Raft(plan)   |  | placement,      |
   | topology, recovery   |  | ZK(plan) | fence CAS |  | repair (local)  |
   | IMPLEMENTED (MVP)    |  | IMPLEMENTED (MVP)    |  | IMPLEMENTED     |
   +----------------------+  +----------------------+  +-----------------+

   +---------------------------------------------------------------+
   | htap-common : Version, FencingToken, error types  IMPLEMENTED |
   +---------------------------------------------------------------+
```

### Current In-Process Execution Call Flow (Implemented Local Slice)

In the implemented local slice, all SQL execution is synchronous and in-process. `EmbeddedClient` forwards calls directly to `LocalServer`, which acquires its execution lock, parses and binds the statement against the durable catalog, classifies the route, and delegates directly to the appropriate storage or transaction subsystem:

```mermaid
flowchart TD
    subgraph CurrentDirectCalls ["Direct Current In-Process Execution"]
        EC["EmbeddedClient.execute(sql)"] --> LS["LocalServer.execute(sql)"]
        LS --> Lock["Acquire execution_lock<br/>(parking_lot::Mutex)"]
        Lock --> SQL["htap_sql::parse_one(sql)<br/>htap_catalog::LocalCatalogStore.load()<br/>htap_sql::bind(stmt, snapshot)"]
        SQL --> Route["htap_sql::classify_route(bound, storage)"]

        Route -->|"Route::CatalogDdl<br/>(CREATE TABLE)"| DDL["DDL Catalog CAS<br/>LocalCatalogStore.compare_and_set"]
        Route -->|"Route::RowstoreWrite<br/>(INSERT / DELETE)"| DML["TransactionManager.commit_request<br/>RowstoreParticipant (ID 1)<br/>htap_rowstore::Engine (WAL + Memtable)"]
        Route -->|"Route::RowstorePointRead<br/>(complete-PK SELECT)"| PointRead["Snapshot(visible_version)<br/>htap_rowstore::Engine.get(key)"]
        Route -->|"Route::OlapScan<br/>(AnalyticSelect)"| OlapScan["execute_analytic_select<br/>Row: scan rowstore<br/>Column/Converting: read_column_partition_compact_core<br/>at &lt;root&gt;/colstore (PK+requested union, 1 leaf pushdown)<br/>suppress/overlay deltas, eval residual SQL"]
    end

    subgraph PlannedDeferred ["Planned / Deferred Components (Not in Direct SQL Path)"]
        Daemon["htapd daemon / MySQL wire listener"] -.->|planned| LS
        DataFusion["DataFusion vectorized scan engine"] -.->|planned| ColEngine["htap-colstore scans"]
    end
```

### DML Transaction Execution Sequence

Transactional mutations (`INSERT` and `DELETE`) execute through `LocalServer`'s single execution lock, 2PC `TransactionManager` logging, and rowstore participant application, returning the assigned monotonic MVCC version:

```mermaid
sequenceDiagram
    autonumber
    actor Caller as Caller / Host Application
    participant Client as EmbeddedClient
    participant Server as LocalServer
    participant Parser as htap-sql (parse & bind)
    participant Catalog as LocalCatalogStore
    participant TxnMgr as TransactionManager
    participant Journal as txn.journal
    participant RowstorePart as RowstoreParticipant (ID 1)
    participant Engine as Rowstore Engine

    Caller->>Client: execute(sql) [INSERT or DELETE]
    Client->>Server: execute(sql)
    Note over Server: Acquire execution_lock (Mutex)
    Server->>Parser: parse_one(sql)
    Parser-->>Server: AST
    Server->>Catalog: load()
    Catalog-->>Server: CatalogSnapshot
    Server->>Parser: bind(AST, CatalogSnapshot)
    Parser-->>Server: BoundStatement (Insert / Delete)
    Server->>Parser: classify_route(BoundStatement, partition.storage)
    Parser-->>Server: Route::RowstoreWrite

    Server->>TxnMgr: commit_request(TransactionRequest)
    Note over TxnMgr: Acquire manager lock (serializes commit decision)
    TxnMgr->>RowstorePart: prepare(snapshot, payload)
    RowstorePart-->>TxnMgr: Ok
    TxnMgr->>Journal: Append & fsync INTENT frame
    TxnMgr->>Journal: Append & fsync COMMIT frame (irrevocable)
    TxnMgr->>RowstorePart: apply(txn_id, version, payload)
    RowstorePart->>Engine: apply(mutations) -> write WAL & memtable
    Engine-->>RowstorePart: Ok
    RowstorePart-->>TxnMgr: Ok
    TxnMgr->>RowstorePart: publish(txn_id, version)
    RowstorePart->>Engine: publish(version)
    Engine-->>RowstorePart: Ok
    RowstorePart-->>TxnMgr: Ok
    TxnMgr->>TxnMgr: Advance visible_version watermark
    TxnMgr-->>Server: CommittedTransaction { version, ... }
    Note over Server: Release execution_lock
    Server-->>Client: StatementResult::dml(affected, Some(version))
    Client-->>Caller: StatementResult::Command(CommandResult::Dml { affected, version })
```

---

## Subsystem Boundaries: LocalServer, LocalConverter, and LocalCoordinator

It is important to emphasize that `LocalServer::open` does **not** instantiate or supervise `LocalConverter` (`crates/htap-convert`) or `LocalCoordinator` (`crates/htap-coord`):
- `LocalServer` integrates only `LocalCatalogStore`, `htap_rowstore::Engine`, `TransactionManager` (with a single registered `RowstoreParticipant`), and `LocalDataMover`.
- `LocalConverter` is a partition-scoped conversion state machine that runs independently to transcode rowstore data into columnar segments (`htap-colstore`) and advance catalog metadata via CAS.
- `LocalCoordinator` is an independent cluster coordination and placement engine persisting its own state envelope at `<coord_root>/COORDINATOR` (`HTAPCRD1`).
Neither converter background workers nor coordinator lease managers are created or managed by `LocalServer` or `EmbeddedClient`. They are standalone crate capabilities used directly in migration or coordination tasks.

---

## Process and role model

**Status: `implemented (local MVP)`** (synchronous `LocalServer` in-process execution façade and `EmbeddedClient` implemented for the narrow local slice; `htapd` daemon, network listeners, and MySQL wire protocol are planned/deferred).

The system architecture envisions a future **single binary, `htapd`** (planned), which can be run as:

- the **frontend role** — SQL surface, catalog, planner, transaction
  coordinator;
- the **backend role** — storage, execution, compaction;
- **both roles in one process**, planned for single-node development and deployments.

For the completed narrow local slice, `htap-server` provides `LocalServer` and `htap-client` provides `EmbeddedClient`,
synchronous in-process façades composing the durable catalog (`LocalCatalogStore`),
`htap-txn` transaction manager, and `htap-rowstore` LSM engine. `LocalServer` currently requires tables
to have exactly one partition, one tablet, and one healthy leader replica (verified in
`crates/htap-server/tests/local_server.rs`). Sharding and placement capabilities in `htap-coord` and `htap-movement`
provide deterministic placement planning and local replica activation simulation, not sharded SQL serving across distributed nodes.
The current README demo and test suite use the `EmbeddedClient -> LocalServer` in-process façade to directly execute
`CREATE TABLE` (deterministic one-partition row topology), literal `INSERT`, PK `DELETE`,
and complete-PK `SELECT` with reopen recovery and error mapping, without networking or wire protocol overhead
(covered in `crates/htap-server/tests/local_server.rs` and `crates/htap-client/tests/embedded_client.rs`).

The frontend/backend boundary is preserved as an internal module boundary,
policed by crate dependencies. Splitting the two into separate processes is
therefore a **deployment choice, not a rewrite**.

---

## Dual-format storage

**Status: `implemented (local MVP)`** (`htap-rowstore`, `htap-colstore`, and local one-way row-to-column conversion via `htap-convert` are implemented as local MVPs; reverse conversion, delete vectors, compaction, and distributed conversion are planned/deferred).

| Format | Crate | Structure | Status |
| ------ | ----- | --------- | ------ |
| Row store (OLTP) | `htap-rowstore` | LSM: WAL, memtable, immutable sorted runs (SSTs), primary-key index, MVCC versions. | `implemented` |
| Column store (OLAP) | `htap-colstore` | Immutable segments of encoded (plain/dictionary), compressed (zstd) column blocks with typed zone maps and vectorized scanning. | `implemented` |

The `htap-colstore` implementation delivers the standalone columnar engine MVP:
durable binary segment files, plain and dictionary column encodings, optional zstd compression,
per-block CRC32C integrity checksums, typed zone maps (min, max, and nullability flags),
and vectorized scan execution with conservative pushdown pruning and selective column decoding
(verified in `crates/htap-colstore/tests/segment_roundtrip.rs`, `crates/htap-colstore/tests/scan.rs`,
and `crates/htap-colstore/tests/zone_map_skip.rs`).
Phase 4 integrates `htap-colstore` segments into partition-scoped conversion (`htap-convert`),
registering columnar segments in durable tablet manifests while keeping the rowstore authoritative
for online point mutations and post-conversion base-plus-delta queries.

Both formats share **one MVCC version domain** (`htap-common::Version`, which
is `implemented`). In the target architecture, a unified WAL across formats is envisioned.
In the current implementation, however, there is **no single shared WAL that atomically mutates
both row and column formats within a single transaction**: the **rowstore remains authoritative**
for all writes and commits through `TransactionManager` using `RowstoreParticipant` only
(`crates/htap-server/src/lib.rs`). Columnar storage is created via partition-scoped conversion
(`htap-convert`), and its durability and visibility are published separately via tablet manifests
(`HTAPTBM1`) and catalog metadata CAS updates. Current SQL statements touch the rowstore participant
exclusively; single transactions mutating both row and column representations simultaneously are
planned/deferred.

This separation preserves a unified MVCC version domain across storage formats without requiring
distributed two-phase commit between independent version spaces (see [ADR-004](./DECISIONS.md)).

---

## Freshness: delta store and merge-on-read

**Status: `implemented (local MVP)`** (authoritative rowstore base-plus-delta freshness overlay implemented for local conversion; delete vectors and background base compaction are planned/deferred).

A partition converted to or held in column format still accepts writes. In the Phase 4 local MVP,
those writes land directly in the authoritative **row store**, which acts as the live delta store:

- Historical reads prior to the conversion base version read directly from the rowstore.
- Materialized partition scans via the converter API (`LocalConverter::read_column_partition` / `htap_convert::read_column_partition`, verified in `crates/htap-convert/tests/materialization.rs`) scan the columnar base segments up to the conversion snapshot version and overlay rowstore mutations (`Put` and `Delete`) committed after that base version up to the target snapshot. Note that `read_column_partition` is a standalone converter query API, not SQL execution.
- Online transactional writes (`INSERT`, `DELETE`) and point reads (`SELECT` by primary key) execute directly against the row store (`Route::RowstoreWrite` and `Route::RowstorePointRead`), ensuring zero read or write interruption during and after conversion (verified in `crates/htap-server/tests/local_server.rs`).
- Bitmap delete vectors directly on columnar segments, physical rowstore reclamation, and background compaction folding deltas into new columnar segments are explicitly deferred.

---

## Query routing — the R5 guarantee, structurally enforced

**Status: `implemented (local MVP)`** (structural route classifier implemented for point lookups, DDL/DML, and narrow OLAP scans with compact base scan pushdown; full SQL breadth, vectorized aggregation / operator pipelines, compound pushdown beyond one leaf, and multi-partition routing are planned/deferred).

A router inspects the **bound** statement and the partition's **storage descriptor**:

- **Point lookups and short transactions that resolve fully against a primary key** take a dedicated fast path: index probe → row fetch. There is no plan-fragment construction and no vectorized operator pipeline. In `htap-sql`, complete-PK `SELECT` queries strictly bind to `PointSelect` and route to `Route::RowstorePointRead { key }` across all three storage formats (`Row`, `Column`, `Converting`), serving point reads directly from the authoritative rowstore without invoking OLAP execution or the converter (verified in `crates/htap-sql/tests/route.rs`, `crates/htap-sql/tests/parse_bind.rs`, `crates/htap-server/tests/local_server.rs`, and `crates/htap-client/tests/embedded_client.rs`). Complete-PK point read execution remains separate and unchanged.
- **Narrow OLAP Scans (`Route::OlapScan`):** `htap-sql` binds non-PK queries on a single unaliased table into typed `AnalyticSelect` structures routing to `Route::OlapScan`. `LocalServer` executes these narrow analytical scans over logical rowstore rows or the converter's rowstore-authoritative base-plus-delta view using server-root `<root>/colstore` for materialized `Column` and `Converting` partitions (validating catalog vs disk manifest generations).
  - **Supported OLAP SQL:** Exactly one unaliased table in `FROM`; plain projections (named columns or `*`, with optional aliases); AND-only typed filters (`=`, `!=`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`) with SQL three-valued logic; aggregate functions `COUNT(*)`, `COUNT(column)`, `SUM(column)` (for `Int32`, `Int64`, and `Float64`), `MIN(column)`, and `MAX(column)`; deterministic `GROUP BY` with SQL NULL grouping; evaluated against the current visible snapshot (`visible_version`).
  - **Base scan pushdown optimization:** For materialized `Column` and `Converting` partitions, `LocalServer` executes projection-aware compact reads (`read_column_partition_compact_core`) using the union of primary-key indices and requested source columns (from projections, `GROUP BY`, and filter leaves). It safely pushes down at most one eligible predicate leaf (`=`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`) directly into columnar `SegmentReader::scan`. Stale base rows are suppressed via newest post-base rowstore deltas, mutations (`Put`/`Delete`) are overlaid, and rows are ordered by primary key deterministically before complete residual SQL filter, aggregate, and group evaluation.
  - **Internal execution evidence:** Columnar scan statistics and block pruning (`ScanStats`) are captured as internal execution evidence during compact reads, but the SQL layer continues to evaluate materialized logical rows; vectorized aggregation is not implemented.
- **Route Acceptance across Storage Formats:** `classify_route` accepts `StorageDescriptor::Row`, `StorageDescriptor::Column`, and `StorageDescriptor::Converting`:
  - `CREATE TABLE` routes to `Route::CatalogDdl`.
  - Literal `INSERT` and PK `DELETE` route to `Route::RowstoreWrite` regardless of whether the partition is `Row`, `Column`, or `Converting`, preserving rowstore write-authority and zero mutation downtime.
  - Complete-PK `SELECT` routes to `Route::RowstorePointRead { key }` across all three storage formats, strictly bypassing analytical execution and the converter.
  - `AnalyticSelect` routes to `Route::OlapScan` across `Row`, `Column`, and `Converting` formats.
- **Current SQL-Created Row Topology:** While `classify_route` accepts all three descriptors, tables created via SQL DDL (`CREATE TABLE`) in `LocalServer` are currently initialized exclusively with a single-partition `StorageDescriptor::Row` topology. Setting a partition to `Column` or `Converting` occurs via `LocalServer::convert_table`, catalog updates, or `LocalConverter` workflows.
- **Explicitly Deferred OLAP & SQL Capabilities:** Direct SegmentReader pushdown optimization is implemented for the compact base path (single leaf pushdown). Joins, CTEs (`WITH`), window functions (`OVER`), `ORDER BY`, `LIMIT`, `HAVING`, `OR`/`NOT`/arithmetic/casts, `AVG` and `DISTINCT` aggregates, compound AND pushdown beyond one leaf, `!=` pushdown, vectorized aggregation / operator pipelines, multi-tablet or distributed partition scans, resource quotas/spill/cancellation, DataFusion/Arrow integration, and full MySQL dialect breadth remain deferred.

This separation is enforced **by construction, not by a runtime heuristic**:
the OLTP fast path lives in a crate that has no dependency on the analytical
execution crate. It is therefore not possible for a point lookup to
accidentally acquire analytical-engine overhead through a mis-tuned cost
threshold — the code to do so is not linked into that path. See
[ADR-001](./DECISIONS.md).

---

## Resource isolation

**Status: `planned`.**

The transactional and analytical paths get **separate thread pools and
separate memory budgets**, both configurable. A large analytical scan
therefore cannot starve concurrent point queries of either CPU or memory.

---

## Storage-format conversion (R2)

**Status: `implemented (local MVP)`** (`htap-convert`).

Conversion operates as a partition-scoped, crash-resumable state machine for local single-tablet partitions:

```text
StorageDescriptor::Row
        │
        ▼ (pin visible version V, allocate conversion generation)
ConversionPhase::SnapshotPinned
        │
        ▼ (write columnar segments, atomically persist MANIFEST)
ConversionPhase::SegmentsWritten
        │
        ▼ (advance catalog CAS with incremented generation)
ConversionPhase::ReadyToPublish
        │
        ▼ (final catalog CAS cutover to Column, clear conversion metadata)
StorageDescriptor::Column
```

### Conversion execution steps

1. **Topology validation and snapshot pinning (`SnapshotPinned`):**
   The converter validates that the partition contains exactly one tablet with one healthy leader replica.
   If in `StorageDescriptor::Row`, it pins snapshot version `V` from the rowstore's current visible version
   (or `Version(1)` if unwritten), allocates a new catalog generation, and updates the partition via catalog
   compare-and-set (CAS) to `StorageDescriptor::Converting { from: Row, to: Column }` with phase
   `ConversionPhase::SnapshotPinned`. If resuming an existing conversion, the persisted pinned snapshot
   version and generation are reused without taking a new snapshot.

2. **Columnar transcoding and atomic manifest write (`SegmentsWritten`):**
   The converter scans the rowstore at pinned snapshot version `V`, collapses MVCC versions and tombstones
   into logical rows, and encodes durable columnar segments (`htap-colstore`) into the tablet directory.
   It writes the tablet columnar manifest (`HTAPTBM1` binary envelope with CRC32C integrity checksum)
   atomically via temporary file replacement (`MANIFEST.tmp` -> `MANIFEST`).
   The catalog is then advanced via CAS to `ConversionPhase::SegmentsWritten`.

3. **Publication gating (`ReadyToPublish`):**
   The converter advances the catalog via CAS to `ConversionPhase::ReadyToPublish` with an incremented catalog
   generation, ensuring all segment and manifest writes are durably observable before final cutover.

4. **Atomic cutover (`Column`):**
   The final catalog CAS transitions the partition from `StorageDescriptor::Converting` in `ReadyToPublish`
   phase to `StorageDescriptor::Column`. The conversion descriptor is cleared and the tablet's
   `ColumnManifestRef` is registered in the catalog in a single atomic metadata update.

### System invariants during and after conversion

- **Atomic per-tablet publication:** Columnar segments and the tablet manifest are written to disk with
  checksum verification and atomic file renaming before catalog cutover. The manifest only ever references
  readable, durable segments, preventing orphaned or partially written files from becoming visible.
- **Rowstore authoritative base-plus-delta overlay:** The rowstore remains the authoritative source of truth.
  Queries reading full partitions via the converter API (`LocalConverter::read_column_partition` / `htap_convert::read_column_partition`, verified in `crates/htap-convert/tests/materialization.rs`; note this is a converter API, not SQL execution) read base rows from columnar segments
  up to the conversion base version `V`, and overlay rowstore mutations (`Put` and `Delete`) committed after
  version `V` up to the target snapshot. Historical reads for versions earlier than `V` are served directly
  from the rowstore.
- **Online point writes and reads:** Primary-key point lookups (`SELECT` by PK) and mutations (`INSERT`,
  `DELETE`) continue to execute online without interruption through `LocalServer` and `htap-sql` query routing
  (`Route::RowstoreWrite` and `Route::RowstorePointRead`), which operate directly on the rowstore regardless
  of whether the partition is `Row`, `Converting`, or `Column`.
- **Reverse conversion:** Reverse `Column -> Row` conversion is **not implemented** and is not claimed.
  Attempting conversion in any direction other than `Row -> Column` returns `HtapError::Unsupported`.

### Explicitly deferred features

- **Delete vectors:** Per-segment bitmap delete vectors on columnar segments are deferred; deletions are
  tracked via rowstore tombstones in the base-plus-delta overlay.
- **Physical rowstore reclamation:** Converted rows remain in the rowstore SSTs and WAL; physical space
  reclamation / truncation of converted rowstore history is deferred.
- **Compaction:** Background merge-on-read compaction folding accumulated rowstore deltas into new columnar
  segments is deferred.
- **Vectorized aggregation, compound pushdown, and distributed OLAP scans:** Direct `SegmentReader` pushdown optimization is now implemented for the compact base path (projection-aware compact reads and single safe predicate leaf pushdown). Vectorized aggregation, vectorized operator pipelines, compound `AND` pushdown beyond one leaf, `!=` pushdown, joins, CTEs, windows, and multi-tablet/distributed scans remain deferred.
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions,
  range/list partition boundaries, and distributed multi-node coordinated cutovers are deferred to Phase 5
  and Phase 6.

---

## Coordination

**Status: `implemented (local MVP)`** (`htap-coord` implements local durable coordination, membership, fencing, and fenced catalog CAS; distributed consensus backends like Raft/ZooKeeper are planned/deferred).

The synchronous `Coordinator` trait defines cluster coordination, membership tracking, scoped leadership election, fence validation, and atomic coordinator-fenced catalog compare-and-set updates:

```rust
pub trait Coordinator: Send + Sync {
    fn register_node(&self, node_id: NodeId) -> Result<()>;
    fn remove_node(&self, node_id: NodeId) -> Result<()>;
    fn list_nodes(&self) -> Result<Vec<NodeId>>;
    fn acquire_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership>;
    fn current_leadership(&self, scope: &str) -> Result<Option<Leadership>>;
    fn release_leadership(&self, scope: &str) -> Result<()>;
    fn replace_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership>;
    fn validate_fence(&self, scope: &str, token: FencingToken) -> Result<()>;
    fn fenced_catalog_compare_and_set(
        &self,
        scope: &str,
        token: FencingToken,
        catalog: &dyn CatalogStore,
        expected_generation: u64,
        next: CatalogSnapshot,
    ) -> Result<()>;
}
```

### Local coordinator implementation (`LocalCoordinator`)

The single-node implementation (`LocalCoordinator`) persists membership, scoped leadership leases, highest scope tokens, and next token allocator state at `<root>/COORDINATOR` using the `HTAPCRD1` binary envelope format (`HEADER_MAGIC = b"HTAPCRD1"`, `FORMAT_VERSION = 1`, 18-byte header with payload length and CRC32C checksum). Updates follow atomic staging semantics (`COORDINATOR.tmp` write -> fsync -> rename -> directory fsync).

Key operational guarantees:
- **Sorted membership:** `list_nodes` returns registered `NodeId` entries in deterministic ascending order.
- **Strictly monotonic tokens:** Fencing tokens (`FencingToken`) advance strictly monotonically on every leadership acquisition and replacement, and are persisted durably so that tokens are never reused across process restarts.
- **Fenced catalog compare-and-set:** `fenced_catalog_compare_and_set` executes under the coordinator's state lock, validating that the caller's fencing token matches the current active leader token for the target scope before invoking `CatalogStore::compare_and_set`. Stale leaders receive `HtapError::Fenced` and cannot mutate catalog state.

### Backend status and roadmap

| Backend | Status | Purpose |
| ------- | ------ | ------- |
| Local single-node (`LocalCoordinator`) | `implemented (local MVP)` | Single-node durable coordinator with `HTAPCRD1` envelopes, one-owner OS advisory `ProcessLock`, and fenced catalog CAS. |
| Embedded Raft (`openraft`) | `planned` | Distributed consensus default for multi-node deployments. |
| ZooKeeper | `planned` | Integration with existing external ensembles. |

### Architectural boundaries and explicitly deferred capabilities

- **Direct CatalogStore CAS and movement repair bypass fence:** Direct calls to `CatalogStore::compare_and_set` and older movement repair APIs (`htap_movement::repair_replica`) operate directly against catalog storage without coordinator fence validation. Fencing is strictly enforced when mutations route through `fenced_catalog_compare_and_set` or `activate_placement_addition`.
- **One-owner OS advisory ProcessLock:** `LocalCoordinator`'s root has a one-owner OS advisory `ProcessLock` (`<root>/LOCK` via `flock`), rejecting concurrent process opens with `HtapError::Conflict`; distributed consensus and concurrent multi-node coordination remain unsupported.
- **No distributed consensus:** Neither Raft (`openraft`) nor ZooKeeper backends are implemented. Coordination is single-node local only.
- **No watches, distributed locks, or KV store semantics:** The `Coordinator` trait focuses on membership, scoped leadership, and catalog CAS gating. General-purpose KV storage, ephemeral path watches, and lock lease expirations are deferred.
- **No remote physical movement or network transport:** Tablet movement and placement activation execute locally using `htap_movement::LocalDataMover`, local filesystem paths, and local rowstore engine snapshots.
- **No leader handoff, ongoing replication, capacity/rack placement, or live rebalance:** Leadership turnover immediately revokes previous tokens but executes no consensus handoff protocol; replication uses static snapshot clone packages rather than ongoing log replication; dynamic placement does not rebalance live clusters or consider rack topology or storage capacity.

---

## Sharding and placement

**Status: `implemented (local MVP)`** (the `htap-catalog`, `htap-movement`, and `htap-coord` crates implement single-node tablet sharding, clone packages, deterministic placement planning, and local replica activation simulation; multi-node network coordination, remote physical movement, and physical sharded SQL serving are planned/deferred).

```text
Table
  └── Partition        (range or list, on a partition key)
        └── Tablet     (hash bucketed, on a distribution key)
              └── Replica
```

The **tablet is the unit of placement, replication, movement, and repair**.

**Current LocalServer Single-Tablet Requirement vs. Placement Simulation:**
`LocalServer` currently requires tables to have exactly one partition, one tablet, and one healthy leader replica (verified in `crates/htap-server/tests/local_server.rs`). Sharding and placement capabilities (`plan_placement`, `stage_placement_addition`, `activate_placement_addition`, `activate_placement_plan`) operate purely on catalog metadata, placement planning algorithms, and local replica snapshot clone/activation simulation (verified in `crates/htap-coord/tests/placement_movement.rs` and `crates/htap-movement/tests/tablet_simulation.rs`). They do not provide physical sharded SQL serving across multiple nodes.

### Deterministic placement planning (`plan_placement`)

The placement planner computes pure, deterministic, colocation-free replica placement plans over an immutable `CatalogSnapshot`, a candidate `NodeId` list, and a target replication factor:
1. **Canonical sorting:** Sorts candidate nodes and tablets in ascending order of their IDs to ensure identical plans regardless of input order.
2. **Colocation prevention:** Strictly validates that no node hosts multiple replicas of the same tablet.
3. **Greedy load balancing:** Preserves existing healthy replicas and assigns new replicas to candidate nodes with the lowest current replica count, breaking ties deterministically by smallest `NodeId`.
4. **Monotonic replica ID allocation:** Allocates new `ReplicaId` values starting from `max_existing_id + 1` with checked arithmetic overflow validation.

### Coordinator-mediated local replica activation

Replica activation proceeds through coordinator-fenced phases:
1. **Staging (`stage_placement_addition`):** Under coordinator fence validation for the tablet scope, registers target `ReplicaDescriptor` in the catalog with `is_leader = false` and `healthy = false` via `fenced_catalog_compare_and_set`.
2. **Logical clone & package verification:** Generates a snapshot clone package via `htap_movement::clone_tablet` and verifies package checksums and metadata via `htap_movement::verify_package` (`HTAPMNF1`).
3. **Activation (`activate_placement_addition`):** Transitions the target replica to `healthy = true` via `fenced_catalog_compare_and_set`. If cloning, verification, or fencing fails at any step, the target replica remains unready (`healthy = false`) in the catalog.
4. **Batch activation (`activate_placement_plan`):** Sequentially stages, clones, verifies, and activates all additions in a `PlacementPlan`.

### Hash bucketing contract

The hash bucketing contract is fixed explicitly and versioned: the bucket is
`crc32(concatenated per-column binary encodings) % tablet_count`, where the
modulus is the actual tablet count of that index in that partition. The
per-type byte encoding is a versioned contract rather than an implementation
detail — see finding 11 in [`RESEARCH.md`](./RESEARCH.md).
