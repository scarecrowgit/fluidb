# Operational & Persistence Architecture

This document describes the on-disk storage layout, crash-recovery boundaries, and operational characteristics of `LocalServer` and `LocalCoordinator`.

---

## Important Operational Boundaries & Non-Features

The HTAP local MVP is an embedded, single-process, synchronous storage and execution library. It operates strictly within the calling application process.

The following production operational facilities are **not implemented**:

- **No daemon lifecycle or supervisor management:** No `systemd` service units, init scripts, process supervision, background workers, or signal handling (such as `SIGHUP` reload or graceful shutdown signals).
- **No network ports or sockets:** No TCP/IP listeners, Unix domain sockets, or remote procedure calls (RPC).
- **No authentication, TLS, or authorization:** No user credentials, authentication handshakes, TLS encryption certificates, or role-based access control (RBAC).
- **No observability or monitoring infrastructure:** No Prometheus metrics endpoints, OpenTelemetry exporters, health-check probes, or operational telemetry daemons.
- **No container or container orchestration:** No Docker images, Containerfile, Docker Compose setups, or Kubernetes manifests.
- **No High Availability (HA) or multi-node consensus:** No Raft cluster consensus, ZooKeeper cluster integration, or automatic failover.
- **No multi-process ownership:** `LocalServer` and `LocalCoordinator` do not use inter-process file locking (e.g., `flock`). Concurrent access to the same root directory by multiple operating system processes is **unsafe** and strictly unsupported.

---

## LocalServer Filesystem Layout

When a `LocalServer` is opened at a specified `root` directory (`LocalServer::open(root)`), it manages four dedicated sub-paths:

```text
<root>/
├── catalog/
│   ├── CATALOG             # Durable catalog snapshot state
│   └── CATALOG.tmp         # Staging file for atomic replacement
├── rowstore/
│   ├── wal-*.log           # Framed write-ahead log segments (CRC32C protected)
│   ├── sst-*.sst           # Immutable Sorted String Tables (blocks, bloom filter, CRC32C)
│   ├── MANIFEST            # Manifest tracking active SSTs and LSM generations
│   └── VISIBLE             # Monotonically increasing visible version watermark
├── txn.journal             # 2PC transaction manager write-ahead log
└── movement/
    ├── jobs.json           # Durable job tracking file (HTAPJOB1 envelope)
    └── packages/           # Transient staging and tablet package clone archives
```

### Component Details

1. **`catalog/` (`LocalCatalogStore`):**
   - Tracks table definitions, schema, partition descriptors, tablets, and replica topologies.
   - Enforces optimistic concurrency control using integer generations (`compare_and_set`).
   - Mutations stage to `CATALOG.tmp`, call `sync_all()`, and atomically rename to `CATALOG`, followed by a directory `sync_all()`.

2. **`rowstore/` (`htap_rowstore::Engine`):**
   - **`wal-*.log`:** Write-ahead log files recording transactional row mutations (`Put` and `Delete`). Each entry is framed with magic, length, sequence, payload, and CRC32C checksum.
   - **`sst-*.sst`:** Immutable SST files containing ordered key-value pairs organized into indexed blocks with Bloom filters.
   - **`MANIFEST`:** Atomic LSM metadata recording active SST sets and compaction generations.
   - **`VISIBLE`:** Tracks the monotonically advanced `visible_version`. Records applied but uncommitted/unpublished remain invisible across crashes until published.

3. **`txn.journal` (`htap_txn::TransactionManager`):**
   - 2-Phase Commit (2PC) coordination journal tracking transaction lifecycle: `Prepare`, `Commit`, `Abort`.
   - On server startup (`open`), the transaction manager recovers the journal and instructs participants (such as `RowstoreParticipant`) to replay or finalize uncommitted/committed state.

4. **`movement/` (`htap_movement::LocalDataMover`):**
   - Tracks data movement jobs (CSV/JSONL import/export, tablet snapshot migrations).
   - Job metadata is stored under `jobs.json` protected by the `HTAPJOB1` envelope.
   - Snapshot clone packages (`HTAPMNF1`) stage logical partitions into directory packages containing manifest checksums.

---

## LocalCoordinator Filesystem Layout

The `LocalCoordinator` manages cluster membership, scoped leadership leases, and monotonic fencing tokens independently under its own root directory:

```text
<coord_root>/
├── COORDINATOR             # Durable coordinator state envelope (HTAPCRD1)
└── COORDINATOR.tmp         # Staging file for atomic publish
```

### Binary Envelope Specification (`HTAPCRD1`)

Coordinator state is stored in a versioned binary envelope:
- **Header Magic (8 bytes):** `b"HTAPCRD1"`
- **Format Version (2 bytes):** `0x0001` (big-endian `u16`)
- **Payload Length (4 bytes):** Big-endian `u32` (limited to 64 MiB)
- **Checksum (4 bytes):** CRC32C over the payload bytes
- **Payload:** JSON/Bincode serialized state tracking:
  - Registered cluster nodes (`BTreeSet<NodeId>`).
  - Active leadership leases (`scope -> (holder, FencingToken)`).
  - High-water mark issued fencing tokens per scope and globally.

All state transitions follow a two-phase atomic write protocol:
`COORDINATOR.tmp` write -> `sync_all()` -> `rename` over `COORDINATOR` -> parent directory `sync_all()`.

---

## Backup & Recovery Boundaries

### Safe Local Backup Boundaries

Because `LocalServer` writes across multiple internal components (`catalog`, `rowstore`, and `txn.journal`):

1. **Quiescent / Offline Backup (Recommended):**
   - Completely shut down or drop the `LocalServer` instance within the host application.
   - Once all locks are released and file handles closed, take a filesystem-level copy (e.g., `tar`, `cp -a`, or filesystem snapshot) of the entire root directory.

2. **Online / Running Backup:**
   - **Do not** perform arbitrary file-by-file copies of an active server root. Doing so risks capturing torn states between the `txn.journal` and rowstore `wal-*.log` / `MANIFEST`.
   - If online backup is necessary, rely on filesystem-level point-in-time snapshots (e.g., ZFS snapshots or LVM snapshots) that provide crash-consistent atomic snapshots across the storage volume.

### Reopen Recovery Guarantees

When reopening an existing directory via `LocalServer::open(path)`:
1. **Catalog Recovery:** Loads `CATALOG`, validates snapshot structure, and initializes optimistic concurrency control at the recorded generation. If a `.tmp` file is present from an interrupted write, it is discarded or ignored in favor of the durable `CATALOG`.
2. **Rowstore LSM Recovery:** Replays WAL records up to the last clean boundary, repairs torn tails caused by abrupt power loss, reconstructs the active memtable, and reads the `VISIBLE` version watermark.
3. **Transaction Manager Recovery:** Replays `txn.journal`, matches prepared and committed states, and coordinates with `RowstoreParticipant` to ensure only committed transactions are exposed.
4. **Fencing Token Monotonicity:** On reopening `LocalCoordinator`, persisted high-water tokens are restored, ensuring subsequent leadership acquisitions yield strictly greater fencing tokens than any token issued prior to restart.
