# HTAP Database Engine (Local MVP)

A local, single-node Hybrid Transactional/Analytical Processing (HTAP) database engine written in Rust.

This repository implements a modular local MVP covering LSM row storage, columnar segment storage with zone-map block pruning, transactional snapshot isolation, row-to-column storage conversion, single-node data movement, fenced coordination, and an in-process SQL execution engine and embedded client.

---

## Toolchain Requirement

- **Rust Version:** `1.95.0` (as specified in `rust-version = "1.95"` in `Cargo.toml`).

---

## Important Scope Exclusions & Architectural Boundaries

This project is delivered as a verified local library, a network daemon, and a test suite. The following
components are **deliberately not implemented** and are out of scope for this local MVP:

- **No MySQL binary/prepared-statement protocol:** `htapd`/`htap-wire` implement the MySQL *text* protocol
  only (`COM_QUERY`, `COM_PING`, `COM_INIT_DB`, `COM_QUIT`). Prepared statements and the binary protocol
  (`COM_STMT_PREPARE`/`COM_STMT_EXECUTE`), `COM_RESET_CONNECTION`, and `COM_CHANGE_USER` are answered with an
  error rather than implemented.
- **No TLS, compression, or per-user ACL:** The wire server has no TLS, no protocol compression, and a
  single shared password with no per-user accounts or RBAC; see "Network server (`htapd`)" below.
- **No Docker image or Docker Compose deployment:** No `Dockerfile`, `docker-compose.yml`, or container images are provided or required.
- **No interactive sessions or session state:** Each statement executes independently without connection-level state, session variables, or explicit transaction handles (`BEGIN`/`COMMIT`/`ROLLBACK`), whether reached in-process via `EmbeddedClient` or over the network via `RemoteClient`.
- **No TPC-C or TPC-H compliance:** The system does not implement the TPC-C or TPC-H benchmark specifications, relational transaction models, or analytical query profiles. Microbenchmarks evaluate isolated internal subsystem performance only.
- **No window functions, correlated subqueries, or cost-based optimization:** The general query executor
  (see "Supported SQL Subset" below) handles joins, expressions, subqueries, and set operations over
  materialized logical rows held in memory, with no spilling, no cost-based planner, and no worker-pool
  parallelism above the per-partition scan. `OVER`, correlated subqueries, `FULL OUTER`/`NATURAL`/`USING`
  joins, recursive CTEs, and `EXCEPT`/`INTERSECT` are not implemented.
- **No physical reclamation on `DROP TABLE`:** Dropping a table removes it from the catalog in one CAS;
  the rowstore data and columnar segments of its tablets stay on disk, unreachable (dropped identifiers are
  never reissued, so they can never be aliased by a new table, but disk space is not freed).
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

### SQL breadth: joins across storage engines, `UPDATE`, `DROP TABLE`, `SHOW`

`execute(sql)` also drives the general query executor (joins, expressions, aggregates, subqueries,
`UNION`), `UPDATE`, `DROP TABLE`, and `SHOW`/`DESCRIBE` — over `EmbeddedClient`, `RemoteClient`, or
`LocalServer` directly. Joining a converted (`Column`) table against a plain `Row` table works because
every base table side is read through the same storage path at one MVCC snapshot per statement. This
example uses `htap_server::LocalServer` directly to also call `convert_table_to_column`, which
`EmbeddedClient` does not expose:

```rust
use htap_common::Result;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

fn main() -> Result<()> {
    let server = LocalServer::open("/tmp/htap_demo2")?;
    server.execute("CREATE TABLE customers (id BIGINT PRIMARY KEY, name VARCHAR);")?;
    server.execute("CREATE TABLE orders (order_id BIGINT PRIMARY KEY, customer_id BIGINT, amount DOUBLE);")?;
    server.execute("INSERT INTO customers (id, name) VALUES (1, 'Alice'), (2, 'Bob');")?;
    server.execute("INSERT INTO orders (order_id, customer_id, amount) VALUES (10, 1, 20.0), (11, 1, 5.0);")?;

    // Convert `customers` to columnar storage; `orders` stays a row table.
    server.convert_table_to_column("customers")?;

    // A join across a Column table and a Row table, one snapshot for both sides.
    let join_res = server.execute(
        "SELECT c.name, COUNT(o.order_id) AS n, COALESCE(SUM(o.amount), 0) AS total \
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
         GROUP BY c.name ORDER BY total DESC LIMIT 10;",
    )?;
    if let StatementResult::Query(qr) = join_res {
        for row in qr.rows {
            println!("{row:?}");
        }
    }

    // UPDATE (point form, complete-PK WHERE; a filtered WHERE scans all partitions instead).
    server.execute("UPDATE orders SET amount = amount * 1.1 WHERE order_id = 10;")?;

    // SHOW / DESCRIBE, answered from the catalog only.
    server.execute("SHOW TABLES;")?;
    server.execute("DESCRIBE customers;")?;

    // DROP TABLE is metadata-only: catalog CAS, no physical reclamation of dropped data.
    server.execute("DROP TABLE orders;")?;

    Ok(())
}
```

Deferred on this path: window functions, correlated subqueries, recursive CTEs, `EXCEPT`/`INTERSECT`,
`INSERT ... SELECT`, `UPDATE` with joins/subqueries, filtered `DELETE`, and cost-based optimization — see
"Unsupported & Deferred SQL & Partition Features" below.

---

## Network server (`htapd`)

`htapd` exposes a `LocalServer` root over the MySQL text protocol, so the same engine `EmbeddedClient` drives
in-process can also be reached over TCP, from any MySQL client or from `htap-client::RemoteClient`.

```bash
# Build and run the daemon (default bind: 127.0.0.1:3307, loopback only)
cargo run -p htapd -- --root /tmp/htap_demo

# Optional flags
cargo run -p htapd -- --root /tmp/htap_demo --listen 127.0.0.1:3307 --max-connections 64 --password secret
```

The password may also come from the `HTAPD_PASSWORD` environment variable; `--password` wins if both are
set, and with neither set no password is required.

Connect with any MySQL client, e.g. the `mysql` CLI (if installed):

```bash
mysql -h 127.0.0.1 -P 3307 -u root
```

Or from Rust, with `htap_client::RemoteClient` (same result shape as `EmbeddedClient`):

```rust
use htap_client::RemoteClient;

let mut client = RemoteClient::connect("127.0.0.1:3307", Some("secret"))?;
let result = client.execute("SELECT name, age FROM users WHERE id = 1;")?;
```

**Security caveat:** there is no TLS. The password exchange (`mysql_native_password`) is hashed, but query
text and result rows travel in cleartext, so binding a non-loopback address requires a trusted network or an
SSH tunnel. See the "Network layer" and "Security model" sections of
[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md), ADR-016, and [`docs/OPERATIONS.md`](./docs/OPERATIONS.md)
for the full protocol scope and operational lifecycle.

---

## Workspace Architecture

The workspace consists of 14 modular crates (plus the vendored `vendor/sqlparser`) separated by architectural boundaries:

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
| `crates/htap-sql` | SQL front-end using `sqlparser` (MySQL dialect): strict narrow catalog binder (typed `PointSelect`/`AnalyticSelect`), a general query binder (`query`/`expr`/`binder_query`: joins, expressions, subqueries, `UPDATE`/`DROP TABLE`/`SHOW`), and structural query router (`Route::RowstorePointRead`, `Route::OlapScan`, `Route::Query`, `Route::RowstoreUpdate`, `Route::CatalogRead`). |
| `crates/htap-server` | Durable synchronous in-process engine façade (`LocalServer`) integrating catalog, rowstore, transactions, data movement, narrow analytical scan execution (`<root>/colstore`), and the general query executor (`query_exec`: joins, expressions, subqueries, `UNION`, `UPDATE`, `DROP TABLE`, `SHOW`). |
| `crates/htap-client` | Synchronous in-process embedded client (`EmbeddedClient`) and network client (`RemoteClient`) providing an ergonomic SQL execution interface over `LocalServer`, in-process or over TCP. |
| `crates/htap-wire` | Hand-written, synchronous MySQL text-protocol server (`WireServer`) exposing `LocalServer` over TCP, and the `WireClient` used by `RemoteClient`. |
| `crates/htapd` | Network daemon binary: opens a `LocalServer` root and serves it via `htap-wire::WireServer`. |
| `crates/htap-bench` | Criterion microbenchmark suite (`benches/local_mvp.rs`) measuring rowstore point lookups, columnar zone-map scans, conversion, CSV import, and coordination. |

---

## Supported SQL Subset & Partition Execution

The SQL engine and embedded client execute an explicit, synchronous subset of SQL across unpartitioned and partitioned tables:

- **`CREATE TABLE`:** Defines table schema with typed columns (`BIGINT`, `INT`, `VARCHAR`, etc.) and a primary key constraint. Tables created without partitioning clauses receive a default single-partition / single-tablet row topology (`partitions.len() == 1`, `tablets.len() == 1`, default partition `"p0"`). MySQL `PARTITION BY RANGE [COLUMNS] (...)` and `PARTITION BY LIST [COLUMNS] (...)` (including `VALUES LESS THAN MAXVALUE` on the final partition) are supported via SQL DDL as well as via the native admin API.
- **`ALTER TABLE` (Partition Lifecycle):** Supports typed MySQL partition lifecycle DDL:
  - `ALTER TABLE t ADD PARTITION (PARTITION p VALUES LESS THAN (v|MAXVALUE))` or `VALUES IN (v1, ...)`.
  - `ALTER TABLE t DROP PARTITION p[, ...]`.
  - `ALTER TABLE t REORGANIZE PARTITION p[, ...] INTO (PARTITION ... definitions...)`.
  - **Empty-Source Safety:** `DROP PARTITION` and `REORGANIZE PARTITION` inspect visible rows in the rowstore snapshot before catalog mutation; populated source partitions are strictly rejected with `HtapError::InvalidArgument` to prevent data loss.
  - **Native LocalServer API:** `LocalServer::alter_partitions` remains available with candidate catalog validation, atomic catalog CAS, empty-source safety, and checked ID allocation without ID burn on validation errors.
  - **Strict Rejections:** Partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), subpartitioning, hash/key, expressions in bounds, `IF [NOT] EXISTS`, and non-partition ALTER operations (`ADD COLUMN`, `RENAME TABLE`, etc.) are rejected.
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
  - Multi-partition scanning: For partitioned tables, analytical queries scan partitions at a single visible snapshot and combine results globally or per group. Conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented as narrow local features; distributed fanout, disk spilling, query cancellation, and resource quotas remain deferred.
  - Base scan pushdown optimization: For materialized `Column` and `Converting` partitions, `LocalServer` executes projection-aware compact reads unioning primary key and requested columns, safely pushing down at most one eligible predicate leaf (`=`, `<`, `<=`, `>`, `>=`, `IS NULL`, `IS NOT NULL`) directly into `SegmentReader::scan`. Stale base rows are suppressed via newest post-base rowstore deltas, mutations (`Put`/`Delete`) are overlaid, and rows are ordered by primary key deterministically before complete residual SQL filter, aggregate, and group evaluation. `ScanStats`/pruning is tracked internally as execution evidence, but SQL evaluation operates on materialized logical rows (vectorized aggregation is not implemented).
  - Point read isolation: Complete-PK `Route::RowstorePointRead` queries remain strictly isolated, separate, and unchanged.
- **General `SELECT` (joins, expressions, subqueries, `UNION`; `Route::Query`):** Any `SELECT` that does not
  fit the narrow shape above (a join, an alias, a `LIMIT`/`HAVING`/`DISTINCT`, a subquery, an arithmetic
  projection, etc.) binds through the general query binder and executes via `htap-server::query_exec`:
  - Joins: `INNER`/`LEFT`/`RIGHT`/`CROSS` (left-deep chains, comma joins), table aliases, qualified names, `*`/`t.*`.
  - Expressions: arithmetic (`+ - * / %`, `/` always widens to `Float64`, checked overflow), comparisons
    (incl. column-vs-column), `AND`/`OR`/`NOT`, `IS [NOT] NULL`/`TRUE`/`FALSE`, `LIKE`, `IN (list)`,
    `BETWEEN`, `CASE`, `CAST`, and scalar functions `UPPER`/`LOWER`/`LENGTH`/`CHAR_LENGTH`/`CONCAT`/`ABS`/
    `COALESCE`/`IFNULL`/`NULLIF`.
  - Aggregation: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with `DISTINCT`, `GROUP BY` with strict grouping validation,
    `HAVING`, `SELECT DISTINCT`.
  - Ordering/paging: `ORDER BY` expressions/aliases/ordinals with `ASC`/`DESC`/`NULLS FIRST`/`LAST`,
    `LIMIT`/`OFFSET` (incl. MySQL `LIMIT off, cnt`).
  - Composition: `UNION`/`UNION ALL` with numeric widening, derived tables, non-recursive `WITH` CTEs, and
    uncorrelated scalar/`IN`/`EXISTS` subqueries.
  - Cross-engine consistency: every base table side of a join is read through the same storage path as
    `Route::OlapScan` above, all at **one** MVCC snapshot per statement, so a join between a `Row` table and
    a converted `Column`/`Converting` table is consistent. Per-slot partition pruning and single-leaf
    predicate pushdown apply as above, except a conjunct on the null-supplying side of an outer join is kept
    as a residual filter rather than pushed down.
  - Limits: intermediate results (scanned rows, hash tables, groups) are held in memory without bounds — no
    spilling, no cost-based planning, and no worker-pool parallelism above the per-slot scan.
  - R5 is preserved structurally: a purely syntactic shape test (`is_narrow_select_shape`) runs before any
    deep binding, so a complete-PK lookup or narrow scan keeps its existing route unchanged, and a clause
    that would otherwise be silently dropped (`LIMIT`, alias, join, `OR` on a PK lookup) instead falls
    through to the general path rather than being ignored.
- **`UPDATE`** (`Route::RowstoreUpdate`):
  ```sql
  UPDATE orders SET amount = amount * 1.1 WHERE order_id = 10;
  UPDATE orders SET amount = amount + 1 WHERE customer_id = 1;
  ```
  A complete-PK `WHERE` takes a point read-modify-write and commits one `Mutation::Put`; any other `WHERE`
  (or none) scans every partition at one snapshot and commits all rewritten rows in **one** transaction
  (16 MiB 2PC payload cap, no chunking). Assignments evaluate left to right against the progressively
  updated row. Rejected: assigning a primary-key or partition-key column, subqueries in `SET`, and
  `UPDATE ... FROM`/`JOIN`/`ORDER BY`/`LIMIT`.
- **`DROP TABLE`** (`Route::CatalogDdl`):
  ```sql
  DROP TABLE IF EXISTS orders;
  ```
  Removes the table and its partitions/tablets/replicas in one catalog CAS; refuses while any partition is
  `Converting`. Metadata-only — the dropped tablets' rowstore data and columnar segments are not physically
  reclaimed, but their identifiers are never reissued.
- **`SHOW` / `DESCRIBE`** (`Route::CatalogRead`, answered from the catalog only):
  ```sql
  SHOW TABLES LIKE 'ord%';
  SHOW DATABASES;
  SHOW COLUMNS FROM orders;
  DESCRIBE orders;
  ```

### Partitioned Table Support (SQL DDL & Native Admin API)

Partitioned tables can be defined via SQL DDL or via the native `LocalServer` admin API:

- **Supported SQL Partition DDL:**
  - `PARTITION BY RANGE (col)` or `PARTITION BY RANGE COLUMNS (col)` with `(PARTITION p0 VALUES LESS THAN (v0), ..., PARTITION pN VALUES LESS THAN (vN) | VALUES LESS THAN MAXVALUE)`.
  - `PARTITION BY LIST (col)` or `PARTITION BY LIST COLUMNS (col)` with `(PARTITION p0 VALUES IN (v1, v2), ...)`.
- **Native Topology API:** `LocalServer::create_partitioned_table(definition)` accepts a [`PartitionedTableDefinition`](crates/htap-server/src/lib.rs) with:
  - `PartitionTopology::Range { key_column, partitions }`: ordered non-overlapping half-open intervals `[lower, upper)`.
  - `PartitionTopology::List { key_column, partitions }`: disjoint sets of explicit values.
- **Catalog Validation:**
  - The partition key column must be a single non-null column included in the primary key (`primary_key` must contain `key_column`).
  - Range bounds require strictly increasing values (`prev_bound < curr_bound`), non-overlapping intervals, and typed compatibility with the key column. `MAXVALUE` is permitted only on the final range partition.
  - List partitions require non-empty disjoint value lists without duplicate entries across or within partitions.
  - Duplicate partition names, empty partition lists, type mismatches, and schema/table name collisions are rejected with `HtapError::InvalidArgument` or `HtapError::Conflict`.
- **Local Topology Invariant:**
  - Each partition is initialized with `StorageDescriptor::Row`, exactly one bucket-0 row tablet (`tablets.len() == 1`, `bucket = 0`), and one healthy local leader replica on node 1 (`NodeId(1)`).
  - No hash buckets, sub-partitioning, or physical sharding across distributed nodes is implemented.
- **Multi-Partition DML & Scan Behavior:**
  - Multi-row `INSERT` routes each row by partition key and atomically applies all mutations in one transaction version.
  - Complete-PK `DELETE` and `SELECT` locate the partition key at its index in the primary key tuple, route to the target partition, and execute against that partition's row tablet. Complete-PK lookups bypass OLAP and preserve the rowstore `Engine::get` fast path.
  - Analytic `SELECT` executes across all partitions at the same transaction snapshot, combining rows for global filters, aggregates, and groupings.
- **Storage Format Conversion & Demotion:**
  - `LocalServer::convert_table(table_name)` remains available for single-partition tables.
  - Table-wide conversion: `convert_table_to_column(table_name)` converts all partitions to columnar format, returning a deterministic [`TableConversionReport`](crates/htap-convert/src/lib.rs).
  - Metadata demotion: `convert_table_to_row(table_name)` demotes columnar partitions back to row storage via catalog CAS, clearing `column_manifest` references while retaining rowstore data (authoritative throughout) and existing column segment files on disk.
  - Explicit policy ticks: `LocalServer::conversion_tick(policy)` and `LocalServer::tick()` execute synchronous policy steps; `tick()` resumes persisted jobs only without initiating new conversions, and there is no autonomous background scheduler.
  - Fail-closed storage validation: `LocalServer::open` verifies that catalog partition metadata matches `<root>/colstore` manifests and segments on disk, failing closed (with `HtapError::Corruption` or `HtapError::Io` depending on the cause) if inconsistencies are detected.

### Unsupported & Deferred SQL & Partition Features
Direct `SegmentReader` pushdown optimization is implemented for the compact base path (single leaf pushdown), used by `Route::OlapScan` and, per slot, `Route::Query`. Simple unqualified source/projected column `ORDER BY` is implemented for `AnalyticSelect`; the general query path additionally supports joins, CTEs, expressions, aliases, full `ORDER BY`, `LIMIT`/`OFFSET`, `HAVING`, `OR`/`NOT`/arithmetic/casts, `AVG`/`DISTINCT` aggregates, and `UPDATE`/`DROP TABLE`/`SHOW` (see "General `SELECT`" and the `UPDATE`/`DROP TABLE`/`SHOW` bullets above). The following features are still explicitly deferred:
- Compound `AND` pushdown beyond one leaf, and `!=` pushdown (evaluated as residual SQL filters).
- Vectorized aggregation, vectorized/pipelined operator execution, and worker-pool parallelism for the general query path (each slot's own partition scan still uses the narrow path's scan workers; joins/grouping/ordering run single-threaded in memory).
- Window functions (`OVER`), correlated subqueries, `FULL OUTER`/`NATURAL`/`USING` joins, parenthesized nested join trees, recursive CTEs, `EXCEPT`/`INTERSECT`, `GROUP BY` ordinals, `LIMIT BY`.
- `INSERT ... SELECT`, `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, `DELETE` by non-PK filter (still complete-PK only), `TRUNCATE`, non-partition `ALTER TABLE`.
- Cost-based query optimization, memory bounds/spilling for the general query path, physical reclamation of data on `DROP TABLE`.
- Multi-tablet or distributed scans, distributed fanout, resource quotas, disk spilling, query cancellation (conservative finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global merge/order are implemented locally).
- DataFusion and Apache Arrow integration.
- Full MySQL dialect breadth (incl. implicit string<->number coercion — comparisons between incompatible types are bind errors here), semi-join rewrites of `IN`/`EXISTS`, integer `DIV`, broader string/date functions, sessions, and transaction controls (`BEGIN`, `COMMIT`, `ROLLBACK`).
- **MySQL Partition DDL & Partition Lifecycle Boundary:**
  - Supported SQL partitioning: MySQL `CREATE TABLE ... PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` (including `VALUES LESS THAN MAXVALUE` on the final partition) are supported via vendored `sqlparser` and bound to validated catalog partition models.
  - Supported SQL lifecycle DDL: `ALTER TABLE <table> ADD PARTITION`, `DROP PARTITION`, and `REORGANIZE PARTITION` for strict finite range and list forms and final `MAXVALUE` where supported, gated by empty-source rowstore checks before catalog mutation.
  - Native lifecycle API: `LocalServer::alter_partitions` provides programmatic partition management with candidate catalog validation, atomic CAS, empty safety, and checked ID allocation without ID burn.
  - Unsupported partitioning forms: Partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), `SUBPARTITION`, `LIST DEFAULT`, expressions in partition keys, multi-column `COLUMNS`, and non-final/malformed `MAXVALUE` are strictly rejected with parse or binder errors.
  - Deferred partition & conversion capabilities: Populated partition data migration during reorganization, physical storage reclamation (space of dropped partitions or demoted column files is not physically reclaimed), delete vectors, background compaction, autonomous background conversion scheduler, hash tablets / multiple tablets per partition, distributed/remote partition movement, replica consensus/HA, and an inter-node replication/movement network protocol remain deferred (the client-facing MySQL wire protocol is implemented; see "Network server (`htapd`)" above).

### Verification & Test Evidence

Partition metadata, lifecycle, and conversion execution are verified by named integration test suites:
- **Server Partition & Lifecycle Execution Tests (`crates/htap-server/tests/local_server.rs`):**
  - `test_sql_range_partitioning_ddl_and_maxvalue_routing`: verifies SQL range partitioning DDL and MAXVALUE routing.
  - `test_sql_list_partitioning_ddl_and_routing`: verifies SQL list partitioning DDL and routing.
  - `test_server_sql_alter_partition_lifecycle`: verifies SQL ADD, DROP, and REORGANIZE PARTITION execution and routing.
  - `test_server_alter_partitions_drop_empty_and_populated_guard`: verifies empty-source safety and rejection of populated DROP partitions.
  - `test_server_alter_partitions_reorganize_empty_and_populated_guard`: verifies contiguity and empty-source safety for REORGANIZE partitions.
  - `test_server_convert_table_multi_partition_reports_and_demotion_equivalence`: verifies table-wide Row->Column conversion, Column->Row metadata demotion, and query results.
  - `test_server_conversion_tick_idempotent_and_resume_snapshot_pinned`: verifies manual conversion ticks and resuming in-flight snapshot-pinned conversions.
  - `test_server_open_fail_closed_missing_or_corrupt_manifest`: verifies fail-closed storage validation on reopen when manifests are missing or corrupted.
  - `test_partitioned_native_range_topology_catalog_reopen_continuation`: verifies range topology creation, CAS persistence, catalog reload, and version continuation across reopen.
  - `test_partitioned_native_list_topology_catalog_reopen_continuation`: verifies list topology creation, catalog reload, and reopen.
  - `test_partitioned_boundary_unmatched_null_type_errors`: verifies rejection of out-of-range keys, unmatched list values, NULL partition keys, and type mismatches.
  - `test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`: verifies multi-row insert routing across partitions in one commit version and point delete.
  - `test_partitioned_composite_pk_partition_key_not_first`: verifies partition key resolution when the partition key is not the first column in a composite PK.
  - `test_partitioned_olap_across_partitions_and_empty_aggregate`: verifies analytical scan across all partitions, aggregate calculations, and empty table handling.
  - `test_convert_table_multi_partition_guard`: verifies that `convert_table` strictly rejects multi-partition tables.
  - `test_partitioned_empty_topology_rejection_no_catalog_mutation`: verifies that empty partition topology definitions are rejected without mutating catalog state.
- **Catalog Recovery & Alteration Tests (`crates/htap-catalog/tests/catalog_recovery.rs`):**
  - `test_partitioning_legacy_decode_and_reopen`, `test_range_partitioning_routing_and_boundaries`, `test_list_partitioning_routing`, `test_partitioning_duplicate_violations`, `test_range_overlap_and_order_violations`, `test_partitioning_type_and_null_violations`, `test_partitioning_ownership_and_method_consistency`, `test_partitioning_cas_and_reopen_lifecycle`, `test_partition_alteration_add_range_and_list`, `test_partition_alteration_drop_range_and_list`, `test_partition_alteration_reorganize_contiguous`, `test_partition_alteration_cas_and_reopen`.
- **SQL Parser Boundary & Partition Lifecycle Tests (`crates/htap-sql/tests/parse_bind.rs`, `route.rs`):**
  - `test_mysql_partition_ddl_parsed_and_bound`, `test_mysql_partition_ddl_negative_parser_and_binder`, `test_mysql_alter_partition_parsed_and_bound`, `test_mysql_alter_partition_negative`, `test_negative_create_table`.
- **Conversion Materialization & Demotion Tests (`crates/htap-convert/tests/materialization.rs`):**
  - `test_demote_partition_to_row_clearing_manifest_and_retained_data`, `test_demote_partition_rejections_active_converting_and_missing_and_corrupt`, `test_conversion_tick_resumes_snapshot_pinned`.

General query executor, `UPDATE`, `DROP TABLE`, and `SHOW`/`DESCRIBE` (Phase 9) are verified by:
- **`crates/htap-sql/src/expr.rs`** unit tests: `three_valued_logic_tables`, `numeric_promotion_and_overflow`, `like_in_between_case_cast`, `scalar_functions`, `subquery_and_aggregate_context`, `expr_type_inference`.
- **`crates/htap-sql/tests/query_bind.rs`:** `test_join_binding_kinds_aliases_and_wildcards`, `test_join_binding_errors`, `test_expressions_functions_and_type_checks`, `test_aggregates_group_by_having_and_grouping_rules`, `test_order_by_limit_distinct`, `test_subqueries_ctes_derived_tables_and_union`, `test_update_drop_show_binding`, `test_bound_predicate_evaluation_with_joined_rows`.
- **`crates/htap-sql/tests/route.rs`:** `test_route_classification`, `test_point_read_fast_path_pinned_against_general_query_path` (the R5 pin test).
- **`crates/htap-server/tests/query_exec.rs`:** `test_joins_across_row_column_and_converting_tables`, `test_outer_joins_null_padding_residual_on_and_null_keys`, `test_expressions_aggregates_having_order_limit_distinct`, `test_union_derived_tables_ctes_and_subqueries`, `test_partition_pruning_and_pushdown_through_general_path`, `test_single_snapshot_across_engines_and_freshness`, `test_general_query_over_reopened_server`, `test_update_by_primary_key_and_reopen_recovery`, `test_update_by_filter_across_partitions_and_storage_formats_with_reopen`, `test_show_tables_databases_columns_and_describe`, `test_drop_table_reopen_and_no_id_reuse`.
- **`crates/htap-catalog/tests/catalog_recovery.rs`** (catalog format v2 and identifier high-water mark): `test_catalog_v1_envelope_decodes_and_counters_fall_back_to_live_max`, `test_catalog_id_high_water_prevents_reuse_after_removal`, `test_corruption_and_truncation`.
- **`crates/htap-wire/tests/wire_server.rs`:** `test_general_sql_over_wire` (joins, `UPDATE`, `SHOW`/`DESCRIBE`, `DROP TABLE` over the MySQL wire protocol).

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
