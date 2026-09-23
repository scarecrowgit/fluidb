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

- **No server-side cursors:** `COM_STMT_FETCH` is answered with a clean error rather than implemented;
  there is no cursor support of any kind. (`htapd`/`htap-wire` do implement the MySQL text *and* binary
  protocols, including prepared statements, `COM_RESET_CONNECTION`, and `COM_CHANGE_USER` — see "Prepared
  statements" below; only cursors are excluded here.)
- **No roles, delegated administration, or host-based ACL:** TLS, protocol compression, and per-user
  accounts/privileges are implemented (Phase 12) — see "Network server (`htapd`)" below — but there are no
  roles, no way to delegate a subset of superuser authority to another account (`WITH GRANT OPTION` parses but
  is rejected), only the `%` host is accepted, and there is no `caching_sha2_password` support.
- **No Docker image or Docker Compose deployment:** No `Dockerfile`, `docker-compose.yml`, or container images are provided or required.
- **No `SELECT ... FOR UPDATE`, locking reads, savepoints, or XA:** Sessions and
  explicit transactions (`BEGIN`/`COMMIT`/`ROLLBACK`, session variables) are implemented — see "Sessions and
  explicit transactions" below — but there is no locking-read syntax, no savepoints, and no distributed (XA)
  transactions. There is also no idle-transaction timeout/reaping yet.
- **No unsigned 64-bit values, exact `DECIMAL`, or `TIME` parameters in prepared statements:** there is no
  `UInt64` value type in the engine (a permanent limitation, not "not yet implemented"); `DECIMAL`/
  `NEWDECIMAL` parameters are kept as text and bound as a numeric literal (no arbitrary-precision decimal
  type); `TIME`-typed parameters are rejected. See "Prepared statements" below.
- **No TPC-C or TPC-H compliance:** The system does not implement the TPC-C or TPC-H benchmark specifications, relational transaction models, or analytical query profiles. Microbenchmarks evaluate isolated internal subsystem performance only.
- **No vectorized execution, and no worker-pool parallelism or memory-bounded spilling for outer joins:** The
  general query executor (see "Supported SQL Subset" below) handles joins (including arbitrarily nested join
  trees), window functions, correlated subqueries (one level deep only), recursive CTEs, `EXCEPT`/
  `INTERSECT`, and other set operations over materialized logical rows in memory. As of Phase 14, this
  executor has statistics-driven cost-based join reordering (`ANALYZE TABLE`, `htap_sql::optimize`, enabled
  by default), `EXPLAIN`/`EXPLAIN ANALYZE`, a per-statement memory budget with disk spilling, and bounded
  parallelism for `GROUP BY` and `INNER`/`CROSS` hash joins — see "Cost-based optimization, `EXPLAIN`,
  spilling, and parallelism (Phase 14)" below. Still not implemented: a vectorized/pipelined operator engine,
  and worker-pool parallelism or memory-bounded spilling specifically for `LEFT`/`RIGHT`/`FULL` joins (which
  stay single-threaded and, while not structurally excluded from spilling, are not exercised by a spill test
  either). The memory budget and spilling apply to this general executor only: a single-table `SELECT` with
  `ORDER BY`, `GROUP BY`, or a plain aggregate and no join routes to the narrow analytic scan path instead,
  which has no memory budget and never spills, by design.
- **`DROP TABLE` reclaims its own artifacts, eventually, not instantly:** Dropping a table removes it from the
  catalog and marks its tablets `pending_reclaim` in the same CAS. Column-store directories and movement
  artifacts are deleted as soon as a per-tablet lease is available; rowstore bytes are purged by the same
  tier-driven `Engine::compact_once` LSM compaction described below, which can take several `compaction_tick`
  calls to fully clear a large or contended table. Dropped identifiers are never reissued, so unreclaimed data
  can never be aliased by a new table in the meantime. Still not implemented: reclamation for `ALTER TABLE ...
  DROP/REORGANIZE PARTITION` (which only ever operates on empty partitions) and for demoted (`Column -> Row`)
  column files.
- **No vectorized/columnar delta-to-base background compaction:** The rowstore's own LSM compaction
  (`Engine::compact_once`) collapses superseded MVCC versions and reclaims dropped-partition bytes as an
  explicit, synchronous `compaction_tick()` — no background thread, no SQL trigger, and a movement- or
  reclaim-leased (busy) partition blocks compaction of every SST that contains or spans it (the rowstore is
  one shared keyspace, so protection is per SST, not per row). Folding accumulated rowstore deltas forward
  into new columnar segments, and delete vectors on columnar segments, remain deferred.
- **Exclusive Storage Ownership, Now With IPC Forwarding for a Second Process (Phase 16):** `LocalServer` and `LocalCoordinator` still enforce exclusive ownership of their root directory using an OS-level advisory lock (`<root>/LOCK` via `flock`); exactly one process ever touches storage directly, and this is still not concurrent shared-root writers. What changed: a second (or later) process opening the same root is no longer just rejected with `HtapError::Conflict` — it becomes an IPC client and forwards SQL/session calls to the owner over a Unix domain socket at `<root>/htap.sock` (mode `0600`, Unix-only; non-Unix targets keep the unconditional `Conflict`). Three rounds of post-landing storage re-review (ADR-025's "batch D/E/F") found 28 defects the passing test suite alone had not caught and fixed 27 of them (one, a `change_user` gate-ordering quirk, is recorded as a limitation instead), including two client-mode panic paths, a socket-permission window, and — found only once a fake regression test was replaced with a real one — a private-staging-directory leak on every server start; see "Concurrent multiprocess use: owner plus IPC (Phase 16)" in `docs/ARCHITECTURE.md` and ADR-025 in `docs/DECISIONS.md` for the full design, and `docs/LIMITATIONS.md` for the disclosed gaps (client sessions cannot change users, a client-mode handle's configuration setters (both `with_*` builder and `set_*` mutable forms) are accepted no-ops, administrative/data-mover/conversion/compaction operations stay owner-only, a lost client connection is terminal, and a socket bind failure falls back to the pre-Phase-16 lock-only mode). Low-level standalone subsystem instances (`htap_rowstore::Engine::open`, `htap_catalog::LocalCatalogStore::open`, `htap_movement::LocalDataMover::new`) still do not participate in this and remain unsafe for concurrent shared-root use.

---

## Embedded Client API & Usage

The `htap-client` crate provides [`EmbeddedClient`], an ergonomic synchronous in-process façade over `LocalServer`:

- **`EmbeddedClient::open(root)`:** Opens or recovers the local database rooted at `root`, acquiring `<root>/LOCK`, loading catalog metadata, recovering committed rowstore transactions, and initializing data movement. If another process already holds `<root>/LOCK`, this instead becomes an IPC client that forwards SQL/session calls to that owner over `<root>/htap.sock` (Phase 16, ADR-025) — it does none of the local storage-opening steps above in that case, and `EmbeddedClient::execute`/`open_session` behave identically either way from the caller's perspective. See "Concurrent multiprocess use: owner plus IPC (Phase 16)" in `docs/ARCHITECTURE.md`.
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

### SQL breadth: joins across storage engines, windows, `UPDATE`, `DELETE` by filter, `DROP TABLE`, `SHOW`

`execute(sql)` also drives the general query executor (joins including `FULL OUTER`/`NATURAL`/`USING` and
arbitrarily nested join trees, expressions, aggregates, window functions, correlated and uncorrelated
subqueries, `WITH RECURSIVE`, `UNION`/`EXCEPT`/`INTERSECT`), `UPDATE`, `DELETE` by filter, `TRUNCATE`,
`INSERT ... SELECT`, `DROP TABLE`, and `SHOW`/`DESCRIBE` — over `EmbeddedClient`, `RemoteClient`, or
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

    // A window function: running total per customer, evaluated after GROUP BY/HAVING.
    server.execute(
        "SELECT customer_id, amount, \
         SUM(amount) OVER (PARTITION BY customer_id ORDER BY order_id) AS running_total \
         FROM orders;",
    )?;

    // DELETE by an arbitrary filter (not just a complete-PK predicate), one transaction.
    server.execute("DELETE FROM orders WHERE amount < 1.0;")?;

    // SHOW / DESCRIBE, answered from the catalog only.
    server.execute("SHOW TABLES;")?;
    server.execute("DESCRIBE customers;")?;

    // DROP TABLE removes the table from the catalog in one CAS; its rowstore/columnar/movement
    // artifacts are physically reclaimed eventually by compaction_tick/reclaim_tick (Phase 15),
    // not necessarily by the time this call returns.
    server.execute("DROP TABLE orders;")?;

    Ok(())
}
```

Deferred on this path: `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, `LIMIT BY`, and non-partition
`ALTER TABLE`. Cost-based join reordering is implemented as of Phase 14 (`htap_sql::optimize`, enabled by
default) — see "Unsupported & Deferred SQL & Partition Features" below.

---

## Network server (`htapd`)

`htapd` exposes a `LocalServer` root over the MySQL text and binary protocols, so the same engine `EmbeddedClient` drives
in-process can also be reached over TCP, from any MySQL client or from `htap-client::RemoteClient`.

```bash
# Build and run the daemon (default bind: 127.0.0.1:3307, loopback only)
cargo run -p htapd -- --root /tmp/htap_demo

# Optional flags
cargo run -p htapd -- --root /tmp/htap_demo --listen 127.0.0.1:3307 --max-connections 64 --password secret \
  --max-allowed-packet 67108864
```

The password may also come from the `HTAPD_PASSWORD` environment variable, and the packet-size limit from
`HTAPD_MAX_ALLOWED_PACKET`; the flag wins over the environment variable in both cases. `--password`/
`HTAPD_PASSWORD` seeds the `root` account's password exactly once, the first time this root directory is
opened (see "Accounts and per-user privileges" below); with neither set, `root` starts with an empty
password. `--max-allowed-packet` defaults to 64 MiB (MySQL's own default) and bounds every protocol message,
including prepared-statement `SEND_LONG_DATA` buffers; it is reported dynamically as `@@max_allowed_packet`.

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

### TLS and compression (Phase 12)

```bash
# Enable TLS and require it (reject plaintext logins), and disable protocol compression
cargo run -p htapd -- --root /tmp/htap_demo --listen 0.0.0.0:3307 \
  --tls-cert /etc/htapd/server.crt --tls-key /etc/htapd/server.key \
  --require-secure-transport --disable-compression --password secret
```

`--tls-cert`/`--tls-key` (or `HTAPD_TLS_CERT`/`HTAPD_TLS_KEY`) must be supplied together; a bad path or a
cert/key that don't match each other fails startup with a clear error rather than silently serving plaintext.
`--require-secure-transport`/`HTAPD_REQUIRE_SECURE_TRANSPORT` rejects a plaintext login before credentials are
even checked, and refuses to start without TLS configured. MySQL protocol compression (zlib/zstd) is
negotiated automatically whenever a client requests it; `--disable-compression`/`HTAPD_DISABLE_COMPRESSION`
turns that off. Connecting with TLS from Rust:

```rust
use htap_client::RemoteClient;
use htap_wire::{ClientOptions, TlsMode};

let mut client = RemoteClient::connect_with(
    "127.0.0.1:3307",
    ClientOptions {
        username: "root".into(),
        password: Some("secret".into()),
        tls: TlsMode::Required { ca_cert: "/etc/htapd/ca.crt".into(), server_name: None },
        ..Default::default()
    },
)?;
```

**Security caveat:** TLS is opt-in. A server started without `--tls-cert`/`--tls-key` still exchanges
cleartext query text and result rows, so binding a non-loopback address without TLS requires a trusted
network or an SSH tunnel. See "TLS and compression (Phase 12)" in [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md),
ADR-020, and [`docs/OPERATIONS.md`](./docs/OPERATIONS.md) for cert provisioning, reload, and the full
protocol scope.

### Accounts and per-user privileges (Phase 12)

`htapd` authenticates against catalog-backed accounts instead of one shared password. The first time a root
directory is opened, a superuser `root` account is created from `--password`/`HTAPD_PASSWORD` (or an empty
password if neither is set); after that, `--password` only matters if you're recreating `root`'s password via
SQL — it is not re-checked on every login.

```sql
CREATE USER 'app'@'%' IDENTIFIED BY 'app-password';
GRANT SELECT, INSERT, UPDATE ON htap.orders TO 'app'@'%';
GRANT SELECT ON *.* TO 'app'@'%';           -- read-only on every table
SHOW GRANTS FOR 'app'@'%';
REVOKE UPDATE ON htap.orders FROM 'app'@'%';
DROP USER 'app'@'%';
```

Only the `%` host is accepted; `CREATE`/`ALTER`/`DROP USER` and `GRANT`/`REVOKE` require a superuser and are
rejected inside an open transaction; `WITH GRANT OPTION` parses but is rejected (no delegated administration).
A privilege check runs on every statement against the freshly loaded catalog (never cached), so a `REVOKE`
takes effect on the very next statement in an already-open session. See "Accounts and privileges (Phase 12)"
in [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) and ADR-021 for the full model and remaining gaps (no
roles, no `caching_sha2_password`, no `ACCOUNT LOCK` syntax).

---

## Sessions and explicit transactions

`EmbeddedClient::execute`/`RemoteClient::execute` keep auto-committing exactly as before. `open_session()`
opens an explicit `Session` (one `EmbeddedClient::open_session()` call, or one wire connection, is one
session for its lifetime) supporting `BEGIN`/`START TRANSACTION`, `COMMIT`, `ROLLBACK`, `autocommit`, and
`@user`/`@@system` variables. Uncommitted writes are buffered in the session and never touch the WAL,
transaction journal, or memtable — a crash before `COMMIT` is an implicit `ROLLBACK` — and are visible only
to statements run through that same session (read-your-own-writes) until `COMMIT` runs the existing 2PC path
once. Isolation is snapshot isolation with first-writer-wins (write skew permitted), reported as
`REPEATABLE READ`; a concurrent write to the same row is a clean `Conflict` at `COMMIT`.

Embedded:

```rust
use htap_client::EmbeddedClient;
use htap_common::Result;

fn main() -> Result<()> {
    let client = EmbeddedClient::open("/tmp/htap_demo3")?;
    client.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance INT);")?;
    client.execute("INSERT INTO accounts (id, balance) VALUES (1, 100), (2, 0);")?;

    let mut session = client.open_session()?;
    session.begin()?;
    session.execute("UPDATE accounts SET balance = balance - 50 WHERE id = 1;")?;
    session.execute("UPDATE accounts SET balance = balance + 50 WHERE id = 2;")?;
    // Read-your-own-writes: visible inside the session before COMMIT, not to `client.execute`.
    session.execute("SELECT balance FROM accounts WHERE id = 1;")?;
    session.commit()?; // one 2PC transaction for both UPDATEs

    assert!(!session.in_transaction());
    Ok(())
}
```

Over the wire, the same `BEGIN`/`COMMIT`/`ROLLBACK` are ordinary SQL on one connection — a `mysql` client
session, or `RemoteClient`:

```bash
mysql -h 127.0.0.1 -P 3307 -u root <<'SQL'
BEGIN;
UPDATE accounts SET balance = balance - 50 WHERE id = 1;
UPDATE accounts SET balance = balance + 50 WHERE id = 2;
COMMIT;
SQL
```

```rust
use htap_client::RemoteClient;
use htap_common::Result;

fn main() -> Result<()> {
    let mut client = RemoteClient::connect("127.0.0.1:3307", None)?;
    client.execute("BEGIN;")?;
    client.execute("UPDATE accounts SET balance = balance - 50 WHERE id = 1;")?;
    client.execute("UPDATE accounts SET balance = balance + 50 WHERE id = 2;")?;
    client.execute("COMMIT;")?;
    Ok(())
}
```

Deferred: `SELECT ... FOR UPDATE`/locking reads, savepoints, XA,
idle-transaction timeout/reaping, and MVCC garbage collection. Only `REPEATABLE READ` is offered (other
isolation levels are rejected, not silently downgraded); DDL is rejected inside an open transaction (the
transaction survives, unpoisoned). See "Sessions and explicit transactions (Phase 10)" in
[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md), ADR-018 in [`docs/DECISIONS.md`](./docs/DECISIONS.md), and
[`docs/LIMITATIONS.md`](./docs/LIMITATIONS.md) for the full contract, remaining gaps, and test evidence.

---

## Prepared statements

`RemoteClient::prepare(sql)` returns a `PreparedStatement` handle that `execute_prepared` can run repeatedly
with different parameters over the MySQL binary protocol (`COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`); `?` is a
placeholder anywhere the binder accepts a literal, including inside subqueries, derived tables, CTEs,
`UNION` branches, and `LIMIT`/`OFFSET`:

```rust
use htap_client::RemoteClient;
use htap_common::types::Value;
use htap_common::Result;

fn main() -> Result<()> {
    let mut client = RemoteClient::connect("127.0.0.1:3307", None)?;
    client.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance INT);")?;

    let insert = client.prepare("INSERT INTO accounts (id, balance) VALUES (?, ?);")?;
    client.execute_prepared(&insert, &[Value::Int64(1), Value::Int32(100)])?;
    client.execute_prepared(&insert, &[Value::Int64(2), Value::Int32(0)])?;
    client.close_prepared(insert)?;

    let select = client.prepare("SELECT balance FROM accounts WHERE id = ?;")?;
    let result = client.execute_prepared(&select, &[Value::Int64(1)])?;
    client.close_prepared(select)?;

    println!("{result:?}");
    Ok(())
}
```

Parameters are substituted at the AST level (never by re-rendering the statement to text and reparsing),
so there is no string-interpolation risk and no `BLOB`/float precision loss. Placeholder position and count
are cross-checked against a raw tokenizer scan of the SQL text at `PREPARE` time; a `?` in a position this
engine cannot substitute (an identifier, a DDL default, a `SET` target) is rejected up front rather than
silently ignored. Only `INSERT`/`UPDATE`/`DELETE`/`SELECT` can be prepared. `PREPARE` response metadata
(result column definitions) is best-effort: it is the real output schema when every placeholder's type can
be inferred from local context, or `num_columns = 0` otherwise. There is no `UInt64` value type, so an
unsigned 64-bit parameter above `i64::MAX` is rejected cleanly; `DECIMAL`/`NEWDECIMAL` parameters bind as a
numeric literal (no arbitrary-precision decimal type); `TIME`-typed parameters and `COM_STMT_FETCH`
(server-side cursors) are rejected. See "Prepared statements and binary protocol (Phase 11)" in
[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md), ADR-019 in [`docs/DECISIONS.md`](./docs/DECISIONS.md), and
[`docs/LIMITATIONS.md`](./docs/LIMITATIONS.md) for the full contract, remaining gaps, and test evidence.

---

## Workspace Architecture

The workspace consists of 14 modular crates (plus the vendored `vendor/sqlparser`) separated by architectural boundaries:

| Crate | Role & Status |
| ----- | ------------- |
| `crates/htap-common` | Common types (`Value`, `Row`, `Schema`, `Mutation`), MVCC `Version`, `FencingToken`, key encoding/decoding, error models, and the shared durability module (`fs::{sync_dir, atomic_publish, ...}`, `envelope::{encode_envelope, decode_envelope, ...}`, `bytecursor::ByteReader`) used by every crate below that publishes a durable file. |
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
| `crates/htap-wire` | Hand-written, synchronous MySQL text- and binary-protocol server (`WireServer`) exposing `LocalServer` over TCP, including prepared statements (`COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`/`SEND_LONG_DATA`), `COM_RESET_CONNECTION`/`COM_CHANGE_USER`, and the `WireClient` used by `RemoteClient`. |
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
- **General `SELECT` (joins, windows, subqueries, recursion, set ops; `Route::Query`):** Any `SELECT` that
  does not fit the narrow shape above (a join, an alias, a `LIMIT`/`HAVING`/`DISTINCT`, a subquery, an
  arithmetic projection, etc.) binds through the general query binder and executes via
  `htap-server::query_exec`:
  - Joins: `INNER`/`LEFT`/`RIGHT`/`CROSS`/`FULL OUTER`, `NATURAL`/`USING` with real column coalescing, table
    aliases, qualified names, `*`/`t.*`, and arbitrarily nested parenthesized join trees (e.g.
    `a LEFT JOIN (b JOIN c ON ...) ON ...`) — a left-deep, flat `JoinSpec` chain is synthesized into the same
    tree shape at bind time, so one join evaluator (`evaluate_join_tree`) runs every join, whether written
    flat or explicitly parenthesized (a separate flat-loop executor existed through Phase 13 and was removed
    in Phase 14; a differential test still pins a flat-written query and its explicitly-parenthesized
    equivalent to identical results). As of Phase 14, an `INNER`/`CROSS` join component may be cost-reordered
    by `htap_sql::optimize` before execution (see "Cost-based optimization, `EXPLAIN`, spilling, and
    parallelism (Phase 14)" below).
  - Expressions: arithmetic (`+ - * / % DIV`, `/` always widens to `Float64`, `DIV` truncates and stays
    `Int64`, checked overflow), comparisons (incl. column-vs-column), `AND`/`OR`/`NOT`, `IS [NOT] NULL`/
    `TRUE`/`FALSE`, `LIKE`, `IN (list)`, `BETWEEN`, `CASE`, `CAST`, and scalar functions `UPPER`/`LOWER`/
    `LENGTH`/`CHAR_LENGTH`/`CONCAT`/`ABS`/`COALESCE`/`IFNULL`/`NULLIF`.
  - Aggregation: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with `DISTINCT`, `GROUP BY` (expressions or ordinals) with
    strict grouping validation, `HAVING`, `SELECT DISTINCT`.
  - Window functions: `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `LAG`, `LEAD`, `FIRST_VALUE`,
    `LAST_VALUE`, and ordinary aggregates as window functions, with `PARTITION BY`/`ORDER BY` and `ROWS`/
    peer-`RANGE`/value-offset-`RANGE` frames (the last requires exactly one numeric/`Timestamp` `ORDER BY`
    key). Windows are evaluated after `GROUP BY`/`HAVING`, so they can combine with aggregates; `HAVING`
    cannot reference a window's result.
  - Correlated subqueries, one level deep only (a reference needing a grandparent is a specific bind error),
    in `WHERE`/`SELECT`/`HAVING`, bounded by a per-statement invocation/nesting budget.
  - Ordering/paging: `ORDER BY` expressions/aliases/ordinals with `ASC`/`DESC`/`NULLS FIRST`/`LAST`,
    `LIMIT`/`OFFSET` (incl. MySQL `LIMIT off, cnt`).
  - Composition: `UNION`/`UNION ALL`/`EXCEPT`/`INTERSECT` (`ALL`/`DISTINCT`) with numeric widening and
    correct multiset semantics, derived tables, non-recursive and recursive (`WITH RECURSIVE`, one
    self-referencing CTE, capped iterations/rows/bytes) CTEs, and uncorrelated/correlated scalar/`IN`/
    `EXISTS` subqueries.
  - Cross-engine consistency: every base table side of a join is read through the same storage path as
    `Route::OlapScan` above, all at **one** MVCC snapshot per statement, so a join between a `Row` table and
    a converted `Column`/`Converting` table is consistent. Per-slot partition pruning and single-leaf
    predicate pushdown apply as above, except a conjunct on the null-supplying side of an outer join is kept
    as a residual filter rather than pushed down.
  - Limits (see "Cost-based optimization, `EXPLAIN`, spilling, and parallelism (Phase 14)" below for the
    full contract): a per-statement memory budget (default 256 MiB) bounds hash joins, `GROUP BY`,
    `ORDER BY`, `DISTINCT`/`EXCEPT`/`INTERSECT`, and window partitions, spilling to disk one level deep and
    failing cleanly if a partition is still over budget after that; `LEFT`/`RIGHT`/`FULL` joins and non-equi/
    `CROSS` joins are not parallelized, and are not covered by a spill test even though the spill code path
    is not itself join-kind-restricted. Bounded parallelism covers `GROUP BY` and `INNER`/`CROSS` hash joins
    only. Recursive CTE working tables remain in memory without a budget.
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
  (effective 2PC payload cap of about 4 MiB — nominally 16 MiB, but the durable journal frame's own encoding
  makes the actual bound smaller; see `docs/LIMITATIONS.md` — no chunking). Assignments evaluate left to right
  against the progressively updated row. Rejected: assigning a primary-key or partition-key column, subqueries
  in `SET`, and `UPDATE ... FROM`/`JOIN`/`ORDER BY`/`LIMIT`.
- **`DELETE` by filter, and `TRUNCATE`** (`Route::RowstoreDelete`):
  ```sql
  DELETE FROM orders WHERE amount < 1.0;
  TRUNCATE TABLE orders;
  ```
  `DELETE` accepts any `WHERE` filter, not just a complete-PK predicate; a non-PK filter scans every
  partition at one snapshot and commits all matching deletes in **one** transaction (same payload cap and
  no-chunking rule as filtered `UPDATE`; requires `DELETE` privilege always, plus `SELECT` when filtered).
  `TRUNCATE TABLE t` / `TRUNCATE t` bind to the exact same unfiltered-`DELETE` representation — transactional
  and rollback-able, a disclosed deviation from real MySQL `TRUNCATE`. Unsupported `TRUNCATE` options
  (multiple targets, `PARTITION`, `IDENTITY`, `CASCADE`) are bind errors.
- **`INSERT ... SELECT`** (`Route::RowstoreWrite`):
  ```sql
  INSERT INTO archived_orders (order_id, customer_id, amount)
    SELECT order_id, customer_id, amount FROM orders WHERE amount > 100;
  ```
  Requires the same explicit target column list every `INSERT` requires, and an exact per-column static type
  match between the source query's output and the target columns (no widening; a NULL literal/variable is
  permissive). The source executes fully at the statement's one snapshot before any target row is inserted,
  so a self-referencing `INSERT INTO t SELECT ... FROM t` reads only the pre-insert snapshot and inserts each
  source row exactly once.
- **`DROP TABLE`** (`Route::CatalogDdl`):
  ```sql
  DROP TABLE IF EXISTS orders;
  ```
  Removes the table and its partitions/tablets/replicas in one catalog CAS; refuses while any partition is
  `Converting`. That same CAS marks the dropped tablets `pending_reclaim`: their rowstore data and columnar
  segments are physically reclaimed by `compaction_tick`/`reclaim_tick` over as many calls as it takes, not
  necessarily by the time this statement returns; their identifiers are never reissued in the meantime.
- **`SHOW` / `DESCRIBE`** (`Route::CatalogRead`, answered from the catalog only):
  ```sql
  SHOW TABLES LIKE 'ord%';
  SHOW DATABASES;
  SHOW COLUMNS FROM orders;
  DESCRIBE orders;
  ```
- **`ANALYZE TABLE`** (`Route::CatalogDdl`, Phase 14):
  ```sql
  ANALYZE TABLE orders;
  ```
  Scans every partition at one MVCC snapshot and records an exact row count and, per column, null count,
  min, max, and an exact distinct count (capped at a configurable limit — past the cap, `distinct_count` is
  reported as unknown rather than approximated). Publishes by catalog CAS of only the `stats` field.
  `FOR COLUMNS`, `NOSCAN`, and partition-scoped forms are rejected. Statistics are table-level (aggregated
  across partitions) and never expire automatically. `ANALYZE TABLE` counts as DDL, so like
  `CREATE TABLE`/`DROP TABLE` it is rejected inside an open transaction — see "Cost-based optimization,
  `EXPLAIN`, spilling, and parallelism (Phase 14)" below.
- **`EXPLAIN` / `EXPLAIN ANALYZE`** (Phase 14):
  ```sql
  EXPLAIN SELECT * FROM orders o JOIN customers c ON o.customer_id = c.id;
  EXPLAIN ANALYZE SELECT * FROM orders WHERE amount > 100;
  ```
  Renders the query's plan (one row per node: `node_id`, `parent_id`, `operation`, `table`, `est_rows`,
  `estimate_source`, `build_side`). For a complete-PK point lookup or a narrow single-table scan, renders a
  single-node plan and never invokes the cost-based optimizer — preserving R5 through `EXPLAIN` too.
  `EXPLAIN ANALYZE` additionally executes the statement and reports the root node's actual row count and
  elapsed time. `verbose`/`query_plan`/`estimate`/non-default `format`, and nested `EXPLAIN`, are rejected.

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
Direct `SegmentReader` pushdown optimization is implemented for the compact base path (single leaf pushdown), used by `Route::OlapScan` and, per slot, `Route::Query`. Simple unqualified source/projected column `ORDER BY` is implemented for `AnalyticSelect`; the general query path additionally supports joins (including `FULL OUTER`/`NATURAL`/`USING` and arbitrarily nested join trees), CTEs (including `WITH RECURSIVE`), expressions (including integer `DIV`), aliases, full `ORDER BY`/`GROUP BY` (including ordinals), `LIMIT`/`OFFSET`, `HAVING`, `OR`/`NOT`/arithmetic/casts, `AVG`/`DISTINCT` aggregates, window functions, correlated subqueries, `EXCEPT`/`INTERSECT`, and `UPDATE`/`DELETE` by filter/`TRUNCATE`/`INSERT ... SELECT`/`DROP TABLE`/`SHOW` (see "General `SELECT`" and the bullets above). The following features are still explicitly deferred:
- Compound `AND` pushdown beyond one leaf, and `!=` pushdown (evaluated as residual SQL filters).
- Vectorized aggregation and vectorized/pipelined operator execution for the general query path (each slot's
  own partition scan still uses the narrow path's scan workers; as of Phase 14, `GROUP BY` and `INNER`/`CROSS`
  hash joins are bounded-parallel and memory-budgeted with disk spilling — see "Cost-based optimization,
  `EXPLAIN`, spilling, and parallelism (Phase 14)" below — but filter/`ORDER BY`/`DISTINCT`/set-operation/
  window stages and `LEFT`/`RIGHT`/`FULL` joins still run single-threaded, and `LEFT`/`RIGHT`/`FULL`/non-equi/
  `CROSS` joins are not parallelized).
- `LIMIT BY`. (Cost-based join reordering is implemented as of Phase 14 — see below — not deferred.)
- `UPDATE` with joins/subqueries/`ORDER BY`/`LIMIT`, non-partition `ALTER TABLE`.
- Memory-bounded spilling for non-equi/`CROSS` joins on the general query path (evaluated by an in-memory
  nested loop with no budget check at all — a genuine gap, unlike `LEFT`/`RIGHT`/`FULL` equi-hash joins, whose
  spilling is not itself kind-restricted in code but is exercised by a test only for `INNER` joins; memory
  budgeting and spilling for `GROUP BY` and `INNER` equi-hash joins is implemented and tested as of Phase 14).
  (Physical reclamation of `DROP TABLE`'s own artifacts is implemented, eventually, as of Phase 15 — see
  "Scope Exclusions" and "Rowstore compaction, `DROP TABLE` reclaim, and journal checkpoint (Phase 15)" above;
  physical reclamation for `ALTER TABLE ... DROP/REORGANIZE PARTITION` and for demoted column files remains
  deferred.)
- Multi-tablet or distributed scans, distributed fanout, resource quotas, query cancellation (conservative
  finite range/list partition pruning, bounded in-process partition scan workers, and deterministic global
  merge/order are implemented locally; local disk spilling for the general query path is implemented as of
  Phase 14 — see below — distributed spill/fanout is not).
- DataFusion and Apache Arrow integration.
- Full MySQL dialect breadth (incl. implicit string<->number coercion — comparisons between incompatible types are bind errors here), semi-join rewrites of `IN`/`EXISTS`, and broader string/date/`DATE`/`DECIMAL`/`EXTRACT`/`SUBSTRING`/`INTERVAL`/view functions. (Sessions and explicit transactions — `BEGIN`, `COMMIT`, `ROLLBACK` — are implemented; see "Sessions and explicit transactions" below.)
- **MySQL Partition DDL & Partition Lifecycle Boundary:**
  - Supported SQL partitioning: MySQL `CREATE TABLE ... PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` (including `VALUES LESS THAN MAXVALUE` on the final partition) are supported via vendored `sqlparser` and bound to validated catalog partition models.
  - Supported SQL lifecycle DDL: `ALTER TABLE <table> ADD PARTITION`, `DROP PARTITION`, and `REORGANIZE PARTITION` for strict finite range and list forms and final `MAXVALUE` where supported, gated by empty-source rowstore checks before catalog mutation.
  - Native lifecycle API: `LocalServer::alter_partitions` provides programmatic partition management with candidate catalog validation, atomic CAS, empty safety, and checked ID allocation without ID burn.
  - Unsupported partitioning forms: Partition options (`ENGINE`, `COMMENT`, `TABLESPACE`, `DATA DIRECTORY`), `SUBPARTITION`, `LIST DEFAULT`, expressions in partition keys, multi-column `COLUMNS`, and non-final/malformed `MAXVALUE` are strictly rejected with parse or binder errors.
  - Deferred partition & conversion capabilities: Populated partition data migration during reorganization, physical storage reclamation for `ALTER TABLE ... DROP/REORGANIZE PARTITION` (empty partitions only, so currently inert) or demoted column files, delete vectors, delta-to-base background columnar compaction, autonomous background conversion scheduler, hash tablets / multiple tablets per partition, distributed/remote partition movement, replica consensus/HA, and an inter-node replication/movement network protocol remain deferred (the client-facing MySQL wire protocol is implemented; see "Network server (`htapd`)" above). `DROP TABLE`'s own rowstore/columnar/movement artifacts are physically reclaimed as of Phase 15 — see "Scope Exclusions" above.

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

## Cost-based optimization, `EXPLAIN`, spilling, and parallelism (Phase 14)

Phase 14 added statistics, a cost-based optimizer stage, `EXPLAIN`/`EXPLAIN ANALYZE`, memory-bounded
execution with disk spilling, and bounded intra-query parallelism to the general query executor
(`Route::Query`) only — `Route::RowstorePointRead` and `Route::OlapScan` are byte-for-byte unchanged. See
ADR-023 in `docs/DECISIONS.md` and `docs/PROGRESS.md`'s Phase 14 row for the full contract and evidence.

- **Statistics:** `ANALYZE TABLE t` (see "Supported SQL Subset" above) records exact per-table row count and
  per-column null count/min/max/capped-exact-distinct-count, published by catalog CAS. The `HTAPCAT1` catalog
  envelope bumped format version 3 -> 4 to carry `TableDescriptor.stats`; a version-3-or-earlier catalog still
  decodes with no statistics. Published statistics are structurally validated before they are accepted (column
  count, null count, distinct count, min/max type agreement, `min <= max`, and finite float bounds) — no
  further format change.
- **Scope: the general executor only.** Every item below applies to `Route::Query`. A single-table `SELECT`
  with `ORDER BY`, `GROUP BY`, or a plain aggregate (no join) routes to the narrow analytic scan path
  (`Route::OlapScan`) instead, which has no memory budget and never spills — this is by design, not an
  oversight, and is why several spill tests below use a join or another shape that reaches the general
  executor.
- **Cost-based optimization:** `htap_sql::optimize`, a storage-agnostic stage enabled by default for every
  general query, estimates row counts/selectivities from statistics (falling back to disclosed defaults when
  absent), classifies predicates by provenance to avoid the classic outer-join placement traps, and reorders
  `INNER`/`CROSS` join components (subset dynamic programming up to 8 relations, greedy above that) under an
  always-on conservation validator that falls back to the unoptimized plan on any internal inconsistency
  rather than risking a wrong result.
- **`EXPLAIN`/`EXPLAIN ANALYZE`:** see "Supported SQL Subset" above.
- **Spilling:** a per-statement memory budget (default 256 MiB) bounds each operator's own working memory
  (hash tables, sort runs, aggregate state, partition buffers) — it does not bound the rows a non-pipelined
  executor materializes between operators. One level of disk spilling — non-durable scratch under
  `<root>/spill/`, swept in full on every `LocalServer::open` — covers hash joins, `GROUP BY`, `ORDER BY`,
  `DISTINCT`/`EXCEPT`/`INTERSECT`, and window functions. Hash-join partition count is sized from the input and
  the remaining budget, capped at 128 (windows share this cap, to bound open file descriptors and the writer
  buffers the budget doesn't count); `GROUP BY` and the set operators each partition into their own fixed 16
  partitions. A partition that still doesn't fit after that one level — skew, or the partition-count cap —
  fails cleanly with the memory-budget error rather than recursing into a second spill level. Unlike hash
  join/window, `GROUP BY`'s and the set operators' fixed 16-partition count does not scale with input size or
  the remaining budget: an input much larger than roughly 16x the budget fails with the memory-budget error
  rather than spilling successfully (observed: a set operation over 4,096 awkward-double rows failed at a
  64 KiB budget, and succeeded at 1,536 rows). Budget-sized partitioning for these operators, like the hash
  join's, is a deferred improvement. Window spilling
  hash-partitions by the `PARTITION BY` key and evaluates one window partition at a time (a window with no
  `PARTITION BY` is a single partition). `GROUP BY` spill reserves each partition's rows as they are read back,
  then releases that reservation before in-memory aggregation runs (to avoid double-charging the same bytes) —
  peak memory during one partition's aggregation can therefore approach about twice the budget: a disclosed
  imprecision, not an exact bound.
- **Spill telemetry:** `LocalServer` exposes per-operator test-telemetry accessors —
  `last_query_hash_join_spilled()`, `last_query_group_by_spilled()`, `last_query_sort_spilled()`,
  `last_query_distinct_spilled()`, `last_query_set_operation_spilled()`, `last_query_window_spilled()` — each
  reporting whether that operator kind spilled in the caller thread's most recent statement. Every spill test
  asserts its named operator actually spilled, not merely that the statement succeeded.
- **Float semantics:** `+`, `-`, `*`, `/`, and `SUM`/`AVG` overflow return `"DOUBLE value is out of range"`
  (MySQL-compatible) instead of producing `NaN`/`Infinity`; division by zero still yields `NULL`. This closes a
  brick risk: `serde_json` (used to encode rows and catalog statistics) cannot represent a non-finite float, so
  an unchecked one could be written and then fail to decode. On the row path this was already caught, if
  confusingly, by the 2PC participant's own pre-commit payload decode; the catalog statistics path had no such
  incidental protection and is now closed by rejecting non-finite bounds up front and by statistics structural
  validation (see "Statistics" above). `CAST(... AS DOUBLE)` from a string and non-finite float literals are
  rejected the same way, and the narrow analytic scan path's own `SUM` aggregator now carries the same
  overflow check.
- **Parallelism:** bounded intra-query parallelism (`std::thread::scope`, no new dependency) for `GROUP BY`
  above a size threshold and for `INNER`/`CROSS` hash joins; `LEFT`/`RIGHT`/`FULL` joins stay single-threaded.
  One shared worker budget per statement keeps a join nested inside a parallel `GROUP BY` from multiplying
  thread counts.
- **New `LocalServer` settings** (Rust builder API only — no `htapd` CLI flag yet; see `docs/OPERATIONS.md`):
  `with_query_memory_budget`/`query_memory_budget()` (default 256 MiB), `with_query_parallelism`/
  `query_parallelism()` (default `available_parallelism()`), and `with_analyze_distinct_limit`/
  `analyze_distinct_limit()` (default 200,000).
- **Stated behavior, not silently assumed:** `ANALYZE TABLE` is gated by the "no DDL inside an open
  transaction" rule exactly like `CREATE TABLE`/`DROP TABLE` are — it is rejected inside any open transaction
  and the transaction survives the rejection, and `EXPLAIN ANALYZE` follows the transaction rules of the
  statement it executes while plain `EXPLAIN` remains permitted (`crates/htap-server/tests/session.rs::{test_analyze_table_rejected_inside_explicit_transaction_and_txn_survives, test_explain_analyze_wrapping_ddl_rejected_inside_open_transaction, test_explain_analyze_wrapping_insert_rejected_inside_read_only_transaction}`, `crates/htap-server/tests/explain.rs::test_plain_explain_select_permitted_inside_open_transaction`).
- **Disclosed gaps, not silently assumed:** Hash-join
  spilling is not itself restricted to a join kind in code, but only `INNER`-join spilling is covered by a
  test. Window evaluation carries every materialized column of the joined input into its spill partitions, not
  just the columns the query needs (measured: 8 columns / ~424 B per row where 3 are needed), so window
  partitions are larger than necessary under a budget; a column-trimming improvement is deferred. The
  optimizer's leaf-cost and outer-join cardinality estimates are cost-quality-only weaknesses — they can pick a
  worse plan but never change results: outer joins are never reordered, and null-padding is independent of
  which side was chosen to build. The planned shared binder leaf-helper extraction between `htap_sql::binder`
  and `htap_sql::binder_query` (`docs/PROBLEMS.md` P2) was not delivered — the two binder entry points are kept
  deliberately separate so R5 stays structural, but each still holds its own copy of that leaf-level logic.

---

## Rowstore compaction, `DROP TABLE` reclaim, and journal checkpoint (Phase 15)

Phase 15 added narrow local MVPs for rowstore LSM compaction with a real MVCC-safe garbage-collection horizon,
physical reclamation of a dropped table's rowstore/columnar/movement artifacts, and crash-safe compaction of
`txn.journal` — no query-execution or on-disk row/column format change; `Route::RowstorePointRead`/
`Route::OlapScan`/`Route::Query` are byte-for-byte unchanged. See ADR-024 in `docs/DECISIONS.md` and
`docs/PROGRESS.md`'s Phase 15 row for the full contract and evidence.

- **`Engine::compact_once`** selects one *contiguous* run of the manifest's SST list (entry-count tiered, or
  an explicit id set for `DROP TABLE`'s forced-priority path) and splices its merged output into that run's
  original manifest position, never prepending it — an initial draft did prepend, and could resurrect a stale
  value hidden behind a tombstone in a newer, unselected SST; fixed and pinned by
  `crates/htap-rowstore/tests/compaction_ordering.rs`. Per key, every version above a computed GC horizon is
  kept unconditionally; among versions at or below it, only the single newest survives (`Put` or `Delete`,
  never elided).
- **`HTAPMAN1` bumps to format version 3** to carry `committed_version_high_water` and `gc_low_water`, both
  monotonic and refusing to publish a regression; a read at a real snapshot below `gc_low_water` now fails
  with a clear error instead of silently returning collapsed data. `Engine::open` also now takes an exclusive
  lock on `<rowstore>/LOCK` for its whole lifetime.
- **`DROP TABLE` now physically reclaims its artifacts**, eventually: the same catalog CAS that removes the
  table marks its tablets `pending_reclaim` (`HTAPCAT1` bumps to format version 5).
  `LocalServer::reclaim_tick`/`compaction_tick` delete column-store directories and movement artifacts
  (including that tablet's movement job records) under a per-tablet lease, and drive rowstore purge
  confirmation to completion across as many `compaction_tick` calls as it takes.
- **`TransactionManager::checkpoint()`** (a new `HTAPTXC1` envelope, `txn.checkpoint`) compacts `txn.journal`
  by dropping resolved `Intent`/`Commit`/`Abort` records past a durable baseline, triggered opportunistically
  after a commit and finalized once at `LocalServer::open`. Any error partway through the journal rewrite
  unconditionally latches `RecoveryRequired` rather than risking a stale, untrusted handle.
- **Disclosed, not fixed:** the rowstore is one shared keyspace, so a busy (leased) tablet blocks compaction
  of every SST that contains or spans it, not just its own rows; the explicit-SST-id compaction path compacts
  only the first contiguous run per call, so a scattered dropped partition purges over several passes;
  compaction is explicit-tick-only with no background thread or SQL trigger; and movement/reclaim leases are
  intentionally non-durable (sound only because nothing that survives a crash can still be reading the tablet
  a lease protected — see ADR-024). Two latent, currently-unreachable API hazards: `TransactionManager::new`
  does not itself load the durable checkpoint baseline (its only caller, `open_with_options`, overwrites it
  right after), and the `txn.checkpoint` file name is fixed per directory (unreachable since `LocalServer`
  always uses one `txn.journal` per data root) — see `docs/LIMITATIONS.md`.
- **A review round ("X batch") fixed five further issues:** exports now hold their tablet lease for the
  whole scan-and-write, so they can no longer race a reclaim lease on the same tablet; `compact_once`'s GC
  horizon clamp has no write-side exemption for the `u64::MAX` sentinel, so `gc_low_water` always rises to
  match what actually collapsed; tablet artifact deletion now fsyncs its parent directories; the journal
  checkpoint reads through a raised ceiling `open`/`recover()` also use, so an already-oversized journal
  can still be checkpointed back under its configured limit; and `compaction_tick`'s `entries_purged` now
  counts only confirmations whose catalog CAS actually succeeded.
- **A follow-up storage re-review ("Y batch") corrected two of those fixes and found three more:** the GC
  horizon clamp is now against `visible_version`, not `committed_version` — clamping to `committed_version`
  could raise `gc_low_water` above `visible_version` whenever `apply_external` had committed ahead of what was
  published, rejecting every fresh snapshot; the checkpoint's read ceiling is
  `max(configured_max_journal_size, 2 GiB)`, not a fixed 2 GiB cap, which would itself refuse a journal between
  2 GiB and a larger configured limit; tablet artifact deletion now tolerates a missing `movement/jobs/`
  directory instead of failing reclamation outright; the colstore reclaim step now fsyncs `<root>/colstore`
  after deleting a tablet's colstore directory; and `LocalDataMover`'s single global lease mutex being held
  across an entire reclaim deletion (stalling every other tablet's lease operations meanwhile) is disclosed as
  a performance-only limitation, not fixed.

Verified by `crates/htap-rowstore/tests/{compaction.rs,compaction_ordering.rs,manifest_v3.rs,purge_reopen.rs,
gc_low_water.rs,wal_purge.rs,preview_sst_ids.rs,horizon_clamp.rs}`, `crates/htap-server/tests/{compaction_tick.rs,reclaim.rs,
sandwiched_purge.rs,tier_shift_protection.rs,movement_artifacts_reclaim.rs,purge_no_resurrection.rs}`,
`crates/htap-movement/tests/{leasing.rs,movement_fixes.rs,export_leasing.rs,missing_jobs_dir.rs}`, `crates/htap-txn/src/manager.rs`'s
`test_checkpoint_*` unit tests, and `crates/htap-catalog/tests/catalog_recovery.rs`'s v5 tests — see
`docs/PROGRESS.md`'s Phase 15 row for the full, named list.

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
