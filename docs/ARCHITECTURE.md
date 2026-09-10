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
Later components described below remain `planned` or `in progress` (explicitly deferred:
MySQL protocol/`htapd` daemon, sessions/`BEGIN`/`COMMIT`/`ROLLBACK`, `UPDATE`/`ALTER`/`DROP`,
scans/aggregates/joins/CTEs/windows/subqueries, columnstore SQL execution, multi-partition routing,
and broad MySQL compatibility).
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
      | htap-convert : row <-> column format conversion (R2)  planned |
      +---------------------------------------------------------------+

   +----------------------+  +----------------------+  +-----------------+
   | htap-catalog         |  | htap-coord   planned |  | htap-movement   |
   | durable local store, |  | local | Raft | ZK    |  | placement,      |
   | topology, recovery   |  | fencing tokens       |  | repair  planned |
   | in progress          |  |                      |  |                 |
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

**Status: `in progress`** (`htap-rowstore` and `htap-colstore` are `implemented`; integration, delta store, and conversion are `planned`).

| Format | Crate | Structure | Status |
| ------ | ----- | --------- | ------ |
| Row store (OLTP) | `htap-rowstore` | LSM: WAL, memtable, immutable sorted runs (SSTs), primary-key index, MVCC versions. | `implemented` |
| Column store (OLAP) | `htap-colstore` | Immutable segments of encoded (plain/dictionary), compressed (zstd) column blocks with typed zone maps and vectorized scanning. | `implemented` |

The `htap-colstore` implementation delivers the standalone columnar engine MVP:
durable binary segment files, plain and dictionary column encodings, optional zstd compression,
per-block CRC32C integrity checksums, typed zone maps (min, max, and nullability flags),
and vectorized scan execution with conservative pushdown pruning and selective column decoding.
At this stage, `htap-colstore` does not integrate with the SQL query engine, transactional MVCC
version visibility, online conversion state machines, or rowstore query routing.

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

**Status: `planned`.**

A partition held in column format still accepts writes. Those writes land in a
**row-format delta** plus a **delete vector** over the columnar base.

- Reads merge the base (minus its delete vector) with the delta.
- Background compaction folds the delta into a new base.

This keeps freshly written rows visible to analytical queries without imposing
a per-row merge cost on scans: the base is subtracted by a single bitmap
ANDNOT, and only the (small) delta is merged.

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

**Status: `planned`** (the `htap-convert` crate exists as a skeleton).

Conversion is a **partition-scoped state machine**:

1. Snapshot the partition at version `V`.
2. Transcode rows to columnar segments.
3. Apply the delta accumulated since `V`.
4. Atomically swap the partition's storage descriptor in a metadata
   transaction.
5. Garbage-collect the old data after a grace period.

Properties:

- The state is **persisted at each transition**, so conversion is
  crash-resumable rather than restart-from-scratch.
- The swap is a **single metadata record**, so a reader observes either the old
  format or the new one, never a mix.
- The **same machinery runs in reverse** for column → row conversion.

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

**Status: `planned`** (the `htap-catalog` and `htap-movement` crates exist as
skeletons).

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
