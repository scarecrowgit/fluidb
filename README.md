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

    // 5. Analytical SELECT (Narrow OLAP Scan)
    let olap_res = client.execute(
        "SELECT count(*), max(age) FROM users WHERE age >= 25;"
    )?;
    if let StatementResult::Query(qr) = olap_res {
        println!("OLAP aggregate count: {}", qr.num_rows());
    }

    // 6. Complete-PK DELETE (Point DML)
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
| `crates/htap-sql` | SQL front-end using `sqlparser` (MySQL dialect), strict catalog schema binder (typed `PointSelect` and `AnalyticSelect`), and structural query router (`Route::RowstorePointRead`, `Route::OlapScan`). |
| `crates/htap-server` | Durable synchronous in-process engine façade (`LocalServer`) integrating catalog, rowstore, transactions, data movement, and narrow analytical scan execution (`<root>/colstore`). |
| `crates/htap-client` | Synchronous in-process embedded client (`EmbeddedClient`) providing an ergonomic SQL execution interface over `LocalServer`. |
| `crates/htap-bench` | Criterion microbenchmark suite (`benches/local_mvp.rs`) measuring rowstore point lookups, columnar zone-map scans, conversion, CSV import, and coordination. |

---

## Supported SQL Subset & Partition Execution

The SQL engine and embedded client execute an explicit, synchronous subset of SQL across unpartitioned and partitioned tables:

- **`CREATE TABLE`:** Defines table schema with typed columns (`BIGINT`, `INT`, `VARCHAR`, etc.) and a primary key constraint. Tables created via SQL DDL remain unpartitioned with a default single-partition / single-tablet row topology (`partitions.len() == 1`, `tablets.len() == 1`). MySQL `PARTITION BY RANGE/LIST` syntax is rejected at the parser level; partitioned tables are created via the native admin API.
- **Literal `INSERT`:** Single- or multi-row insert statements with literal value lists:
  ```sql
  INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30), (2, 'Bob', 25);
  ```
  On partitioned tables, each row is routed to its target partition by partition key, and all mutations across partitions are committed in a single atomic transaction payload and single version step.
- **Complete-PK `DELETE`:** Point delete specifying equality predicates for the complete primary key in the `WHERE` clause:
  ```sql
  DELETE FROM users WHERE id = 1;
  ```
  On partitioned tables, routed by the partition key position within the primary key.
- **Complete-PK `SELECT`:** Point lookup projecting specific columns or `*` specifying equality predicates for the complete primary key in the `WHERE` clause:
  ```sql
  SELECT name, age FROM users WHERE id = 2;
  ```
  Strictly routes to `Route::RowstorePointRead`, routes by partition key position to the target partition, and preserves the `Engine::get` fast path, bypassing the analytical engine and converter.
- **Analytical `SELECT` (OLAP Scans):** Narrow analytical queries over single unaliased tables, routing to `Route::OlapScan` and evaluated across logical rowstore or base-plus-delta columnar partitions (using server-root `<root>/colstore` for materialized `Column`/`Converting` partitions) at the current visible snapshot:
  - Projections: plain column lists or `*` (with optional aliases).
  - Filters: AND-only typed comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`) with SQL three-valued logic.
  - Aggregates: `COUNT(*)`, `COUNT(column)`, `SUM(column)` (for `Int32`, `Int64`, `Float64`), `MIN(column)`, `MAX(column)`. Empty global aggregates return 1 row with `COUNT = 0` and other aggregates `NULL`.
  - Grouping: deterministic `GROUP BY` with SQL `NULL` grouping semantics.
  - Multi-partition scanning: For partitioned tables, analytical queries scan all partitions at a single visible snapshot and combine results globally or per group. Partition pruning, parallel multi-core execution, and global cross-partition ordering are not claimed or implemented.
  - Base scan pushdown optimization: For materialized `Column` and `Converting` partitions, `LocalServer` executes projection-aware compact reads unioning primary key and requested columns, safely pushing down at most one eligible predicate leaf (`=`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`) directly into `SegmentReader::scan`. Stale base rows are suppressed via newest post-base rowstore deltas, mutations (`Put`/`Delete`) are overlaid, and rows are ordered by primary key deterministically before complete residual SQL filter, aggregate, and group evaluation. `ScanStats`/pruning is tracked internally as execution evidence, but SQL evaluation operates on materialized logical rows (vectorized aggregation is not implemented).
  - Point read isolation: Complete-PK `Route::RowstorePointRead` queries remain strictly isolated, separate, and unchanged.

### Native Partitioned Table Execution (Non-SQL Admin API)

Because MySQL partition DDL is not supported by the pinned SQL parser, partitioned table topology is defined and managed via the native `LocalServer` admin API:

- **Topology API:** `LocalServer::create_partitioned_table(definition)` accepts a [`PartitionedTableDefinition`](crates/htap-server/src/lib.rs) with:
  - `PartitionTopology::Range { key_column, partitions }`: ordered non-overlapping half-open intervals `[lower, upper)`.
  - `PartitionTopology::List { key_column, partitions }`: disjoint sets of explicit values.
- **Catalog Validation:**
  - The partition key column must be a non-null column included in the primary key (`primary_key` must contain `key_column`).
  - Range bounds require `lower < upper`, strictly non-overlapping intervals, and typed compatibility with the key column.
  - List partitions require non-empty disjoint value lists without duplicate entries across or within partitions.
  - Duplicate partition names, empty partition lists, type mismatches, and schema/table name collisions are rejected with `HtapError::InvalidArgument` or `HtapError::Conflict`.
- **Local Topology Invariant:**
  - Each partition is initialized with `StorageDescriptor::Row`, exactly one bucket-0 row tablet (`tablets.len() == 1`, `bucket = 0`), and one healthy local leader replica on node 1 (`NodeId(1)`).
  - No hash buckets, sub-partitioning, or physical sharding across distributed nodes is implemented.
- **Multi-Partition DML & Scan Behavior:**
  - Multi-row `INSERT` routes each row by partition key and atomically applies all mutations in one transaction version.
  - Complete-PK `DELETE` and `SELECT` locate the partition key at its index in the primary key tuple, route to the target partition, and execute against that partition's row tablet. Complete-PK lookups bypass OLAP and preserve the rowstore `Engine::get` fast path.
  - Analytic `SELECT` executes across all partitions at the same transaction snapshot, combining rows for global filters, aggregates, and groupings.
- **Single-Partition Restrictions:**
  - Format conversion (`LocalServer::convert_table`) is guarded to single-partition tables and explicitly rejects multi-partition tables (`HtapError::Unsupported`).

### Unsupported & Deferred SQL & Partition Features
Direct `SegmentReader` pushdown optimization is implemented for the compact base path (single leaf pushdown). Simple unqualified source/projected column `ORDER BY` is implemented for `AnalyticSelect` with ASC/DESC and NULLS FIRST/LAST/default policy, global deterministic tie-break. The following features are explicitly deferred:
- Compound `AND` pushdown beyond one leaf, and `!=` pushdown (evaluated as residual SQL filters).
- Vectorized aggregation and vectorized operator execution pipelines.
- Joins and multiple tables in `FROM`, table aliases, CTEs (`WITH`), window functions (`OVER`), subqueries.
- Query modifiers/clauses: expressions, aliases if rejected, and aggregate ordering in `ORDER BY`; broad MySQL ordering; `LIMIT`/`OFFSET`, `HAVING`.
- Predicate expressions: `OR`, `NOT`, arithmetic, explicit type casts.
- Aggregates: `AVG`, `DISTINCT` aggregates (`COUNT(DISTINCT ...)`).
- Multi-tablet or distributed scans, partition pruning, parallel scan pipelines, resource quotas, disk spilling, query cancellation.
- DataFusion and Apache Arrow integration.
- Full MySQL dialect breadth, sessions, and transaction controls (`BEGIN`, `COMMIT`, `ROLLBACK`).
- Non-PK DML / DDL (`UPDATE`, `ALTER TABLE`, `DROP TABLE`).
- **MySQL Partition DDL & Partition Lifecycle Boundary:**
  - SQL parser rejection: MySQL `CREATE TABLE ... PARTITION BY RANGE ...` and `PARTITION BY LIST ...` return `HtapError::InvalidArgument` from `parse_one` under `sqlparser 0.62` / `MySqlDialect` because the pinned parser does not retain MySQL partition definitions. If partition clauses or `partition_by` AST fields are manually populated, the binder rejects them with `HtapError::Unsupported`; no lossy reinterpretation of unrelated AST nodes is made. Native admin API (`create_partitioned_table`) is the supported mechanism to define partition topology.
  - Partition lifecycle DDL (`ALTER TABLE ... ADD/DROP/REORGANIZE PARTITION`), partition split, merge, and drop remain deferred.
  - Multi-partition format conversion, multi-partition data movement, hash tablets, distributed/remote partition serving across network nodes, replica failover, and network wire protocol remain deferred.
  - Future parser upgrades require explicit mapping of finite typed range/list definitions; `MAXVALUE` and partition options remain unsupported until catalog models change.

### Verification & Test Evidence

Partition metadata and execution are verified by named integration test suites:
- **Server Partition Execution Tests (`crates/htap-server/tests/local_server.rs`):**
  - `test_partitioned_native_range_topology_catalog_reopen_continuation`: verifies range topology creation, CAS persistence, catalog reload, and version continuation across reopen.
  - `test_partitioned_native_list_topology_catalog_reopen_continuation`: verifies list topology creation, catalog reload, and reopen.
  - `test_partitioned_boundary_unmatched_null_type_errors`: verifies rejection of out-of-range keys, unmatched list values, NULL partition keys, and type mismatches.
  - `test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`: verifies multi-row insert routing across partitions in one commit version and point delete.
  - `test_partitioned_composite_pk_partition_key_not_first`: verifies partition key resolution when the partition key is not the first column in a composite PK.
  - `test_partitioned_olap_across_partitions_and_empty_aggregate`: verifies analytical scan across all partitions, aggregate calculations, and empty table handling.
  - `test_convert_table_multi_partition_guard`: verifies that `convert_table` strictly rejects multi-partition tables.
  - `test_partitioned_empty_topology_rejection_no_catalog_mutation`: verifies that empty partition topology definitions are rejected without mutating catalog state.
- **Catalog Recovery & Validation Tests (`crates/htap-catalog/tests/catalog_recovery.rs`):**
  - `test_partitioning_legacy_decode_and_reopen`, `test_range_partitioning_routing_and_boundaries`, `test_list_partitioning_routing`, `test_partitioning_duplicate_violations`, `test_range_overlap_and_order_violations`, `test_partitioning_type_and_null_violations`, `test_partitioning_ownership_and_method_consistency`, `test_partitioning_cas_and_reopen_lifecycle`.
- **SQL Parser Boundary Tests (`crates/htap-sql/tests/parse_bind.rs`):**
  - `test_mysql_partition_ddl_rejected_at_parser_level`, `test_negative_create_table`.

*(Local embedded prototype only; no production claim.)*

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

To run focused integration tests for the primary SQL, server, client, and conversion components:

```bash
# Test SQL parsing, binding, and route classification
cargo test -p htap-sql --test parse_bind
cargo test -p htap-sql --test route

# Test LocalServer engine integration, recovery, and OLAP scans
cargo test -p htap-server --test local_server

# Test EmbeddedClient SQL CRUD and OLAP lifecycle
cargo test -p htap-client --test embedded_client

# Test Row-to-Column conversion and base-plus-delta materialization
cargo test -p htap-convert --test materialization
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
