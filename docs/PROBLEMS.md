# Code Health Problems and Refactor Plan

Structural problems found in a code-health review on 2026-09-19, and when each one gets fixed. The review covered
about 108k lines of `crates/*/src`, with Phase 13 (SQL breadth) still uncommitted in the working tree. The figures
below are a snapshot from that date and are not kept up to date.

Verdict: **no rewrite is needed.** The crate boundaries (frontend/backend, rowstore/colstore) and the durability
design (ADR-004, ADR-008, ADR-009) hold up. The problems are duplicated low-level code and too many parallel
paths through the query layer. Both grow with every phase, so they are fixed in a planned order (see
[Schedule](#schedule)) rather than all at once.

## P1 — Duplicated durability primitives (addressed: stage R)

**Status: addressed.** The same crash-safety building blocks used to be written separately in each storage
crate:

| Primitive | Copies |
|---|---|
| `sync_dir` | Six copies, not five as originally counted: `htap-catalog/src/local.rs`, `htap-coord/src/lib.rs`, `htap-movement/src/job.rs` (reused by `htap-movement/src/tablet.rs`), `htap-convert/src/lib.rs`, `htap-rowstore/src/manifest.rs` (reused by `htap-rowstore/src/engine.rs`) — all five byte-identical and `cfg(unix)`-gated — plus a sixth, non-identical copy, `htap-rowstore/src/wal.rs::fsync_dir`, which is unconditional (not `cfg(unix)`-gated). |
| atomic publish (temp → `sync_all` → `rename` → `sync_dir`) | `htap-catalog/src/local.rs` (`atomic_publish`), `htap-coord/src/lib.rs` (`atomic_publish`), `htap-convert/src/lib.rs` (`write_atomic`, `write_atomic_to`), `htap-rowstore/src/manifest.rs` (`atomic_publish`) |
| envelope framing (magic + format version + CRC32C) | about 10 files: rowstore `wal.rs`, `sst.rs`, `manifest.rs`; colstore `segment.rs`; catalog `local.rs`; txn `journal.rs`; movement `job.rs`, `tablet.rs`; coord `lib.rs`; convert `lib.rs` |
| little-endian byte decoding with manual bounds checks | each of the files above, e.g. `htap-colstore/src/encoding.rs` and `htap-rowstore/src/sst.rs` |

**Why it mattered.** Every copy had to be right for the CLAUDE.md durability invariants to hold. Phase 15 (DROP
reclaim, rowstore compaction/GC, journal checkpoint — now done, see the Schedule below) added three new durable
files/format bumps (`HTAPMAN1` v3, `HTAPCAT1` v5, `HTAPTXC1`) built on exactly the Stage R helpers this section
describes, confirming the investment; Phase 16 (multiprocess ownership) will add more. The power-loss safety
phase would have had to audit every copy.

**Fix (shipped, stage R).** Added one shared module to `htap-common` (`fs.rs`, `envelope.rs`, `bytecursor.rs`)
providing:
- directory sync (`sync_dir`) and atomic publish of a file (`write_new_tmp_file`, `remove_file_if_exists`,
  `fsync_file`, `atomic_publish`);
- envelope encode/decode (`encode_envelope`/`decode_envelope`, magic + version + CRC32C, explicit rejection of
  unknown versions via `EnvelopeError::UnsupportedVersion`) plus a bare length+CRC frame (`encode_bare_frame`)
  for the WAL and the txn journal;
- a checked little-endian byte reader (`ByteReader`) that returns a structured error instead of slicing with
  `unwrap()`.

Migrated all seven crates (`htap-catalog`, `htap-coord`, `htap-movement`, `htap-convert`, `htap-rowstore`,
`htap-txn`, `htap-colstore`) onto it with **zero on-disk byte changes and zero pre-existing test edits**: every
existing recovery/crash test still passes unchanged, plus a golden-bytes test per envelope and per-fault-class
error-text tests. `htap-rowstore/src/wal.rs::fsync_dir` — the sixth, non-`cfg(unix)`-gated `sync_dir` copy found
during this stage — was deliberately **not** migrated: unlike the five identical copies, moving it in either
direction would change untested non-Unix behavior, so it was left in place with a comment explaining why. One
message text legitimately changed as part of this stage: `Manifest::read_from_file`'s untested trailing-probe
error text, now produced by the shared `read_file_exact_bounded` and pinned by a new test. See
[`docs/PROGRESS.md`](./PROGRESS.md) (Stage R row), the storage-format compatibility table in
[`docs/ARCHITECTURE.md`](./ARCHITECTURE.md#dual-format-storage), and [`docs/DECISIONS.md`](./DECISIONS.md) for
the full record.

## P2 — Parallel paths through the query layer (partially fixed: Phase 14, Option B)

**Status: partially fixed.** The coordinator chose Option B (keep two binder entry points; see ADR-023 decision
10/"P2 binder convergence" for the full reasoning against ADR-017's structural-R5 argument) over unifying the
binders. What Phase 14 actually converged, and what is still open:

- **Two binders — still open, by deliberate choice, not an oversight:**
  - `htap-sql/src/binder.rs` binds point reads and narrow scans (`PointSelect`, `AnalyticSelect`);
  - `htap-sql/src/binder_query.rs` binds general queries (`BoundQuery`).
  - Each still maintains its own copy of the leaf-level sub-problems both solve (literal binding, cast-target
    mapping, type-compatibility/comparability checks, scalar-function signature checking, schema column
    lookup). The planned shared `bind_helpers`-shaped module for these was **not delivered** in this phase's
    diff — this is the one part of the original Phase 14 plan for P2 that did not land; it remains open for a
    future phase. Kept apart deliberately: `is_narrow_select_shape` still runs before any general-binder code
    at all, so R5 stays a compile-time/structural property (this route variant never calls that function)
    rather than a runtime one — unifying the two entry points would have restored R5 only via the kind of
    runtime check ADR-017 already rejected.
- **Several executors — fixed:** the flat join loop inside `run_select` and the recursive tree evaluator have
  been merged into one. `SelectBody.join_tree` is now always populated at bind time (`left_deep_join_tree`
  synthesizes a tree from a flat `Vec<JoinSpec>` when needed); the old `SelectBody.joins`/`tree_only` fields and
  the separate flat-loop branch are gone. `evaluate_join_tree` is the only join execution path.
- **Hand-built evaluation context — fixed:** every hand-built `EvalContext { .. }` literal (the counted 15 sites
  across `query_exec.rs`, `lib.rs`, and `session.rs`) now goes through one constructor
  (`EvalServices`/`EvalContext::new`, exposed as `htap_sql::eval_context!`). `rg "EvalContext \{" crates/htap-server/src`
  returns no hand-built literals outside `htap-sql/src/expr.rs` itself.
- **Long parameter lists — fixed in the files this phase touched:** the
  `#[allow(clippy::too_many_arguments)]` count in `crates/htap-server/src/query_exec.rs` dropped to zero via
  context structs (following the pre-existing `JoinRowsInput` precedent). `binder_query.rs` (3 allows) and
  `htap-server/src/lib.rs` (1, on the pre-existing `scan_partition_compact`) were left alone — Phase 14 scoped
  Track 0 to `query_exec.rs` and to binder *leaf helpers*, not the binder's top-level clause-driving functions;
  `htap-coord`/`htap-convert`'s allows (2 each) remain P4 ("fix when touched"), unrelated to Phase 14.

**Evidence.** `cargo test -p htap-sql -p htap-server` and `cargo clippy --workspace --all-targets -- -D
warnings` pass with the join-evaluator and `EvalContext` unifications in place; see `docs/PROGRESS.md`'s
Phase 14 row and ADR-023 for the full record, including exactly what was not delivered.

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
| done | Phase 13 — SQL breadth | — |
| **done** | **Stage R — shared durability primitives** | P1 |
| **done** | **Phase 14 — CBO, spill, parallelism** | P2 (partial — see above; leaf-helper convergence still open), P4 where touched |
| **done** | **Phase 15 — DROP reclaim, rowstore compaction/GC, journal checkpoint** | P4 where touched (uses R's helpers): `htap-catalog/src/model.rs::validate` (already on the P4 list, 702 lines) gained the `pending_reclaim` overlap/duplicate checks and was not split — still open, unchanged severity. No other P4-listed function was touched (`htap-convert`, `htap-sql`'s `bind_create_table`/`bind_select_body` were not part of this diff). |
| **next** | Phase 16 — multiprocess owner + IPC | P3, P4 where touched (uses R's helpers) |
| then | Phases 17–18 — TPC-H, TPC-C | — |
| then | SERIALIZABLE isolation (serializable snapshot isolation) | — |
| then | Power-loss safety | audits R's single implementation |
| then | Docker | — |

Stage R followed the normal non-trivial workflow: researcher plan, validator plan gate, implementer, **storage-reviewer**,
docs-keeper, ext review, `./ci.sh`, validator diff gate.
