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
| Phase 1 — Row store | `Not started` | — |
| Phase 2 — Columnar store | `Not started` | — |
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
