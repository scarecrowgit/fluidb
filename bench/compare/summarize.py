"""Render results.jsonl as markdown tables (stdout)."""

import json
import sys
from collections import defaultdict

DBS = ["fluidb", "postgres", "clickhouse"]
path = sys.argv[1] if len(sys.argv) > 1 else "results.jsonl"
recs = [json.loads(line) for line in open(path)]
last_rows = {}
for r in recs:  # `size` lines carry no scale: attribute them to the db's preceding run
    if r.get("rows"):
        last_rows[r["db"]] = r["rows"]
    elif r["kind"] == "size":
        r["rows"] = last_rows.get(r["db"])
scales = sorted({r["rows"] for r in recs if r.get("rows")})


def fmt_s(s):
    if s is None:
        return "—"
    if s < 1:
        return f"{s * 1e3:.1f} ms"
    return f"{s:.2f} s"


def fmt_n(n):
    return "—" if n is None else f"{n:,.0f}"


def mb(b):
    return "—" if b in (None, "null") else f"{b / 2**20:,.0f} MiB"


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out)


for n in scales:
    R = [r for r in recs if r.get("rows") == n]
    print(f"\n### {n:,} orders rows\n")

    load = {r["db"]: r for r in R if r["kind"] == "load"}
    size = {r["db"]: r for r in R if r["kind"] == "size"}
    mem = defaultdict(dict)
    for r in R:
        if r["kind"] == "mem":
            mem[r["db"]][r["phase"]] = r["peak_bytes"]
    print("#### Load, storage, memory\n")
    print(table(["", *DBS], [
        ["INSERT load (5k-row batches), rows/s", *[fmt_n(load.get(d, {}).get("orders_rows_per_s")) for d in DBS]],
        ["post-load step", *[fmt_s(load.get(d, {}).get("post_load_s")) for d in DBS]],
        ["on-disk size", *[mb(size.get(d, {}).get("bytes")) for d in DBS]],
        ["peak memory (whole run)", *[mb(mem[d].get("all")) for d in DBS]],
    ]))

    olap = defaultdict(dict)
    for r in R:
        if r["kind"] == "olap":
            olap[r["query"]][r["db"]] = r
    print("\n#### Analytical queries (median wall time; ratio = fluidb / best other)\n")
    rows, mismatch = [], []
    for q, by in olap.items():
        cells = []
        for d in DBS:
            r = by.get(d)
            cells.append("error" if r is None or "error" in r else fmt_s(r["median_s"]))
        others = [by[d]["median_s"] for d in DBS[1:] if d in by and "median_s" in by[d]]
        f = by.get("fluidb", {}).get("median_s")
        ratio = f"{f / min(others):,.0f}×" if f and others else "—"
        rows.append([q, *cells, ratio])
        answers = {json.dumps(by[d].get("answer")) for d in DBS if d in by and "answer" in by[d]}
        if len(answers) > 1:
            mismatch.append(q)
    print(table(["query", *DBS, "fluidb slowdown"], rows))
    print(f"\nAnswers identical across engines: {'yes' if not mismatch else 'NO: ' + ', '.join(mismatch)}")
    errs = [(q, d, by[d]["error"]) for q, by in olap.items() for d in by if "error" in by[d]]
    for q, d, e in errs:
        print(f"\n- {d} {q}: `{e}`")

    oltp = defaultdict(dict)
    for r in R:
        if r["kind"] == "oltp":
            oltp[(r["op"], r["concurrency"])][r["db"]] = r
    print("\n#### OLTP (10 s per cell): throughput ops/s · p99 latency\n")
    rows = []
    for (op, c), by in sorted(oltp.items(), key=lambda kv: (["point_select", "point_update", "insert_one", "txn"].index(kv[0][0]), kv[0][1])):
        cells = []
        for d in DBS:
            r = by.get(d)
            cells.append("n/a" if r is None else f"{fmt_n(r['ops_per_s'])} · {r['p99_ms']:.2f} ms"
                         + (f" ({r['errors']} err)" if r["errors"] else ""))
        rows.append([op, str(c), *cells])
    print(table(["operation", "clients", *DBS], rows))

    mixed = defaultdict(dict)
    for r in R:
        if r["kind"] == "mixed":
            mixed[r["db"]][r["phase"]] = r
    print("\n#### Mixed HTAP (8 clients 50/50 point-select/insert, + 1 client looping Q3)\n")
    rows = []
    for d in DBS:
        a, b = mixed[d].get("oltp_only"), mixed[d].get("oltp_plus_olap")
        if not a or not b:
            continue
        iso = olap.get("Q3 group by region", {}).get(d, {}).get("median_s")
        rows.append([d, fmt_n(a["ops_per_s"]), f"{a['p99_ms']:.2f} ms", fmt_n(b["ops_per_s"]),
                     f"{b['p99_ms']:.2f} ms", f"{(1 - b['ops_per_s'] / a['ops_per_s']) * 100:.0f}%",
                     fmt_s(iso), fmt_s(b.get("olap_median_s")), str(b.get("olap_runs"))])
    print(table(["engine", "OLTP ops/s alone", "p99 alone", "OLTP ops/s with Q3", "p99 with Q3",
                 "OLTP drop", "Q3 alone", "Q3 under OLTP", "Q3 runs"], rows))
