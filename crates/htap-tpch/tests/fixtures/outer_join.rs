use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_fixture() -> Arc<LocalServer> {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let path = directory.keep();
    let server = Arc::new(LocalServer::open(&path).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (1, 'Customer One', 'Address 1', 1, '11-000-000-0000', 100.00, 'BUILDING', 'customer'),
             (2, 'Customer Two', 'Address 2', 1, '12-000-000-0000', 200.00, 'BUILDING', 'customer'),
             (3, 'Customer Three', 'Address 3', 1, '13-000-000-0000', 300.00, 'BUILDING', 'customer'),
             (4, 'Customer Four', 'Address 4', 1, '14-000-000-0000', 400.00, 'BUILDING', 'customer'),
             (5, 'Customer Five', 'Address 5', 1, '15-000-000-0000', 500.00, 'BUILDING', 'customer')",
        )
        .expect("insert customers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1, 1, 'O', 100.00, DATE '1995-01-01', '1-URGENT', 'Clerk#1', 0, 'ordinary order'),
             (2, 1, 'O', 200.00, DATE '1995-01-02', '1-URGENT', 'Clerk#2', 0, 'regular order'),
             (3, 2, 'O', 300.00, DATE '1995-01-03', '2-HIGH', 'Clerk#3', 0, 'ordinary order'),
             (4, 4, 'O', 400.00, DATE '1995-01-04', '2-HIGH', 'Clerk#4', 0, 'special handling requests'),
             (5, 5, 'O', 500.00, DATE '1995-01-05', '3-MEDIUM', 'Clerk#5', 0, 'ordinary order'),
             (6, 5, 'O', 600.00, DATE '1995-01-06', '3-MEDIUM', 'Clerk#6', 0, 'special delivery requests')",
        )
        .expect("insert orders");

    server
}
