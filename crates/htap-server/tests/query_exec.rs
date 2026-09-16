//! General query executor: joins across storage formats, expressions, aggregates,
//! ordering, limits, set operations, subqueries, and the single-snapshot invariant.

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{ConversionDescriptor, ConversionPhase, StorageDescriptor, StorageFormat};
use htap_common::types::{DataType, Row, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn rows(server: &LocalServer, sql: &str) -> Vec<Row> {
    match server.execute(sql) {
        Ok(StatementResult::Query(q)) => q.rows,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(e) => panic!("{sql}: {e}"),
    }
}

fn query(server: &LocalServer, sql: &str) -> htap_sql::QueryResult {
    match server.execute(sql) {
        Ok(StatementResult::Query(q)) => q,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(e) => panic!("{sql}: {e}"),
    }
}

fn i64s(values: &[i64]) -> Row {
    Row::new(values.iter().map(|v| Value::Int64(*v)).collect())
}

/// Three tables with identical content in three storage states:
/// `r` stays Row, `c` is converted to Column, `k` is put into `Converting`
/// (`SnapshotPinned`, no manifest yet, rowstore fallback).
fn setup_three_engines(dir: &TempDir) -> LocalServer {
    let server = LocalServer::open(dir.path()).unwrap();
    for t in ["r", "c", "k"] {
        server
            .execute(&format!(
                "CREATE TABLE {t} (id BIGINT PRIMARY KEY, grp VARCHAR(8), v INT, f DOUBLE);"
            ))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO {t} (id, grp, v, f) VALUES (1, 'a', 10, 1.5), (2, 'a', 20, NULL), \
                 (3, 'b', 30, 3.5), (4, NULL, NULL, 4.5);"
            ))
            .unwrap();
    }
    let report = server.convert_table_to_column("c").unwrap();
    assert!(report.is_success(), "{report:?}");

    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cur = cat_store.load().unwrap().unwrap();
    let k = cur.table_by_name("k").unwrap();
    let part_id = k.partitions[0];
    let next_gen = cur.generation + 1;
    let mut next = cur.clone();
    next.generation = next_gen;
    let p = next
        .partitions
        .iter_mut()
        .find(|p| p.id == part_id)
        .unwrap();
    p.generation = next_gen;
    p.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: next_gen,
    };
    p.conversion = Some(ConversionDescriptor::new(
        next_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    cat_store.compare_and_set(cur.generation, next).unwrap();
    drop(cat_store);

    // Delta on top of the columnar base: one more row and one overwrite-by-delete.
    server
        .execute("INSERT INTO c (id, grp, v, f) VALUES (5, 'b', 50, 5.5);")
        .unwrap();
    server
        .execute("INSERT INTO r (id, grp, v, f) VALUES (5, 'b', 50, 5.5);")
        .unwrap();
    server
        .execute("INSERT INTO k (id, grp, v, f) VALUES (5, 'b', 50, 5.5);")
        .unwrap();
    server
}

#[test]
fn test_joins_across_row_column_and_converting_tables() {
    let dir = TempDir::new().unwrap();
    let server = setup_three_engines(&dir);

    // Every side is read through its own storage path and the result is identical.
    let expected = vec![
        i64s(&[1, 10, 10, 10]),
        i64s(&[2, 20, 20, 20]),
        i64s(&[3, 30, 30, 30]),
        i64s(&[5, 50, 50, 50]),
    ];
    let three_way = rows(
        &server,
        "SELECT r.id, r.v, c.v, k.v FROM r JOIN c ON r.id = c.id JOIN k ON k.id = c.id \
         WHERE r.v IS NOT NULL ORDER BY r.id;",
    );
    let expected_rows: Vec<Row> = expected
        .into_iter()
        .map(|r| {
            let v = r.into_values();
            Row::new(vec![
                v[0].clone(),
                Value::Int32(match v[1] {
                    Value::Int64(x) => x as i32,
                    _ => unreachable!(),
                }),
                Value::Int32(match v[2] {
                    Value::Int64(x) => x as i32,
                    _ => unreachable!(),
                }),
                Value::Int32(match v[3] {
                    Value::Int64(x) => x as i32,
                    _ => unreachable!(),
                }),
            ])
        })
        .collect();
    assert_eq!(three_way, expected_rows);

    // Aggregates over a join between the columnar and the row table.
    let agg = rows(
        &server,
        "SELECT c.grp, COUNT(*), SUM(r.v), AVG(c.f) FROM c JOIN r ON c.id = r.id \
         GROUP BY c.grp ORDER BY c.grp;",
    );
    assert_eq!(
        agg,
        vec![
            Row::new(vec![
                Value::Null,
                Value::Int64(1),
                Value::Null,
                Value::Float64(4.5)
            ]),
            Row::new(vec![
                Value::String("a".into()),
                Value::Int64(2),
                Value::Int64(30),
                Value::Float64(1.5)
            ]),
            Row::new(vec![
                Value::String("b".into()),
                Value::Int64(2),
                Value::Int64(80),
                Value::Float64(4.5)
            ]),
        ]
    );

    // Point reads keep the rowstore fast path on every storage kind.
    for t in ["r", "c", "k"] {
        assert_eq!(
            rows(&server, &format!("SELECT v FROM {t} WHERE id = 5;")),
            vec![Row::new(vec![Value::Int32(50)])]
        );
    }
}

#[test]
fn test_outer_joins_null_padding_residual_on_and_null_keys() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE l (id BIGINT PRIMARY KEY, k INT, tag VARCHAR(4));")
        .unwrap();
    server
        .execute("CREATE TABLE rt (id BIGINT PRIMARY KEY, k INT, w INT);")
        .unwrap();
    server
        .execute("INSERT INTO l (id, k, tag) VALUES (1, 1, 'a'), (2, 2, 'b'), (3, NULL, 'n'), (4, 4, 'd');")
        .unwrap();
    server
        .execute(
            "INSERT INTO rt (id, k, w) VALUES (10, 1, 100), (11, 1, 5), (12, NULL, 7), (13, 9, 9);",
        )
        .unwrap();

    // LEFT JOIN with a residual condition: l.id=1 matches only rt.id=10 (w > 50),
    // l.id=2/3/4 are preserved with NULL padding; NULL keys never match.
    let left = rows(
        &server,
        "SELECT l.id, rt.id, rt.w FROM l LEFT JOIN rt ON l.k = rt.k AND rt.w > 50 ORDER BY l.id;",
    );
    assert_eq!(
        left,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(10), Value::Int32(100)]),
            Row::new(vec![Value::Int64(2), Value::Null, Value::Null]),
            Row::new(vec![Value::Int64(3), Value::Null, Value::Null]),
            Row::new(vec![Value::Int64(4), Value::Null, Value::Null]),
        ]
    );
    // Without the residual, l.id=1 matches two rows (right input order preserved).
    let left2 = rows(
        &server,
        "SELECT l.id, rt.id FROM l LEFT JOIN rt ON l.k = rt.k ORDER BY l.id, rt.id;",
    );
    assert_eq!(
        left2,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(10)]),
            Row::new(vec![Value::Int64(1), Value::Int64(11)]),
            Row::new(vec![Value::Int64(2), Value::Null]),
            Row::new(vec![Value::Int64(3), Value::Null]),
            Row::new(vec![Value::Int64(4), Value::Null]),
        ]
    );
    // RIGHT JOIN preserves unmatched right rows (including the NULL-key row).
    let right = rows(
        &server,
        "SELECT l.id, rt.id FROM l RIGHT JOIN rt ON l.k = rt.k ORDER BY rt.id;",
    );
    assert_eq!(
        right,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(10)]),
            Row::new(vec![Value::Int64(1), Value::Int64(11)]),
            Row::new(vec![Value::Null, Value::Int64(12)]),
            Row::new(vec![Value::Null, Value::Int64(13)]),
        ]
    );
    // Inner join on NULL = NULL produces nothing; cross join is a full product.
    assert!(rows(
        &server,
        "SELECT l.id FROM l JOIN rt ON l.k = rt.k WHERE l.k IS NULL;"
    )
    .is_empty());
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM l CROSS JOIN rt;"),
        vec![Row::new(vec![Value::Int64(16)])]
    );
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM l, rt WHERE l.id < rt.id;"),
        vec![Row::new(vec![Value::Int64(16)])]
    );
    // Non-equi join condition falls back to a nested loop: (1,9), (2,9), (4,9).
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM l JOIN rt ON l.k < rt.k;"),
        vec![Row::new(vec![Value::Int64(3)])]
    );
    // Output nullability of the null-supplied side.
    let q = query(&server, "SELECT rt.w FROM l LEFT JOIN rt ON l.k = rt.k;");
    assert!(q.columns[0].nullable);
}

#[test]
fn test_expressions_aggregates_having_order_limit_distinct() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE s (id INT PRIMARY KEY, grp VARCHAR(4), v INT, note VARCHAR(16));")
        .unwrap();
    server
        .execute(
            "INSERT INTO s (id, grp, v, note) VALUES (1, 'a', 1, 'Alpha'), (2, 'a', 3, 'beta'), \
             (3, 'b', 5, NULL), (4, 'b', 5, 'Delta'), (5, 'c', NULL, 'echo');",
        )
        .unwrap();

    let q = query(
        &server,
        "SELECT grp, COUNT(*) AS n, COUNT(DISTINCT v) AS dv, AVG(v) AS av, SUM(v) * 2 AS s2 \
         FROM s GROUP BY grp HAVING n >= 1 AND COUNT(*) > 0 ORDER BY av DESC, grp;",
    );
    let names: Vec<&str> = q.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["grp", "n", "dv", "av", "s2"]);
    assert_eq!(q.columns[3].data_type, DataType::Float64);
    assert_eq!(
        q.rows,
        vec![
            Row::new(vec![
                Value::String("b".into()),
                Value::Int64(2),
                Value::Int64(1),
                Value::Float64(5.0),
                Value::Int64(20)
            ]),
            Row::new(vec![
                Value::String("a".into()),
                Value::Int64(2),
                Value::Int64(2),
                Value::Float64(2.0),
                Value::Int64(8)
            ]),
            Row::new(vec![
                Value::String("c".into()),
                Value::Int64(1),
                Value::Int64(0),
                Value::Null,
                Value::Null
            ]),
        ]
    );

    // Expressions, CASE, LIKE (case-insensitive), IN, BETWEEN, OR, NOT, IS NULL, CAST.
    assert_eq!(
        rows(
            &server,
            "SELECT id, CASE WHEN v > 2 THEN 'big' WHEN v IS NULL THEN 'none' ELSE 'small' END, \
             UPPER(note), v * 10 + 1, CAST(id AS DOUBLE) / 2 \
             FROM s WHERE (note LIKE 'a%' OR note LIKE '%ta') AND NOT id IN (99) ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::String("small".into()),
                Value::String("ALPHA".into()),
                Value::Int64(11),
                Value::Float64(0.5)
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::String("big".into()),
                Value::String("BETA".into()),
                Value::Int64(31),
                Value::Float64(1.0)
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::String("big".into()),
                Value::String("DELTA".into()),
                Value::Int64(51),
                Value::Float64(2.0)
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM s WHERE v BETWEEN 3 AND 5 AND note IS NOT NULL ORDER BY id DESC;"
        ),
        vec![
            Row::new(vec![Value::Int32(4)]),
            Row::new(vec![Value::Int32(2)])
        ]
    );
    // 3-valued logic: NULL comparisons never pass a WHERE.
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM s WHERE v <> 5;"),
        vec![Row::new(vec![Value::Int64(2)])]
    );
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM s WHERE v <> 5 OR v IS NULL;"),
        vec![Row::new(vec![Value::Int64(3)])]
    );

    // ORDER BY alias vs source column, NULLS placement, both LIMIT forms, DISTINCT.
    assert_eq!(
        rows(
            &server,
            "SELECT id, v AS note FROM s ORDER BY note DESC, id LIMIT 2;"
        ),
        vec![
            Row::new(vec![Value::Int32(3), Value::Int32(5)]),
            Row::new(vec![Value::Int32(4), Value::Int32(5)]),
        ]
    );
    assert_eq!(
        rows(&server, "SELECT id FROM s ORDER BY v ASC, id LIMIT 1, 2;"),
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)])
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM s ORDER BY v DESC NULLS FIRST LIMIT 1;"
        ),
        vec![Row::new(vec![Value::Int32(5)])]
    );
    assert_eq!(
        rows(&server, "SELECT id FROM s ORDER BY id LIMIT 2 OFFSET 4;"),
        vec![Row::new(vec![Value::Int32(5)])]
    );
    assert!(rows(&server, "SELECT id FROM s LIMIT 0;").is_empty());
    assert!(rows(&server, "SELECT id FROM s ORDER BY id LIMIT 5 OFFSET 100;").is_empty());
    assert_eq!(
        rows(&server, "SELECT DISTINCT v FROM s ORDER BY v;"),
        vec![
            Row::new(vec![Value::Null]),
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(3)]),
            Row::new(vec![Value::Int32(5)]),
        ]
    );
    assert_eq!(
        rows(&server, "SELECT DISTINCT grp, v FROM s WHERE grp = 'b';"),
        vec![Row::new(vec![Value::String("b".into()), Value::Int32(5)])]
    );

    // Aggregate over an empty input yields one row; grouped empty input yields none.
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*), MAX(v), AVG(v) FROM s WHERE id > 100;"
        ),
        vec![Row::new(vec![Value::Int64(0), Value::Null, Value::Null])]
    );
    assert!(rows(
        &server,
        "SELECT grp, COUNT(*) FROM s WHERE id > 100 GROUP BY grp;"
    )
    .is_empty());
    // FROM-less select.
    assert_eq!(
        rows(&server, "SELECT 1 + 2, 'x', NULL;"),
        vec![Row::new(vec![
            Value::Int64(3),
            Value::String("x".into()),
            Value::Null
        ])]
    );
    // Runtime errors surface as InvalidArgument.
    let err = server
        .execute("SELECT CAST(note AS BIGINT) FROM s WHERE id = 1 OR id = 2;")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
}

#[test]
fn test_union_derived_tables_ctes_and_subqueries() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE a (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE b (id BIGINT PRIMARY KEY, v DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 1), (2, 2), (3, 3);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, v) VALUES (2, 2.0), (3, 3.5), (4, 4.0);")
        .unwrap();

    // UNION widens Int32/Int64 -> Int64 and Int32/Float64 -> Float64; DISTINCT dedups.
    let q = query(
        &server,
        "SELECT id, v FROM a UNION SELECT id, v FROM b ORDER BY id, v;",
    );
    assert_eq!(q.columns[0].data_type, DataType::Int64);
    assert_eq!(q.columns[1].data_type, DataType::Float64);
    assert_eq!(
        q.rows,
        vec![
            Row::new(vec![Value::Int64(1), Value::Float64(1.0)]),
            Row::new(vec![Value::Int64(2), Value::Float64(2.0)]),
            Row::new(vec![Value::Int64(3), Value::Float64(3.0)]),
            Row::new(vec![Value::Int64(3), Value::Float64(3.5)]),
            Row::new(vec![Value::Int64(4), Value::Float64(4.0)]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*) FROM (SELECT id FROM a UNION ALL SELECT id FROM b) AS u;"
        ),
        vec![Row::new(vec![Value::Int64(6)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM a UNION ALL SELECT id FROM b ORDER BY id LIMIT 2;"
        ),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)])
        ]
    );

    // Derived table with aggregate, CTE chain, and uncorrelated subqueries.
    assert_eq!(
        rows(
            &server,
            "SELECT t.total, t.n FROM (SELECT SUM(v) AS total, COUNT(*) AS n FROM a) AS t;",
        ),
        vec![Row::new(vec![Value::Int64(6), Value::Int64(3)])]
    );
    assert_eq!(
        rows(
            &server,
            "WITH big AS (SELECT id FROM b WHERE v > 2.5), \
             joined AS (SELECT a.id, a.v FROM a JOIN big ON a.id = big.id) \
             SELECT id, v FROM joined ORDER BY id;",
        ),
        vec![Row::new(vec![Value::Int32(3), Value::Int32(3)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM a WHERE id IN (SELECT id FROM b) AND v < (SELECT MAX(v) FROM b) \
             AND EXISTS (SELECT 1 FROM b WHERE v > 3) ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(3)])
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM a WHERE id NOT IN (SELECT id FROM b) ORDER BY id;"
        ),
        vec![Row::new(vec![Value::Int32(1)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT (SELECT COUNT(*) FROM b), id FROM a WHERE id = 1 OR id = 99;"
        ),
        vec![Row::new(vec![Value::Int64(3), Value::Int32(1)])]
    );
    // Scalar subquery with zero rows is NULL; with more than one row it is an error.
    assert_eq!(
        rows(
            &server,
            "SELECT (SELECT v FROM b WHERE id = 99) FROM a WHERE id = 1 OR id = 99;"
        ),
        vec![Row::new(vec![Value::Null])]
    );
    let err = server
        .execute("SELECT id FROM a WHERE v > (SELECT v FROM b);")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
    assert!(matches!(
        server
            .execute("SELECT id FROM a WHERE v > (SELECT v FROM b WHERE b.id = a.id);")
            .unwrap_err(),
        HtapError::Unsupported(_)
    ));
}

#[test]
fn test_partition_pruning_and_pushdown_through_general_path() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, day INT, v INT, PRIMARY KEY (id)) \
             PARTITION BY RANGE (day) (PARTITION p0 VALUES LESS THAN (10), \
             PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN MAXVALUE);",
        )
        .or_else(|_| {
            server.execute(
                "CREATE TABLE ev (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
                 PARTITION BY RANGE (day) (PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN MAXVALUE);",
            )
        })
        .unwrap();
    server
        .execute("INSERT INTO ev (id, day, v) VALUES (1, 1, 5), (2, 15, 6), (3, 25, 7), (4, 5, 8);")
        .unwrap();
    let report = server.convert_table_to_column("ev").unwrap();
    assert!(report.is_success(), "{report:?}");
    server
        .execute("INSERT INTO ev (id, day, v) VALUES (5, 12, 9);")
        .unwrap();

    // Pushdown-eligible conjunct on the partition key plus a residual expression.
    assert_eq!(
        rows(
            &server,
            "SELECT id, v FROM ev WHERE day >= 10 AND day < 20 AND v * 2 >= 12 ORDER BY id;"
        ),
        vec![
            Row::new(vec![Value::Int64(2), Value::Int32(6)]),
            Row::new(vec![Value::Int64(5), Value::Int32(9)]),
        ]
    );
    // Literal on the left and fractional literal are handled (not pushed, still correct).
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*) FROM ev WHERE 10 <= day AND day > 4.5;"
        ),
        vec![Row::new(vec![Value::Int64(3)])]
    );
    // Self join across partitions.
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*) FROM ev a JOIN ev b ON a.day < b.day;"
        ),
        vec![Row::new(vec![Value::Int64(10)])]
    );
}

/// All sides of a join are read at one snapshot per statement, and every statement sees
/// the latest committed version (including rows written into the columnar delta).
#[test]
fn test_single_snapshot_across_engines_and_freshness() {
    let dir = TempDir::new().unwrap();
    let server = setup_three_engines(&dir);

    let count = |sql: &str| match rows(&server, sql)[0].get(0) {
        Some(Value::Int64(n)) => *n,
        other => panic!("{other:?}"),
    };
    let join_sql = "SELECT COUNT(*) FROM r JOIN c ON r.id = c.id JOIN k ON k.id = r.id;";
    assert_eq!(count(join_sql), 5);

    // Rows written to the columnar table's rowstore delta and to the row table after the
    // previous statement are visible to the next statement, on all sides consistently.
    server
        .execute("INSERT INTO c (id, grp, v, f) VALUES (6, 'z', 60, 6.5);")
        .unwrap();
    assert_eq!(count(join_sql), 5, "c has id 6 but r and k do not");
    server
        .execute("INSERT INTO r (id, grp, v, f) VALUES (6, 'z', 60, 6.5);")
        .unwrap();
    server
        .execute("INSERT INTO k (id, grp, v, f) VALUES (6, 'z', 60, 6.5);")
        .unwrap();
    assert_eq!(count(join_sql), 6);
    // A delete on the columnar side is honoured through the delta overlay.
    server.execute("DELETE FROM c WHERE id = 1;").unwrap();
    assert_eq!(count(join_sql), 5);
    assert_eq!(
        count("SELECT COUNT(*) FROM r LEFT JOIN c ON r.id = c.id WHERE c.id IS NULL;"),
        1
    );
    // The result of one statement is internally consistent: a self-join of the columnar
    // table over all three storage paths yields exactly one row per key.
    assert_eq!(
        count("SELECT COUNT(*) FROM c x JOIN c y ON x.id = y.id JOIN k z ON z.id = x.id;"),
        5
    );
}

#[test]
fn test_general_query_over_reopened_server() {
    let dir = TempDir::new().unwrap();
    {
        let server = setup_three_engines(&dir);
        assert_eq!(
            rows(&server, "SELECT COUNT(*) FROM r JOIN c ON r.id = c.id;"),
            vec![Row::new(vec![Value::Int64(5)])]
        );
    }
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        rows(&server, "SELECT r.id FROM r JOIN c ON r.id = c.id JOIN k ON k.id = r.id ORDER BY r.id DESC LIMIT 1;"),
        vec![Row::new(vec![Value::Int64(5)])]
    );
}

fn dml(server: &LocalServer, sql: &str) -> (u64, Option<Version>) {
    match server.execute(sql) {
        Ok(StatementResult::Command(cmd)) => (cmd.affected(), cmd.version()),
        Ok(other) => panic!("expected command result for {sql}, got {other:?}"),
        Err(e) => panic!("{sql}: {e}"),
    }
}

#[test]
fn test_update_by_primary_key_and_reopen_recovery() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE acct (id BIGINT PRIMARY KEY, owner VARCHAR(16) NOT NULL, balance DOUBLE, n INT);")
            .unwrap();
        let (_, v1) = dml(&server, "INSERT INTO acct (id, owner, balance, n) VALUES (1, 'ann', 10.0, 1), (2, 'bob', 20.0, 2);");
        // Assignments apply left to right: n uses the already-updated balance.
        let (affected, v2) = dml(
            &server,
            "UPDATE acct SET balance = balance * 2, n = CAST(balance AS INT), owner = UPPER(owner) WHERE id = 1;",
        );
        assert_eq!(affected, 1);
        assert_eq!(v2.unwrap().get(), v1.unwrap().get() + 1);
        assert_eq!(
            rows(&server, "SELECT owner, balance, n FROM acct WHERE id = 1;"),
            vec![Row::new(vec![
                Value::String("ANN".into()),
                Value::Float64(20.0),
                Value::Int32(20)
            ])]
        );
        // Missing key: nothing happens and no version is consumed.
        assert_eq!(
            dml(&server, "UPDATE acct SET n = 0 WHERE id = 99;"),
            (0, None)
        );
        let (_, v3) = dml(&server, "UPDATE acct SET n = NULL WHERE id = 2;");
        assert_eq!(v3.unwrap().get(), v2.unwrap().get() + 1);
        // Constraint violations are reported and leave the row untouched.
        let err = server
            .execute("UPDATE acct SET owner = NULL WHERE id = 2;")
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
        for sql in [
            "UPDATE acct SET id = 5 WHERE id = 2;",
            "UPDATE acct SET n = (SELECT MAX(n) FROM acct) WHERE id = 2;",
        ] {
            assert!(
                matches!(server.execute(sql).unwrap_err(), HtapError::Unsupported(_)),
                "{sql}"
            );
        }
        assert!(matches!(
            server
                .execute("UPDATE acct SET n = 'x' WHERE id = 2;")
                .unwrap_err(),
            HtapError::InvalidArgument(_)
        ));
    }
    // Updated versions survive reopen.
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        rows(
            &server,
            "SELECT id, owner, balance, n FROM acct ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int64(1),
                Value::String("ANN".into()),
                Value::Float64(20.0),
                Value::Int32(20)
            ]),
            Row::new(vec![
                Value::Int64(2),
                Value::String("bob".into()),
                Value::Float64(20.0),
                Value::Null
            ]),
        ]
    );
    // Point reads see the updated row through the rowstore fast path.
    assert_eq!(
        rows(&server, "SELECT n FROM acct WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(20)])]
    );
}

#[test]
fn test_update_by_filter_across_partitions_and_storage_formats_with_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute(
                "CREATE TABLE ev (id BIGINT, day INT, v INT, tag VARCHAR(8), PRIMARY KEY (id, day)) \
                 PARTITION BY RANGE (day) (PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN MAXVALUE);",
            )
            .unwrap();
        server
            .execute("INSERT INTO ev (id, day, v, tag) VALUES (1, 1, 5, 'x'), (2, 15, 6, 'y'), (3, 25, 7, 'x'), (4, 5, 8, NULL);")
            .unwrap();
        // One statement, one transaction, rows in both partitions.
        let (affected, v) = dml(
            &server,
            "UPDATE ev SET v = v + 100, tag = COALESCE(tag, 'none') WHERE v >= 6 OR tag IS NULL;",
        );
        assert_eq!(affected, 3);
        assert!(v.is_some());
        assert_eq!(
            rows(&server, "SELECT id, v, tag FROM ev ORDER BY id;"),
            vec![
                Row::new(vec![
                    Value::Int64(1),
                    Value::Int32(5),
                    Value::String("x".into())
                ]),
                Row::new(vec![
                    Value::Int64(2),
                    Value::Int32(106),
                    Value::String("y".into())
                ]),
                Row::new(vec![
                    Value::Int64(3),
                    Value::Int32(107),
                    Value::String("x".into())
                ]),
                Row::new(vec![
                    Value::Int64(4),
                    Value::Int32(108),
                    Value::String("none".into())
                ]),
            ]
        );
        // Zero matches: no transaction.
        assert_eq!(
            dml(&server, "UPDATE ev SET v = 0 WHERE v > 1000;"),
            (0, None)
        );
        // Partition key assignment is rejected.
        assert!(matches!(
            server
                .execute("UPDATE ev SET day = 3 WHERE id = 1;")
                .unwrap_err(),
            HtapError::Unsupported(_)
        ));

        // Unfiltered UPDATE on a columnar table writes into the rowstore delta.
        server
            .execute("CREATE TABLE col (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO col (id, v) VALUES (1, 1), (2, 2);")
            .unwrap();
        assert!(server.convert_table_to_column("col").unwrap().is_success());
        assert_eq!(dml(&server, "UPDATE col SET v = v * 10;").0, 2);
        assert_eq!(
            rows(&server, "SELECT SUM(v) FROM col;"),
            vec![Row::new(vec![Value::Int64(30)])]
        );
        assert_eq!(
            rows(&server, "SELECT v FROM col WHERE id = 2;"),
            vec![Row::new(vec![Value::Int32(20)])]
        );
    }
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        rows(&server, "SELECT SUM(v) FROM ev;"),
        vec![Row::new(vec![Value::Int64(326)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT c.v, e.v FROM col c JOIN ev e ON c.id = e.id ORDER BY c.id;"
        ),
        vec![
            Row::new(vec![Value::Int32(10), Value::Int32(5)]),
            Row::new(vec![Value::Int32(20), Value::Int32(106)]),
        ]
    );
}

fn strings(server: &LocalServer, sql: &str) -> Vec<String> {
    rows(server, sql)
        .into_iter()
        .map(|r| match r.get(0) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn test_show_tables_databases_columns_and_describe() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    assert!(rows(&server, "SHOW TABLES;").is_empty());
    server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(16) NOT NULL, age INT, raw VARBINARY(8), ts TIMESTAMP, ok BOOL, score DOUBLE);")
        .unwrap();
    server
        .execute("CREATE TABLE orders (tenant INT, oid BIGINT, PRIMARY KEY (tenant, oid));")
        .unwrap();

    let q = query(&server, "SHOW TABLES;");
    assert_eq!(q.columns[0].name, "Tables_in_htap");
    assert_eq!(strings(&server, "SHOW TABLES;"), ["orders", "users"]);
    assert_eq!(strings(&server, "SHOW TABLES LIKE 'u%';"), ["users"]);
    assert_eq!(strings(&server, "SHOW DATABASES;"), ["htap"]);

    let describe = query(&server, "DESCRIBE users;");
    let names: Vec<&str> = describe.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Field", "Type", "Null", "Key", "Default", "Extra"]);
    let s = |v: &str| Value::String(v.into());
    assert_eq!(
        describe.rows,
        vec![
            Row::new(vec![
                s("id"),
                s("bigint"),
                s("NO"),
                s("PRI"),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("name"),
                s("varchar"),
                s("NO"),
                s(""),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("age"),
                s("int"),
                s("YES"),
                s(""),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("raw"),
                s("varbinary"),
                s("YES"),
                s(""),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("ts"),
                s("timestamp"),
                s("YES"),
                s(""),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("ok"),
                s("bool"),
                s("YES"),
                s(""),
                Value::Null,
                s("")
            ]),
            Row::new(vec![
                s("score"),
                s("double"),
                s("YES"),
                s(""),
                Value::Null,
                s("")
            ]),
        ]
    );
    assert_eq!(
        query(&server, "SHOW COLUMNS FROM orders;").rows,
        query(&server, "DESC orders;").rows
    );
    assert_eq!(
        rows(&server, "SHOW COLUMNS FROM orders;")[1],
        Row::new(vec![
            s("oid"),
            s("bigint"),
            s("NO"),
            s("PRI"),
            Value::Null,
            s("")
        ])
    );
    assert!(matches!(
        server.execute("DESCRIBE nope;").unwrap_err(),
        HtapError::NotFound(_)
    ));
}

#[test]
fn test_drop_table_reopen_and_no_id_reuse() {
    let dir = TempDir::new().unwrap();
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let ids_of = |name: &str| {
        let cat = cat_store.load().unwrap().unwrap();
        let t = cat.table_by_name(name).unwrap();
        let parts: Vec<u64> = t.partitions.iter().map(|p| p.as_u64()).collect();
        let tablets: Vec<u64> = cat
            .partitions
            .iter()
            .filter(|p| p.table_id == t.id)
            .flat_map(|p| p.tablets.iter().map(|x| x.as_u64()))
            .collect();
        (t.id.as_u64(), parts, tablets)
    };
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute(
                "CREATE TABLE big (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
                 PARTITION BY RANGE (day) (PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN MAXVALUE);",
            )
            .unwrap();
        server
            .execute("INSERT INTO big (id, day, v) VALUES (1, 1, 1), (2, 15, 2), (3, 25, 3);")
            .unwrap();
        assert!(server.convert_table_to_column("big").unwrap().is_success());
        server
            .execute("INSERT INTO big (id, day, v) VALUES (4, 30, 4);")
            .unwrap();
        let (big_table, big_parts, big_tablets) = ids_of("big");
        assert_eq!(big_parts.len(), 3);

        // Drop it (metadata only), then verify it is gone from every surface.
        assert_eq!(
            server.execute("DROP TABLE big;").unwrap(),
            StatementResult::ddl(1)
        );
        assert!(matches!(
            server.execute("SELECT COUNT(*) FROM big;").unwrap_err(),
            HtapError::NotFound(_)
        ));
        assert!(matches!(
            server.execute("DROP TABLE big;").unwrap_err(),
            HtapError::NotFound(_)
        ));
        assert_eq!(
            server.execute("DROP TABLE IF EXISTS big;").unwrap(),
            StatementResult::ddl(0)
        );
        assert!(rows(&server, "SHOW TABLES;").is_empty());
        let cat = cat_store.load().unwrap().unwrap();
        assert!(
            cat.tables.is_empty()
                && cat.partitions.is_empty()
                && cat.tablets.is_empty()
                && cat.replicas.is_empty()
        );
        // The colstore files of the dropped tablets are left on disk (no reclamation).
        assert!(dir
            .path()
            .join("colstore")
            .read_dir()
            .unwrap()
            .next()
            .is_some());

        // New tables never reuse the dropped identifiers.
        server
            .execute("CREATE TABLE fresh1 (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("CREATE TABLE fresh2 (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        for name in ["fresh1", "fresh2"] {
            let (t, parts, tablets) = ids_of(name);
            assert!(t > big_table, "{name}");
            for p in &parts {
                assert!(
                    !big_parts.contains(p) && *p > *big_parts.iter().max().unwrap(),
                    "{name}"
                );
            }
            for tb in &tablets {
                assert!(!big_tablets.contains(tb), "{name}");
            }
        }
        // A table with the dropped name can be recreated and starts empty (no resurrected rows).
        server
            .execute(
                "CREATE TABLE big (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
                 PARTITION BY RANGE (day) (PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN MAXVALUE);",
            )
            .unwrap();
        assert_eq!(
            rows(&server, "SELECT COUNT(*) FROM big;"),
            vec![Row::new(vec![Value::Int64(0)])]
        );
        server
            .execute("INSERT INTO big (id, day, v) VALUES (1, 1, 100);")
            .unwrap();
        assert_eq!(
            rows(&server, "SELECT v FROM big WHERE id = 1 AND day = 1;"),
            vec![Row::new(vec![Value::Int32(100)])]
        );
    }
    // Reopen: fail-closed startup validation ignores the orphaned manifests, data is intact,
    // and allocation continues above the persisted high-water mark.
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        strings(&server, "SHOW TABLES;"),
        ["big", "fresh1", "fresh2"]
    );
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM big;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    let (_, fresh2_parts, _) = ids_of("fresh2");
    server
        .execute("CREATE TABLE fresh3 (id BIGINT PRIMARY KEY);")
        .unwrap();
    let (_, fresh3_parts, _) = ids_of("fresh3");
    assert!(fresh3_parts[0] > fresh2_parts[0]);
    let hw = cat_store.load().unwrap().unwrap().id_high_water();
    assert!(hw.partition >= fresh3_parts[0]);

    // Dropping while a partition is converting is refused.
    let cur = cat_store.load().unwrap().unwrap();
    let t = cur.table_by_name("fresh1").unwrap();
    let pid = t.partitions[0];
    let mut next = cur.clone();
    next.generation += 1;
    let p = next.partitions.iter_mut().find(|p| p.id == pid).unwrap();
    p.generation = next.generation;
    p.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: next.generation,
    };
    p.conversion = Some(ConversionDescriptor::new(
        next.generation,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    cat_store.compare_and_set(cur.generation, next).unwrap();
    assert!(matches!(
        server.execute("DROP TABLE fresh1;").unwrap_err(),
        HtapError::Conflict(_)
    ));
}

/// A catalog written before the identifier high-water mark existed (format v1, mark all
/// zeros) may already have dropped tablets whose `colstore/tablet-N` directories remain.
/// On open the server seeds the tablet counter from the on-disk inventory so a new table
/// never receives such an id (which would make its later conversion trip over the stale
/// manifest).
#[test]
fn test_legacy_catalog_seeds_tablet_high_water_from_colstore_inventory() {
    use htap_catalog::local::{encode_snapshot, HEADER_MAGIC};
    let dir = TempDir::new().unwrap();
    let orphan_tablet = {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE old (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO old (id, v) VALUES (1, 1);")
            .unwrap();
        assert!(server.convert_table_to_column("old").unwrap().is_success());
        let cat = LocalCatalogStore::open(dir.path().join("catalog"))
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        cat.tablets[0].id
    };
    assert!(dir
        .path()
        .join("colstore")
        .join(format!("tablet-{}", orphan_tablet.as_u64()))
        .join("MANIFEST")
        .is_file());

    // Rewrite the catalog as a legacy v1 file: the table is gone (as after a pre-upgrade
    // DROP PARTITION) and the mark is absent (decodes as zeros).
    {
        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let cur = cat_store.load().unwrap().unwrap();
        let legacy =
            htap_catalog::CatalogSnapshot::new(cur.generation + 1, vec![], vec![], vec![], vec![]);
        let mut bytes = encode_snapshot(&legacy).unwrap();
        assert_eq!(&bytes[0..8], HEADER_MAGIC);
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(dir.path().join("catalog").join("CATALOG"), bytes).unwrap();
    }

    {
        let server = LocalServer::open(dir.path()).unwrap();
        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let migrated = cat_store.load().unwrap().unwrap();
        assert!(migrated.id_high_water.tablet >= orphan_tablet.as_u64());
        server
            .execute("CREATE TABLE fresh (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        let cat = cat_store.load().unwrap().unwrap();
        let fresh_tablet = cat.tablets[0].id;
        assert!(fresh_tablet.as_u64() > orphan_tablet.as_u64());
        server
            .execute("INSERT INTO fresh (id, v) VALUES (1, 10), (2, 20);")
            .unwrap();
        assert!(server
            .convert_table_to_column("fresh")
            .unwrap()
            .is_success());
        assert_eq!(
            rows(&server, "SELECT SUM(v) FROM fresh;"),
            vec![Row::new(vec![Value::Int64(30)])]
        );
    }
    // Reopen passes fail-closed validation and the data is intact.
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        rows(&server, "SELECT v FROM fresh WHERE id = 2;"),
        vec![Row::new(vec![Value::Int32(20)])]
    );
}

/// Point UPDATE against a Column partition rewrites the row in the rowstore delta; the
/// columnar base still holds the old version and the overlay wins, across reopen.
#[test]
fn test_update_by_primary_key_on_column_and_converting_partitions() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE col (id BIGINT PRIMARY KEY, v INT, tag VARCHAR(8));")
            .unwrap();
        server
            .execute("INSERT INTO col (id, v, tag) VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c');")
            .unwrap();
        assert!(server.convert_table_to_column("col").unwrap().is_success());
        assert_eq!(
            dml(
                &server,
                "UPDATE col SET v = v + 100, tag = CONCAT(tag, '!') WHERE id = 2;"
            )
            .0,
            1
        );
        assert_eq!(
            rows(&server, "SELECT v, tag FROM col WHERE id = 2;"),
            vec![Row::new(vec![
                Value::Int32(102),
                Value::String("b!".into())
            ])]
        );
        assert_eq!(
            rows(
                &server,
                "SELECT id, v FROM col WHERE v > 50 OR id = 1 ORDER BY id;"
            ),
            vec![
                Row::new(vec![Value::Int64(1), Value::Int32(1)]),
                Row::new(vec![Value::Int64(2), Value::Int32(102)]),
            ]
        );

        // Partition in `Converting` (SnapshotPinned): the update lands in the rowstore,
        // and resuming the conversion keeps it (rowstore is authoritative).
        server
            .execute("CREATE TABLE conv (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO conv (id, v) VALUES (1, 1), (2, 2);")
            .unwrap();
        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let cur = cat_store.load().unwrap().unwrap();
        let pid = cur.table_by_name("conv").unwrap().partitions[0];
        let mut next = cur.clone();
        next.generation += 1;
        let p = next.partitions.iter_mut().find(|p| p.id == pid).unwrap();
        p.generation = next.generation;
        p.storage = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: next.generation,
        };
        p.conversion = Some(ConversionDescriptor::new(
            next.generation,
            StorageFormat::Row,
            StorageFormat::Column,
            Version::new(1),
            ConversionPhase::SnapshotPinned,
        ));
        cat_store.compare_and_set(cur.generation, next).unwrap();
        drop(cat_store);
        assert_eq!(dml(&server, "UPDATE conv SET v = 22 WHERE id = 2;").0, 1);
        assert_eq!(
            dml(&server, "UPDATE conv SET v = v * 10 WHERE v < 10;").0,
            1
        );
        let tick = server.tick().unwrap();
        assert!(tick.is_success(), "{tick:?}");
        assert_eq!(
            rows(&server, "SELECT id, v FROM conv ORDER BY id;"),
            vec![
                Row::new(vec![Value::Int64(1), Value::Int32(10)]),
                Row::new(vec![Value::Int64(2), Value::Int32(22)]),
            ]
        );
    }
    let server = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        rows(&server, "SELECT v FROM col WHERE id = 2;"),
        vec![Row::new(vec![Value::Int32(102)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT c.v, k.v FROM col c JOIN conv k ON c.id = k.id ORDER BY c.id;"
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(102), Value::Int32(22)]),
        ]
    );
}
