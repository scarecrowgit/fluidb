use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    // Query 16 suppliers. Query 21 rows may be added separately without changing
    // these supplier keys or their Query 16 comments.
    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (1, 'Supplier 1', 'Address 1', 1, '13-001-000-0000', 100.00, 'Reliable supplier'),
             (2, 'Supplier 2', 'Address 2', 1, '13-002-000-0000', 100.00, 'Reliable supplier'),
             (3, 'Supplier 3', 'Address 3', 1, '13-003-000-0000', 100.00, 'Supplier with Customer Complaints'),
             (4, 'Supplier 4', 'Address 4', 1, '13-004-000-0000', 100.00, 'Reliable supplier'),
             (5, 'Supplier 5', 'Address 5', 1, '13-005-000-0000', 100.00, 'Reliable supplier'),
             (6, 'Supplier 6', 'Address 6', 1, '13-006-000-0000', 100.00, 'Reliable supplier'),
             (7, 'Supplier 7', 'Address 7', 1, '13-007-000-0000', 100.00, 'Reliable supplier'),
             (8, 'Supplier 8', 'Address 8', 1, '13-008-000-0000', 100.00, 'Reliable supplier'),
             (9, 'Supplier 9', 'Address 9', 1, '13-009-000-0000', 100.00, 'Supplier with Complaints Customer'),
             (10, 'Supplier 10', 'Address 10', 1, '13-010-000-0000', 100.00, 'Reliable supplier')",
        )
        .expect("insert Query 16 suppliers");

    // Query 16 parts. Query 21 rows may be added with distinct keys later.
    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (100, 'Q16 Group One Part A', 'MFGR#1', 'Brand#10', 'SMALL STEEL', 14, 'SM CASE', 100.00, 'Q16 group one'),
             (101, 'Q16 Group One Part B', 'MFGR#1', 'Brand#10', 'SMALL STEEL', 14, 'SM CASE', 100.00, 'Q16 group one'),
             (200, 'Q16 Group Two Part', 'MFGR#2', 'Brand#20', 'BIG WOOD', 49, 'LG CASE', 200.00, 'Q16 group two'),
             (102, 'Q16 Brand Decoy', 'MFGR#1', 'Brand#45', 'SMALL STEEL', 14, 'SM CASE', 100.00, 'Q16 brand decoy'),
             (103, 'Q16 Type Decoy', 'MFGR#1', 'Brand#10', 'MEDIUM POLISHED WOOD', 14, 'SM CASE', 100.00, 'Q16 type decoy'),
             (104, 'Q16 Size Decoy', 'MFGR#1', 'Brand#10', 'SMALL STEEL', 99, 'SM CASE', 100.00, 'Q16 size decoy')",
        )
        .expect("insert Query 16 parts");

    // Query 16 part-supplier relationships.
    server
        .execute(
            "INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_availqty, ps_supplycost, ps_comment) VALUES
             (100, 1, 100, 10.00, 'Q16 group one'),
             (100, 2, 100, 10.00, 'Q16 group one'),
             (101, 2, 100, 10.00, 'Q16 group one duplicate supplier'),
             (101, 3, 100, 10.00, 'Q16 excluded complaint supplier'),
             (101, 4, 100, 10.00, 'Q16 group one'),
             (101, 9, 100, 10.00, 'Q16 reversed comment supplier'),
             (200, 5, 100, 10.00, 'Q16 group two'),
             (200, 6, 100, 10.00, 'Q16 group two'),
             (102, 7, 100, 10.00, 'Q16 brand decoy'),
             (103, 8, 100, 10.00, 'Q16 type decoy'),
             (104, 10, 100, 10.00, 'Q16 size decoy')",
        )
        .expect("insert Query 16 part suppliers");

    // Query 21 nations and suppliers. These rows use distinct order keys and do
    // not change Query 16's part-supplier relationships.
    server
        .execute(
            "INSERT INTO nation (n_nationkey, n_name, n_regionkey, n_comment) VALUES
             (1, 'SAUDI ARABIA', 0, 'Query 21 qualifying nation'),
             (2, 'GERMANY', 0, 'Query 21 non-qualifying nation')",
        )
        .expect("insert Query 21 nations");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (11, 'Supplier 11', 'Address 11', 2, '13-011-000-0000', 100.00, 'Reliable supplier'),
             (12, 'Supplier 12', 'Address 12', 1, '13-012-000-0000', 100.00, 'Reliable supplier')",
        )
        .expect("insert Query 21 suppliers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (2101, 1, 'F', 100.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000000001', 0, 'Q21 qualifying order'),
             (2102, 1, 'F', 100.00, DATE '1995-01-02', '1-URGENT', 'Clerk#000000001', 0, 'Q21 vetoed order'),
             (2103, 1, 'O', 100.00, DATE '1995-01-03', '1-URGENT', 'Clerk#000000001', 0, 'Q21 wrong-status order'),
             (2104, 1, 'F', 100.00, DATE '1995-01-04', '1-URGENT', 'Clerk#000000001', 0, 'Q21 foreign-supplier order'),
             (2105, 1, 'F', 100.00, DATE '1995-01-05', '1-URGENT', 'Clerk#000000001', 0, 'Q21 equal-date order'),
             (2106, 1, 'F', 100.00, DATE '1995-01-06', '1-URGENT', 'Clerk#000000001', 0, 'Q21 single-supplier order')",
        )
        .expect("insert Query 21 orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (2101, 1, 100, 1, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q21 qualifying late line'),
             (2101, 2, 101, 1, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-04', 'DELIVER IN PERSON', 'MAIL', 'Q21 same-supplier late line'),
             (2101, 3, 200, 2, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-03', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q21 other-supplier on-time line'),
             (2102, 1, 100, 4, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-02', DATE '1995-01-03', DATE '1995-01-04', 'DELIVER IN PERSON', 'MAIL', 'Q21 candidate vetoed by other late line'),
             (2102, 2, 101, 5, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-02', DATE '1995-01-03', DATE '1995-01-04', 'DELIVER IN PERSON', 'MAIL', 'Q21 vetoing other late line'),
             (2103, 1, 100, 6, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-03', DATE '1995-01-04', DATE '1995-01-05', 'DELIVER IN PERSON', 'MAIL', 'Q21 wrong-status late line'),
             (2103, 2, 101, 7, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-03', DATE '1995-01-05', DATE '1995-01-05', 'DELIVER IN PERSON', 'MAIL', 'Q21 wrong-status companion'),
             (2104, 1, 100, 11, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-04', DATE '1995-01-05', DATE '1995-01-06', 'DELIVER IN PERSON', 'MAIL', 'Q21 foreign late line'),
             (2104, 2, 101, 8, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-04', DATE '1995-01-06', DATE '1995-01-06', 'DELIVER IN PERSON', 'MAIL', 'Q21 foreign companion'),
             (2105, 1, 100, 9, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-05', DATE '1995-01-06', DATE '1995-01-06', 'DELIVER IN PERSON', 'MAIL', 'Q21 equal receipt and commit'),
             (2105, 2, 101, 6, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-05', DATE '1995-01-07', DATE '1995-01-06', 'DELIVER IN PERSON', 'MAIL', 'Q21 strict-comparison companion'),
             (2106, 1, 100, 12, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-06', DATE '1995-01-07', DATE '1995-01-08', 'DELIVER IN PERSON', 'MAIL', 'Q21 single-supplier late line')",
        )
        .expect("insert Query 21 line items");

    (directory, server)
}
