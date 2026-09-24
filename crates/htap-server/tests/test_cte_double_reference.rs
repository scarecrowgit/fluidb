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

#[test]
fn test_cte_referenced_twice_in_outer_query_and_scalar_subquery() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE products (product_id INT PRIMARY KEY, revenue INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO products (product_id, revenue) VALUES \
             (1, 100), (2, 200), (3, 150), (4, 200);",
        )
        .unwrap();

    let result = rows(
        &server,
        "WITH revenue_cte AS ( \
             SELECT product_id, revenue FROM products \
         ) \
         SELECT product_id, revenue \
         FROM revenue_cte \
         WHERE revenue = (SELECT MAX(revenue) FROM revenue_cte);",
    );

    assert_eq!(result.len(), 2);
    assert!(result.contains(&Row::new(vec![Value::Int32(2), Value::Int32(200)])));
    assert!(result.contains(&Row::new(vec![Value::Int32(4), Value::Int32(200)])));
}

#[test]
fn test_cte_referenced_twice_in_aggregating_join_and_scalar_subquery() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE suppliers (\
                 supplier_id INT PRIMARY KEY, \
                 supplier_name VARCHAR\
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE sales (\
                 sale_id INT PRIMARY KEY, \
                 supplier_id INT, \
                 amount INT\
             );",
        )
        .unwrap();

    server
        .execute(
            "INSERT INTO suppliers (supplier_id, supplier_name) VALUES \
             (1, 'Supplier One'), (2, 'Supplier Two'), (3, 'Supplier Three');",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO sales (sale_id, supplier_id, amount) VALUES \
             (1, 1, 100), (2, 1, 150), \
             (3, 2, 200), (4, 2, 250), \
             (5, 3, 50);",
        )
        .unwrap();

    let query = "\
        WITH supplier_revenue AS ( \
            SELECT supplier_id, SUM(amount) AS total_revenue \
            FROM sales \
            GROUP BY supplier_id \
        ) \
        SELECT suppliers.supplier_id, suppliers.supplier_name, supplier_revenue.total_revenue \
        FROM suppliers \
        JOIN supplier_revenue ON suppliers.supplier_id = supplier_revenue.supplier_id \
        WHERE supplier_revenue.total_revenue = ( \
            SELECT MAX(total_revenue) FROM supplier_revenue \
        );";

    let result = rows(&server, query);

    assert_eq!(result.len(), 1);
    assert!(result.contains(&Row::new(vec![
        Value::Int32(2),
        Value::String("Supplier Two".to_string()),
        Value::Int64(450),
    ])));

    server.execute("DELETE FROM sales;").unwrap();
    server
        .execute(
            "INSERT INTO sales (sale_id, supplier_id, amount) VALUES \
             (1, 1, 100), (2, 1, 150), \
             (3, 2, 200), (4, 2, 250), \
             (5, 3, 200), (6, 3, 250);",
        )
        .unwrap();

    let result = rows(&server, query);

    assert_eq!(result.len(), 2);
    assert!(result.contains(&Row::new(vec![
        Value::Int32(2),
        Value::String("Supplier Two".to_string()),
        Value::Int64(450),
    ])));
    assert!(result.contains(&Row::new(vec![
        Value::Int32(3),
        Value::String("Supplier Three".to_string()),
        Value::Int64(450),
    ])));
}
