//! TPC-H query text reproduced by permission of the Transaction Processing
//! Performance Council for regression-testing purposes.
//! Copyright © 1993 - 2022 Transaction Processing Performance Council.
//! TPC BENCHMARK H (Decision Support) Standard Specification,
//! Revision 3.0.1, 28 April 2022. Query definitions: Clause 2.4,
//! sub-clauses 2.4.1 through 2.4.22.
//! Regression coverage for canonical TPC-H correlated subqueries.

use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn rows(server: &LocalServer, sql: &str) -> Vec<Row> {
    match server.execute(sql) {
        Ok(StatementResult::Query(query)) => query.rows,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn text(value: &str) -> Value {
    Value::String(value.into())
}

#[test]
fn test_tpch_q2_minimum_cost_supplier_per_part() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE part (p_partkey INT PRIMARY KEY, p_mfgr VARCHAR(16), p_size INT, p_type VARCHAR(32));")
        .unwrap();
    server
        .execute("CREATE TABLE supplier (s_suppkey INT PRIMARY KEY, s_name VARCHAR(32), s_address VARCHAR(32), s_nationkey INT, s_phone VARCHAR(32), s_acctbal DOUBLE, s_comment VARCHAR(64));")
        .unwrap();
    server
        .execute("CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_supplycost DOUBLE, PRIMARY KEY (ps_partkey, ps_suppkey));")
        .unwrap();
    server
        .execute("CREATE TABLE nation (n_nationkey INT PRIMARY KEY, n_name VARCHAR(32), n_regionkey INT);")
        .unwrap();
    server
        .execute("CREATE TABLE region (r_regionkey INT PRIMARY KEY, r_name VARCHAR(32));")
        .unwrap();

    server
        .execute("INSERT INTO region (r_regionkey, r_name) VALUES (1, 'EUROPE'), (2, 'ASIA');")
        .unwrap();
    server
        .execute("INSERT INTO nation (n_nationkey, n_name, n_regionkey) VALUES (10, 'GERMANY', 1), (20, 'CHINA', 2);")
        .unwrap();
    server
        .execute("INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES (1, 'Low Cost', 'A', 10, '11-111', 100.0, 'ok'), (2, 'High Cost', 'B', 10, '22-222', 200.0, 'ok'), (3, 'Outside', 'C', 20, '33-333', 300.0, 'ok');")
        .unwrap();
    server
        .execute("INSERT INTO part (p_partkey, p_mfgr, p_size, p_type) VALUES (100, 'MFGR#1', 15, 'STANDARD BRASS'), (101, 'MFGR#2', 15, 'STANDARD STEEL');")
        .unwrap();
    server
        .execute("INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_supplycost) VALUES (100, 1, 10.0), (100, 2, 20.0), (100, 3, 1.0), (101, 1, 1.0);")
        .unwrap();

    // Part 100 is the only size-15 BRASS part. Among its EUROPE suppliers, supplier
    // 1 costs 10 and supplier 2 costs 20, so the correlated minimum-cost predicate
    // retains only supplier 1; supplier 3 is outside EUROPE.
    assert_eq!(
        rows(
            &server,
            "select
    s_acctbal,
    s_name,
    n_name,
    p_partkey,
    p_mfgr,
    s_address,
    s_phone,
    s_comment
from
    part,
    supplier,
    partsupp,
    nation,
    region
where
    p_partkey = ps_partkey
    and s_suppkey = ps_suppkey
    and p_size = 15
    and p_type like '%BRASS'
    and s_nationkey = n_nationkey
    and n_regionkey = r_regionkey
    and r_name = 'EUROPE'
    and ps_supplycost = (
        select
            min(ps_supplycost)
        from
            partsupp,
            supplier,
            nation,
            region
        where
            p_partkey = ps_partkey
            and s_suppkey = ps_suppkey
            and s_nationkey = n_nationkey
            and n_regionkey = r_regionkey
            and r_name = 'EUROPE'
    )
order by
    s_acctbal desc,
    n_name,
    s_name,
    p_partkey
limit 100;",
        ),
        vec![Row::new(vec![
            Value::Float64(100.0),
            text("Low Cost"),
            text("GERMANY"),
            Value::Int32(100),
            text("MFGR#1"),
            text("A"),
            text("11-111"),
            text("ok"),
        ])]
    );
}

#[test]
fn test_tpch_q4_order_priority_with_late_lineitem() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE orders (o_orderkey INT PRIMARY KEY, o_orderdate DATE, o_orderpriority VARCHAR(32));")
        .unwrap();
    server
        .execute("CREATE TABLE lineitem (l_orderkey INT, l_linenumber INT, l_commitdate DATE, l_receiptdate DATE, PRIMARY KEY (l_orderkey, l_linenumber));")
        .unwrap();

    server
        .execute("INSERT INTO orders (o_orderkey, o_orderdate, o_orderpriority) VALUES (1, DATE '1993-08-01', '1-URGENT'), (2, DATE '1993-09-01', '2-HIGH'), (3, DATE '1993-10-01', '1-URGENT');")
        .unwrap();
    server
        .execute("INSERT INTO lineitem (l_orderkey, l_linenumber, l_commitdate, l_receiptdate) VALUES (1, 1, DATE '1993-08-05', DATE '1993-08-07'), (2, 1, DATE '1993-09-07', DATE '1993-09-07'), (3, 1, DATE '1993-10-02', DATE '1993-10-04');")
        .unwrap();

    // Orders 1 and 2 are in the quarter, but only order 1 has commitdate before
    // receiptdate. Order 3 is late but lies on the exclusive upper date boundary.
    assert_eq!(
        rows(
            &server,
            "select
    o_orderpriority,
    count(*) as order_count
from
    orders
where
    o_orderdate >= date '1993-07-01'
    and o_orderdate < date '1993-10-01'
    and exists (
        select
            *
        from
            lineitem
        where
            l_orderkey = o_orderkey
            and l_commitdate < l_receiptdate
    )
group by
    o_orderpriority
order by
    o_orderpriority;",
        ),
        vec![Row::new(vec![text("1-URGENT"), Value::Int64(1)])]
    );
}

#[test]
fn test_tpch_q17_small_quantity_order_revenue() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE part (p_partkey INT PRIMARY KEY, p_brand VARCHAR(32), p_container VARCHAR(32));")
        .unwrap();
    server
        .execute("CREATE TABLE lineitem (l_orderkey INT, l_linenumber INT, l_partkey INT, l_quantity INT, l_extendedprice DOUBLE, PRIMARY KEY (l_orderkey, l_linenumber));")
        .unwrap();

    server
        .execute("INSERT INTO part (p_partkey, p_brand, p_container) VALUES (1, 'Brand#23', 'MED BOX'), (2, 'Brand#23', 'SM BOX');")
        .unwrap();
    server
        .execute("INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_quantity, l_extendedprice) VALUES (1, 1, 1, 1, 100.0), (2, 1, 1, 5, 200.0), (3, 1, 1, 20, 300.0), (4, 1, 1, 20, 400.0), (5, 1, 2, 100, 999.0), (6, 1, 2, 100, 999.0);")
        .unwrap();

    // Part 1's average quantity is 11.5, so its threshold is 2.3 and only its
    // quantity-1 line qualifies. Across all lineitems the average is 41, making
    // the uncorrelated threshold 8.2 and incorrectly admitting Part 1's
    // quantity-5 line as well.
    assert_eq!(
        rows(
            &server,
            "select
    sum(l_extendedprice) / 7.0 as avg_yearly
from
    lineitem,
    part
where
    p_partkey = l_partkey
    and p_brand = 'Brand#23'
    and p_container = 'MED BOX'
    and l_quantity < (
        select
            0.2 * avg(l_quantity)
        from
            lineitem
        where
            l_partkey = p_partkey
    );",
        ),
        vec![Row::new(vec![Value::Float64(100.0 / 7.0)])]
    );
}

#[test]
fn test_tpch_q20_potential_part_promotion() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE supplier (s_suppkey INT PRIMARY KEY, s_name VARCHAR(32), s_address VARCHAR(32), s_nationkey INT);")
        .unwrap();
    server
        .execute("CREATE TABLE nation (n_nationkey INT PRIMARY KEY, n_name VARCHAR(32));")
        .unwrap();
    server
        .execute("CREATE TABLE part (p_partkey INT PRIMARY KEY, p_name VARCHAR(64));")
        .unwrap();
    server
        .execute("CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_availqty INT, PRIMARY KEY (ps_partkey, ps_suppkey));")
        .unwrap();
    server
        .execute("CREATE TABLE lineitem (l_orderkey INT, l_linenumber INT, l_partkey INT, l_suppkey INT, l_quantity INT, l_shipdate DATE, PRIMARY KEY (l_orderkey, l_linenumber));")
        .unwrap();

    server
        .execute("INSERT INTO nation (n_nationkey, n_name) VALUES (1, 'CANADA'), (2, 'BRAZIL');")
        .unwrap();
    server
        .execute("INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey) VALUES (1, 'Qualified', 'North', 1), (2, 'Insufficient', 'North', 1), (3, 'Foreign', 'South', 2), (4, 'Non Forest', 'North', 1);")
        .unwrap();
    server
        .execute(
            "INSERT INTO part (p_partkey, p_name) VALUES (10, 'forest green'), (11, 'blue steel');",
        )
        .unwrap();
    server
        .execute("INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_availqty) VALUES (10, 1, 6), (10, 2, 4), (10, 3, 10), (11, 1, 100), (11, 2, 4), (11, 4, 6);")
        .unwrap();
    server
        .execute("INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_shipdate) VALUES (1, 1, 10, 1, 10, DATE '1994-06-01'), (2, 1, 10, 2, 10, DATE '1994-06-01'), (3, 1, 10, 3, 10, DATE '1994-06-01'), (4, 1, 10, 1, 99, DATE '1995-01-01'), (5, 1, 11, 2, 10, DATE '1994-06-01'), (6, 1, 11, 4, 10, DATE '1994-06-01');")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "select
    s_name,
    s_address
from
    supplier,
    nation
where
    s_suppkey in (
        select
            ps_suppkey
        from
            partsupp,
            part
        where
            ps_partkey = p_partkey
            and p_name like 'forest%'
            and ps_availqty > (
                select
                    0.5 * sum(l_quantity)
                from
                    lineitem
                where
                    l_partkey = ps_partkey
                    and l_suppkey = ps_suppkey
                    and l_shipdate >= date '1994-01-01'
                    and l_shipdate < date '1995-01-01'
            )
    )
    and s_nationkey = n_nationkey
    and n_name = 'CANADA'
order by
    s_name;",
        ),
        vec![Row::new(vec![text("Qualified"), text("North")])]
    );
}

#[test]
fn test_tpch_q21_suppliers_with_mixed_delivery_orders() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE supplier (s_suppkey INT PRIMARY KEY, s_name VARCHAR(32), s_nationkey INT);")
        .unwrap();
    server
        .execute("CREATE TABLE nation (n_nationkey INT PRIMARY KEY, n_name VARCHAR(32));")
        .unwrap();
    server
        .execute("CREATE TABLE orders (o_orderkey INT PRIMARY KEY, o_orderstatus VARCHAR(2));")
        .unwrap();
    server
        .execute("CREATE TABLE lineitem (l_orderkey INT, l_linenumber INT, l_suppkey INT, l_commitdate DATE, l_receiptdate DATE, PRIMARY KEY (l_orderkey, l_linenumber));")
        .unwrap();

    server
        .execute(
            "INSERT INTO nation (n_nationkey, n_name) VALUES (1, 'SAUDI ARABIA'), (2, 'EGYPT');",
        )
        .unwrap();
    server
        .execute("INSERT INTO supplier (s_suppkey, s_name, s_nationkey) VALUES (1, 'Late Only', 1), (2, 'On Time', 1), (3, 'Foreign Late', 2);")
        .unwrap();
    server
        .execute("INSERT INTO orders (o_orderkey, o_orderstatus) VALUES (100, 'F'), (101, 'O'), (102, 'F'), (103, 'F');")
        .unwrap();
    server
        .execute("INSERT INTO lineitem (l_orderkey, l_linenumber, l_suppkey, l_commitdate, l_receiptdate) VALUES (100, 1, 1, DATE '1994-01-01', DATE '1994-01-03'), (100, 2, 2, DATE '1994-01-01', DATE '1994-01-01'), (101, 1, 1, DATE '1994-01-01', DATE '1994-01-03'), (101, 2, 3, DATE '1994-01-01', DATE '1994-01-01'), (102, 1, 2, DATE '1994-01-01', DATE '1994-01-03'), (103, 1, 1, DATE '1994-01-01', DATE '1994-01-03'), (103, 2, 2, DATE '1994-01-01', DATE '1994-01-03');")
        .unwrap();

    // Fulfilled order 100 has a late shipment from supplier 1 and an on-time shipment
    // from supplier 2, so supplier 1 is counted once. Fulfilled order 102 has only
    // supplier 2 and is excluded by EXISTS. On fulfilled order 103 both suppliers
    // are late and are excluded by NOT EXISTS. Order 101 is not fulfilled.
    assert_eq!(
        rows(
            &server,
            "select
    s_name,
    count(*) as numwait
from
    supplier,
    lineitem l1,
    orders,
    nation
where
    s_suppkey = l1.l_suppkey
    and o_orderkey = l1.l_orderkey
    and o_orderstatus = 'F'
    and l1.l_receiptdate > l1.l_commitdate
    and exists (
        select
            *
        from
            lineitem l2
        where
            l2.l_orderkey = l1.l_orderkey
            and l2.l_suppkey <> l1.l_suppkey
    )
    and not exists (
        select
            *
        from
            lineitem l3
        where
            l3.l_orderkey = l1.l_orderkey
            and l3.l_suppkey <> l1.l_suppkey
            and l3.l_receiptdate > l3.l_commitdate
    )
    and s_nationkey = n_nationkey
    and n_name = 'SAUDI ARABIA'
group by
    s_name
order by
    numwait desc,
    s_name
limit 100;",
        ),
        vec![Row::new(vec![text("Late Only"), Value::Int64(1)])]
    );
}

#[test]
fn test_tpch_q22_revenue_by_customer_demographics() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE customer (c_custkey INT PRIMARY KEY, c_phone VARCHAR(32), c_acctbal DOUBLE);")
        .unwrap();
    server
        .execute("CREATE TABLE orders (o_orderkey INT PRIMARY KEY, o_custkey INT);")
        .unwrap();

    server
        .execute("INSERT INTO customer (c_custkey, c_phone, c_acctbal) VALUES (1, '13-111-111', 100.0), (2, '13-222-222', 300.0), (3, '31-333-333', 200.0), (4, '99-444-444', 1000.0), (5, '13-555-555', 400.0);")
        .unwrap();
    server
        .execute("INSERT INTO orders (o_orderkey, o_custkey) VALUES (10, 1), (11, 5);")
        .unwrap();

    // The eligible country-code customers with positive balances have average
    // (100 + 300 + 200 + 400) / 4 = 250. Customers 2 and 5 are above average,
    // but customer 5 has an order. The anti-join retains only customer 2, so
    // country code 13 contributes one customer and 300.0 revenue.
    assert_eq!(
        rows(
            &server,
            "select
    cntrycode,
    count(*) as numcust,
    sum(c_acctbal) as totacctbal
from
    (
        select
            substring(c_phone from 1 for 2) as cntrycode,
            c_acctbal
        from
            customer
        where
            substring(c_phone from 1 for 2) in
                ('13', '31', '23', '29', '30', '18', '17')
            and c_acctbal > (
                select
                    avg(c_acctbal)
                from
                    customer
                where
                    c_acctbal > 0.00
                    and substring(c_phone from 1 for 2) in
                        ('13', '31', '23', '29', '30', '18', '17')
            )
            and not exists (
                select
                    *
                from
                    orders
                where
                    o_custkey = c_custkey
            )
    ) as custsale
group by
    cntrycode
order by
    cntrycode;",
        ),
        vec![Row::new(vec![
            text("13"),
            Value::Int64(1),
            Value::Float64(300.0),
        ])]
    );
}
