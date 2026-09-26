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
            "INSERT INTO nation (n_nationkey, n_name, n_regionkey, n_comment) VALUES
             (6, 'FRANCE', 3, 'nation'),
             (7, 'GERMANY', 3, 'nation'),
             (8, 'ITALY', 3, 'nation')",
        )
        .expect("insert nations");

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (301, 'green profit part', 'MFGR#9', 'Brand#91', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q9 green part'),
             (302, 'blue profit decoy', 'MFGR#9', 'Brand#92', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q9 non-green decoy'),
             (100, 'Q11 German dominant part', 'MFGR#11', 'Brand#111', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q11 part'),
             (101, 'Q11 German second part', 'MFGR#11', 'Brand#112', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q11 part'),
             (102, 'Q11 German tiny part', 'MFGR#11', 'Brand#113', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q11 part'),
             (103, 'Q11 France decoy part', 'MFGR#11', 'Brand#114', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q11 part'),
             (5001, 'Q14 promo part', 'MFGR#14', 'Brand#141', 'PROMO ANODIZED STEEL', 10, 'SM CASE', 100.00, 'Q14 promo part'),
             (5002, 'Q14 standard part', 'MFGR#14', 'Brand#142', 'STANDARD STEEL', 10, 'SM CASE', 100.00, 'Q14 standard part')",
        )
        .expect("insert parts");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (3, 'Q9 France Supplier', 'Address 3', 6, '33-003-000-0000', 1000.00, 'Q9 supplier'),
             (4, 'Q9 Italy Supplier', 'Address 4', 8, '39-004-000-0000', 1000.00, 'Q9 supplier'),
             (10, 'Q15 Single Line Supplier', 'Address 10', 6, '33-010-000-0000', 1000.00, 'Q15 supplier'),
             (11, 'Q15 Multi Line Supplier', 'Address 11', 8, '39-011-000-0000', 1000.00, 'Q15 supplier'),
             (12, 'Q15 Lower Supplier', 'Address 12', 6, '33-012-000-0000', 1000.00, 'Q15 supplier'),
             (5, 'Q11 Germany Supplier', 'Address 5', 7, '49-005-000-0000', 1000.00, 'Q11 supplier'),
             (6, 'Q11 Germany Second Supplier', 'Address 6', 7, '49-006-000-0000', 1000.00, 'Q11 supplier')",
        )
        .expect("insert suppliers");

    server
        .execute(
            "INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_availqty, ps_supplycost, ps_comment) VALUES
             (301, 3, 100, 10.00, 'Q9 France cost'),
             (301, 4, 100, 20.00, 'Q9 Italy cost'),
             (302, 3, 100, 1.00, 'Q9 non-green cost'),
             (100, 5, 100, 1000.00, 'Q11 German dominant supply'),
             (101, 6, 100, 800.00, 'Q11 German second supply'),
             (102, 5, 100, 0.01, 'Q11 German tiny supply'),
             (103, 3, 80000, 10000.00, 'Q11 France decoy supply')",
        )
        .expect("insert part supplies");

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (101, 'Customer Germany', 'Address 101', 7, '13-101-000-0000', 1000.00, 'BUILDING', 'customer'),
             (102, 'Customer France', 'Address 102', 6, '31-102-000-0000', 2000.00, 'BUILDING', 'customer')",
        )
        .expect("insert customers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1001, 101, 'O', 100.00, DATE '1993-10-01', '1-URGENT', 'Clerk#1001', 0, 'Q10 boundary order'),
             (1002, 101, 'O', 250.00, DATE '1993-12-15', '2-HIGH', 'Clerk#1002', 0, 'Q10 accumulation order'),
             (1003, 102, 'O', 10399.00, DATE '1993-11-20', '3-MEDIUM', 'Clerk#1003', 0, 'Q10 return-flag order'),
             (2001, 101, 'O', 100.00, DATE '1994-06-01', '1-URGENT', 'Clerk#2001', 0, 'Q12 MAIL urgent'),
             (2002, 102, 'O', 200.00, DATE '1994-06-02', '2-HIGH', 'Clerk#2002', 0, 'Q12 MAIL high'),
             (2003, 101, 'O', 300.00, DATE '1994-06-03', '3-MEDIUM', 'Clerk#2003', 0, 'Q12 SHIP medium'),
             (2004, 102, 'O', 400.00, DATE '1994-06-04', '1-URGENT', 'Clerk#2004', 0, 'Q12 equal commit receipt decoy'),
             (2005, 101, 'O', 500.00, DATE '1994-06-05', '3-MEDIUM', 'Clerk#2005', 0, 'Q12 equal ship commit decoy'),
             (2006, 102, 'O', 600.00, DATE '1994-06-06', '2-HIGH', 'Clerk#2006', 0, 'Q12 AIR decoy'),
             (2007, 101, 'O', 700.00, DATE '1994-06-07', '1-URGENT', 'Clerk#2007', 0, 'Q12 lower date-boundary decoy'),
             (2008, 102, 'O', 800.00, DATE '1994-06-08', '2-HIGH', 'Clerk#2008', 0, 'Q12 upper date-boundary decoy'),
             (3001, 101, 'O', 1000.00, DATE '1995-06-01', '3-MEDIUM', 'Clerk#3001', 0, 'Q9 France 1995'),
             (3002, 101, 'O', 1000.00, DATE '1994-06-01', '3-MEDIUM', 'Clerk#3002', 0, 'Q9 France 1994'),
             (3003, 102, 'O', 1000.00, DATE '1995-06-01', '3-MEDIUM', 'Clerk#3003', 0, 'Q9 Italy 1995'),
             (4001, 101, 'O', 1000.00, DATE '1996-01-15', '3-MEDIUM', 'Clerk#4001', 0, 'Q15 single-line supplier'),
             (4002, 101, 'O', 1000.00, DATE '1996-02-15', '3-MEDIUM', 'Clerk#4002', 0, 'Q15 multi-line supplier'),
             (4003, 102, 'O', 1000.00, DATE '1996-03-15', '3-MEDIUM', 'Clerk#4003', 0, 'Q15 lower supplier'),
             (4004, 102, 'O', 1000.00, DATE '1996-04-01', '3-MEDIUM', 'Clerk#4004', 0, 'Q15 upper boundary decoy'),
             (5001, 101, 'O', 2000.00, DATE '1995-09-15', '3-MEDIUM', 'Clerk#5001', 0, 'Q14 qualifying order'),
             (5002, 101, 'O', 1000.00, DATE '1995-08-31', '3-MEDIUM', 'Clerk#5002', 0, 'Q14 lower boundary decoy'),
             (5003, 101, 'O', 1000.00, DATE '1995-10-01', '3-MEDIUM', 'Clerk#5003', 0, 'Q14 upper boundary decoy')",
        )
        .expect("insert orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1001, 1, 1, 1, 1.00, 100.00, 0.10, 0.00, 'R', 'F', DATE '1993-10-10', DATE '1993-10-12', DATE '1993-10-15', 'DELIVER IN PERSON', 'MAIL', 'Q10 first return'),
             (1002, 1, 1, 1, 1.00, 250.00, 0.04, 0.00, 'R', 'F', DATE '1993-12-20', DATE '1993-12-22', DATE '1993-12-25', 'DELIVER IN PERSON', 'SHIP', 'Q10 second return'),
             (1003, 1, 1, 1, 1.00, 400.00, 0.05, 0.00, 'R', 'F', DATE '1993-11-25', DATE '1993-11-27', DATE '1993-11-30', 'DELIVER IN PERSON', 'MAIL', 'Q10 France return'),
             (1003, 2, 1, 1, 1.00, 9999.00, 0.00, 0.00, 'N', 'O', DATE '1993-11-25', DATE '1993-11-27', DATE '1993-11-30', 'DELIVER IN PERSON', 'MAIL', 'Q10 non-return decoy'),
             (2001, 1, 1, 1, 1.00, 100.00, 0.02, 0.00, 'N', 'O', DATE '1994-06-02', DATE '1994-06-04', DATE '1994-06-10', 'DELIVER IN PERSON', 'MAIL', 'Q12 qualifying MAIL urgent'),
             (2002, 1, 1, 1, 1.00, 200.00, 0.03, 0.00, 'N', 'O', DATE '1994-06-03', DATE '1994-06-05', DATE '1994-06-11', 'DELIVER IN PERSON', 'MAIL', 'Q12 qualifying MAIL high'),
             (2003, 1, 1, 1, 1.00, 300.00, 0.04, 0.00, 'N', 'O', DATE '1994-06-04', DATE '1994-06-06', DATE '1994-06-12', 'DELIVER IN PERSON', 'SHIP', 'Q12 qualifying SHIP medium'),
             (2004, 1, 1, 1, 1.00, 400.00, 0.05, 0.00, 'N', 'O', DATE '1994-06-05', DATE '1994-06-10', DATE '1994-06-10', 'DELIVER IN PERSON', 'MAIL', 'Q12 commit equals receipt decoy'),
             (2005, 1, 1, 1, 1.00, 500.00, 0.06, 0.00, 'N', 'O', DATE '1994-06-10', DATE '1994-06-10', DATE '1994-06-15', 'DELIVER IN PERSON', 'SHIP', 'Q12 ship equals commit decoy'),
             (2006, 1, 1, 1, 1.00, 600.00, 0.07, 0.00, 'N', 'O', DATE '1994-06-06', DATE '1994-06-08', DATE '1994-06-16', 'DELIVER IN PERSON', 'AIR', 'Q12 AIR mode decoy'),
             (2007, 1, 1, 1, 1.00, 700.00, 0.08, 0.00, 'N', 'O', DATE '1993-12-20', DATE '1993-12-25', DATE '1993-12-31', 'DELIVER IN PERSON', 'MAIL', 'Q12 lower receipt boundary decoy'),
             (2008, 1, 1, 1, 1.00, 800.00, 0.09, 0.00, 'N', 'O', DATE '1994-12-20', DATE '1994-12-25', DATE '1995-01-01', 'DELIVER IN PERSON', 'SHIP', 'Q12 upper receipt boundary decoy'),
             (3001, 1, 301, 3, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-10', DATE '1995-06-11', DATE '1995-06-12', 'DELIVER IN PERSON', 'MAIL', 'Q9 France first contribution'),
             (3001, 2, 301, 3, 1.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-10', DATE '1995-06-11', DATE '1995-06-12', 'DELIVER IN PERSON', 'MAIL', 'Q9 France second contribution'),
             (3001, 3, 302, 3, 1.00, 999.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-10', DATE '1995-06-11', DATE '1995-06-12', 'DELIVER IN PERSON', 'MAIL', 'Q9 non-green decoy'),
             (3002, 1, 301, 3, 1.00, 50.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-05', 'DELIVER IN PERSON', 'MAIL', 'Q9 France earlier year'),
             (3003, 1, 301, 4, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-06-10', DATE '1995-06-11', DATE '1995-06-12', 'DELIVER IN PERSON', 'MAIL', 'Q9 Italy contribution'),
             (4001, 1, 1, 10, 1.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1996-01-15', DATE '1996-01-16', DATE '1996-01-17', 'DELIVER IN PERSON', 'MAIL', 'Q15 single-line maximum'),
             (4002, 1, 1, 11, 1.00, 600.00, 0.00, 0.00, 'N', 'O', DATE '1996-02-15', DATE '1996-02-16', DATE '1996-02-17', 'DELIVER IN PERSON', 'MAIL', 'Q15 multi-line first maximum contribution'),
             (4002, 2, 1, 11, 1.00, 500.00, 0.20, 0.00, 'N', 'O', DATE '1996-02-15', DATE '1996-02-16', DATE '1996-02-17', 'DELIVER IN PERSON', 'MAIL', 'Q15 multi-line second maximum contribution'),
             (4003, 1, 1, 12, 1.00, 900.00, 0.00, 0.00, 'N', 'O', DATE '1996-03-15', DATE '1996-03-16', DATE '1996-03-17', 'DELIVER IN PERSON', 'MAIL', 'Q15 lower revenue'),
             (4004, 1, 1, 12, 1.00, 99999.00, 0.00, 0.00, 'N', 'O', DATE '1996-04-01', DATE '1996-04-02', DATE '1996-04-03', 'DELIVER IN PERSON', 'MAIL', 'Q15 upper boundary decoy'),
             (5001, 1, 5001, 3, 1.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1995-09-15', DATE '1995-09-16', DATE '1995-09-17', 'DELIVER IN PERSON', 'MAIL', 'Q14 promo contribution'),
             (5001, 2, 5002, 3, 1.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1995-09-15', DATE '1995-09-16', DATE '1995-09-17', 'DELIVER IN PERSON', 'MAIL', 'Q14 standard contribution'),
             (5002, 1, 5001, 3, 1.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1995-08-31', DATE '1995-09-01', DATE '1995-09-02', 'DELIVER IN PERSON', 'MAIL', 'Q14 lower boundary decoy'),
             (5003, 1, 5001, 3, 1.00, 1000.00, 0.00, 0.00, 'N', 'O', DATE '1995-10-01', DATE '1995-10-02', DATE '1995-10-03', 'DELIVER IN PERSON', 'MAIL', 'Q14 upper boundary decoy')",
        )
        .expect("insert line items");

    (directory, server)
}
