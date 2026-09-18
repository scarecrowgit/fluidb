"""Load 200k rows into a fresh fluidb, then do 200 single-row autocommit INSERTs (strace window)."""
import sys
import time

sys.argv = ["bench.py"]
import bench

db, rows = "fluidb", 200_000
conn = bench.connect(db)
for s in bench.ddl(db):
    bench.run(db, conn, s)
for start in range(1, rows + 1, bench.BATCH):
    vals = ",".join(bench.order_values(bench.order_row(i, rows)) for i in range(start, start + bench.BATCH))
    bench.run(db, conn, f"INSERT INTO orders {bench.ORDER_COLS} VALUES {vals}")
open("/bench/diag.ready", "w").close()
time.sleep(3)  # strace attaches here
t = time.perf_counter()
for i in range(200):
    bench.run(db, conn, f"INSERT INTO orders {bench.ORDER_COLS} VALUES {bench.order_values(bench.order_row(10**9 + i, rows))}")
print(f"200 single-row inserts: {(time.perf_counter() - t) / 200 * 1e3:.2f} ms each")
