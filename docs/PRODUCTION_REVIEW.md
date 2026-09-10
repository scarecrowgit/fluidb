# Production Architecture & Hardening Review

**Date:** 2026-09-10  
**Status:** Approved Architectural Review & Hardening Record  
**Target:** Local HTAP Database Engine (`LocalServer` / `LocalCoordinator`)

---

## 1. Executive Summary & Root Ownership Model

This document records the production review findings (**C1/C2/H1–H5**), verified code paths, failure scenarios, architectural impacts, positive observations, and prioritized engineering roadmap for the HTAP storage engine.

### Strict Process-Exclusive Root Ownership

**Only one owner process may open a server or coordinator root.**

The hardening unit implemented on 2026-09-10 enforces **strict single-process exclusive root ownership**:
- `LocalServer::open(root)` and `LocalCoordinator::open(root)` canonicalize the target directory path and acquire an OS-level non-blocking exclusive advisory lock (`flock`) on `<root>/LOCK`.
- Any subsequent attempt by another operating system process (or redundant instance within the same process) to open the same root directory—or any symlink alias resolving to it—is immediately rejected with `HtapError::Conflict`.
- Contention errors include diagnostic metadata (lock-holding PID and start timestamp), but the held OS file lock is the authoritative source of ownership.
- **Scope Boundary:** This mechanism provides **exclusive process ownership**, *not* concurrent shared-root operation. Concurrent multiprocess writers or readers against a shared directory root remain strictly unsupported and unsafe. Low-level standalone subsystem instances (`Engine`, `CatalogStore`, `LocalDataMover`) opened directly outside `LocalServer` do not acquire the root lock and must not be used concurrently on shared storage roots.

---

## 2. Positive Observations & Architectural Strengths

A thorough audit of the codebase revealed key architectural strengths and resilient implementations:

1. **Strict Memory Safety:** All workspace crates enforce `#![forbid(unsafe_code)]`. Advisory locking via `fs2` is encapsulated entirely within safe Rust abstractions (`ProcessLock`).
2. **Crash-Resilient Atomic Envelope Staging:** Persistent state files (`CATALOG`, `COORDINATOR`, `jobs.json`, tablet columnar manifests) use an atomic two-phase write protocol:
   $$\text{write to } .tmp \longrightarrow \text{fsync file} \longrightarrow \text{atomic rename} \longrightarrow \text{fsync parent directory}$$
3. **Framed Envelopes with CRC32C Integrity:** Binary envelopes (`HTAPCRD1`, `HTAPJOB1`, `HTAPMNF1`) enforce fixed magic headers, version checks, strict payload length bounds (guarding against unbounded memory allocations), and CRC32C payload checksum validation.
4. **WAL Torn-Tail Truncation:** The LSM write-ahead log cleanly identifies and truncates uncommitted torn tails at power-loss boundaries without corrupting previously committed transaction frames.
5. **Two-Phase Commit Recovery:** The transaction manager (`htap-txn`) coordinates multi-participant 2PC with durable intent and commit markers, verifying participant reconciliation and crash recovery replay.
6. **Deterministic Query Pruning:** The columnar scan engine (`htap-colstore`) utilizes typed zone-map metadata (min/max/nullability) to prune non-matching data blocks with $\ge 90\%$ skip efficiency.

---

## 3. Production Review Findings

### C1 — Uncoordinated Concurrent Multi-Process Root Access (Critical)

- **Verified Code Paths:**
  - `crates/htap-server/src/lib.rs` (`LocalServer::open`)
  - `crates/htap-coord/src/lib.rs` (`LocalCoordinator::open`)
  - `crates/htap-common/src/lock.rs` (`ProcessLock::acquire`)
- **Scenario:**
  Two or more operating system processes attempt to open the same database or coordinator storage root simultaneously, or through symlink aliases pointing to the same directory.
- **Failure Mode & Impact:**
  Without exclusive OS-level locking, concurrent processes execute independent WAL appends (`wal-*.log`), generate conflicting SSTable generation IDs in `MANIFEST`, overwrite 2PC journal entries (`txn.journal`), or race on atomic file replacement (`CATALOG`, `COORDINATOR`). This causes silent metadata desynchronization, split-brain corruption, and irrecoverable data loss upon restart.
- **Hardening Delivered (2026-09-10):**
  `LocalServer::open` and `LocalCoordinator::open` now canonicalize `root` and acquire an exclusive non-blocking advisory lock on `<root>/LOCK` before opening any subcomponents. Contending processes fail immediately with `HtapError::Conflict`.
- **Remaining Boundary:**
  Concurrent multiprocess operation on a shared root is **not** supported; exclusive root ownership is enforced.

---

### C2 — Direct Unfenced Catalog Mutations and Coordinator Bypass (Critical)

- **Verified Code Paths:**
  - `crates/htap-catalog/src/store.rs` (`CatalogStore::compare_and_set`)
  - `crates/htap-catalog/src/local.rs` (`LocalCatalogStore::compare_and_set`)
  - `crates/htap-movement/src/tablet.rs` (`repair_replica`)
- **Scenario:**
  Application code or internal migration routines invoke `CatalogStore::compare_and_set` directly rather than routing updates through `Coordinator::fenced_catalog_compare_and_set`.
- **Failure Mode & Impact:**
  Direct invocation bypasses active coordinator leadership and fencing token (`FencingToken`) validation. If a partitioned or demoted leader process executes a direct catalog update, it can overwrite schema descriptors, partition mappings, or tablet replica states, causing split-brain metadata divergence despite active fencing tokens.
- **Current Status:**
  Fencing validation is strictly opt-in. Full mandatory fencing across all catalog mutation points is tracked in the P1 roadmap.

---

### H1 — Rowstore Single-Scalar Visible Watermark & Contiguous Version Constraint (High)

- **Verified Code Paths:**
  - `crates/htap-rowstore/src/engine.rs` (`Engine::publish`, `apply_prepared_locked`)
  - `crates/htap-txn/src/manager.rs` (`TransactionManager::commit`)
- **Scenario:**
  A multi-participant transaction involves only a subset of storage partitions, or an intermediate distributed transaction is aborted after version allocation, creating a gap in monotonically sequential version IDs on a given participant.
- **Failure Mode & Impact:**
  The rowstore LSM engine enforces strict sequential publication: `version == visible_version.next()`. If a version number is skipped due to a multi-partition version gap, `Engine::publish` rejects the call with `HtapError::InvalidArgument`, halting the publication pipeline. Consequently, transactions must strictly proceed as dense contiguous single-participant sequences.
- **Current Status:**
  Documented in `LIMITATIONS.md`. Addressing sparse global version watermarks or bitmap tracking is tracked in the P1 roadmap.

---

### H2 — Standalone Low-Level Subsystem Opens Lack Process Locking (High)

- **Verified Code Paths:**
  - `crates/htap-rowstore/src/engine.rs` (`Engine::open`)
  - `crates/htap-catalog/src/local.rs` (`LocalCatalogStore::open`)
  - `crates/htap-movement/src/lib.rs` (`LocalDataMover::new`)
- **Scenario:**
  Low-level engine, catalog, or data mover instances are initialized directly via their standalone constructors against a path that is currently managed by an active `LocalServer` or another process.
- **Failure Mode & Impact:**
  Standalone constructors do not acquire `<root>/LOCK`. Concurrent access by standalone components bypasses the server-level exclusive lock, leading to file access races, torn WAL writes, and catalog corruption.
- **Current Status:**
  Documented in `OPERATIONS.md`, `README.md`, and `LIMITATIONS.md` as an unsafe operational boundary. Adding protective lock guards to standalone components is tracked in the P1 roadmap.

---

### H3 — Physical Storage Power-Loss Durability Gap (High)

- **Verified Code Paths:**
  - `crates/htap-rowstore/src/wal.rs` (`append_commit`, `sync`)
  - `crates/htap-txn/src/journal.rs` (`append_intent`, `append_commit`)
  - `crates/htap-rowstore/tests/wal_crash.rs`, `crates/htap-rowstore/tests/engine_crash.rs`
- **Scenario:**
  An abrupt hardware power outage, host kernel panic, or block-device power loss occurs while writes are in flight.
- **Failure Mode & Impact:**
  Existing crash tests verify replay integrity against process `SIGKILL` termination. While `SIGKILL` verifies recovery from torn tails and log reassembly across process death, it does not invalidate operating system page caches. Un-flushed disk controller write caches or non-barrier writes can lead to undetected data loss during true physical power cutoffs.
- **Current Status:**
  Fsync ordering is validated by inspection and unit tests. Physical fault injection testing (e.g., via `dm-flakey` or FUSE) is tracked in the P2 roadmap.

---

### H4 — HTAP Conversion Storage Amplification & Missing Delta Compaction (High)

- **Verified Code Paths:**
  - `crates/htap-convert/src/lib.rs` (`LocalConverter::convert_partition`, `read_materialized_partition`)
  - `crates/htap-server/src/lib.rs` (`execute_insert`, `execute_delete`)
- **Scenario:**
  A table undergoes row-to-column conversion, followed by prolonged operational point inserts and deletes.
- **Failure Mode & Impact:**
  Converted rows are never physically reclaimed or purged from rowstore SSTables and WAL files. All subsequent point writes continue accumulating in the rowstore without an automatic background compaction task to fold rowstore deltas back into columnar segments. Over time, this leads to significant storage amplification and degraded base-plus-delta scan latency.
- **Current Status:**
  Documented in `LIMITATIONS.md`. Background delta compaction and physical rowstore space reclamation are tracked in the P2 roadmap.

---

### H5 — Coordination Scope Bounded to Single-Node Local Envelope (High)

- **Verified Code Paths:**
  - `crates/htap-coord/src/lib.rs` (`LocalCoordinator`)
  - `crates/htap-coord/src/placement.rs` (`plan_placement`)
- **Scenario:**
  Multi-node cluster failover, dynamic cluster topology changes, or node membership churn across network partitions.
- **Failure Mode & Impact:**
  `LocalCoordinator` persists leadership leases and fencing tokens strictly to a local filesystem envelope (`HTAPCRD1`). It provides no distributed consensus protocol (Raft/ZAB), network session heartbeats, or ephemeral watch semantics. Multi-node coordination cannot execute across machines without external consensus.
- **Current Status:**
  Documented in `LIMITATIONS.md` and ADR-006. Distributed consensus integration is tracked in the P2 roadmap.

---

## 4. Prioritized Engineering Roadmap

```
+-----------------------------------------------------------------------------+
| P0 — Immediate Hardening (Delivered 2026-09-10)                             |
| - Safe Unix/Linux advisory file lock on <root>/LOCK (fs2)                   |
| - Path canonicalization resolving symlink aliases                           |
| - Clear HtapError::Conflict on contention with diagnostic PID/timestamp     |
| - Subprocess contention tests for LocalServer and LocalCoordinator          |
+-----------------------------------------------------------------------------+
                                       |
                                       v
+-----------------------------------------------------------------------------+
| P1 — Near-Term Architectural Hardening                                      |
| - Enforce coordinator fencing across all catalog mutation points (C2)       |
| - Multi-participant sparse version tracking in rowstore engine (H1)         |
| - Component-level lock encapsulation for standalone Engine/Catalog/Mover(H2)|
+-----------------------------------------------------------------------------+
                                       |
                                       v
+-----------------------------------------------------------------------------+
| P2 — Long-Term Production Readiness                                         |
| - Power-loss chaos validation harness (dm-flakey / FUSE) (H3)               |
| - Background delta compaction & rowstore space reclamation (H4)             |
| - Distributed consensus backend (Raft / ZooKeeper) & cluster heartbeats(H5) |
+-----------------------------------------------------------------------------+
```

### P0 (Completed — 2026-09-10)
- Enforce single-process exclusive root ownership in `LocalServer::open` and `LocalCoordinator::open`.
- Reject cross-process contention with `HtapError::Conflict` and informative diagnostic metadata.
- Validate subprocess contention and symlink resolution in integration test suites.
- Establish verified production review and operational boundaries documentation.

### P1 (Near-Term Hardening)
- **Mandatory Catalog Fencing (C2):** Ensure all `CatalogStore` mutation paths mandate a valid `FencingToken` verified by the coordinator.
- **Sparse Version Progression (H1):** Implement active transaction tracking or multi-watermark bitmaps in `htap-rowstore` to accommodate distributed version gaps.
- **Standalone Component Locking (H2):** Add advisory root locks to `Engine::open`, `LocalCatalogStore::open`, and `LocalDataMover::new` to prevent direct uncoordinated access.

### P2 (Long-Term Readiness)
- **Storage Chaos Testing (H3):** Construct automated CI harnesses simulating kernel crashes and power loss using Linux `dm-flakey`.
- **Conversion Delta Compaction (H4):** Implement background delta-to-base compaction and physical rowstore space reclamation for converted partitions.
- **Distributed Coordination (H5):** Implement a distributed `Coordinator` backend backed by Raft or Apache ZooKeeper with ephemeral heartbeats and watches.
