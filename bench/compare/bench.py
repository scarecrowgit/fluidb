"""fluidb vs PostgreSQL vs ClickHouse comparison harness.

Every engine is driven over its native network protocol from the same Python client, with the
same SQL text wherever the engines allow it. Usage:

    python bench.py load   <db> <rows>
    python bench.py olap   <db> <rows>
    python bench.py oltp   <db> <rows> [concurrency,...]
    python bench.py mixed  <db> <rows>
    python bench.py size   <db>

Results are appended as JSON lines to results.jsonl.
"""

import json
import multiprocessing as mp
import os
import random
import statistics
import sys
import time

RESULTS = os.environ.get("RESULTS", "/bench/results.jsonl")
N_CUSTOMERS = 100_000
BATCH = 5_000
T0 = 1_735_689_600  # 2025-01-01 UTC
YEAR = 365 * 86_400
STATUSES = ["new", "paid", "shipped", "delivered", "returned"]
REGIONS = ["eu-west", "eu-east", "us-east", "us-west", "apac", "latam", "mea", "anz"]
COUNTRIES = ["DE", "FR", "US", "GB", "JP", "BR", "IN", "AU", "CA", "ES", "IT", "NL"]
SEGMENTS = ["retail", "smb", "enterprise"]
OLTP_SECONDS = float(os.environ.get("OLTP_SECONDS", "10"))
QUERY_TIMEOUT = 900


# ---------------------------------------------------------------- connections

def connect(db):
    if db == "fluidb":
        import MySQLdb

        return MySQLdb.connect(host="127.0.0.1", port=3307, user="root", passwd="",
                               autocommit=True, read_timeout=QUERY_TIMEOUT)
    if db == "postgres":
        import psycopg

        c = psycopg.connect("host=127.0.0.1 port=5432 user=postgres password=bench dbname=postgres",
                            autocommit=True)
        c.execute(f"SET statement_timeout = '{QUERY_TIMEOUT}s'")
        return c
    if db == "clickhouse":
        from clickhouse_driver import Client

        return Client(host="127.0.0.1", port=9000, user="default", password="bench",
                      settings={"max_execution_time": QUERY_TIMEOUT})
    raise SystemExit(f"unknown db {db}")


def run(db, conn, sql):
    """Execute one statement and return all rows (list of tuples) or []."""
    if db == "clickhouse":
        return conn.execute(sql) or []
    if db == "postgres":
        cur = conn.execute(sql)
        return cur.fetchall() if cur.description else []
    cur = conn.cursor()
    cur.execute(sql)
    rows = cur.fetchall() if cur.description else []
    cur.close()
    return list(rows)


# ---------------------------------------------------------------- schema / data

def ddl(db):
    if db == "clickhouse":
        return [
            "DROP TABLE IF EXISTS orders",
            "DROP TABLE IF EXISTS customers",
            "CREATE TABLE orders (id Int64, customer_id Int64, product_id Int32, quantity Int32, "
            "price Float64, status String, region String, created_ts Int64) "
            "ENGINE = MergeTree ORDER BY id",
            "CREATE TABLE customers (id Int64, name String, country String, segment String) "
            "ENGINE = MergeTree ORDER BY id",
        ]
    drop = ["DROP TABLE IF EXISTS orders", "DROP TABLE IF EXISTS customers"]
    return drop + [
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT NOT NULL, product_id INT NOT NULL, "
        "quantity INT NOT NULL, price DOUBLE PRECISION NOT NULL, status VARCHAR(16) NOT NULL, "
        "region VARCHAR(16) NOT NULL, created_ts BIGINT NOT NULL)".replace(
            "DOUBLE PRECISION", "DOUBLE PRECISION" if db == "postgres" else "DOUBLE"),
        "CREATE TABLE customers (id BIGINT PRIMARY KEY, name VARCHAR(64) NOT NULL, "
        "country VARCHAR(32) NOT NULL, segment VARCHAR(16) NOT NULL)",
    ]


def order_row(i, rows):
    h = (i * 2_654_435_761) & 0xFFFFFFFF
    return (
        i,
        h % N_CUSTOMERS + 1,
        (i * 7) % 5_000,
        h % 10 + 1,
        ((i * 37) % 10_000) / 100.0,
        STATUSES[(h >> 8) % len(STATUSES)],
        REGIONS[(h >> 16) % len(REGIONS)],
        T0 + (i * YEAR) // max(rows, 1),
    )


def order_values(r):
    return f"({r[0]},{r[1]},{r[2]},{r[3]},{r[4]!r},'{r[5]}','{r[6]}',{r[7]})"


ORDER_COLS = "(id, customer_id, product_id, quantity, price, status, region, created_ts)"


def result(kind, db, rows, **kw):
    rec = {"kind": kind, "db": db, "rows": rows, "ts": time.time(), **kw}
    with open(RESULTS, "a") as f:
        f.write(json.dumps(rec) + "\n")
    print(json.dumps(rec), flush=True)


def cmd_load(db, rows):
    conn = connect(db)
    for s in ddl(db):
        try:
            run(db, conn, s)
        except Exception as e:  # fluidb has no DROP ... IF EXISTS on an empty root? keep going
            if "DROP" not in s:
                raise
            print("ignored:", e)
    t = time.perf_counter()
    for start in range(1, N_CUSTOMERS + 1, BATCH):
        vals = ",".join(
            f"({i},'customer_{i}','{COUNTRIES[i % len(COUNTRIES)]}','{SEGMENTS[i % len(SEGMENTS)]}')"
            for i in range(start, min(start + BATCH, N_CUSTOMERS + 1)))
        run(db, conn, f"INSERT INTO customers (id, name, country, segment) VALUES {vals}")
    t_cust = time.perf_counter() - t

    t = time.perf_counter()
    for start in range(1, rows + 1, BATCH):
        vals = ",".join(order_values(order_row(i, rows)) for i in range(start, min(start + BATCH, rows + 1)))
        run(db, conn, f"INSERT INTO orders {ORDER_COLS} VALUES {vals}")
    t_orders = time.perf_counter() - t

    t = time.perf_counter()
    if db == "postgres":
        run(db, conn, "VACUUM ANALYZE")
    elif db == "clickhouse":
        run(db, conn, "OPTIMIZE TABLE orders FINAL")
        run(db, conn, "OPTIMIZE TABLE customers FINAL")
    t_post = time.perf_counter() - t
    result("load", db, rows, batch=BATCH, orders_s=t_orders, orders_rows_per_s=rows / t_orders,
           customers_s=t_cust, post_load_s=t_post)


# ---------------------------------------------------------------- analytics

def queries(rows):
    lo = rows // 2
    a, b = T0 + YEAR // 4, T0 + YEAR // 4 + 30 * 86_400
    return [
        ("Q1 count(*)", "SELECT COUNT(*) FROM orders"),
        ("Q2 filtered sum", "SELECT SUM(price * quantity) FROM orders WHERE status = 'shipped'"),
        ("Q3 group by region",
         "SELECT region, COUNT(*), SUM(price), AVG(quantity) FROM orders GROUP BY region ORDER BY region"),
        ("Q4 2-key group + filter",
         "SELECT status, region, COUNT(*) FROM orders WHERE quantity > 5 GROUP BY status, region "
         "ORDER BY status, region"),
        ("Q5 top-10 customers",
         "SELECT customer_id, SUM(price) AS s FROM orders GROUP BY customer_id "
         "ORDER BY s DESC, customer_id LIMIT 10"),
        ("Q6 PK range 10k rows",
         f"SELECT COUNT(*), SUM(price) FROM orders WHERE id >= {lo} AND id < {lo + 10_000}"),
        ("Q7 30-day time range", f"SELECT COUNT(*) FROM orders WHERE created_ts >= {a} AND created_ts < {b}"),
        ("Q8 count distinct", "SELECT COUNT(DISTINCT customer_id) FROM orders"),
        ("Q9 join + group",
         "SELECT c.country, SUM(o.price) AS rev FROM orders o JOIN customers c ON o.customer_id = c.id "
         "GROUP BY c.country ORDER BY rev DESC"),
    ]


def norm(rows):
    out = []
    for r in rows:
        out.append(tuple(float(f"{float(v):.6g}") if not isinstance(v, str) else v for v in r))
    return out


def cmd_olap(db, rows):
    conn = connect(db)
    reps = int(os.environ.get("OLAP_REPS", "5"))
    for name, sql in queries(rows):
        try:
            t = time.perf_counter()
            res = run(db, conn, sql)  # warm-up (also the checked answer)
            first = time.perf_counter() - t
            times = [first]
            if first < 120:
                times = []
                for _ in range(reps):
                    t = time.perf_counter()
                    run(db, conn, sql)
                    times.append(time.perf_counter() - t)
            result("olap", db, rows, query=name, cold_s=first, median_s=statistics.median(times),
                   min_s=min(times), runs=len(times), answer=norm(res)[:12])
        except Exception as e:
            result("olap", db, rows, query=name, error=str(e)[:300])
            conn = connect(db)


# ---------------------------------------------------------------- OLTP

def op_factory(db, rows, worker):
    rng = random.Random(1000 + worker)
    next_id = [rows + 1 + random.SystemRandom().randrange(1, 1 << 30) * 1_000_000]

    def fresh():
        next_id[0] += 1
        return order_values(order_row(next_id[0], rows))

    def point_select(conn):
        run(db, conn, f"SELECT * FROM orders WHERE id = {rng.randint(1, rows)}")

    def point_update(conn):
        run(db, conn, f"UPDATE orders SET quantity = quantity + 1 WHERE id = {rng.randint(1, rows)}")

    def insert_one(conn):
        run(db, conn, f"INSERT INTO orders {ORDER_COLS} VALUES {fresh()}")

    def txn(conn):
        k = rng.randint(1, rows)
        run(db, conn, "BEGIN")
        run(db, conn, f"SELECT price FROM orders WHERE id = {k}")
        run(db, conn, f"UPDATE orders SET quantity = quantity + 1 WHERE id = {k}")
        run(db, conn, f"INSERT INTO orders {ORDER_COLS} VALUES {fresh()}")
        run(db, conn, "COMMIT")

    def mix(conn):
        (point_select if rng.random() < 0.5 else insert_one)(conn)

    return {"point_select": point_select, "point_update": point_update, "insert_one": insert_one,
            "txn": txn, "mix": mix}


def worker_main(db, rows, worker, opname, seconds, barrier, q):
    conn = connect(db)
    op = op_factory(db, rows, worker)[opname]
    try:
        op(conn)
    finally:
        barrier.wait()
    lat, errs = [], 0
    end = time.perf_counter() + seconds
    while True:
        t = time.perf_counter()
        if t >= end:
            break
        try:
            op(conn)
        except Exception as e:
            errs += 1
            if errs == 1:
                print(f"[{db} {opname} w{worker}] {e}", flush=True)
            if errs > 1000:
                break
            try:
                conn = connect(db)
            except Exception:
                pass
            continue
        lat.append(time.perf_counter() - t)
    q.put((lat, errs))


def olap_loop(db, rows, seconds, barrier, q):
    conn = connect(db)
    sql = dict(queries(rows))["Q3 group by region"]
    barrier.wait()
    lat = []
    end = time.perf_counter() + seconds
    while time.perf_counter() < end:
        t = time.perf_counter()
        run(db, conn, sql)
        lat.append(time.perf_counter() - t)
    q.put(("olap", lat))


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def run_workers(db, rows, opname, conc, seconds, with_olap=False):
    ctx = mp.get_context("fork")
    q = ctx.Queue()
    barrier = ctx.Barrier(conc + (1 if with_olap else 0))
    procs = [ctx.Process(target=worker_main, args=(db, rows, w, opname, seconds, barrier, q))
             for w in range(conc)]
    if with_olap:
        procs.append(ctx.Process(target=olap_loop, args=(db, rows, seconds, barrier, q)))
    for p in procs:
        p.start()
    lat, errs, olap = [], 0, None
    for _ in procs:
        item = q.get()
        if item[0] == "olap":
            olap = item[1]
        else:
            lat += item[0]
            errs += item[1]
    for p in procs:
        p.join()
    stats = {"op": opname, "concurrency": conc, "seconds": seconds, "ops": len(lat),
             "ops_per_s": len(lat) / seconds, "errors": errs,
             "p50_ms": (pct(lat, 50) or 0) * 1e3, "p95_ms": (pct(lat, 95) or 0) * 1e3,
             "p99_ms": (pct(lat, 99) or 0) * 1e3}
    if olap is not None:
        stats.update(olap_runs=len(olap), olap_median_s=statistics.median(olap) if olap else None)
    return stats


def cmd_oltp(db, rows, concs):
    ops = ["point_select", "insert_one"]
    if db != "clickhouse":  # ClickHouse has no row UPDATE / multi-statement transactions
        ops += ["point_update", "txn"]
    for opname in ops:
        for conc in concs:
            result("oltp", db, rows, **run_workers(db, rows, opname, conc, OLTP_SECONDS))


def cmd_mixed(db, rows):
    conc = int(os.environ.get("MIX_CONC", "8"))
    secs = float(os.environ.get("MIX_SECONDS", "20"))
    base = run_workers(db, rows, "mix", conc, secs)
    result("mixed", db, rows, phase="oltp_only", **base)
    both = run_workers(db, rows, "mix", conc, secs, with_olap=True)
    result("mixed", db, rows, phase="oltp_plus_olap", **both)


def cmd_size(db):
    conn = connect(db)
    if db == "postgres":
        b = run(db, conn, "SELECT pg_total_relation_size('orders') + pg_total_relation_size('customers')")[0][0]
    elif db == "clickhouse":
        b = run(db, conn, "SELECT sum(bytes_on_disk) FROM system.parts WHERE active AND database = 'default'")[0][0]
    else:
        b = None
    result("size", db, None, bytes=b)


if __name__ == "__main__":
    cmd, db = sys.argv[1], sys.argv[2]
    if cmd == "size":
        cmd_size(db)
        sys.exit()
    rows = int(sys.argv[3])
    if cmd == "load":
        cmd_load(db, rows)
    elif cmd == "olap":
        cmd_olap(db, rows)
    elif cmd == "oltp":
        concs = [int(c) for c in (sys.argv[4] if len(sys.argv) > 4 else "1,8,32").split(",")]
        cmd_oltp(db, rows, concs)
    elif cmd == "mixed":
        cmd_mixed(db, rows)
