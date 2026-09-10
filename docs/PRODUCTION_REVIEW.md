# Production Architecture & Hardening Review

**Date:** 2026-09-10  
**Status:** Audit & Hardening Record  
**Target:** Local HTAP Database Engine (`LocalServer` / `LocalCoordinator`)

---

## 1. Current Status: Hardened Local Embedded MVP

**This project is a hardened local embedded MVP, not a production-ready database system.**

The storage, transaction, and coordination components have undergone focused hardening against concurrency hazards, crash recovery boundaries, transaction commit irrevocability, and persistence bounds. However, **production readiness is not claimed**. The system operates strictly as an in-process embedded library without daemon lifecycle management, client networking, distributed consensus, or power-loss fault verification.

---

## 2. Root Ownership Model

### Exclusive Root Ownership (`1083fbd`)

- `LocalServer::open(root)` and `LocalCoordinator::open(root)` canonicalize the target directory path and acquire an OS-level non-blocking exclusive advisory lock (`flock`) on `<root>/LOCK`.
- Any subsequent attempt by another operating system process (or redundant instance within the same process) to open the same root directory—or any symlink alias resolving to it—is immediately rejected with `HtapError::Conflict`.
- Contention errors include diagnostic metadata (lock-holding PID and start timestamp), but the held OS file lock is the authoritative source of ownership.
- **Scope Boundary:** This mechanism provides **one-owner multiprocess-exclusive mode, not concurrent shared-root writers**. Concurrent multiprocess writers or readers against a shared directory root remain strictly unsupported and unsafe. Low-level standalone subsystem instances (`Engine`, `CatalogStore`, `LocalDataMover`) opened directly outside `LocalServer` do not acquire the root lock and must not be used concurrently on shared storage roots.

---

## 3. Completed Hardening Units (Fixed Findings)

The following critical and high-severity architectural issues have been verified and resolved:

### C1 — WAL-GC Transaction Identity Replay (Fixed: `f7a4975`)
- **Verified Code Paths:**
  - `crates/htap-rowstore/src/manifest.rs` (`ManifestCodec`, `ExternalLedgerEntry`)
  - `crates/htap-rowstore/src/engine.rs` (`Engine::open`, `apply_prepared_locked`)
  - `crates/htap-rowstore/tests/external_ledger.rs`
- **Scenario:**
  WAL prefix garbage collection pruned older WAL segments that recorded external transaction identities. Upon restart or retry, the engine could not determine if an external transaction ID had already been applied, risking duplicate execution or conflicting identity reuse.
- **Resolution:**
  Upgraded rowstore `MANIFEST` to v2, introducing a bounded, no-eviction external apply ledger mapping external transaction IDs to applied versions while preserving backward decode compatibility for v1 manifests. The ledger is restored prior to WAL replay, rejects conflicting identities, and ensures exact external replays remain idempotent across WAL GC prefix pruning. Manifest updates publish atomically alongside SST updates before checkpoint advance.

### C2 — Durable Commit Reversibility (Fixed: `88cc314`)
- **Verified Code Paths:**
  - `crates/htap-txn/src/manager.rs` (`TransactionManager::commit`, `recover`)
  - `crates/htap-txn/src/journal.rs` (`Journal::append_commit`)
  - `crates/htap-txn/tests/journal.rs`, `crates/htap-txn/tests/rowstore_adapter.rs`
- **Scenario:**
  A failure occurring after a transaction commit record was durably written and fsynced to `txn.journal` (such as during post-decision participant apply, publication, or secondary sync) could cause the transaction manager to return an error resembling an abort or leave uncommitted state, violating commit durability and irrevocability.
- **Resolution:**
  Transaction decisions are strictly irrevocable once the commit record is synced to `txn.journal`. Abort requests for committed transactions are rejected; journal logs containing commit followed by abort are treated as corruption. Post-decision failures return a structured `DurablePending` outcome rather than rolling back, and recovery deterministically drives durable commits to completion.

### H1 — Manager Decision Serialization (Fixed: `88cc314`)
- **Verified Code Paths:**
  - `crates/htap-txn/src/manager.rs` (`TransactionManager`)
  - `crates/htap-txn/tests/journal.rs`, `crates/htap-txn/tests/rowstore_adapter.rs`
- **Scenario:**
  Concurrent transaction requests could race through prepare, intent logging, version assignment, commit logging, participant application, and publication, creating interleaved states or out-of-order publication.
- **Resolution:**
  Enforced manager-wide decision serialization using a unified lock across prepare, intent, version allocation, commit sync, participant apply, publication, and recovery replay.

### H2 — Engine Post-WAL Failures Surface as DurablePending with Retry/Recovery (Fixed: `c5ee281`)
- **Verified Code Paths:**
  - `crates/htap-rowstore/src/engine.rs` (`Engine::apply_prepared_locked`, `Engine::flush_frozen_memtable`)
  - `crates/htap-rowstore/tests/durable_completion.rs`
  - `crates/htap-txn/tests/rowstore_adapter.rs`
- **Scenario:**
  When `Engine::apply_prepared` succeeded in appending and syncing a WAL commit but failed during subsequent active memtable insertion, automatic SST flush, or visible marker update, the engine surfaced errors that risked discarding committed data or stalling the flush pipeline.
- **Resolution:**
  Once the WAL commit boundary is reached, post-WAL errors return `DurablePending`. The engine retains committed and applied state, retries failed immutable memtable flushes before admitting new active data, preserves SST/manifest/checkpoint ordering, and allows retry or reopen recovery to complete publication exactly once semantically.

### H5/M2 — Owned Persistence Bounds and Internal Path Validation (Fixed: `b7ff200`)
- **Verified Code Paths:**
  - `crates/htap-common/src/fs.rs` (`read_exact_bounded`)
  - `crates/htap-catalog/src/local.rs`, `crates/htap-coord/src/lib.rs`
  - `crates/htap-convert/src/lib.rs`, `crates/htap-movement/src/job.rs`, `crates/htap-movement/src/tablet.rs`
  - `crates/htap-rowstore/src/manifest.rs`, `crates/htap-rowstore/src/engine.rs`
  - `crates/htap-txn/src/journal.rs`
- **Scenario:**
  Persistence envelopes (`CATALOG`, `COORDINATOR`, `jobs.json` (historical pre-hardening finding; current layout uses `movement/jobs/<job-id>/JOB`), tablet manifests, `MANIFEST`, `VISIBLE`, `txn.journal`) used unbounded file reads, exposing the engine to memory exhaustion attacks from maliciously enlarged or corrupted files. Internal movement job IDs, package IDs, and conversion segment paths lacked strict validation.
- **Resolution:**
  Added a shared metadata-bounded exact-file reader (`read_exact_bounded`) enforcing explicit size caps on all owned persistence envelopes before memory allocation, rejecting oversized, truncated, trailing, or growth-raced files. Bounded the transaction journal total size and stream frame validation with fixed probe buffers. Validated internal movement job/package IDs and conversion segment relative paths. (External `CopyOptions` paths remain caller-controlled by design.)

### Exclusive Root Ownership (Fixed: `1083fbd`)
- **Verified Code Paths:**
  - `crates/htap-server/src/lib.rs` (`LocalServer::open`)
  - `crates/htap-coord/src/lib.rs` (`LocalCoordinator::open`)
  - `crates/htap-common/src/lock.rs` (`ProcessLock`)
- **Scenario:**
  Two or more OS processes attempted to open the same database or coordinator storage root simultaneously or via symlink aliases.
- **Resolution:**
  `LocalServer::open` and `LocalCoordinator::open` canonicalize paths and acquire an OS-level non-blocking exclusive advisory lock (`<root>/LOCK`). Contending processes immediately fail with `HtapError::Conflict`. Operates in one-owner multiprocess-exclusive mode (not concurrent shared-root writers).

---

## 4. Remaining Open Issues & Architectural Boundaries

The following limitations and architectural boundaries remain explicitly open:

1. **No Journal/Ledger Compaction or Coordinated Retention; Ledger Hard Cap Blocks New External Applies:**
   Neither the transaction journal (`txn.journal`) nor the rowstore `MANIFEST` v2 external apply ledger implements compaction, pruning, or coordinated retention. The external ledger enforces a hard cap (`MAX_APPLIED_EXTERNAL_TXNS = 1_000_000`). Once this cap is saturated, subsequent new external transaction applies are rejected with `CapacityExceeded`. Coordinated journal and ledger retention tied to participant checkpoints remains future work.

2. **Possible Later Flush-Boundary Duplicate SST Publication After Crash:**
   If a crash occurs immediately after an SST file is published to disk but before reader registration, manifest update, or checkpoint advance, a subsequent reopen/flush cycle may republish duplicate SST data. Full resolution requires a future staged flush recovery mechanism.

3. **No Power-Loss Proof:**
   Integration crash tests verify process `SIGKILL` termination, torn-tail truncation, and log reassembly across process death. They do not prove durability against true physical power outages, operating system kernel panics, or un-flushed disk controller write caches (no `dm-flakey` or FUSE power-cut testing).

4. **No Distributed Consensus, Remote Replica Serving, or Real HA:**
   Coordination is strictly single-node via local filesystem binary envelopes (`HTAPCRD1`). No Raft consensus (`openraft`), ZooKeeper ensemble backend, network session heartbeats, ephemeral watches, remote RPC replica streaming, or active HA failover exists.

5. **Whole-Dataset Materialization in Conversion, Export, and Clone:**
   HTAP row-to-column conversion (`htap-convert`), data export (`htap-movement`, where exports materialize the full logical partition before writing), and tablet snapshot cloning materialize entire datasets in memory or intermediate staging directories rather than utilizing streaming, chunked pipelines.

6. **No Network, MySQL Daemon, Authentication, Security Boundary, or Full SQL Analytics:**
   Interaction is limited to synchronous in-process calls to `LocalServer`. No MySQL wire protocol listener, network server daemon (`htapd`), client authentication, TLS encryption, or role-based access control (RBAC) exists. SQL execution supports a narrow OLTP slice (PK lookups, single-partition literal mutations) and narrow single-table OLAP scans (plain projections, AND-only filters, `COUNT(*)`, `COUNT(col)`, `SUM`, `MIN`, `MAX`, deterministic `GROUP BY`); joins, CTEs, windows, `ORDER BY`, `LIMIT`, `HAVING`, direct `SegmentReader` pushdown, vectorized SQL execution, and full SQL analytics remain unsupported.

7. **External CopyOptions Paths Remain Caller-Controlled by Design:**
   While internal persistence bounds and internal paths (job IDs, package IDs, segment filenames) are strictly validated, external filesystem paths provided in `CopyOptions` for CSV/JSONL import and export are caller-controlled by design.

---

## 5. Prioritized Engineering Roadmap

```
+-----------------------------------------------------------------------------+
| Completed Hardening (Current Local Embedded MVP)                            |
| - Exclusive root process lock (<root>/LOCK) (1083fbd)                       |
| - C1: MANIFEST v2 external ledger for WAL-GC identity replay (f7a4975)      |
| - C2: Irrevocable commit decision + DurablePending (88cc314)                |
| - H1: Manager decision serialization (88cc314)                              |
| - H2: Engine post-WAL failure DurablePending & retry/recovery (c5ee281)     |
| - H5/M2: Owned persistence bounds & internal path validation (b7ff200)      |
+-----------------------------------------------------------------------------+
                                       |
                                       v
+-----------------------------------------------------------------------------+
| P1 — Near-Term Robustness & Lifecycle                                       |
| - Coordinated journal and MANIFEST v2 ledger retention/compaction           |
| - Staged flush recovery to eliminate duplicate SST publication risks        |
| - Standalone subsystem process-lock encapsulation (Engine, Catalog, Mover)  |
| - Mandatory coordinator fencing across all direct CatalogStore mutations    |
+-----------------------------------------------------------------------------+
                                       |
                                       v
+-----------------------------------------------------------------------------+
| P2 — Distribution, Analytics & Full Durability                              |
| - Power-loss chaos validation harness (dm-flakey / FUSE)                    |
| - Distributed consensus backend (Raft / ZooKeeper) and remote replication   |
| - Streaming non-materializing conversion, clone, and export pipelines       |
| - Network server daemon, MySQL wire protocol, and auth security boundary    |
| - Full analytical query execution (vectorized pushdown, joins, CTEs, windows)|
+-----------------------------------------------------------------------------+
```
