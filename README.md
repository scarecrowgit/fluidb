# HTAP Database Engine (Local MVP)

A local, single-node Hybrid Transactional/Analytical Processing (HTAP) database engine written in Rust.

This repository implements a modular local MVP covering LSM row storage, columnar segment storage with zone-map block pruning, transactional snapshot isolation, row-to-column storage conversion, single-node data movement, fenced coordination, and an in-process SQL execution engine and embedded client.

---

## Toolchain Requirement

- **Rust Version:** `1.95.0` (as specified in `rust-version = "1.95"` in `Cargo.toml`).

---

## Important Scope Exclusions & Architectural Boundaries

This project is delivered as a verified in-process local library and test suite. The following components are **deliberately not implemented** and are out of scope for this local MVP:

- **No `htapd` or application daemon:** There is no background server daemon, process supervisor, or service entrypoint. The engine runs strictly in-process via `LocalServer` or `EmbeddedClient`.
- **No MySQL wire server or client compatibility:** There is no MySQL binary wire protocol listener, packet framing, handshake protocol, authentication layer, or compatibility with standard MySQL client libraries.
- **No Docker image or Docker Compose deployment:** No `Dockerfile`, `docker-compose.yml`, or container images are provided or required. All execution is local and filesystem-based.
- **No network endpoint:** There are no network listeners, TCP/IP sockets, Unix domain sockets, or HTTP/gRPC endpoints.
- **No interactive sessions or session state:** Each statement executes independently without connection-level state, session variables, or transaction handles.
- **No prepared statements:** Statements are parsed, validated, and planned synchronously on every execution call without prepared statement handles or binary parameter binding.
- **No TPC-C or TPC-H compliance:** The system does not implement the TPC-C or TPC-H benchmark specifications, relational transaction models, or analytical query profiles. Microbenchmarks evaluate isolated internal subsystem performance only.
- **Exclusive Process Ownership (No Concurrent Multiprocess Operation):** `LocalServer` and `LocalCoordinator` enforce exclusive ownership of their root directory using an OS-level advisory lock (`<root>/LOCK` via `flock`). Concurrent access or duplicate opens by multiple processes against the same root directory (or its symlink aliases) are strictly rejected with `HtapError::Conflict`. This is single-process exclusive ownership, not concurrent shared-root operation; concurrent multiprocess writers are not supported. Low-level standalone subsystem instances (`htap_rowstore::Engine::open`, `htap_catalog::LocalCatalogStore::open`, `htap_movement::LocalDataMover::new`) do not acquire this lock and remain unsafe for concurrent shared-root use.

---

## Embedded Client API & Usage

The `htap-client` crate provides [`EmbeddedClient`], an ergonomic synchronous in-process façade over `LocalServer`:

- **`EmbeddedClient::open(root)`:** Opens or recovers the local database rooted at `root`, acquiring `<root>/LOCK`, loading catalog metadata, recovering committed rowstore transactions, and initializing data movement.
- **`client.execute(sql)`:** Synchronously executes a single SQL statement against the embedded engine, returning a [`StatementResult`].
- **Re-exported Result Types:**
  - [`StatementResult`]: `StatementResult::Command(CommandResult)` or `StatementResult::Query(QueryResult)`.
  - [`CommandResult`]: `CommandResult::Ddl { affected }` for schema changes, or `CommandResult::Dml { affected, version }` with the optional committed MVCC [`Version`] (`Option<Version>`).
  - [`QueryResult`]: Tabular point lookup result providing column definitions via `qr.columns()` and data rows via `qr.rows()`, `qr.num_rows()`, and `qr.is_empty()`.

### Rust Usage Example

The following example demonstrates the complete embedded workflow without any external daemon or network dependency:

```rust
use htap_client::{CommandResult, EmbeddedClient, QueryResult, StatementResult};
use htap_common::Result;

fn main() -> Result<()> {
    // 1. Open or recover the embedded engine at a local filesystem directory
    let client = EmbeddedClient::open("/tmp/htap_demo")?;

    // 2. CREATE TABLE (DDL)
    let ddl_res = client.execute(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);"
    )?;
    assert_eq!(ddl_res, StatementResult::Command(CommandResult::Ddl { affected: 1 }));

    // 3. Literal INSERT (DML) - single or multi-row
    let insert_res = client.execute(
        "INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30), (2, 'Bob', 25);"
    )?;
    match insert_res {
        StatementResult::Command(CommandResult::Dml { affected, version }) => {
            println!("Inserted {affected} rows at MVCC version {version:?}");
        }
        _ => unreachable!(),
    }

    // 4. Complete-PK SELECT (Point Query)
    let select_res = client.execute(
        "SELECT name, age FROM users WHERE id = 1;"
    )?;
    if let StatementResult::Query(qr) = select_res {
        println!("Found {} row(s):", qr.num_rows());
        for row in qr.rows() {
            println!("  name = {:?}, age = {:?}", row.get(0), row.get(1));
        }
    }

    // 5. Complete-PK DELETE (Point DML)
    let delete_res = client.execute(
        "DELETE FROM users WHERE id = 1;"
    )?;
    if let StatementResult::Command(cmd) = delete_res {
        println!("Deleted {} row(s) at version {:?}", cmd.affected(), cmd.version());
    }

    Ok(())
}
```

---

## Workspace Architecture

The workspace consists of modular crates separated by architectural boundaries:

| Crate | Role & Status |
| ----- | ------------- |
| `crates/htap-common` | Common types (`Value`, `Row`, `Schema`, `Mutation`), MVCC `Version`, `FencingToken`, key encoding/decoding, and error models. |
| `crates/htap-rowstore` | LSM-tree rowstore engine with WAL, memtable, immutable SSTables, block index, Bloom filter, CRC checks, and snapshot isolation. |
| `crates/htap-colstore` | Columnar storage engine with binary segment layout, dictionary/plain encoding, optional zstd compression, and zone-map block pruning. |
| `crates/htap-catalog` | Durable local catalog store (`LocalCatalogStore`) persisting versioned schema, table, partition, and tablet topology snapshots. |
| `crates/htap-txn` | Two-phase commit transaction manager (`TransactionManager`) with write-ahead journal (`txn.journal`) and rowstore participant. |
| `crates/htap-convert` | Partition-scoped row-to-column conversion engine (`LocalConverter`, `HTAPTBM1` manifest envelope, and base-plus-delta overlay). |
| `crates/htap-movement` | Single-node data movement engine (`LocalDataMover`, `HTAPJOB1` job log, CSV/JSONLines streaming, and tablet snapshot cloning/repair). |
| `crates/htap-coord` | Local coordination and placement engine (`LocalCoordinator`, `HTAPCRD1` state envelope, monotonic fencing tokens, and deterministic placement planner). |
| `crates/htap-sql` | SQL front-end using `sqlparser` (MySQL dialect), strict catalog schema binder, and structural rowstore point query router. |
| `crates/htap-server` | Durable synchronous in-process engine façade (`LocalServer`) integrating catalog, rowstore, transactions, and data movement. |
| `crates/htap-client` | Synchronous in-process embedded client (`EmbeddedClient`) providing an ergonomic SQL execution interface over `LocalServer`. |
| `crates/htap-bench` | Criterion microbenchmark suite (`benches/local_mvp.rs`) measuring rowstore point lookups, columnar zone-map scans, conversion, CSV import, and coordination. |

---

## Supported SQL Subset

The SQL engine and embedded client execute an explicit, synchronous single-partition subset of SQL:

- **`CREATE TABLE`:** Defines table schema with typed columns (`BIGINT`, `INT`, `VARCHAR`, etc.) and a primary key constraint.
- **Literal `INSERT`:** Single- or multi-row insert statements with literal value lists:
  ```sql
  INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30), (2, 'Bob', 25);
  ```
- **Complete-PK `DELETE`:** Point delete specifying equality predicates for the complete primary key in the `WHERE` clause:
  ```sql
  DELETE FROM users WHERE id = 1;
  ```
- **Complete-PK `SELECT`:** Point lookup projecting specific columns or `*` specifying equality predicates for the complete primary key in the `WHERE` clause:
  ```sql
  SELECT name, age FROM users WHERE id = 2;
  ```

### Unsupported SQL Features
Transactions (`BEGIN`, `COMMIT`, `ROLLBACK`), `UPDATE`, `ALTER TABLE`, `DROP TABLE`, non-PK filters, table scans, aggregations (`COUNT`, `SUM`, `GROUP BY`), joins, CTEs (`WITH`), window functions (`OVER`), subqueries, and prepared statements are rejected with explicit errors (`HtapError::Unsupported` or `HtapError::InvalidArgument`).

---

## Validation & CI Commands

All checks can be verified using the standard workflow tools:

```bash
# Code formatting check
cargo fmt --all -- --check

# Linter check (fails on any warning)
cargo clippy --workspace --all-targets -- -D warnings

# Build all workspace crates
cargo build --workspace

# Run complete workspace unit and integration test suite
cargo test --workspace

# Verify benchmark compilation without executing timings
cargo bench --workspace --no-run

# Run full CI script
./ci.sh
```

---

## Focused Component Tests

To run focused integration tests for the primary engine façade and client:

```bash
# Test LocalServer engine integration and recovery
cargo test -p htap-server --test local_server

# Test EmbeddedClient SQL CRUD lifecycle, recovery, and error mapping
cargo test -p htap-client --test embedded_client
```

---

## Benchmarks

The benchmark suite in `crates/htap-bench` contains isolated microbenchmarks for core engine components:

```bash
# List available benchmark targets
cargo bench -p htap-bench --bench local_mvp -- --list

# Compile benchmarks without running
cargo bench --workspace --no-run

# Execute all local microbenchmarks
cargo bench -p htap-bench --bench local_mvp
```

For detailed benchmark specifications, correctness preconditions, and reproducibility recording, see [`docs/BENCHMARKS.md`](./docs/BENCHMARKS.md).
For directory layout and operational persistence details, see [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).
