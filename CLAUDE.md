# fluidb — HTAP database engine (local embedded MVP)

Single-node, in-process Rust HTAP engine: LSM rowstore + columnar segments, snapshot isolation MVCC,
2PC transactions, row→column conversion, data movement, fenced local coordination, and a narrow SQL
layer (sqlparser, MySQL dialect). Workspace: `crates/htap-*` (12 crates) + `vendor/sqlparser`.

Read before non-trivial work: `docs/ARCHITECTURE.md` (component statuses), `docs/DECISIONS.md` (ADRs),
`docs/LIMITATIONS.md` (deferred scope), `docs/PROGRESS.md` (requirement → test evidence map).

## Commands

- Full gate (must pass before a change is done): `./ci.sh` — fmt check, clippy `-D warnings`, build, test, bench compile.
- Fast loop: `cargo test -p htap-<crate>`, `cargo test -p htap-<crate> --test <file>`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`.
- Benchmarks: `cargo bench -p htap-bench` (see `docs/BENCHMARKS.md`; record results with its template).
- Toolchain is pinned to Rust 1.95.0 (`rust-toolchain.toml`).

## Hard rules

- **Scope.** Do not implement anything listed as `planned`/`deferred` in ARCHITECTURE/LIMITATIONS (daemon,
  MySQL wire, Raft/ZooKeeper, DataFusion, joins/ORDER/LIMIT, UPDATE, sessions, compaction, …) unless the user
  explicitly asks. Status vocabulary: `implemented`, `implemented (local MVP)`, `in progress`, `planned`, `deferred`.
- **No copied code.** `examples/starrocks` (git-ignored) is a read-only design reference. Never copy, translate,
  or closely paraphrase its source (see `ATTRIBUTION.md`). Re-derive designs and cite them in `docs/RESEARCH.md`.
- **`vendor/sqlparser` is a vendored fork.** Change it only when unavoidable, and say so in the commit message.
- **No `unsafe`.** The workspace currently has none. Any new `unsafe` needs a validator `approve` and a `// SAFETY:` comment.
- **Durability invariants** (see ADR-004, ADR-008, ADR-009):
  - One MVCC version domain and one WAL shared by both storage formats; versions strictly monotonic.
  - Durable writes go temp file → `sync_all` → `rename` → `sync_dir`, using the crate's existing
    `atomic_publish` / `write_atomic` / `sync_dir` helpers; never write a published file in place.
  - Every on-disk envelope has a magic (`HTAPSST1`, `HTAPMAN1`, `HTAPCAT1`, `HTAPCOL1`, `HTAPVIS1`, `HTAPTBM1`,
    `HTAPJOB1`, `HTAPCRD1`, `HTAPMNF1`) plus CRC32C. A layout change bumps the format version, keeps or explicitly
    rejects old versions, and adds a recovery test.
  - Rowstore is authoritative during conversion (base-plus-delta overlay); `DurablePending` must never be reported as committed.
  - R5: complete-PK point reads route to `RowstorePointRead` and never touch the analytical path.
  - Catalog mutations go through CAS; coordinator-fenced paths must check `FencingToken`.
- **Evidence.** A requirement or feature claim in docs must name a runnable test or benchmark (`docs/PROGRESS.md`).
- **Commits:** conventional style with crate scope, e.g. `feat(sql): …`, `fix(rowstore): …`, `docs(olap): …`.

## Roles (`.claude/agents/`)

| Role | Model | Use it for |
|---|---|---|
| `researcher` | sonnet | Turn a problem into a precise, file-level implementation plan. Read-only. |
| `validator` | opus | Cold gate: `approve` / `reject` / `edit` / `flag` on a plan or a diff checkpoint. |
| `implementer` | haiku wrapper → 9router `cx/gpt-5.6-terra` (escalates to `cx/gpt-5.6-sol`; reviewed by `cx/gpt-5.6-luna-review`) | Execute one approved, well-specified task: the 9router model writes the code, the wrapper applies it and verifies with cargo. |
| `storage-reviewer` | opus | Deep review of durability, recovery, MVCC, 2PC, fencing, and on-disk format changes. |
| `docs-keeper` | sonnet | Bring README/ARCHITECTURE/PROGRESS/LIMITATIONS/ADRs in line with the code after a change. |

## Workflow

**Trivial changes** (typos, comments, formatting, test-only additions, docs wording) need no gates.

**Non-trivial changes** (anything under `crates/*/src`, `vendor/`, `Cargo.toml`, `ci.sh`, or an on-disk format)
follow these steps. The validator gates are mandatory, not judgment calls:

1. `researcher` → plan (tasks, files, risks, tests, doc updates).
2. `validator` with `CHECKPOINT: plan` → do not start implementing without `approve`.
3. Implement: `implementer` only (code authored by 9router `cx/gpt-5.6-terra` via MCP), one task at a time; fast loop per crate.
4. If the diff touches rowstore/txn/catalog/convert/movement/coord persistence, recovery, MVCC, or envelopes → `storage-reviewer`.
5. `docs-keeper` if behavior, status, scope, or evidence changed.
6. Ext review of the whole diff: `mcp__9router__ask` with `model: "cx/gpt-5.6-sol-review"`, diff + touched files attached. Confirm each finding in the code; send confirmed ones back through `implementer`.
7. `./ci.sh` green, then `validator` with `CHECKPOINT: diff` → do not report done or commit without `approve`.

Checkpoint format sent to `validator` (compact; the validator starts cold):

```
CHECKPOINT: plan | diff
TASK: <one line>
RECOMMENDATION: <proceed|ship> because <one line>
DETAIL:
- files: <paths>
- risks / open questions: <...>
- verification: <exact command → quoted result>   (diff only)
- storage-reviewer: <result or "n/a">              (diff only)
- ext-review: <N findings: fixed / dismissed+why>  (diff only)
```

## External models (9router MCP)

- `mcp__9router__ask` / `mcp__9router__panel` are available in every role that lists them. Pass files by path.
- Prefer `cx/*` models (funded). Lineup: `reasoner` (= `cx/gpt-6-astra`) for design/trade-offs; `cx/gpt-5.6-sol` for
  strong general work, long reads and escalated coding; `cx/gpt-5.6-terra` for coding; `cx/gpt-5.5` as a third design voice;
  `cx/gpt-5.6-luna` (fastest) for quick/bulk work: summarizing files or StarRocks sources, triaging long test/clippy output,
  checking Mermaid or format details. Reviews use one model per layer: `cx/gpt-5.6-luna-review` per implementer task,
  `cx/gpt-5.6-sol-review` on the whole diff, `cx/gpt-5.6-terra-review` + `reasoner` in storage-reviewer.
  Don't stack extra reviewers beyond that. `gemini` only as a fallback when input exceeds cx context.
- `cx/gpt-5.6-terra` (called by raw id; the `coder` alias is a different model) is the code author behind `implementer`; Claude roles don't write implementation code.
  Enforced by hooks in `.claude/settings.json` (`.claude/hooks/9router_guard.py`): edits to `crates/`, `vendor/`, `Cargo.toml`,
  `ci.sh`, `*.rs` are denied unless the text came from a 9router answer the same agent received and its added lines were not
  already in that agent's prompt (no dictating code to the author, in briefs either: describe behavior, don't write the lines);
  start Claude with `FLUIDB_CLAUDE_EDITS=1` to bypass.
- `architect` (heavy tier, rarely) is for ADR-level decisions that are costly to reverse. `heavy` (rarely) is for
  corruption or concurrency questions where other models disagree.
- External answers are input, not truth: confirm every claim against the code before acting on it.
