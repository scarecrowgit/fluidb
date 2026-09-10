# Architecture

This document describes the intended system. Every component carries a status:

- `implemented` — built and covered by tests.
- `in progress` — partially built.
- `planned` — designed, not yet built.

**Current state of the repository.** The cargo workspace skeleton, `htap-common`
(the `Version` MVCC domain, `FencingToken`, and shared error types),
`htap-rowstore` (WAL, memtable, SST writer/reader, and LSM row-store engine), and
`htap-colstore` (immutable encoded/compressed segments, typed zone maps, and vectorized scans)
are `implemented`. Phase 3 has a completed narrow local slice: sqlparser MySQL dialect,
strict binder, structural rowstore route classifier (`htap-sql`), durable catalog with reopen recovery
(`htap-catalog`), and synchronous `LocalServer` (`htap-server`) supporting `CREATE TABLE`, literal
`INSERT`, PK `DELETE`, and complete-PK `SELECT` with reopen recovery.
Phase 4 has a completed local conversion MVP: partition-scoped row-to-column conversion (`htap-convert`)
with durable tablet columnar manifest envelopes, a four-phase state machine
(`SnapshotPinned -> SegmentsWritten -> ReadyToPublish -> Column`), atomic per-tablet manifest and
catalog publication, rowstore-authoritative base-plus-delta overlay, and online point writes/reads on
converting and columnar storage partitions.
Later components described below remain `planned` or `in progress` (explicitly deferred:
reverse `Column -> Row` conversion, delete vectors, physical rowstore reclamation, compaction,
SQL analytical scans, multi-partition routing, MySQL wire protocol/`htapd` daemon,
sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`, and broad MySQL compatibility).
See [`PROGRESS.md`](./PROGRESS.md).

---

## Component diagram

```text
                        +---------------------------+
                        |   client (MySQL wire)     |
                        +-------------+-------------+
                                      |
====================================  |  ==================================
 htapd  --role frontend | backend | both      (single binary, htap-server; LocalServer in progress)
======================================================================
                                      |
                        +-------------v-------------+
                        |  htap-sql                 |  in progress (narrow slice)
                        |  parse (sqlparser, MySQL) |
                        |  bind / catalog resolve   |
                        +-------------+-------------+
                                      |
                        +-------------v-------------+
                        |  query router             |  in progress (rowstore classifier)
                        +------+-------------+------+
                    PK point   |             |  everything else
                    lookup /   |             |
                    short txn  |             |
             +-----------------v--+       +--v---------------------+
             | OLTP fast path     |       | OLAP engine            |
             | index probe -> row |       | (DataFusion, vectorized|
             | fetch, no planner  |       |  plan fragments)       |
             | planned            |       | planned                |
             +---------+----------+       +-----------+------------+
                       |                              |
                       |   (no crate dependency)      |
                       +--------------+---------------+
                                      |
      +-------------------------------v-------------------------------+
      |  htap-txn : one MVCC version domain, one shared WAL  planned  |
      +-------------------------------+-------------------------------+
                                      |
              +-----------------------+-----------------------+
              |                                               |
   +----------v-----------+                       +-----------v----------+
   | htap-rowstore (OLTP) |                       | htap-colstore (OLAP) |
   | WAL, memtable,       |                       | immutable segments,  |
   | SSTs, PK index,      |<---- delta store ---->| column chunks,       |
   | MVCC snapshot engine |      + delete vec     | per-page zone maps   |
   | IMPLEMENTED          |                       | IMPLEMENTED          |
   +----------+-----------+                       +-----------+----------+
              |                                               |
              +-----------------------+-----------------------+
                                      |
      +-------------------------------v-------------------------------+
      | htap-convert : row -> col conversion (R2)  IMPLEMENTED (MVP)   |
      +---------------------------------------------------------------+

   +----------------------+  +----------------------+  +-----------------+
   | htap-catalog         |  | htap-coord   planned |  | htap-movement   |
   | durable local store, |  | local | Raft | ZK    |  | placement,      |
   | topology, recovery   |  | fencing tokens       |  | repair (local)  |
   | in progress          |  |                      |  | IMPLEMENTED     |
   +----------------------+  +----------------------+  +-----------------+

   +---------------------------------------------------------------+
   | htap-common : Version, FencingToken, error types  IMPLEMENTED |
   +---------------------------------------------------------------+
```

---

## Process and role model

**Status: `in progress`** (synchronous `LocalServer` façade implemented for the narrow local slice; `htapd` daemon and MySQL wire protocol are planned).

The system ships as a **single binary, `htapd`** (planned), which can be run as:

- the **frontend role** — SQL surface, catalog, planner, transaction
  coordinator;
- the **backend role** — storage, execution, compaction;
- **both roles in one process**, which is the mode used for single-node
  development and for the README demo.

For the completed Phase 3 narrow local slice, `htap-server` provides `LocalServer`,
a synchronous in-process façade composing the durable catalog (`LocalCatalogStore`),
`htap-txn` transaction manager, and `htap-rowstore` LSM engine. It directly executes
`CREATE TABLE` (deterministic one-partition row topology), literal `INSERT`, PK `DELETE`,
and complete-PK `SELECT` with reopen recovery, without networking or wire protocol overhead.

The frontend/backend boundary is preserved as an internal module boundary,
policed by crate dependencies. Splitting the two into separate processes is
therefore a **deployment choice, not a rewrite**.

---

## Dual-format storage

**Status: `in progress`** (`htap-rowstore`, `htap-colstore`, and local row-to-column conversion via `htap-convert` are `implemented`; reverse conversion, delta compaction, and distributed conversion are `planned`).

| Format | Crate | Structure | Status |
| ------ | ----- | --------- | ------ |
| Row store (OLTP) | `htap-rowstore` | LSM: WAL, memtable, immutable sorted runs (SSTs), primary-key index, MVCC versions. | `implemented` |
| Column store (OLAP) | `htap-colstore` | Immutable segments of encoded (plain/dictionary), compressed (zstd) column blocks with typed zone maps and vectorized scanning. | `implemented` |

The `htap-colstore` implementation delivers the standalone columnar engine MVP:
durable binary segment files, plain and dictionary column encodings, optional zstd compression,
per-block CRC32C integrity checksums, typed zone maps (min, max, and nullability flags),
and vectorized scan execution with conservative pushdown pruning and selective column decoding.
Phase 4 integrates `htap-colstore` segments into partition-scoped conversion (`htap-convert`),
registering columnar segments in durable tablet manifests while keeping the rowstore authoritative
for online point mutations and post-conversion base-plus-delta queries.

Both formats share **one MVCC version domain** (`htap-common::Version`, which
is `implemented`) and **one WAL**, so a single transaction can touch both
formats atomically.

This is the central architectural decision of the project. It is what makes R2
(storage-format conversion) and R5 (mixed OLTP/OLAP workloads) achievable:
without a shared version domain, a format swap would need a distributed
protocol between two independent version spaces, and a transaction spanning
both formats would need two-phase commit against itself. See
[ADR-004](./DECISIONS.md).

---

## Freshness: delta store and merge-on-read

**Status: `in progress`** (authoritative rowstore base-plus-delta overlay implemented for local conversion; delete vectors and background base compaction are planned).

A partition converted to or held in column format still accepts writes. In the Phase 4 local MVP,
those writes land directly in the authoritative **row store**, which acts as the live delta store:

- Historical reads prior to the conversion base version read directly from the rowstore.
- Materialized partition scans (`htap_convert::read_materialized_partition`) scan the columnar base segments up to the conversion snapshot version and overlay rowstore mutations (`Put` and `Delete`) committed after that base version up to the target snapshot.
- Online transactional writes (`INSERT`, `DELETE`) and point reads (`SELECT` by primary key) execute directly against the row store (`Route::RowstoreWrite` and `Route::RowstorePointRead`), ensuring zero read or write interruption during and after conversion.
- Bitmap delete vectors directly on columnar segments, physical rowstore reclamation, and background compaction folding deltas into new columnar segments are explicitly deferred.

---

## Query routing — the R5 guarantee, structurally enforced

**Status: `in progress`** (structural rowstore route classifier implemented for point lookups and DDL/DML; analytical execution and multi-partition routing are planned).

A router inspects the **bound** statement:

- **Point lookups and short transactions that resolve fully against a primary
  key** take a dedicated fast path: index probe → row fetch. There is no
  plan-fragment construction and no vectorized operator pipeline. In the completed
  Phase 3 local slice, `htap_sql::classify_route` inspects the bound AST and catalog
  storage descriptor, routing complete-PK point reads strictly to `Route::RowstorePointLookup`
  and literal mutations to `Route::RowstoreWrite` (verified in `tests/route.rs` / `crates/htap-sql/tests/route.rs`
  and `crates/htap-server/tests/local_server.rs`).
- **Everything else** is destined for the vectorized analytical engine (deferred).

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
  Queries reading full partitions (`read_materialized_partition`) read base rows from columnar segments
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
- **SQL analytical scans:** Vectorized SQL query scans over columnar or converting tables via `LocalServer`
  are deferred (non-point queries return `InvalidArgument` or `Unsupported`).
- **Full partition and table conversion semantics:** Conversions across multi-tablet sharded partitions,
  range/list partition boundaries, and distributed multi-node coordinated cutovers are deferred to Phase 5
  and Phase 6.

---

## Coordination

**Status: `planned`** (the `htap-coord` crate exists as a skeleton).

A `Coordinator` trait covers leader election, membership, a metadata KV store,
watches, and distributed locks. Three backends are planned:

| Backend | Purpose |
| ------- | ------- |
| Local single-node | Tests and the demo. |
| Embedded Raft (`openraft`) | The distributed default. |
| ZooKeeper | Integration with existing ensembles. |

**Fencing tokens** (`htap-common::FencingToken`, `implemented`) are issued on
leadership acquisition and validated on every metadata mutation. The term
fence and session fence described in [`RESEARCH.md`](./RESEARCH.md) live in the
`Coordinator` trait, so both the Raft and ZooKeeper backends inherit the
guarantee rather than re-implementing it.

---

## Sharding

**Status: `in progress`** (the `htap-catalog` and `htap-movement` crates implement single-node tablet sharding, clone packages, and replica repair; multi-node coordination is planned).

```text
Table
  └── Partition        (range or list, on a partition key)
        └── Tablet     (hash bucketed, on a distribution key)
              └── Replica
```

The **tablet is the unit of placement, replication, movement, and repair**.

The hash bucketing contract is fixed explicitly and versioned: the bucket is
`crc32(concatenated per-column binary encodings) % tablet_count`, where the
modulus is the actual tablet count of that index in that partition. The
per-type byte encoding is a versioned contract rather than an implementation
detail — see finding 11 in [`RESEARCH.md`](./RESEARCH.md).
