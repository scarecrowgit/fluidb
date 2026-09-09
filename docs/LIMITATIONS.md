# Limitations and Known Gaps

This file is the running record of everything specified but not yet complete,
and every deviation from the project brief.

---

## Missing input: ZooKeeper reference source

The project brief specified `examples/zookeeper` containing Apache ZooKeeper
source, to be used both for understanding ZAB, session and ephemeral-node
semantics, watches and the client wire protocol, and for running a real
ZooKeeper ensemble in integration tests. **That directory was not present.**
The provided `examples/` directory contained only `starrocks/`.

### Impact and mitigation

ZooKeeper protocol semantics were derived from the public ZooKeeper 3.9
documentation and the `zookeeper-async` Rust client crate rather than from
source. Integration tests for the ZooKeeper coordination backend run against
the official `zookeeper:3.9` Docker image.

This preserves the more important property: session expiry, ephemeral node
loss and watch re-registration are exercised against a real server rather than
a mock, which is where a mock would most likely be wrong.

### Completion plan

If the source tree is supplied, re-verify the session-expiry and
watch-re-registration code paths against the actual server implementation and
record any divergence here.

Cross-reference: ADR-006 in [`DECISIONS.md`](./DECISIONS.md).

---

## Rowstore external coordination: single-participant contiguous version subset

The transaction manager (`htap-txn`) coordinates two-phase commit across participants, using `RowstoreParticipant` to wrap an LSM `Engine`. The rowstore external coordination behavior supports restart-safe visibility (where applied-but-unpublished transactions stay completely hidden across reopen until explicitly published) and idempotent external re-apply during recovery.

### Exact limitation: dense contiguous versions

Rowstore MVCC visibility uses a single scalar watermark (`visible_version`) and enforces sequential monotonic progression (`version == visible_version.next()`). Full sparse global version support across multiple partitions/participants with version gaps (where transactions touch only a subset of participants, leaving non-contiguous version sequences on any single participant) is deferred:

- **Single scalar watermark:** A scalar watermark treats all records with `version <= visible_version` as readable. If a participant were to jump visibility across a sparse gap (e.g. from version 2 to 5) while intermediate transactions were concurrently in flight or applied but unpublished, those intermediate writes would become visible prematurely.
- **Ordered publication invariant:** `Engine::publish` enforces `version == visible_version.next()` to prevent out-of-order publication races across concurrent publishers.
- **Supported subset:** The implemented correct subset supports all single-participant contiguous transaction flows (such as single-rowstore coordinator transactions with monotonically consecutive versions), crash recovery replay, idempotent duplicate apply, and restart-safe visibility isolation via the durable `VISIBLE` watermark file. Multi-participant sparse global version jumps require either an active-transaction tracking bitmap/list or hybrid logical clocks.

---

## Durability testing is bounded by process-level fault injection

The write-ahead log and LSM engine in `htap-rowstore` are covered by integration
tests (`crates/htap-rowstore/tests/wal_crash.rs`, test
`kill_9_loses_no_committed_data`, and `crates/htap-rowstore/tests/engine_crash.rs`,
test `engine_kill_9_recovers_all_reported_commits`) that spawn a real child process,
let it durably commit transactions and report their ids (including periodic SST flushes),
then terminate it with `SIGKILL` and assert that every reported commit is recovered upon
reopening.

### What this proves, and what it does not

The test proves **replay integrity across abrupt process death**. It does not
prove **fsync durability**. This was verified by mutation testing:

| Mutation | Result |
| -------- | ------ |
| Drop the `write_all` in `append()` | Test FAILS (correctly detects data loss) |
| Stub `sync()` to a no-op | Test still PASSES (does not detect the bug) |

The reason is that `SIGKILL` destroys the process but not the operating system
page cache. Bytes written with `write_all` but never fsynced remain readable by
a subsequent reader on the same machine. Only a machine-level failure — power
loss, kernel panic, or a simulated block-device failure — distinguishes the two
cases.

### Completion plan

To close this gap, either (a) run the crash child inside a VM or container
whose storage is dropped without flushing, (b) interpose a FUSE or
device-mapper layer that discards non-fsynced writes on fault injection, or
(c) use a filesystem fault-injection tool such as `dm-flakey` in the chaos
suite planned for Phase 7.

Until one of these is in place, the fsync path is verified by code inspection
only.

---

## Column-store MVP scope and deferred features

The Phase 2 `htap-colstore` implementation delivers the core columnar segment storage format, encoding, compression, zone-map pushdown filtering, and vectorized scans. The following column-store features are deferred to subsequent phases:

- **Delta and delete vectors:** Columnar segments are currently immutable write-once files. Row-level deletions via bitmap delete vectors and merge-on-read delta store integration are deferred to Phase 4.
- **MVCC visibility:** Column segments currently store durable committed rows without transactional version ranges (`htap-common::Version`) per row. Snapshot isolation visibility across columnar data is deferred to transaction coordinator (`htap-txn`) and conversion integration.
- **Conversion and catalog integration:** Standalone segment read/write and scan execution are functional, but online row-to-column transcoding and tablet catalog metadata registration are deferred to Phase 4 (`htap-convert`) and Phase 5 (`htap-catalog`).
- **Richer predicates, joins, and aggregations:** The scan engine supports basic SQL 3-valued comparison predicates (`Eq`, `Lt`, `Lte`, `Gt`, `Gte`, `IsNull`, `IsNotNull`) on single columns with conservative zone-map skipping. Compound predicate expression trees (AND/OR), hash joins, group-by, and vectorized aggregations are deferred to Phase 3 (`htap-sql`).
- **Arrow and DataFusion integration:** Scans yield internal typed `RecordBatch` and `ColumnVector` structures. Exporting to Apache Arrow RecordBatches and DataFusion `TableProvider` / `ExecutionPlan` integration are deferred to the analytical query execution layer in Phase 3.
- **Atomic publication and manifest integration:** Segments are managed individually at filesystem paths. Multi-segment manifest commits, atomic segment swaps, and compaction lifecycle tracking are deferred to storage conversion and tablet management.

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
| Phase 1 — Row store | `Complete` | fsync durability unverified — see above: SIGKILL tests prove restart/replay integrity across abrupt process death, not physical power-loss durability. |
| Phase 2 — Columnar store | `Complete` | Standalone columnar segments, zone-map pruning, and vectorized scans implemented. Deferred to later phases: delta/delete vectors, MVCC visibility, conversion/catalog integration, richer predicates/joins/aggregates, Arrow/DataFusion, and atomic publication/manifest integration. |
| Phase 3 — SQL layer | `Not started` | — |
| Phase 4 — HTAP conversion | `Not started` | — |
| Phase 5 — Data movement | `Not started` | — |
| Phase 6 — Distribution and coordination | `Not started` | — |
| Phase 7 — Hardening, benchmarks, chaos | `Not started` | — |

---

## Deviations from the brief

| Brief requirement | Deviation | Rationale | Where recorded |
| ----------------- | --------- | --------- | -------------- |
| ZooKeeper reference source at `examples/zookeeper` (§3 of the brief) | Input absent; ZooKeeper semantics derived from the ZooKeeper 3.9 specification and the `zookeeper-async` crate, and validated against a real ensemble in Docker rather than a mock. | The source was not supplied. A mock would most likely be wrong precisely on session expiry and ephemeral-node loss, which is the behaviour the coordination layer depends on. | ADR-006 in [`DECISIONS.md`](./DECISIONS.md); "Missing input: ZooKeeper reference source" above. |

> **This table must remain exhaustive.** Anything omitted or changed relative
> to the brief is recorded here or in an ADR, never silently dropped.
