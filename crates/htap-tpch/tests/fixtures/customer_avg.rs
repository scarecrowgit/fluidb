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
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (101, 'Q22 High Unordered Customer', 'Address 101', 1, '13-101-000-0000', 500.00, 'BUILDING', 'Q22 qualifies'),
             (102, 'Q22 Low Unordered Customer', 'Address 102', 1, '13-102-000-0000', 300.00, 'BUILDING', 'Q22 below average'),
             (103, 'Q22 Negative Customer', 'Address 103', 1, '31-103-000-0000', -400.00, 'BUILDING', 'Q22 negative balance'),
             (104, 'Q22 Ordered Customer', 'Address 104', 1, '23-104-000-0000', 600.00, 'BUILDING', 'Q22 has order'),
             (105, 'Q22 Unlisted Customer', 'Address 105', 1, '14-105-000-0000', 700.00, 'BUILDING', 'Q22 unlisted prefix')",
        )
        .expect("insert customers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1001, 104, 'O', 100.00, DATE '1995-01-01', '1-URGENT', 'Clerk#1001', 0, 'Q22 order veto')",
        )
        .expect("insert orders");

    (directory, server)
}
