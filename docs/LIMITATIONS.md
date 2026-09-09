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
