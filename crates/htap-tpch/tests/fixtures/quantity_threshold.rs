use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    // Query 18 customers. Customer 201 owns no order and proves that
    // c_custkey = o_custkey is required.
    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (101, 'Q18 Qualifying Customer', 'Address 101', 1, '13-101-000-0000', 100.00, 'BUILDING', 'owns qualifying order'),
             (102, 'Q18 Threshold Customer', 'Address 102', 1, '13-102-000-0000', 100.00, 'BUILDING', 'owns threshold order'),
             (103, 'Q18 Low Total Customer', 'Address 103', 1, '13-103-000-0000', 100.00, 'BUILDING', 'owns low-total order'),
             (104, 'Q18 Line Join Decoy Customer', 'Address 104', 1, '13-104-000-0000', 100.00, 'BUILDING', 'owns line join decoy order'),
             (201, 'Q18 Unmatched Customer', 'Address 201', 1, '13-201-000-0000', 100.00, 'BUILDING', 'owns no order')",
        )
        .expect("insert Query 18 customers");

    // Query 18 orders. Order 1801 is the sole intended result. Order 1802 is
    // exactly at the HAVING threshold; 1803 is another below-threshold decoy;
    // and 1804 supplies lines that prove the order-lineitem join is required.
    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1801, 101, 'F', 900.50, DATE '1995-03-15', '1-URGENT', 'Clerk#000001801', 0, 'Q18 qualifying total'),
             (1802, 102, 'F', 800.00, DATE '1995-03-16', '2-HIGH', 'Clerk#000001802', 0, 'Q18 exact threshold'),
             (1803, 103, 'F', 700.00, DATE '1995-03-17', '3-MEDIUM', 'Clerk#000001803', 0, 'Q18 below threshold'),
             (1804, 104, 'F', 600.00, DATE '1995-03-18', '4-NOT SPECIFIED', 'Clerk#000001804', 0, 'Q18 line join decoy')",
        )
        .expect("insert Query 18 orders");

    // Query 18 line items. Order 1801 totals 150.50 + 150.00 = 300.50, while
    // neither individual line exceeds 300. Order 1802 totals exactly 300.00 and
    // proves the comparison is strict. Order 1804 totals only 200.00, so the
    // independent lineitem scan in the IN subquery cannot admit it; if the
    // outer order-lineitem equality is removed, its two lines instead contaminate
    // order 1801's aggregate.
    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1801, 1, 201, 1, 150.50, 150.50, 0.00, 0.00, 'N', 'O', DATE '1995-03-01', DATE '1995-03-02', DATE '1995-03-03', 'DELIVER IN PERSON', 'MAIL', 'Q18 qualifying first line'),
             (1801, 2, 202, 1, 150.00, 150.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-01', DATE '1995-03-02', DATE '1995-03-03', 'DELIVER IN PERSON', 'MAIL', 'Q18 qualifying second line'),
             (1802, 1, 203, 1, 200.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-02', DATE '1995-03-03', DATE '1995-03-04', 'DELIVER IN PERSON', 'MAIL', 'Q18 threshold first line'),
             (1802, 2, 204, 1, 100.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-02', DATE '1995-03-03', DATE '1995-03-04', 'DELIVER IN PERSON', 'MAIL', 'Q18 threshold second line'),
             (1803, 1, 205, 1, 150.00, 150.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-03', DATE '1995-03-04', DATE '1995-03-05', 'DELIVER IN PERSON', 'MAIL', 'Q18 low total first line'),
             (1803, 2, 206, 1, 149.99, 149.99, 0.00, 0.00, 'N', 'O', DATE '1995-03-03', DATE '1995-03-04', DATE '1995-03-05', 'DELIVER IN PERSON', 'MAIL', 'Q18 low total second line'),
             (1804, 1, 207, 1, 75.00, 75.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-04', DATE '1995-03-05', DATE '1995-03-06', 'DELIVER IN PERSON', 'MAIL', 'Q18 line join decoy first line'),
             (1804, 2, 208, 1, 125.00, 125.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-04', DATE '1995-03-05', DATE '1995-03-06', 'DELIVER IN PERSON', 'MAIL', 'Q18 line join decoy second line')",
        )
        .expect("insert Query 18 line items");

    (directory, server)
}

pub fn load_q17_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    // Part 1701 is the intended Q17 part. Parts 1702 and 1703 prove that both
    // part predicates are required, while part 1704 proves the quantity average
    // is correlated to the candidate line's part key rather than global.
    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (1701, 'Q17 qualifying part', 'Manufacturer#1', 'Brand#23', 'PROMO ANODIZED STEEL', 1, 'MED BOX', 100.00, 'Q17 qualifying part'),
             (1702, 'Q17 wrong brand part', 'Manufacturer#1', 'Brand#99', 'PROMO ANODIZED STEEL', 1, 'MED BOX', 100.00, 'Q17 wrong brand decoy'),
             (1703, 'Q17 wrong container part', 'Manufacturer#1', 'Brand#23', 'PROMO ANODIZED STEEL', 1, 'LG BOX', 100.00, 'Q17 wrong container decoy'),
             (1704, 'Q17 unrelated quantity part', 'Manufacturer#1', 'Brand#88', 'PROMO ANODIZED STEEL', 1, 'SM BOX', 100.00, 'Q17 correlation decoy')",
        )
        .expect("insert Query 17 parts");

    // For part 1701, the correlated average is (1.00 + 9.00 + 20.00) / 3 =
    // 10.00, so 0.2 * average is 2.000000. Only the 1.00-quantity line is
    // below that threshold. Parts 1702 and 1703 each have a quantity-1.00 line
    // that would qualify on quantity alone. Part 1704's 1000.00 quantity makes
    // a global average much larger than the intended correlated threshold.
    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1701, 1, 1701, 1, 1.00, 70.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 qualifying line'),
             (1701, 2, 1701, 1, 9.00, 90.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 nonqualifying line'),
             (1701, 3, 1701, 1, 20.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 nonqualifying line'),
             (1702, 1, 1702, 1, 1.00, 70.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong brand qualifying line'),
             (1702, 2, 1702, 1, 9.00, 90.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong brand nonqualifying line'),
             (1702, 3, 1702, 1, 20.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong brand nonqualifying line'),
             (1703, 1, 1703, 1, 1.00, 70.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong container qualifying line'),
             (1703, 2, 1703, 1, 9.00, 90.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong container nonqualifying line'),
             (1703, 3, 1703, 1, 20.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 wrong container nonqualifying line'),
             (1704, 1, 1704, 1, 1000.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 unrelated quantity line')",
        )
        .expect("insert Query 17 line items");

    (directory, server)
}

pub fn load_q17_empty_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (1711, 'Q17 empty aggregate part', 'Manufacturer#1', 'Brand#23', 'PROMO ANODIZED STEEL', 1, 'MED BOX', 100.00, 'Q17 empty aggregate part')",
        )
        .expect("insert Query 17 empty aggregate part");

    // The single line's correlated average is its own quantity, 10.00, so its
    // threshold is 0.2 * 10.00 = 2.000000 and 10.00 < 2.000000 is false.
    // With the quantity threshold removed, the actual output row is 70.000000
    // because 490.00 / 7.0 = 70.000000.
    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1711, 1, 1711, 1, 10.00, 490.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q17 empty aggregate line')",
        )
        .expect("insert Query 17 empty aggregate line");

    (directory, server)
}
