use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    server
        .execute(
            "INSERT INTO region (r_regionkey, r_name, r_comment) VALUES
             (2, 'ASIA', 'Q5 shared Asia region'),
             (3, 'EUROPE', 'Q5 shared Europe region'),
             (1, 'AMERICA', 'Q5 shared America region')",
        )
        .expect("insert shared regions");

    server
        .execute(
            "INSERT INTO nation (n_nationkey, n_name, n_regionkey, n_comment) VALUES
             (1, 'INDIA', 2, 'Q5 owning nation'),
             (2, 'INDONESIA', 2, 'Q5 owning nation'),
             (3, 'FRANCE', 3, 'Q5 Europe decoy and Q7 owning nation'),
             (4, 'GERMANY', 3, 'Q7 owning nation'),
             (5, 'BRAZIL', 1, 'Q8 owning nation'),
             (6, 'PERU', 1, 'Q8 owning nation')",
        )
        .expect("insert shared nations");

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (1, 'Q5 India Customer', 'Address 1', 1, '13-001-000-0000', 100.00, 'BUILDING', 'Q5 INDIA customer'),
             (2, 'Q5 Indonesia Customer', 'Address 2', 2, '13-002-000-0000', 100.00, 'BUILDING', 'Q5 INDONESIA customer'),
             (3, 'Q5 Different Asia Customer', 'Address 3', 1, '13-003-000-0000', 100.00, 'BUILDING', 'Q5 different Asia nation decoy'),
             (4, 'Q5 France Customer', 'Address 4', 3, '33-004-000-0000', 100.00, 'BUILDING', 'Q5 Europe region decoy'),
             (5, 'Q5 Before Window Customer', 'Address 5', 1, '13-005-000-0000', 100.00, 'BUILDING', 'Q5 lower date boundary decoy'),
             (6, 'Q5 After Window Customer', 'Address 6', 2, '13-006-000-0000', 100.00, 'BUILDING', 'Q5 upper date boundary decoy')",
        )
        .expect("insert Q5 customers");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (1, 'Q5 India Supplier', 'Address 1', 1, '13-001-000-0000', 100.00, 'Q5 INDIA supplier'),
             (2, 'Q5 Indonesia Supplier', 'Address 2', 2, '13-002-000-0000', 100.00, 'Q5 INDONESIA supplier'),
             (3, 'Q5 Different Asia Supplier', 'Address 3', 2, '13-003-000-0000', 100.00, 'Q5 different Asia nation decoy'),
             (4, 'Q5 France Supplier', 'Address 4', 3, '33-004-000-0000', 100.00, 'Q5 Europe region decoy'),
             (5, 'Q5 Before Window Supplier', 'Address 5', 1, '13-005-000-0000', 100.00, 'Q5 lower date boundary decoy'),
             (6, 'Q5 After Window Supplier', 'Address 6', 2, '13-006-000-0000', 100.00, 'Q5 upper date boundary decoy')",
        )
        .expect("insert Q5 suppliers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1, 1, 'F', 300.00, DATE '1994-06-15', '1-URGENT', 'Clerk#000000001', 0, 'Q5 INDIA group with two line items, sum proves accumulation'),
             (2, 2, 'F', 500.00, DATE '1994-07-20', '1-URGENT', 'Clerk#000000002', 0, 'Q5 INDONESIA group, higher revenue to test DESC ordering'),
             (3, 3, 'F', 300.00, DATE '1994-08-01', '1-URGENT', 'Clerk#000000003', 0, 'Q5 decoy different ASIA nations'),
             (4, 4, 'F', 600.00, DATE '1994-09-15', '1-URGENT', 'Clerk#000000004', 0, 'Q5 decoy France Europe region'),
             (5, 5, 'F', 400.00, DATE '1993-12-31', '1-URGENT', 'Clerk#000000005', 0, 'Q5 decoy before window start'),
             (6, 6, 'F', 700.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000000006', 0, 'Q5 decoy at exclusive window end')",
        )
        .expect("insert Q5 orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1, 1, 1, 1, 1.00, 100.00, 0.10, 0.00, 'N', 'O', DATE '1994-06-15', DATE '1994-06-16', DATE '1994-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q5 INDIA first accumulated line'),
             (1, 2, 2, 1, 1.00, 200.00, 0.05, 0.00, 'N', 'O', DATE '1994-06-15', DATE '1994-06-16', DATE '1994-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q5 INDIA second accumulated line'),
             (2, 1, 3, 2, 1.00, 500.00, 0.15, 0.00, 'N', 'O', DATE '1994-07-20', DATE '1994-07-21', DATE '1994-07-22', 'DELIVER IN PERSON', 'MAIL', 'Q5 INDONESIA higher revenue line'),
             (3, 1, 4, 3, 1.00, 300.00, 0.10, 0.00, 'N', 'O', DATE '1994-08-01', DATE '1994-08-02', DATE '1994-08-03', 'DELIVER IN PERSON', 'MAIL', 'Q5 decoy customer INDIA supplier INDONESIA'),
             (4, 1, 5, 4, 1.00, 600.00, 0.05, 0.00, 'N', 'O', DATE '1994-09-15', DATE '1994-09-16', DATE '1994-09-17', 'DELIVER IN PERSON', 'MAIL', 'Q5 decoy customer and supplier France Europe'),
             (5, 1, 6, 5, 1.00, 400.00, 0.10, 0.00, 'N', 'O', DATE '1993-12-31', DATE '1994-01-01', DATE '1994-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q5 decoy before lower date boundary'),
             (6, 1, 7, 6, 1.00, 700.00, 0.20, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q5 decoy at upper date boundary')",
        )
        .expect("insert Q5 line items");

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (10, 'Q7 Germany Customer', 'Address 10', 4, '49-010-000-0000', 100.00, 'BUILDING', 'Q7 GERMANY customer'),
             (11, 'Q7 France Customer', 'Address 11', 3, '33-011-000-0000', 100.00, 'BUILDING', 'Q7 FRANCE customer'),
             (12, 'Q7 France Same Nation Customer', 'Address 12', 3, '33-012-000-0000', 100.00, 'BUILDING', 'Q7 same nation France decoy'),
             (13, 'Q7 Peru Customer', 'Address 13', 6, '51-013-000-0000', 100.00, 'BUILDING', 'Q7 wrong pair Peru customer decoy'),
             (14, 'Q7 Germany Same Nation Customer', 'Address 14', 4, '49-014-000-0000', 100.00, 'BUILDING', 'Q7 same nation Germany decoy'),
             (15, 'Q7 France Peru Supplier Customer', 'Address 15', 3, '33-015-000-0000', 100.00, 'BUILDING', 'Q7 Peru supplier decoy'),
             (16, 'Q7 Germany Before Window Customer', 'Address 16', 4, '49-016-000-0000', 100.00, 'BUILDING', 'Q7 before window decoy'),
             (17, 'Q7 Peru Germany Supplier Customer', 'Address 17', 6, '51-017-000-0000', 100.00, 'BUILDING', 'Q7 Peru customer decoy'),
             (18, 'Q7 Germany Upper Boundary Customer', 'Address 18', 4, '49-018-000-0000', 100.00, 'BUILDING', 'Q7 upper boundary customer'),
             (19, 'Q7 Germany After Window Customer', 'Address 19', 4, '49-019-000-0000', 100.00, 'BUILDING', 'Q7 after window decoy')",
        )
        .expect("insert Q7 customers");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (10, 'Q7 France Supplier', 'Address 10', 3, '33-010-000-0000', 100.00, 'Q7 FRANCE supplier'),
             (11, 'Q7 Germany Supplier', 'Address 11', 4, '49-011-000-0000', 100.00, 'Q7 GERMANY supplier'),
             (12, 'Q7 France Same Nation Supplier', 'Address 12', 3, '33-012-000-0000', 100.00, 'Q7 same nation France decoy'),
             (13, 'Q7 France Peru Customer Supplier', 'Address 13', 3, '33-013-000-0000', 100.00, 'Q7 Peru customer decoy'),
             (14, 'Q7 Germany Same Nation Supplier', 'Address 14', 4, '49-014-000-0000', 100.00, 'Q7 same nation Germany decoy'),
             (15, 'Q7 Peru France Customer Supplier', 'Address 15', 6, '51-015-000-0000', 100.00, 'Q7 Peru supplier decoy'),
             (16, 'Q7 France Before Window Supplier', 'Address 16', 3, '33-016-000-0000', 100.00, 'Q7 before window decoy'),
             (17, 'Q7 Germany Peru Customer Supplier', 'Address 17', 4, '49-017-000-0000', 100.00, 'Q7 Peru customer decoy'),
             (18, 'Q7 France Upper Boundary Supplier', 'Address 18', 3, '33-018-000-0000', 100.00, 'Q7 upper boundary supplier'),
             (19, 'Q7 France After Window Supplier', 'Address 19', 3, '33-019-000-0000', 100.00, 'Q7 after window decoy')",
        )
        .expect("insert Q7 suppliers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1001, 10, 'F', 300.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000001001', 0, 'Q7 France supplier Germany customer accumulated group'),
             (1002, 11, 'F', 300.00, DATE '1995-06-15', '1-URGENT', 'Clerk#000001002', 0, 'Q7 Germany supplier France customer direction'),
             (1003, 12, 'F', 150.00, DATE '1996-01-01', '1-URGENT', 'Clerk#000001003', 0, 'Q7 same nation France decoy'),
             (1004, 13, 'F', 400.00, DATE '1996-06-15', '1-URGENT', 'Clerk#000001004', 0, 'Q7 Peru customer decoy'),
             (1005, 14, 'F', 250.00, DATE '1995-09-15', '1-URGENT', 'Clerk#000001005', 0, 'Q7 same nation Germany decoy'),
             (1006, 15, 'F', 200.00, DATE '1995-12-31', '1-URGENT', 'Clerk#000001006', 0, 'Q7 Peru supplier decoy'),
             (1007, 16, 'F', 100.00, DATE '1994-12-31', '1-URGENT', 'Clerk#000001007', 0, 'Q7 before lower shipdate boundary decoy'),
             (1008, 17, 'F', 250.00, DATE '1996-09-15', '1-URGENT', 'Clerk#000001008', 0, 'Q7 Peru customer decoy'),
             (1009, 19, 'F', 100.00, DATE '1997-01-01', '1-URGENT', 'Clerk#000001009', 0, 'Q7 after upper shipdate boundary decoy'),
             (1010, 18, 'F', 200.00, DATE '1996-12-31', '1-URGENT', 'Clerk#000001010', 0, 'Q7 inclusive upper shipdate boundary')",
        )
        .expect("insert Q7 orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1001, 1, 10, 10, 1.00, 100.00, 0.10, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q7 France Germany first accumulated line'),
             (1001, 2, 11, 10, 1.00, 200.00, 0.05, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q7 France Germany second accumulated line'),
             (1002, 1, 12, 11, 1.00, 300.00, 0.20, 0.00, 'N', 'O', DATE '1995-06-15', DATE '1995-06-16', DATE '1995-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q7 Germany France direction line'),
             (1003, 1, 13, 12, 1.00, 150.00, 0.10, 0.00, 'N', 'O', DATE '1996-01-01', DATE '1996-01-02', DATE '1996-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q7 same nation France decoy'),
             (1004, 1, 14, 13, 1.00, 400.00, 0.15, 0.00, 'N', 'O', DATE '1996-06-15', DATE '1996-06-16', DATE '1996-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q7 Peru customer decoy'),
             (1005, 1, 15, 14, 1.00, 250.00, 0.10, 0.00, 'N', 'O', DATE '1995-09-15', DATE '1995-09-16', DATE '1995-09-17', 'DELIVER IN PERSON', 'MAIL', 'Q7 same nation Germany decoy'),
             (1006, 1, 16, 15, 1.00, 200.00, 0.05, 0.00, 'N', 'O', DATE '1995-12-31', DATE '1996-01-01', DATE '1996-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q7 Peru supplier decoy'),
             (1007, 1, 17, 16, 1.00, 100.00, 0.10, 0.00, 'N', 'O', DATE '1994-12-31', DATE '1995-01-01', DATE '1995-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q7 before lower shipdate boundary decoy'),
             (1008, 1, 18, 17, 1.00, 250.00, 0.10, 0.00, 'N', 'O', DATE '1996-09-15', DATE '1996-09-16', DATE '1996-09-17', 'DELIVER IN PERSON', 'MAIL', 'Q7 Peru customer decoy'),
             (1009, 1, 19, 19, 1.00, 100.00, 0.10, 0.00, 'N', 'O', DATE '1997-01-01', DATE '1997-01-02', DATE '1997-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q7 after upper shipdate boundary decoy'),
             (1010, 1, 20, 18, 1.00, 200.00, 0.20, 0.00, 'N', 'O', DATE '1996-12-31', DATE '1997-01-01', DATE '1997-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q7 inclusive upper shipdate boundary')",
        )
        .expect("insert Q7 line items");

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (20, 'Q8 Peru Customer', 'Address 20', 6, '51-020-000-0000', 100.00, 'BUILDING', 'Q8 AMERICA customer'),
             (21, 'Q8 Brazil Customer', 'Address 21', 5, '55-021-000-0000', 100.00, 'BUILDING', 'Q8 AMERICA customer'),
             (22, 'Q8 France Customer', 'Address 22', 3, '33-022-000-0000', 100.00, 'BUILDING', 'Q8 EUROPE region decoy'),
             (23, 'Q8 Before Window Customer', 'Address 23', 6, '51-023-000-0000', 100.00, 'BUILDING', 'Q8 before window decoy'),
             (24, 'Q8 After Window Customer', 'Address 24', 5, '55-024-000-0000', 100.00, 'BUILDING', 'Q8 after window decoy')",
        )
        .expect("insert Q8 customers");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (20, 'Q8 Brazil Supplier', 'Address 20', 5, '55-020-000-0000', 100.00, 'Q8 BRAZIL supplier'),
             (21, 'Q8 France Supplier', 'Address 21', 3, '33-021-000-0000', 100.00, 'Q8 FRANCE supplier')",
        )
        .expect("insert Q8 suppliers");

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (50, 'Q8 Main Part', 'Manufacturer#1', 'Brand#01', 'ECONOMY ANODIZED STEEL', 10, 'BOX', 100.00, 'Q8 qualifying part'),
             (51, 'Q8 Second Part', 'Manufacturer#1', 'Brand#02', 'ECONOMY ANODIZED STEEL', 10, 'BOX', 100.00, 'Q8 qualifying part'),
             (52, 'Q8 Wrong Type Part', 'Manufacturer#1', 'Brand#03', 'STANDARD PLATED BRASS', 10, 'BOX', 100.00, 'Q8 part type decoy')",
        )
        .expect("insert Q8 parts");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (2001, 20, 'F', 300.00, DATE '1995-06-15', '1-URGENT', 'Clerk#000002001', 0, 'Q8 Brazil supplier Peru customer'),
             (2002, 21, 'F', 300.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000002002', 0, 'Q8 France supplier Brazil customer'),
             (2003, 22, 'F', 300.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000002003', 0, 'Q8 Europe customer region decoy'),
             (2005, 23, 'F', 300.00, DATE '1994-12-31', '1-URGENT', 'Clerk#000002005', 0, 'Q8 before date window decoy'),
             (2006, 24, 'F', 300.00, DATE '1997-01-01', '1-URGENT', 'Clerk#000002006', 0, 'Q8 after date window decoy'),
             (2007, 20, 'F', 300.00, DATE '1996-01-01', '1-URGENT', 'Clerk#000002007', 0, 'Q8 1996 Brazil supplier'),
             (2008, 20, 'F', 300.00, DATE '1995-01-01', '1-URGENT', 'Clerk#000002008', 0, 'Q8 wrong part type decoy')",
        )
        .expect("insert Q8 orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (2001, 1, 50, 20, 300.00, 150.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-15', DATE '1995-06-16', DATE '1995-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q8 Brazil first accumulated line'),
             (2001, 2, 51, 20, 300.00, 150.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-15', DATE '1995-06-16', DATE '1995-06-17', 'DELIVER IN PERSON', 'MAIL', 'Q8 Brazil second accumulated line'),
             (2002, 1, 50, 21, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q8 France supplier denominator line'),
             (2003, 1, 50, 20, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q8 Europe region decoy'),
             (2005, 1, 50, 20, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1994-12-31', DATE '1995-01-01', DATE '1995-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q8 before date window decoy'),
             (2006, 1, 50, 20, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1997-01-01', DATE '1997-01-02', DATE '1997-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q8 after date window decoy'),
             (2007, 1, 50, 20, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1996-01-01', DATE '1996-01-02', DATE '1996-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q8 1996 Brazil supplier line'),
             (2008, 1, 52, 20, 300.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q8 wrong part type decoy')",
        )
        .expect("insert Q8 line items");

    (directory, server)
}
