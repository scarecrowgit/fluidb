# Progress

## Phases

| Phase | Status | Summary |
| ----- | ------ | ------- |
| Phase 0 — Research and workspace bootstrap | `Complete` | StarRocks source research completed and written up in [`RESEARCH.md`](./RESEARCH.md), covering the mechanisms adopted, those deliberately rejected, and the reasoning for each. A ten-crate cargo workspace was bootstrapped with crate boundaries drawn deliberately along the two seams that matter: the frontend/backend split and the row-format/column-format storage split, so that both remain deployment and routing choices rather than rewrites. `htap-common` carries the shared MVCC `Version` domain and the `FencingToken` type, together with the common error enum — these are shared by every other crate and so were built first. `ci.sh` runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace` and `cargo test --workspace`; all four stages are green on a clean rebuild. |
| Phase 1 | `Not started` | — |
| Phase 2 | `Not started` | — |
| Phase 3 | `Not started` | — |
| Phase 4 | `Not started` | — |
| Phase 5 | `Not started` | — |
| Phase 6 | `Not started` | — |
| Phase 7 | `Not started` | — |

---

## Requirement → Evidence map

Each requirement must be backed by a named, runnable test or benchmark. This
table is the contract between the requirements and the test suite.

| Requirement | Proving test / benchmark | Status |
| ----------- | ------------------------ | ------ |
| R1 — Block skipping via zone maps (≥90% skip criterion) | *(to be filled)* | `pending` |
| R2 — Storage-format conversion (row ↔ column) | *(to be filled)* | `pending` |
| R3 — Sharding, placement, replication and repair | *(to be filled)* | `pending` |
| R4 — SQL breadth (CTEs, window functions, subqueries, cost model) | *(to be filled)* | `pending` |
| R5 — Mixed OLTP/OLAP: point lookups bypass the analytical engine | *(to be filled)* | `pending` |
| R6 — Fencing tokens prevent stale-leader split-brain | *(to be filled)* | `pending` |

> **This table must be filled with concrete test names before the project can
> be considered done.** A requirement whose evidence column still reads
> `pending` is an unproven requirement, regardless of whether code exists that
> appears to implement it.
