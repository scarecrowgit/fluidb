#![allow(dead_code)]

use std::collections::BTreeMap;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{ConversionDescriptor, ConversionPhase, StorageDescriptor, StorageFormat};
use htap_common::types::{Row, Value};
use htap_common::version::Version;
use htap_server::{LocalServer, OptimizationMode};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn query_result(server: &LocalServer, sql: &str) -> htap_sql::QueryResult {
    match server.execute(sql) {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fixture_server(dir: &TempDir) -> LocalServer {
    let server = LocalServer::open(dir.path()).unwrap();

    for table in ["r", "c", "k"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, grp VARCHAR(8), v INT);"
            ))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO {table} (id, grp, v) VALUES \
                 (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, NULL, NULL), (5, 'b', 50);"
            ))
            .unwrap();
    }
    assert!(server.convert_table_to_column("c").unwrap().is_success());

    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let current = catalog_store.load().unwrap().unwrap();
    let partition_id = current.table_by_name("k").unwrap().partitions[0];
    let mut next = current.clone();
    next.generation += 1;
    let generation = next.generation;
    let partition = next
        .partitions
        .iter_mut()
        .find(|partition| partition.id == partition_id)
        .unwrap();
    partition.generation = generation;
    partition.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation,
    };
    partition.conversion = Some(ConversionDescriptor::new(
        generation,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    catalog_store
        .compare_and_set(current.generation, next)
        .unwrap();

    server
        .execute(
            "CREATE TABLE part (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
             PARTITION BY RANGE (day) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO part (id, day, v) VALUES \
             (1, 1, 10), (2, 12, 20), (3, 25, 30), (5, 15, 50);",
        )
        .unwrap();

    for table in ["tiny", "large_a", "large_b"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, k BIGINT);"
            ))
            .unwrap();
    }
    server
        .execute("INSERT INTO tiny (id, k) VALUES (1, 1);")
        .unwrap();
    let large_values = (1..=128)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO large_a (id, k) VALUES {large_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO large_b (id, k) VALUES {large_values};"
        ))
        .unwrap();
    for table in ["tiny", "large_a", "large_b"] {
        assert_eq!(
            server.execute(&format!("ANALYZE TABLE {table};")).unwrap(),
            StatementResult::ddl(1)
        );
    }

    server
}

fn row_multiset(rows: &[Row]) -> BTreeMap<Vec<Value>, usize> {
    let mut counts = BTreeMap::new();
    for row in rows {
        *counts.entry(row.values().to_vec()).or_insert(0) += 1;
    }
    counts
}

fn run_with_mode(
    server: &LocalServer,
    sql: &str,
    optimization_mode: OptimizationMode,
    memory_budget: usize,
    parallelism: usize,
) -> htap_sql::QueryResult {
    match server
        .execute_query_with_options(sql, optimization_mode, memory_budget, parallelism)
        .unwrap()
    {
        StatementResult::Query(result) => result,
        other => panic!("expected query result for {sql}, got {other:?}"),
    }
}

fn assert_differential(server: &LocalServer, sql: &str) {
    let parallelism = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let unoptimized = run_with_mode(
        server,
        sql,
        OptimizationMode::Disabled,
        1024 * 1024 * 1024,
        1,
    );
    let optimized = run_with_mode(
        server,
        sql,
        OptimizationMode::Enabled,
        1024 * 1024 * 1024,
        parallelism,
    );
    let spilling = run_with_mode(
        server,
        sql,
        OptimizationMode::Enabled,
        1024 * 1024,
        parallelism,
    );

    assert_eq!(
        row_multiset(&unoptimized.rows),
        row_multiset(&optimized.rows),
        "optimized result multiset differs: {sql}"
    );
    assert_eq!(
        row_multiset(&unoptimized.rows),
        row_multiset(&spilling.rows),
        "spilling result multiset differs: {sql}"
    );
    assert_eq!(
        unoptimized.rows, optimized.rows,
        "optimized result order differs: {sql}"
    );
    assert_eq!(
        unoptimized.rows, spilling.rows,
        "spilling result order differs: {sql}"
    );
    assert_eq!(unoptimized.columns, optimized.columns, "{sql}");
    assert_eq!(unoptimized.columns, spilling.columns, "{sql}");
}

#[test]
fn differential_query_execution_paths_agree() {
    let dir = TempDir::new().unwrap();
    let mut server = fixture_server(&dir);
    server.set_query_memory_budget(1024 * 1024);
    server.set_query_parallelism(
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
    );

    let queries = [
        "SELECT r.id, c.v, k.v FROM r JOIN c ON r.id = c.id JOIN k ON c.id = k.id ORDER BY r.id;",
        "SELECT r.id, c.id FROM r LEFT JOIN c ON r.id = c.id ORDER BY r.id, c.id;",
        "SELECT r.id, c.id FROM r RIGHT JOIN c ON r.id = c.id ORDER BY c.id;",
        "SELECT r.id, c.id FROM r FULL JOIN c ON r.id = c.id ORDER BY COALESCE(r.id, c.id);",
        "SELECT r.id, c.id, k.id FROM r LEFT JOIN (c JOIN k ON c.id = k.id) ON r.id = c.id ORDER BY r.id;",
        "SELECT * FROM r NATURAL LEFT JOIN c ORDER BY id;",
        "SELECT * FROM r JOIN c USING(id) ORDER BY id;",
        "SELECT r.id FROM r WHERE EXISTS (SELECT 1 FROM c WHERE c.id = r.id AND c.v >= r.v) ORDER BY r.id;",
        "SELECT r.id, (SELECT MAX(c.v) FROM c WHERE c.id = r.id) FROM r ORDER BY r.id;",
        "SELECT id FROM r EXCEPT SELECT id FROM c ORDER BY id;",
        "SELECT id FROM r EXCEPT ALL SELECT id FROM c ORDER BY id;",
        "SELECT id FROM r INTERSECT SELECT id FROM c ORDER BY id;",
        "SELECT id FROM r INTERSECT ALL SELECT id FROM c ORDER BY id;",
        "SELECT id FROM r UNION SELECT id FROM c ORDER BY id;",
        "SELECT id FROM r UNION ALL SELECT id FROM c ORDER BY id;",
        "WITH x AS (SELECT id, v FROM c WHERE v >= 20) SELECT r.id, x.v FROM r JOIN x ON r.id = x.id ORDER BY r.id;",
        "WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 5) SELECT n FROM nums ORDER BY n;",
        "SELECT grp, SUM(v) AS total FROM r GROUP BY grp HAVING SUM(v) >= 30 ORDER BY grp;",
        "SELECT id, SUM(v) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM c ORDER BY id;",
        "SELECT p.id, COUNT(*) FROM part p JOIN r ON p.id = r.id GROUP BY p.id ORDER BY p.id;",
        "SELECT t.id FROM tiny t JOIN large_a a ON t.k = a.k JOIN large_b b ON a.k = b.k ORDER BY t.id;",
    ];

    for sql in queries {
        assert_differential(&server, sql);
    }

    let baseline = query_result(
        &server,
        "SELECT t.id FROM tiny t JOIN large_a a ON t.k = a.k JOIN large_b b ON a.k = b.k ORDER BY t.id;",
    );
    assert_eq!(baseline.rows, vec![Row::new(vec![Value::Int64(1)])]);
}

#[test]
fn differential_outer_join_hoisting_predicate_from_inner_on_clause() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT);")
        .unwrap();
    server
        .execute("CREATE TABLE b (id BIGINT PRIMARY KEY, enabled INT);")
        .unwrap();
    server
        .execute("CREATE TABLE c (id BIGINT PRIMARY KEY, bid BIGINT);")
        .unwrap();

    server
        .execute("INSERT INTO a (id, x) VALUES (1, 10), (2, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, enabled) VALUES (1, 1);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, bid) VALUES (1, 1);")
        .unwrap();

    let sql = "SELECT a.id, b.id, c.id \
               FROM a LEFT JOIN (b JOIN c ON b.id = c.bid AND b.enabled = 1) \
               ON a.id = b.id \
               ORDER BY a.id;";

    let result = query_result(&server, sql);
    assert_eq!(
        result.rows,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(1), Value::Int64(1)]),
            Row::new(vec![Value::Int64(2), Value::Null, Value::Null]),
        ]
    );

    assert_differential(&server, sql);
}

#[test]
fn differential_outer_join_sinking_inner_join_predicate_referencing_null_side() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE a (id BIGINT PRIMARY KEY, x INT);")
        .unwrap();
    server
        .execute("CREATE TABLE b (id BIGINT PRIMARY KEY, x INT);")
        .unwrap();
    server
        .execute("CREATE TABLE c (id BIGINT PRIMARY KEY, aid BIGINT);")
        .unwrap();

    server
        .execute("INSERT INTO a (id, x) VALUES (1, 10), (2, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, x) VALUES (1, 10), (2, 30);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, aid) VALUES (1, 1), (3, 2);")
        .unwrap();

    let sql = "SELECT a.id, b.id, c.id \
               FROM a LEFT JOIN b ON a.id = b.id \
               JOIN c ON a.id = c.aid AND a.x = b.x \
               ORDER BY a.id;";

    let result = query_result(&server, sql);
    assert_eq!(
        result.rows,
        vec![Row::new(vec![
            Value::Int64(1),
            Value::Int64(1),
            Value::Int64(1),
        ])]
    );

    assert_differential(&server, sql);
}
