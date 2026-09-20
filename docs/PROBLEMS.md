# Code Health Problems and Refactor Plan

Structural problems found in a code-health review on 2026-09-19, and when each one gets fixed. The review covered
about 108k lines of `crates/*/src`, with Phase 13 (SQL breadth) still uncommitted in the working tree. The figures
below are a snapshot from that date and are not kept up to date.

Verdict: **no rewrite is needed.** The crate boundaries (frontend/backend, rowstore/colstore) and the durability
design (ADR-004, ADR-008, ADR-009) hold up. The problems are duplicated low-level code and too many parallel
paths through the query layer. Both grow with every phase, so they are fixed in a planned order (see
[Schedule](#schedule)) rather than all at once.

## P1 — Duplicated durability primitives (fix next: stage R)

The same crash-safety building blocks are written separately in each storage crate:

| Primitive | Copies |
|---|---|
| `sync_dir` | `htap-catalog/src/local.rs`, `htap-coord/src/lib.rs`, `htap-movement/src/job.rs`, `htap-convert/src/lib.rs`, `htap-rowstore/src/manifest.rs` |
| atomic publish (temp → `sync_all` → `rename` → `sync_dir`) | `htap-catalog/src/local.rs` (`atomic_publish`), `htap-coord/src/lib.rs` (`atomic_publish`), `htap-convert/src/lib.rs` (`write_atomic`, `write_atomic_to`), `htap-rowstore/src/manifest.rs` (`atomic_publish`) |
| envelope framing (magic + format version + CRC32C) | about 10 files: rowstore `wal.rs`, `sst.rs`, `manifest.rs`; colstore `segment.rs`; catalog `local.rs`; txn `journal.rs`; movement `job.rs`, `tablet.rs`; coord `lib.rs`; convert `lib.rs` |
| little-endian byte decoding with manual bounds checks | each of the files above, e.g. `htap-colstore/src/encoding.rs` and `htap-rowstore/src/sst.rs` |

**Why it matters.** Every copy has to be right for the CLAUDE.md durability invariants to hold. Phase 15 (DROP
reclaim, journal compaction) and Phase 16 (multiprocess ownership) add more durable files. The power-loss safety
phase would have to audit every copy.

**Fix.** Add one shared module to `htap-common` providing:
- directory sync;
- atomic publish of a file;
- envelope encode/decode (magic, version, CRC32C, with explicit rejection of unknown versions);
- a checked byte reader that returns `HtapError::Corruption` instead of slicing with `unwrap()`.

Migrate every crate to it without changing behaviour. On-disk formats must stay **byte-identical**: no magic or
version changes, existing recovery tests unchanged and passing, plus a golden-bytes test per envelope.

## P2 — Parallel paths through the query layer (fix in Phase 14)

- **Two binders:**
  - `htap-sql/src/binder.rs` (3,206 lines) binds point reads and narrow scans (`PointSelect`, `AnalyticSelect`);
  - `htap-sql/src/binder_query.rs` (3,712 lines) binds general queries (`BoundQuery`).
- **Several executors:**
  - the analytic fast path;
  - the general executor in `htap-server/src/query_exec.rs`, with its flat join evaluator;
  - the recursive join-tree evaluator.
- **Hand-built evaluation context:** `EvalContext` is built by hand at 15 sites across `query_exec.rs`, `lib.rs`
  and `session.rs`. A missing field at one site caused a wrong-result bug in Phase 13's correlated subqueries. The
  context now carries a variable lookup and a subquery runner; a third callback should trigger consolidation into
  one execution-services struct.
- **Long parameter lists:** 11 `#[allow(clippy::too_many_arguments)]` in `crates/*/src`.

**Fix.** The Phase 14 cost-based optimizer needs one logical/physical planner anyway. Converge the binders into
one bound representation. Route every query shape through one planner, keeping R5: complete-PK point reads still
route to `RowstorePointRead`. Build `EvalContext` through one constructor. Replace argument lists with context
structs.

## P3 — Two statement pipelines in the server (fix in Phase 16)

`LocalServer::execute` / `dispatch_bound` (`htap-server/src/lib.rs`) and `Session::execute` /
`execute_statement` (`htap-server/src/session.rs`) each parse, check visibility, bind, check privileges and
dispatch. Phase 16 IPC forwarding would add a third entry point.

**Fix.** Build one statement pipeline that all three entry points call, as part of Phase 16.

## P4 — Very long functions (fix when touched)

| Function | File | Lines |
|---|---|---|
| `validate` | `htap-catalog/src/model.rs` | 702 |
| `apply_partition_alteration` | `htap-catalog/src/model.rs` | 524 |
| `convert_partition` | `htap-convert/src/lib.rs` | 460 |
| `open` | `htap-colstore/src/segment.rs` | 431 |
| `bind_create_table` | `htap-sql/src/binder.rs` | 360 |
| `bind_select_body` | `htap-sql/src/binder_query.rs` | 341 |

`htap-convert` is a single 2,777-line `lib.rs`.

**Fix.** Split each function, or split `htap-convert` into modules, whenever a phase already modifies it. No
dedicated stage.

## Checked and not a problem

- **About 170 `unwrap`/`expect`/`panic!` outside test modules.** The storage-decoder instances sampled are guarded
  by explicit bounds checks, and on-disk payloads are CRC-verified before decoding. P1's checked byte reader removes
  most of them anyway.
- **TODO/FIXME markers:** one in the whole workspace.
- **`unsafe`:** none.

## Schedule

| Order | Stage | Problems addressed |
|---|---|---|
| now | Phase 13 — SQL breadth | — |
| **next** | **Stage R — shared durability primitives** | P1 |
| then | Phase 14 — CBO, spill, parallelism | P2, P4 where touched |
| then | Phase 15 — DROP reclaim, journal compaction | P4 where touched (uses R's helpers) |
| then | Phase 16 — multiprocess owner + IPC | P3, P4 where touched (uses R's helpers) |
| then | Phases 17–18 — TPC-H, TPC-C | — |
| then | Power-loss safety | audits R's single implementation |
| then | Docker | — |

Stage R follows the normal non-trivial workflow: researcher plan, validator plan gate, implementer, **storage-reviewer**,
docs-keeper, ext review, `./ci.sh`, validator diff gate.
