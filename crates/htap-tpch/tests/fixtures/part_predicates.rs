use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_q19_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    let brackets = [
        ("Brand#12", "SM CASE", 1, 5, 1.0, 11.0, 3104, 1.0),
        ("Brand#23", "MED BAG", 1, 10, 10.0, 20.0, 3113, 10.0),
        ("Brand#34", "LG CASE", 1, 15, 20.0, 30.0, 3122, 19.0),
    ];

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (3101, 'q19 witness small', 'MFGR#19', 'Brand#12', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q19 clause one witness'),
             (3102, 'q19 witness medium', 'MFGR#19', 'Brand#23', 'STANDARD STEEL', 1, 'MED BAG', 100.00, 'Q19 clause two witness'),
             (3103, 'q19 witness large', 'MFGR#19', 'Brand#34', 'STANDARD STEEL', 1, 'LG CASE', 100.00, 'Q19 clause three witness')",
        )
        .expect("insert Query 19 witness parts");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (3101, 1, 'F', 100.00, DATE '1994-01-01', '1-URGENT', 'Clerk#000003101', 0, 'Q19 fixture order'),
             (3102, 1, 'F', 100.00, DATE '1994-01-02', '1-URGENT', 'Clerk#000003102', 0, 'Q19 fixture order'),
             (3103, 1, 'F', 100.00, DATE '1994-01-03', '1-URGENT', 'Clerk#000003103', 0, 'Q19 fixture order')",
        )
        .expect("insert Query 19 witness orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (3101, 1, 3101, 1, 1.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-01-01', DATE '1994-01-02', DATE '1994-01-03', 'DELIVER IN PERSON', 'AIR', 'Q19 clause one witness'),
             (3102, 1, 3102, 1, 10.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1994-01-01', DATE '1994-01-02', DATE '1994-01-03', 'DELIVER IN PERSON', 'AIR REG', 'Q19 clause two witness'),
             (3103, 1, 3103, 1, 20.00, 300.00, 0.00, 0.00, 'N', 'O', DATE '1994-01-01', DATE '1994-01-02', DATE '1994-01-03', 'DELIVER IN PERSON', 'AIR', 'Q19 clause three witness')",
        )
        .expect("insert Query 19 witness line items");

    for (brand, container, min_size, max_size, min_qty, max_qty, first_key, first_revenue) in
        brackets
    {
        let part_rows = [
            (brand, min_size, container, "join"),
            ("Brand#99", min_size, container, "brand"),
            (brand, min_size, "WRONG CASE", "container"),
            (brand, min_size, container, "quantity low"),
            (brand, min_size, container, "quantity high"),
            (brand, min_size - 1, container, "size low"),
            (brand, max_size + 1, container, "size high"),
            (brand, min_size, container, "shipmode"),
            (brand, min_size, container, "shipinstruct"),
        ];

        for (offset, (part_brand, part_size, part_container, predicate)) in
            part_rows.into_iter().enumerate()
        {
            let part_key = first_key + offset as i32;
            server
                .execute(&format!(
                    "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
                     ({part_key}, 'q19 {predicate} decoy', 'MFGR#19', '{part_brand}', 'STANDARD STEEL', {part_size}, '{part_container}', 100.00, 'Q19 {predicate} decoy')"
                ))
                .expect("insert Query 19 decoy part");
        }

        let line_rows = [
            (
                9999,
                min_qty,
                first_revenue,
                "DELIVER IN PERSON",
                "AIR",
                "join",
            ),
            (
                first_key + 1,
                min_qty,
                first_revenue + 1.0,
                "DELIVER IN PERSON",
                "AIR",
                "brand",
            ),
            (
                first_key + 2,
                min_qty,
                first_revenue + 2.0,
                "DELIVER IN PERSON",
                "AIR",
                "container",
            ),
            (
                first_key + 3,
                min_qty - 1.0,
                first_revenue + 3.0,
                "DELIVER IN PERSON",
                "AIR",
                "quantity low",
            ),
            (
                first_key + 4,
                max_qty + 1.0,
                first_revenue + 4.0,
                "DELIVER IN PERSON",
                "AIR",
                "quantity high",
            ),
            (
                first_key + 5,
                min_qty,
                first_revenue + 5.0,
                "DELIVER IN PERSON",
                "AIR",
                "size low",
            ),
            (
                first_key + 6,
                min_qty,
                first_revenue + 6.5,
                "DELIVER IN PERSON",
                "AIR",
                "size high",
            ),
            (
                first_key + 7,
                min_qty,
                first_revenue + 7.0,
                "DELIVER IN PERSON",
                "MAIL",
                "shipmode",
            ),
            (
                first_key + 8,
                min_qty,
                first_revenue + 8.0,
                "DROP OFF",
                "AIR",
                "shipinstruct",
            ),
        ];

        for (offset, (line_partkey, quantity, revenue, shipinstruct, shipmode, predicate)) in
            line_rows.into_iter().enumerate()
        {
            let order_key = first_key + offset as i32;
            server
                .execute(&format!(
                    "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
                     ({order_key}, 1, 'F', 100.00, DATE '1994-01-01', '1-URGENT', 'Clerk#00000{order_key}', 0, 'Q19 {predicate} decoy order')"
                ))
                .expect("insert Query 19 decoy order");

            server
                .execute(&format!(
                    "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
                     ({order_key}, 1, {line_partkey}, 1, {quantity:.2}, {revenue:.2}, 0.00, 0.00, 'N', 'O', DATE '1994-01-01', DATE '1994-01-02', DATE '1994-01-03', '{shipinstruct}', '{shipmode}', 'Q19 {predicate} decoy')"
                ))
                .expect("insert Query 19 decoy line item");
        }
    }

    (directory, server)
}

pub fn load_fixture() -> (TempDir, Arc<LocalServer>) {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    server
        .execute(
            "INSERT INTO nation (n_nationkey, n_name, n_regionkey, n_comment) VALUES
             (3, 'CANADA', 1, 'Q20 qualifying nation'),
             (4, 'GERMANY', 1, 'Q20 outside-nation decoy')",
        )
        .expect("insert Query 20 nations");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (2001, 'Q20 Qualifying Supplier', 'Address 2001', 3, '13-2001-000-0000', 100.00, 'Reliable supplier'),
             (2002, 'Q20 Other Canadian Supplier', 'Address 2002', 3, '13-2002-000-0000', 100.00, 'Reliable supplier'),
             (2003, 'Q20 Canadian Decoy Supplier', 'Address 2003', 3, '13-2003-000-0000', 100.00, 'Reliable supplier'),
             (2004, 'Q20 German Supplier', 'Address 2004', 4, '13-2004-000-0000', 100.00, 'Reliable supplier'),
             (2005, 'Q20 Upper Boundary Supplier', 'Address 2005', 3, '13-2005-000-0000', 100.00, 'Reliable supplier'),
             (2006, 'Q20 Name Filter Decoy Supplier', 'Address 2006', 3, '13-2006-000-0000', 100.00, 'Reliable supplier'),
             (2007, 'Q20 Equality Threshold Supplier', 'Address 2007', 3, '13-2007-000-0000', 100.00, 'Reliable supplier')",
        )
        .expect("insert Query 20 suppliers");

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (2001, 'forest qualifying part', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 qualifying part'),
             (2002, 'desert name decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 name decoy'),
             (2003, 'forest equality decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 equality decoy'),
             (2004, 'forest supplier correlation decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 supplier correlation decoy'),
             (2005, 'forest part correlation decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 part correlation decoy'),
             (2006, 'forest start boundary decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 start boundary decoy'),
             (2007, 'forest end boundary decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 end boundary decoy'),
             (2008, 'forest foreign supplier decoy', 'MFGR#20', 'Brand#20', 'STANDARD STEEL', 1, 'SM CASE', 100.00, 'Q20 nation decoy')",
        )
        .expect("insert Query 20 parts");

    server
        .execute(
            "INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_availqty, ps_supplycost, ps_comment) VALUES
             (2001, 2001, 11, 10.00, 'Q20 qualifying pair: 11 > half of 20'),
             (2002, 2006, 11, 10.00, 'Q20 name decoy supplier'),
             (2003, 2007, 10, 10.00, 'Q20 equality decoy supplier'),
             (2004, 2001, 1, 10.00, 'Q20 supplier correlation: fails for supplier 2001'),
             (2004, 2002, 11, 10.00, 'Q20 supplier correlation: qualifies only for supplier 2002'),
             (2005, 2001, 11, 10.00, 'Q20 part correlation: qualifies only for part 2005'),
             (2006, 2003, 11, 10.00, 'Q20 start boundary: fails when pre-window line is excluded'),
             (2007, 2005, 11, 10.00, 'Q20 end boundary: fails when end-window line is excluded'),
             (2008, 2004, 11, 10.00, 'Q20 foreign supplier: otherwise qualifying')",
        )
        .expect("insert Query 20 part suppliers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (2001, 1, 'F', 100.00, DATE '1994-01-01', '1-URGENT', 'Clerk#000002001', 0, 'Q20 fixture orders'),
             (2002, 1, 'F', 100.00, DATE '1994-01-02', '1-URGENT', 'Clerk#000002002', 0, 'Q20 fixture orders'),
             (2003, 1, 'F', 100.00, DATE '1994-01-03', '1-URGENT', 'Clerk#000002003', 0, 'Q20 fixture orders'),
             (2004, 1, 'F', 100.00, DATE '1994-01-04', '1-URGENT', 'Clerk#000002004', 0, 'Q20 fixture orders'),
             (2005, 1, 'F', 100.00, DATE '1994-01-05', '1-URGENT', 'Clerk#000002005', 0, 'Q20 fixture orders'),
             (2006, 1, 'F', 100.00, DATE '1994-01-06', '1-URGENT', 'Clerk#000002006', 0, 'Q20 fixture orders'),
             (2007, 1, 'F', 100.00, DATE '1994-01-07', '1-URGENT', 'Clerk#000002007', 0, 'Q20 fixture orders'),
             (2008, 1, 'F', 100.00, DATE '1994-01-08', '1-URGENT', 'Clerk#000002008', 0, 'Q20 fixture orders'),
             (2009, 1, 'F', 100.00, DATE '1994-01-09', '1-URGENT', 'Clerk#000002009', 0, 'Q20 fixture orders'),
             (2010, 1, 'F', 100.00, DATE '1994-01-10', '1-URGENT', 'Clerk#000002010', 0, 'Q20 fixture orders')",
        )
        .expect("insert Query 20 orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (2001, 1, 2001, 2001, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 qualifying shipment'),
             (2002, 1, 2002, 2006, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 name decoy shipment'),
             (2003, 1, 2003, 2007, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 equality decoy shipment'),
             (2004, 1, 2004, 2001, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 supplier correlation shipment'),
             (2005, 1, 2004, 2002, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 supplier correlation shipment'),
             (2006, 1, 2005, 2001, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 part correlation shipment'),
             (2007, 1, 2006, 2003, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1993-12-31', DATE '1994-01-01', DATE '1994-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q20 pre-window shipment'),
             (2008, 1, 2006, 2003, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-01-01', DATE '1994-01-02', DATE '1994-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 window-start shipment'),
             (2009, 1, 2007, 2005, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-12-31', DATE '1995-01-01', DATE '1995-01-02', 'DELIVER IN PERSON', 'MAIL', 'Q20 pre-end shipment'),
             (2010, 1, 2007, 2005, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1995-01-01', DATE '1995-01-02', DATE '1995-01-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 window-end shipment'),
             (2001, 2, 2008, 2004, 20.00, 100.00, 0.00, 0.00, 'N', 'O', DATE '1994-06-01', DATE '1994-06-02', DATE '1994-06-03', 'DELIVER IN PERSON', 'MAIL', 'Q20 foreign supplier shipment')",
        )
        .expect("insert Query 20 line items");

    (directory, server)
}
