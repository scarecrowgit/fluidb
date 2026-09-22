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
fn test_intersect_binds_tighter_than_union() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE sa (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE sb (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE sc (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO sa (id, v) VALUES (1, 1), (2, 2);")
        .unwrap();
    server
        .execute("INSERT INTO sb (id, v) VALUES (1, 2), (2, 3);")
        .unwrap();
    server
        .execute("INSERT INTO sc (id, v) VALUES (1, 2);")
        .unwrap();

    // INTERSECT binds tighter than UNION: {1, 2} UNION ({2, 3} INTERSECT {2}) = {1, 2}.
    assert_eq!(
        rows(
            &server,
            "SELECT v FROM sa UNION SELECT v FROM sb INTERSECT SELECT v FROM sc ORDER BY v;",
        ),
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ]
    );
}

#[test]
fn test_hash_join_preserves_large_integer_keys() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE li (id BIGINT PRIMARY KEY, k BIGINT);")
        .unwrap();
    server
        .execute("CREATE TABLE ri (id BIGINT PRIMARY KEY, k BIGINT);")
        .unwrap();
    server
        .execute("INSERT INTO li (id, k) VALUES (1, 9007199254740992), (2, 7);")
        .unwrap();
    server
        .execute("INSERT INTO ri (id, k) VALUES (10, 9007199254740993), (11, 7);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT li.id, ri.id FROM li JOIN ri ON li.k = ri.k ORDER BY li.id;",
        ),
        vec![Row::new(vec![Value::Int64(2), Value::Int64(11)])]
    );

    server
        .execute("CREATE TABLE i32 (id INT PRIMARY KEY, k INT);")
        .unwrap();
    server
        .execute("INSERT INTO i32 (id, k) VALUES (1, 7);")
        .unwrap();
    assert_eq!(
        rows(
            &server,
            "SELECT i32.id, ri.id FROM i32 JOIN ri ON i32.k = ri.k;",
        ),
        vec![Row::new(vec![Value::Int32(1), Value::Int64(11)])]
    );
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
fn test_using_and_natural_joins_merge_columns() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE ua (id INT PRIMARY KEY, av VARCHAR(4), only_a INT);")
        .unwrap();
    server
        .execute("CREATE TABLE ub (id INT PRIMARY KEY, bv VARCHAR(4), only_b INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO ua (id, av, only_a) VALUES (1, 'a1', 10), (2, 'a2', 20), (3, 'a3', 30);",
        )
        .unwrap();
    server
        .execute("INSERT INTO ub (id, bv, only_b) VALUES (1, 'b1', 100), (4, 'b4', 400);")
        .unwrap();

    assert_eq!(
        rows(&server, "SELECT * FROM ua JOIN ub USING(id) ORDER BY id;"),
        vec![Row::new(vec![
            Value::Int32(1),
            Value::String("a1".into()),
            Value::Int32(10),
            Value::String("b1".into()),
            Value::Int32(100),
        ])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM ua LEFT JOIN ub USING(id) ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::String("a1".into()),
                Value::Int32(10),
                Value::String("b1".into()),
                Value::Int32(100)
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::String("a2".into()),
                Value::Int32(20),
                Value::Null,
                Value::Null
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::String("a3".into()),
                Value::Int32(30),
                Value::Null,
                Value::Null
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM ua RIGHT JOIN ub USING(id) ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::String("a1".into()),
                Value::Int32(10),
                Value::String("b1".into()),
                Value::Int32(100)
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Null,
                Value::Null,
                Value::String("b4".into()),
                Value::Int32(400)
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM ua FULL JOIN ub USING(id) WHERE id >= 3 ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(3),
                Value::String("a3".into()),
                Value::Int32(30),
                Value::Null,
                Value::Null
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Null,
                Value::Null,
                Value::String("b4".into()),
                Value::Int32(400)
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT ua.id, ub.id, id FROM ua FULL JOIN ub USING(id) WHERE id = 4 ORDER BY id;",
        ),
        vec![Row::new(vec![
            Value::Null,
            Value::Int32(4),
            Value::Int32(4)
        ])]
    );

    server
        .execute("CREATE TABLE na (id INT PRIMARY KEY, shared INT, av VARCHAR(4));")
        .unwrap();
    server
        .execute("CREATE TABLE nb (id INT PRIMARY KEY, shared INT, bv VARCHAR(4));")
        .unwrap();
    server
        .execute("INSERT INTO na (id, shared, av) VALUES (1, 9, 'a1'), (2, NULL, 'a2');")
        .unwrap();
    server
        .execute("INSERT INTO nb (id, shared, bv) VALUES (1, 9, 'b1'), (3, 7, 'b3');")
        .unwrap();

    assert_eq!(
        rows(&server, "SELECT * FROM na NATURAL JOIN nb ORDER BY id;"),
        vec![Row::new(vec![
            Value::Int32(1),
            Value::Int32(9),
            Value::String("a1".into()),
            Value::String("b1".into()),
        ])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM na NATURAL LEFT JOIN nb ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int32(9),
                Value::String("a1".into()),
                Value::String("b1".into())
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Null,
                Value::String("a2".into()),
                Value::Null
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM na NATURAL RIGHT JOIN nb ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int32(9),
                Value::String("a1".into()),
                Value::String("b1".into())
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int32(7),
                Value::Null,
                Value::String("b3".into())
            ]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM na NATURAL FULL JOIN nb ORDER BY id;"
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int32(9),
                Value::String("a1".into()),
                Value::String("b1".into())
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Null,
                Value::String("a2".into()),
                Value::Null
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int32(7),
                Value::Null,
                Value::String("b3".into())
            ]),
        ]
    );
}

#[test]
fn test_nested_join_groups_and_full_join_execute() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    for table in [
        "CREATE TABLE a (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE b (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE c (id INT PRIMARY KEY, v INT);",
    ] {
        server.execute(table).unwrap();
    }
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, v) VALUES (1, 100), (2, 200), (3, 300);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, v) VALUES (1, 1000), (3, 3000), (4, 4000);")
        .unwrap();

    // The nested inner group is NULL-padded as one relation by the enclosing LEFT JOIN.
    assert_eq!(
        rows(
            &server,
            "SELECT a.id, b.id, c.id FROM a LEFT JOIN (b JOIN c ON b.id = c.id) \
             ON a.id = b.id ORDER BY a.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(1), Value::Int32(1)]),
            Row::new(vec![Value::Int32(2), Value::Null, Value::Null]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT a.id, b.id, c.id FROM (a LEFT JOIN b ON a.id = b.id) \
             FULL JOIN c ON b.id = c.id ORDER BY COALESCE(a.id, b.id, c.id);",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(1), Value::Int32(1)]),
            Row::new(vec![Value::Int32(2), Value::Int32(2), Value::Null]),
            Row::new(vec![Value::Null, Value::Null, Value::Int32(3)]),
            Row::new(vec![Value::Null, Value::Null, Value::Int32(4)]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT a.id, b.id, c.id FROM a JOIN (b LEFT JOIN c ON b.id = c.id) \
             ON a.id = b.id ORDER BY a.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(1), Value::Int32(1)]),
            Row::new(vec![Value::Int32(2), Value::Int32(2), Value::Null]),
        ]
    );

    server
        .execute("CREATE TABLE fa (id INT PRIMARY KEY, k INT);")
        .unwrap();
    server
        .execute("CREATE TABLE fb (id INT PRIMARY KEY, k INT);")
        .unwrap();
    server
        .execute("INSERT INTO fa (id, k) VALUES (1, 1), (2, 2), (3, NULL);")
        .unwrap();
    server
        .execute("INSERT INTO fb (id, k) VALUES (10, 1), (20, 3), (30, NULL);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT fa.id, fb.id FROM fa FULL OUTER JOIN fb ON fa.k = fb.k;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(2), Value::Null]),
            Row::new(vec![Value::Int32(3), Value::Null]),
            Row::new(vec![Value::Null, Value::Int32(20)]),
            Row::new(vec![Value::Null, Value::Int32(30)]),
        ]
    );
}

#[test]
fn test_nested_join_groups_using_qualified_access_derived_tables_and_subqueries() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    for table in [
        "CREATE TABLE a (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE b (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE c (id INT PRIMARY KEY, v INT);",
    ] {
        server.execute(table).unwrap();
    }
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, v) VALUES (1, 100), (2, 200), (3, 300);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, v) VALUES (1, 1000), (3, 3000), (4, 4000);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT a.id, b.id, c.id FROM a LEFT JOIN (b JOIN c USING(id)) \
             ON a.id = b.id ORDER BY a.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(1), Value::Int32(1)]),
            Row::new(vec![Value::Int32(2), Value::Null, Value::Null]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT a.id, b.v, c.v FROM a JOIN (b JOIN c USING(id)) ON a.id = b.id \
             ORDER BY a.id;",
        ),
        vec![Row::new(vec![
            Value::Int32(1),
            Value::Int32(100),
            Value::Int32(1000),
        ])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT x.id, x.bv, x.cv FROM ( \
                 SELECT b.id, b.v AS bv, c.v AS cv FROM (b JOIN c USING(id)) \
             ) AS x ORDER BY x.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(100), Value::Int32(1000),]),
            Row::new(vec![Value::Int32(3), Value::Int32(300), Value::Int32(3000),]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id FROM a WHERE id IN ( \
                 SELECT b.id FROM (b JOIN c USING(id)) \
             ) ORDER BY id;",
        ),
        vec![Row::new(vec![Value::Int32(1)])]
    );
}

#[test]
fn test_flat_and_tree_join_evaluators_match_for_lowerable_queries() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    for table in [
        "CREATE TABLE a (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE b (id INT PRIMARY KEY, v INT);",
        "CREATE TABLE c (id INT PRIMARY KEY, v INT);",
    ] {
        server.execute(table).unwrap();
    }
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, v) VALUES (1, 100), (2, 200), (4, 400);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, v) VALUES (1, 1000), (2, 2000), (5, 5000);")
        .unwrap();

    // The first query uses the established flat left-deep executor. Parenthesizing the same
    // lowerable inner tree forces recursive evaluation, so rows and result metadata must match.
    for (flat, tree) in [
        (
            "SELECT a.id, a.v, b.v, c.v FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id ORDER BY a.id;",
            "SELECT a.id, a.v, b.v, c.v FROM a JOIN (b JOIN c ON b.id = c.id) ON a.id = b.id ORDER BY a.id;",
        ),
        (
            "SELECT a.id, COUNT(*) FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id GROUP BY a.id ORDER BY a.id;",
            "SELECT a.id, COUNT(*) FROM a JOIN (b JOIN c ON b.id = c.id) ON a.id = b.id GROUP BY a.id ORDER BY a.id;",
        ),
    ] {
        assert_eq!(query(&server, flat), query(&server, tree), "{flat}");
    }
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
fn test_window_rows_frame_entirely_outside_partition_is_empty() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE outside_frame (id INT PRIMARY KEY);")
        .unwrap();
    server
        .execute("INSERT INTO outside_frame (id) VALUES (1);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY id \
                     ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING), \
                 SUM(id) OVER (ORDER BY id \
                     ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING) \
             FROM outside_frame;",
        ),
        vec![Row::new(vec![
            Value::Int32(1),
            Value::Int64(0),
            Value::Null,
        ])]
    );
}

#[test]
fn test_window_order_by_desc_null_placement() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE desc_nulls (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO desc_nulls (id, v) VALUES \
             (1, NULL), (2, 10), (3, 20), (4, NULL);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 ROW_NUMBER() OVER (ORDER BY v DESC NULLS FIRST, id), \
                 ROW_NUMBER() OVER (ORDER BY v DESC NULLS LAST, id) \
             FROM desc_nulls ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(1), Value::Int64(3)]),
            Row::new(vec![Value::Int32(2), Value::Int64(4), Value::Int64(2)]),
            Row::new(vec![Value::Int32(3), Value::Int64(3), Value::Int64(1)]),
            Row::new(vec![Value::Int32(4), Value::Int64(2), Value::Int64(4)]),
        ]
    );
}

#[test]
fn test_window_over_group_by_having_ranks_aggregated_groups() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE having_window (id INT PRIMARY KEY, grp VARCHAR(4));")
        .unwrap();
    server
        .execute(
            "INSERT INTO having_window (id, grp) VALUES \
             (1, 'b'), (2, 'a'), (3, 'b'), (4, 'c');",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT grp, ROW_NUMBER() OVER (ORDER BY grp) AS group_rank \
             FROM having_window GROUP BY grp HAVING COUNT(*) > 0 ORDER BY grp;",
        ),
        vec![
            Row::new(vec![Value::String("a".into()), Value::Int64(1)]),
            Row::new(vec![Value::String("b".into()), Value::Int64(2)]),
            Row::new(vec![Value::String("c".into()), Value::Int64(3)]),
        ]
    );
}

#[test]
fn test_window_first_last_value_respect_nulls() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE respect_nulls (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO respect_nulls (id, v) VALUES \
             (1, NULL), (2, 10), (3, 20), (4, NULL);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 FIRST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 LAST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
             FROM respect_nulls ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Null, Value::Null]),
            Row::new(vec![Value::Int32(2), Value::Null, Value::Null]),
            Row::new(vec![Value::Int32(3), Value::Null, Value::Null]),
            Row::new(vec![Value::Int32(4), Value::Null, Value::Null]),
        ]
    );
}

#[test]
fn test_window_argument_with_subquery_and_variable() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE window_context (id INT PRIMARY KEY);")
        .unwrap();
    server
        .execute("INSERT INTO window_context (id) VALUES (1), (2), (3);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 LAG((SELECT 7), 1, @missing) OVER (ORDER BY id) \
             FROM window_context ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Null]),
            Row::new(vec![Value::Int32(2), Value::Int64(7)]),
            Row::new(vec![Value::Int32(3), Value::Int64(7)]),
        ]
    );
}

#[test]
fn test_window_ranking_functions_ties_and_peers() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE rankings (id INT PRIMARY KEY, grp VARCHAR(4), score INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO rankings (id, grp, score) VALUES \
             (1, 'a', 10), (2, 'a', 10), (3, 'a', 20), (4, 'a', NULL), \
             (5, 'b', 5), (6, 'b', 5), (7, 'b', 8), \
             (8, NULL, 7), (9, NULL, NULL), (10, 'c', 42);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, grp, score, \
                 ROW_NUMBER() OVER (PARTITION BY grp ORDER BY score NULLS LAST), \
                 RANK() OVER (PARTITION BY grp ORDER BY score NULLS LAST), \
                 DENSE_RANK() OVER (PARTITION BY grp ORDER BY score NULLS LAST) \
             FROM rankings ORDER BY grp NULLS FIRST, score NULLS LAST, id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(8),
                Value::Null,
                Value::Int32(7),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int32(9),
                Value::Null,
                Value::Null,
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(2),
            ]),
            Row::new(vec![
                Value::Int32(1),
                Value::String("a".into()),
                Value::Int32(10),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::String("a".into()),
                Value::Int32(10),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::String("a".into()),
                Value::Int32(20),
                Value::Int64(3),
                Value::Int64(3),
                Value::Int64(2),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::String("a".into()),
                Value::Null,
                Value::Int64(4),
                Value::Int64(4),
                Value::Int64(3),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::String("b".into()),
                Value::Int32(5),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int32(6),
                Value::String("b".into()),
                Value::Int32(5),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int32(7),
                Value::String("b".into()),
                Value::Int32(8),
                Value::Int64(3),
                Value::Int64(3),
                Value::Int64(2),
            ]),
            Row::new(vec![
                Value::Int32(10),
                Value::String("c".into()),
                Value::Int32(42),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
            ]),
        ]
    );
}

#[test]
fn test_window_ntile_lag_lead() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE sequence (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO sequence (id, v) VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, v, \
                 NTILE(3) OVER (ORDER BY id), \
                 NTILE(10) OVER (ORDER BY id), \
                 LAG(v) OVER (ORDER BY id), \
                 LEAD(v) OVER (ORDER BY id), \
                 LAG(v, 2) OVER (ORDER BY id), \
                 LEAD(v, 2) OVER (ORDER BY id), \
                 LAG(v, 0) OVER (ORDER BY id), \
                 LEAD(v, 0) OVER (ORDER BY id), \
                 LAG(v, 2, -1) OVER (ORDER BY id), \
                 LEAD(v, 2, -1) OVER (ORDER BY id) \
             FROM sequence ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int32(10),
                Value::Int64(1),
                Value::Int64(1),
                Value::Null,
                Value::Int32(20),
                Value::Null,
                Value::Int32(30),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int64(-1),
                Value::Int32(30),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int32(20),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int32(10),
                Value::Int32(30),
                Value::Null,
                Value::Int32(40),
                Value::Int32(20),
                Value::Int32(20),
                Value::Int64(-1),
                Value::Int32(40),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int32(30),
                Value::Int64(2),
                Value::Int64(3),
                Value::Int32(20),
                Value::Int32(40),
                Value::Int32(10),
                Value::Int32(50),
                Value::Int32(30),
                Value::Int32(30),
                Value::Int32(10),
                Value::Int32(50),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int32(40),
                Value::Int64(2),
                Value::Int64(4),
                Value::Int32(30),
                Value::Int32(50),
                Value::Int32(20),
                Value::Null,
                Value::Int32(40),
                Value::Int32(40),
                Value::Int32(20),
                Value::Int64(-1),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Int32(50),
                Value::Int64(3),
                Value::Int64(5),
                Value::Int32(40),
                Value::Null,
                Value::Int32(30),
                Value::Null,
                Value::Int32(50),
                Value::Int32(50),
                Value::Int32(30),
                Value::Int64(-1),
            ]),
        ]
    );
}

#[test]
fn test_window_functions_over_group_by_execute() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE grouped_window (id INT PRIMARY KEY, grp VARCHAR(4), v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO grouped_window (id, grp, v) VALUES \
             (1, 'a', 10), (2, 'a', 20), (3, 'b', 40), (4, 'c', 15), (5, 'c', 15);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT grp, SUM(v) AS total, \
                 RANK() OVER (ORDER BY SUM(v) DESC) AS total_rank \
             FROM grouped_window GROUP BY grp ORDER BY total_rank, grp;",
        ),
        vec![
            Row::new(vec![
                Value::String("b".into()),
                Value::Int64(40),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::String("a".into()),
                Value::Int64(30),
                Value::Int64(2),
            ]),
            Row::new(vec![
                Value::String("c".into()),
                Value::Int64(30),
                Value::Int64(2),
            ]),
        ]
    );
}

#[test]
fn test_window_result_used_in_order_by_and_limit() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE ordered_window (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO ordered_window (id, v) VALUES (1, 30), (2, 10), (3, 20), (4, 40);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS row_num \
             FROM ordered_window ORDER BY row_num DESC LIMIT 2;",
        ),
        vec![
            Row::new(vec![Value::Int32(4), Value::Int64(4)]),
            Row::new(vec![Value::Int32(1), Value::Int64(3)]),
        ]
    );
}

#[test]
fn test_window_functions_across_storage_formats_and_partitions() {
    let dir = TempDir::new().unwrap();
    let server = setup_three_engines(&dir);

    let expected = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("a".into()),
            Value::Int32(10),
            Value::Int64(1),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("a".into()),
            Value::Int32(20),
            Value::Int64(2),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("b".into()),
            Value::Int32(30),
            Value::Int64(1),
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("b".into()),
            Value::Int32(50),
            Value::Int64(2),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::Null,
            Value::Null,
            Value::Int64(1),
        ]),
    ];

    for table in ["r", "c", "k"] {
        assert_eq!(
            rows(
                &server,
                &format!(
                    "SELECT id, grp, v, \
                         RANK() OVER (PARTITION BY grp ORDER BY v NULLS LAST) AS value_rank \
                     FROM {table} ORDER BY grp NULLS LAST, value_rank, id;"
                ),
            ),
            expected,
            "{table}"
        );
    }
}

#[test]
fn test_window_aggregate_and_value_functions_execute() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE window_input (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO window_input (id, v) VALUES \
             (1, 10), (2, NULL), (3, 30), (4, 40);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 COUNT(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 SUM(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 AVG(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 MIN(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 MAX(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
                 FIRST_VALUE(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 LAST_VALUE(v) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
             FROM window_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(10),
                Value::Float64(10.0),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int32(40),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(10),
                Value::Float64(10.0),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int32(40),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(30),
                Value::Float64(30.0),
                Value::Int32(30),
                Value::Int32(30),
                Value::Int32(10),
                Value::Int32(40),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(70),
                Value::Float64(35.0),
                Value::Int32(30),
                Value::Int32(40),
                Value::Int32(10),
                Value::Int32(40),
            ]),
        ]
    );
}

#[test]
fn test_window_aggregate_default_and_explicit_frames() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE frame_input (id INT PRIMARY KEY, grp VARCHAR(4), v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO frame_input (id, grp, v) VALUES \
             (1, 'a', 10), (2, 'a', NULL), (3, 'a', 30), (4, 'a', 20), \
             (5, 'b', NULL), (6, 'b', 5);",
        )
        .unwrap();

    // With ORDER BY, the default frame is UNBOUNDED PRECEDING through CURRENT ROW.
    // Without ORDER BY, every row sees its whole partition:
    // a non-NULL values [10, 30, 20] => sum 60, count 3, avg 20;
    // b non-NULL values [5] => sum 5, count 1, avg 5.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 SUM(v) OVER (PARTITION BY grp ORDER BY id), \
                 COUNT(v) OVER (PARTITION BY grp ORDER BY id), \
                 AVG(v) OVER (PARTITION BY grp ORDER BY id), \
                 SUM(v) OVER (PARTITION BY grp), \
                 COUNT(v) OVER (PARTITION BY grp), \
                 AVG(v) OVER (PARTITION BY grp) \
             FROM frame_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int64(10),
                Value::Int64(1),
                Value::Float64(10.0),
                Value::Int64(60),
                Value::Int64(3),
                Value::Float64(20.0),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int64(10),
                Value::Int64(1),
                Value::Float64(10.0),
                Value::Int64(60),
                Value::Int64(3),
                Value::Float64(20.0),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int64(40),
                Value::Int64(2),
                Value::Float64(20.0),
                Value::Int64(60),
                Value::Int64(3),
                Value::Float64(20.0),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int64(60),
                Value::Int64(3),
                Value::Float64(20.0),
                Value::Int64(60),
                Value::Int64(3),
                Value::Float64(20.0),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Null,
                Value::Int64(0),
                Value::Null,
                Value::Int64(5),
                Value::Int64(1),
                Value::Float64(5.0),
            ]),
            Row::new(vec![
                Value::Int32(6),
                Value::Int64(5),
                Value::Int64(1),
                Value::Float64(5.0),
                Value::Int64(5),
                Value::Int64(1),
                Value::Float64(5.0),
            ]),
        ]
    );

    // ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING gives these non-NULL frame values:
    // id 1 [10], id 2 [10, 30], id 3 [30, 20], id 4 [30, 20],
    // id 5 [5], id 6 [5]. MIN/MAX ignore NULL arguments.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 SUM(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 COUNT(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 AVG(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 MIN(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 MAX(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM frame_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int64(10),
                Value::Int64(1),
                Value::Float64(10.0),
                Value::Int32(10),
                Value::Int32(10),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int64(40),
                Value::Int64(2),
                Value::Float64(20.0),
                Value::Int32(10),
                Value::Int32(30),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int64(50),
                Value::Int64(2),
                Value::Float64(25.0),
                Value::Int32(20),
                Value::Int32(30),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int64(50),
                Value::Int64(2),
                Value::Float64(25.0),
                Value::Int32(20),
                Value::Int32(30),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Int64(5),
                Value::Int64(1),
                Value::Float64(5.0),
                Value::Int32(5),
                Value::Int32(5),
            ]),
            Row::new(vec![
                Value::Int32(6),
                Value::Int64(5),
                Value::Int64(1),
                Value::Float64(5.0),
                Value::Int32(5),
                Value::Int32(5),
            ]),
        ]
    );

    // The explicit cumulative frame matches the default cumulative behavior.
    // For ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING, the frames by partition are:
    // a: id 1 [], id 2 [10], id 3 [10, NULL], id 4 [NULL, 30];
    // b: id 5 [], id 6 [NULL].
    // COUNT(v) returns 0 for empty or all-NULL frames; the other aggregates return NULL.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 SUM(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 MIN(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 MAX(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 COUNT(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING), \
                 SUM(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING), \
                 AVG(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING), \
                 MIN(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING), \
                 MAX(v) OVER (PARTITION BY grp ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) \
             FROM frame_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int64(10),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int64(10),
                Value::Int32(10),
                Value::Int32(10),
                Value::Int64(1),
                Value::Int64(10),
                Value::Float64(10.0),
                Value::Int32(10),
                Value::Int32(10),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int64(40),
                Value::Int32(10),
                Value::Int32(30),
                Value::Int64(1),
                Value::Int64(10),
                Value::Float64(10.0),
                Value::Int32(10),
                Value::Int32(10),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int64(60),
                Value::Int32(10),
                Value::Int32(30),
                Value::Int64(1),
                Value::Int64(30),
                Value::Float64(30.0),
                Value::Int32(30),
                Value::Int32(30),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(6),
                Value::Int64(5),
                Value::Int32(5),
                Value::Int32(5),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
        ]
    );
}

#[test]
fn test_window_first_last_value_frames() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE value_frame_input (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO value_frame_input (id, v) VALUES \
             (1, NULL), (2, 10), (3, NULL), (4, 20), (5, 30);",
        )
        .unwrap();

    // The default ordered frame and the explicit cumulative frame both cover the
    // partition start through the current row. FIRST_VALUE/LAST_VALUE respect NULL
    // arguments, so their cumulative results use the values at the frame endpoints.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 FIRST_VALUE(v) OVER (ORDER BY id), \
                 LAST_VALUE(v) OVER (ORDER BY id), \
                 FIRST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 LAST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
             FROM value_frame_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Null,
                Value::Int32(10),
                Value::Null,
                Value::Int32(10),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Null,
                Value::Int32(20),
                Value::Null,
                Value::Int32(20),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Null,
                Value::Int32(30),
                Value::Null,
                Value::Int32(30),
            ]),
        ]
    );

    // FIRST_VALUE/LAST_VALUE use the literal frame endpoints, including NULL values.
    // The preceding-only frame is empty at id 1; at id 2 it contains the NULL from id 1.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 FIRST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 LAST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                 FIRST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING), \
                 LAST_VALUE(v) OVER (ORDER BY id \
                     ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) \
             FROM value_frame_input ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Null,
                Value::Int32(10),
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int32(10),
                Value::Int32(20),
                Value::Null,
                Value::Int32(10),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Null,
                Value::Int32(30),
                Value::Int32(10),
                Value::Null,
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Int32(20),
                Value::Int32(30),
                Value::Null,
                Value::Int32(20),
            ]),
        ]
    );
}

#[test]
fn test_window_peer_range_frames() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE peer_frames (id INT PRIMARY KEY, x INT, y INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO peer_frames (id, x, y) VALUES \
             (1, 1, 10), (2, 1, 10), (3, 2, 10), \
             (4, 2, 20), (5, 2, 20), (6, 3, 30);",
        )
        .unwrap();

    // Without ORDER BY, the frame is the whole partition: count 6 and sum(x) 11.
    // With ORDER BY y, the default RANGE frame ends at the current peer group:
    // y=10 => rows 1..3, count 3, sum(x) 4;
    // y=20 => rows 1..5, count 5, sum(x) 8;
    // y=30 => rows 1..6, count 6, sum(x) 11.
    assert_eq!(
        rows(
            &server,
            "SELECT id, x, y, \
                 COUNT(*) OVER (), \
                 SUM(x) OVER (), \
                 COUNT(*) OVER (ORDER BY y), \
                 SUM(x) OVER (ORDER BY y) \
             FROM peer_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(10),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(3),
                Value::Int64(4),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int32(1),
                Value::Int32(10),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(3),
                Value::Int64(4),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int32(2),
                Value::Int32(10),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(3),
                Value::Int64(4),
            ]),
            Row::new(vec![
                Value::Int32(4),
                Value::Int32(2),
                Value::Int32(20),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(5),
                Value::Int64(8),
            ]),
            Row::new(vec![
                Value::Int32(5),
                Value::Int32(2),
                Value::Int32(20),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(5),
                Value::Int64(8),
            ]),
            Row::new(vec![
                Value::Int32(6),
                Value::Int32(3),
                Value::Int32(30),
                Value::Int64(6),
                Value::Int64(11),
                Value::Int64(6),
                Value::Int64(11),
            ]),
        ]
    );

    // Compound ordering forms peers only when both keys match. In (y, x) order,
    // the peer groups end after 2, 3, 5, and 6 rows respectively, with cumulative
    // sums 2, 4, 8, and 11. Equal (y, x) peers therefore share the same frame end.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY y, x), \
                 SUM(x) OVER (ORDER BY y, x) \
             FROM peer_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(2), Value::Int64(2),]),
            Row::new(vec![Value::Int32(2), Value::Int64(2), Value::Int64(2),]),
            Row::new(vec![Value::Int32(3), Value::Int64(3), Value::Int64(4),]),
            Row::new(vec![Value::Int32(4), Value::Int64(5), Value::Int64(8),]),
            Row::new(vec![Value::Int32(5), Value::Int64(5), Value::Int64(8),]),
            Row::new(vec![Value::Int32(6), Value::Int64(6), Value::Int64(11),]),
        ]
    );
}

#[test]
fn test_value_offset_range_frame_numeric_with_nulls() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE offset_frames ( \
                 id INT PRIMARY KEY, numeric_key INT, v INT \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO offset_frames (id, numeric_key, v) VALUES \
             (1, NULL, 1), \
             (2, NULL, 2), \
             (3, 10, 10), \
             (4, 12, 20), \
             (5, 12, 30), \
             (6, 15, 40), \
             (7, 20, 50);",
        )
        .unwrap();

    // Finite numeric bounds use [current_key - 2, current_key + 2]:
    // NULL peers => [1, 2], key 10 => [10, 20, 30], key 12 => [10, 20, 30],
    // key 15 => [40], key 20 => [50]. NULL-keyed rows are not pulled into any
    // non-NULL row's finite arithmetic frame.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING), \
                 SUM(v) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) \
             FROM offset_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(2), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(3), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(4), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(5), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(6), Value::Int64(1), Value::Int64(40)]),
            Row::new(vec![Value::Int32(7), Value::Int64(1), Value::Int64(50)]),
        ]
    );

    // This explicit count proves finite bounds exclude the two NULL-keyed rows
    // from the key-10 frame, even though NULL sorts before the non-NULL keys.
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*) OVER (ORDER BY numeric_key \
                 RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) \
             FROM offset_frames WHERE id = 3;",
        ),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    // UNBOUNDED PRECEDING includes the NULL peer group for every non-NULL key:
    // cumulative sums are 3, 13, 63, 103, and 153 at NULL, 10, 12, 15, and 20.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), \
                 SUM(v) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
             FROM offset_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(2), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(3), Value::Int64(3), Value::Int64(13)]),
            Row::new(vec![Value::Int32(4), Value::Int64(5), Value::Int64(63)]),
            Row::new(vec![Value::Int32(5), Value::Int64(5), Value::Int64(63)]),
            Row::new(vec![Value::Int32(6), Value::Int64(6), Value::Int64(103)]),
            Row::new(vec![Value::Int32(7), Value::Int64(7), Value::Int64(153)]),
        ]
    );
}

#[test]
fn test_value_offset_range_frame_timestamp_with_nulls() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE timestamp_offset_frames (id INT PRIMARY KEY, ts TIMESTAMP, v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO timestamp_offset_frames (id, ts, v) VALUES \
             (1, NULL, 1), \
             (2, NULL, 2), \
             (3, 1000000, 10), \
             (4, 1000002, 20), \
             (5, 1000002, 30), \
             (6, 1000005, 40);",
        )
        .unwrap();

    // Finite timestamp bounds use [current_ts - 2, current_ts + 2] microseconds.
    // NULL timestamps form one peer group [1, 2]. The non-NULL frames are:
    // ts=1000000 => [10, 20, 30], ts=1000002 => [10, 20, 30],
    // ts=1000005 => [40]. In particular, finite bounds exclude NULL-keyed rows.
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY ts \
                     RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING), \
                 SUM(v) OVER (ORDER BY ts \
                     RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) \
             FROM timestamp_offset_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(2), Value::Int64(2), Value::Int64(3)]),
            Row::new(vec![Value::Int32(3), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(4), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(5), Value::Int64(3), Value::Int64(60)]),
            Row::new(vec![Value::Int32(6), Value::Int64(1), Value::Int64(40)]),
        ]
    );

    // The key-1000000 frame has only itself when evaluated after this WHERE filter;
    // neither NULL-keyed row is included by a finite timestamp offset range.
    assert_eq!(
        rows(
            &server,
            "SELECT COUNT(*) OVER (ORDER BY ts \
                 RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) \
             FROM timestamp_offset_frames WHERE id = 3;",
        ),
        vec![Row::new(vec![Value::Int64(1)])]
    );
}

#[test]
fn test_value_offset_range_frame_with_numeric_literal_offset() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE subquery_offset_frames (id INT PRIMARY KEY, numeric_key INT, v INT);",
        )
        .unwrap();
    server
        .execute("CREATE TABLE frame_offset (id INT PRIMARY KEY, offset_value INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO subquery_offset_frames (id, numeric_key, v) VALUES \
             (1, 10, 10), (2, 12, 20), (3, 15, 30), (4, 20, 40);",
        )
        .unwrap();
    server
        .execute("INSERT INTO frame_offset (id, offset_value) VALUES (1, 3);")
        .unwrap();

    // With a literal offset of 3, frames are: key 10 => [10], key 12 => [10, 12],
    // key 15 => [12, 15], and key 20 => [20].
    assert_eq!(
        rows(
            &server,
            "SELECT id, \
                 COUNT(*) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN 3 PRECEDING \
                     AND 0 FOLLOWING), \
                 SUM(v) OVER (ORDER BY numeric_key \
                     RANGE BETWEEN 3 PRECEDING \
                     AND 0 FOLLOWING) \
             FROM subquery_offset_frames ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(1), Value::Int64(10)]),
            Row::new(vec![Value::Int32(2), Value::Int64(2), Value::Int64(30)]),
            Row::new(vec![Value::Int32(3), Value::Int64(2), Value::Int64(50)]),
            Row::new(vec![Value::Int32(4), Value::Int64(1), Value::Int64(40)]),
        ]
    );
}

#[test]
fn test_window_aggregates_across_storage_formats_and_partitions() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE window_row ( \
                 id BIGINT PRIMARY KEY, storage_key INT, grp VARCHAR(4), v INT \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE window_column ( \
                 id BIGINT PRIMARY KEY, storage_key INT, grp VARCHAR(4), v INT \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE window_partitioned ( \
                 id BIGINT, storage_key INT, grp VARCHAR(4), v INT, \
                 PRIMARY KEY (id, storage_key) \
             ) PARTITION BY RANGE (storage_key) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();

    let values = "(1, 1, 'a', 10), (2, 12, 'a', NULL), \
                  (3, 25, 'a', 30), (4, 5, 'a', 20), \
                  (5, 15, 'b', NULL), (6, 22, 'b', 5), \
                  (7, 8, 'b', 15)";
    for table in ["window_row", "window_column", "window_partitioned"] {
        server
            .execute(&format!(
                "INSERT INTO {table} (id, storage_key, grp, v) VALUES {values};"
            ))
            .unwrap();
    }
    assert!(server
        .convert_table_to_column("window_column")
        .unwrap()
        .is_success());

    let window_rows = |table: &str| {
        rows(
            &server,
            &format!(
                "SELECT id, storage_key, grp, v, \
                     SUM(v) OVER (PARTITION BY grp ORDER BY id), \
                     COUNT(v) OVER (PARTITION BY grp ORDER BY id), \
                     AVG(v) OVER (PARTITION BY grp ORDER BY id), \
                     SUM(v) OVER (PARTITION BY grp ORDER BY id \
                         ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                     COUNT(v) OVER (PARTITION BY grp ORDER BY id \
                         ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                     AVG(v) OVER (PARTITION BY grp ORDER BY id \
                         ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                     FIRST_VALUE(v) OVER (PARTITION BY grp ORDER BY id \
                         ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
                     LAST_VALUE(v) OVER (PARTITION BY grp ORDER BY id \
                         ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
                 FROM {table} ORDER BY id;"
            ),
        )
    };

    let expected = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Int32(1),
            Value::String("a".into()),
            Value::Int32(10),
            Value::Int64(10),
            Value::Int64(1),
            Value::Float64(10.0),
            Value::Int64(10),
            Value::Int64(1),
            Value::Float64(10.0),
            Value::Int32(10),
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Int32(12),
            Value::String("a".into()),
            Value::Null,
            Value::Int64(10),
            Value::Int64(1),
            Value::Float64(10.0),
            Value::Int64(40),
            Value::Int64(2),
            Value::Float64(20.0),
            Value::Int32(10),
            Value::Int32(30),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Int32(25),
            Value::String("a".into()),
            Value::Int32(30),
            Value::Int64(40),
            Value::Int64(2),
            Value::Float64(20.0),
            Value::Int64(50),
            Value::Int64(2),
            Value::Float64(25.0),
            Value::Null,
            Value::Int32(20),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::Int32(5),
            Value::String("a".into()),
            Value::Int32(20),
            Value::Int64(60),
            Value::Int64(3),
            Value::Float64(20.0),
            Value::Int64(50),
            Value::Int64(2),
            Value::Float64(25.0),
            Value::Int32(30),
            Value::Int32(20),
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::Int32(15),
            Value::String("b".into()),
            Value::Null,
            Value::Null,
            Value::Int64(0),
            Value::Null,
            Value::Int64(5),
            Value::Int64(1),
            Value::Float64(5.0),
            Value::Null,
            Value::Int32(5),
        ]),
        Row::new(vec![
            Value::Int64(6),
            Value::Int32(22),
            Value::String("b".into()),
            Value::Int32(5),
            Value::Int64(5),
            Value::Int64(1),
            Value::Float64(5.0),
            Value::Int64(20),
            Value::Int64(2),
            Value::Float64(10.0),
            Value::Null,
            Value::Int32(15),
        ]),
        Row::new(vec![
            Value::Int64(7),
            Value::Int32(8),
            Value::String("b".into()),
            Value::Int32(15),
            Value::Int64(20),
            Value::Int64(2),
            Value::Float64(10.0),
            Value::Int64(20),
            Value::Int64(2),
            Value::Float64(10.0),
            Value::Int32(5),
            Value::Int32(15),
        ]),
    ];

    let row_result = window_rows("window_row");
    let column_result = window_rows("window_column");
    let partitioned_result = window_rows("window_partitioned");

    assert_eq!(row_result, expected);
    assert_eq!(column_result, expected);
    assert_eq!(partitioned_result, expected);
    assert_eq!(row_result, column_result);
    assert_eq!(row_result, partitioned_result);
}

#[test]
fn test_order_by_and_group_by_ordinals_execute() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE ordinal (id INT PRIMARY KEY, grp VARCHAR(4), v INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO ordinal (id, grp, v) VALUES \
             (1, 'b', 2), (2, 'a', 5), (3, NULL, 3), (4, 'a', 1);",
        )
        .unwrap();

    assert_eq!(
        rows(&server, "SELECT grp, id FROM ordinal ORDER BY 1;"),
        vec![
            Row::new(vec![Value::Null, Value::Int32(3)]),
            Row::new(vec![Value::String("a".into()), Value::Int32(2)]),
            Row::new(vec![Value::String("a".into()), Value::Int32(4)]),
            Row::new(vec![Value::String("b".into()), Value::Int32(1)]),
        ]
    );
    assert_eq!(
        rows(&server, "SELECT id, v FROM ordinal ORDER BY 2 DESC;"),
        vec![
            Row::new(vec![Value::Int32(2), Value::Int32(5)]),
            Row::new(vec![Value::Int32(3), Value::Int32(3)]),
            Row::new(vec![Value::Int32(1), Value::Int32(2)]),
            Row::new(vec![Value::Int32(4), Value::Int32(1)]),
        ]
    );

    assert_eq!(
        rows(&server, "SELECT SUM(v) FROM ordinal;"),
        vec![Row::new(vec![Value::Int64(11)])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT grp, SUM(v) FROM ordinal GROUP BY 1 ORDER BY 1;"
        ),
        vec![
            Row::new(vec![Value::Null, Value::Int64(3)]),
            Row::new(vec![Value::String("a".into()), Value::Int64(6)]),
            Row::new(vec![Value::String("b".into()), Value::Int64(2)]),
        ]
    );
}

#[test]
fn test_except_and_intersect_multiset_semantics() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE lset (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE rset (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO lset (id, v) VALUES (1, 1), (2, 1), (3, 2), (4, NULL);")
        .unwrap();
    server
        .execute("INSERT INTO rset (id, v) VALUES (10, 1), (11, 3), (12, NULL);")
        .unwrap();

    assert_eq!(
        rows(&server, "SELECT v FROM lset EXCEPT SELECT v FROM rset;"),
        vec![Row::new(vec![Value::Int32(2)])]
    );
    assert_eq!(
        rows(&server, "SELECT v FROM lset EXCEPT ALL SELECT v FROM rset;"),
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ]
    );
    assert_eq!(
        rows(&server, "SELECT v FROM lset INTERSECT SELECT v FROM rset;"),
        vec![Row::new(vec![Value::Int32(1)]), Row::new(vec![Value::Null]),]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT v FROM lset INTERSECT ALL SELECT v FROM rset ORDER BY v DESC LIMIT 1;"
        ),
        vec![Row::new(vec![Value::Int32(1)])]
    );
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
fn test_delete_by_filter_on_row_table_reports_affected_rows_and_remaining_rows() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();

    assert_eq!(dml(&server, "DELETE FROM t WHERE v >= 20;").0, 2);
    assert_eq!(
        rows(&server, "SELECT id, v FROM t ORDER BY id;"),
        vec![Row::new(vec![Value::Int64(1), Value::Int32(10)])]
    );
}

#[test]
fn test_delete_by_filter_on_column_table_reports_affected_rows_and_remaining_rows() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    assert!(server.convert_table_to_column("t").unwrap().is_success());

    assert_eq!(dml(&server, "DELETE FROM t WHERE v >= 20;").0, 2);
    assert_eq!(
        rows(&server, "SELECT id, v FROM t ORDER BY id;"),
        vec![Row::new(vec![Value::Int64(1), Value::Int32(10)])]
    );
}

#[test]
fn test_delete_by_filter_prunes_range_partition_and_preserves_other_partitions() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE ev (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
             PARTITION BY RANGE (day) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO ev (id, day, v) VALUES \
             (1, 1, 10), (2, 15, 20), (3, 16, 30), (4, 25, 40);",
        )
        .unwrap();

    assert_eq!(
        dml(&server, "DELETE FROM ev WHERE day >= 10 AND day < 20;").0,
        2
    );
    assert_eq!(
        rows(&server, "SELECT id, day, v FROM ev ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(4), Value::Int32(25), Value::Int32(40)]),
        ]
    );
}

#[test]
fn test_unfiltered_delete_empties_table_and_reports_affected_rows() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();

    assert_eq!(dml(&server, "DELETE FROM t;").0, 3);
    assert!(rows(&server, "SELECT id, v FROM t;").is_empty());
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
        // DROP immediately reclaims the colstore directories of the dropped tablets.
        let colstore = dir.path().join("colstore");
        assert!(!colstore.exists() || colstore.read_dir().unwrap().next().is_none());

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

#[test]
fn test_insert_select_basic() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE source (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE target (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();
    server
        .execute("INSERT INTO source (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();

    assert_eq!(
        dml(
            &server,
            "INSERT INTO target (id, v) SELECT id, v * 2 FROM source;"
        )
        .0,
        3
    );
    assert_eq!(
        rows(&server, "SELECT id, v FROM target ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(20)]),
            Row::new(vec![Value::Int64(2), Value::Int64(40)]),
            Row::new(vec![Value::Int64(3), Value::Int64(60)]),
        ]
    );
}

#[test]
fn test_insert_select_halloween() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();

    // The SELECT is evaluated from one snapshot before any resulting rows are inserted.
    assert_eq!(
        dml(&server, "INSERT INTO t (id, v) SELECT id + 100, v FROM t;").0,
        2
    );
    assert_eq!(
        rows(&server, "SELECT id, v FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            Row::new(vec![Value::Int64(101), Value::Int32(10)]),
            Row::new(vec![Value::Int64(102), Value::Int32(20)]),
        ]
    );
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM t;"),
        vec![Row::new(vec![Value::Int64(4)])]
    );
}

#[test]
fn test_insert_select_from_join_with_aggregate() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE orders (id BIGINT PRIMARY KEY, grp VARCHAR(8) NOT NULL, amount INT);",
        )
        .unwrap();
    server
        .execute("CREATE TABLE factors (grp VARCHAR(8) PRIMARY KEY, multiplier INT);")
        .unwrap();
    server
        .execute("CREATE TABLE totals (grp VARCHAR(8) PRIMARY KEY, n BIGINT, total BIGINT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO orders (id, grp, amount) VALUES \
             (1, 'a', 10), (2, 'a', 20), (3, 'b', 7);",
        )
        .unwrap();
    server
        .execute("INSERT INTO factors (grp, multiplier) VALUES ('a', 2), ('b', 3);")
        .unwrap();

    assert_eq!(
        dml(
            &server,
            "INSERT INTO totals (grp, n, total) \
             SELECT o.grp, COUNT(*), SUM(o.amount * f.multiplier) \
             FROM orders o JOIN factors f ON o.grp = f.grp \
             GROUP BY o.grp;"
        )
        .0,
        2
    );
    assert_eq!(
        rows(&server, "SELECT grp, n, total FROM totals ORDER BY grp;"),
        vec![
            Row::new(vec![
                Value::String("a".into()),
                Value::Int64(2),
                Value::Int64(60),
            ]),
            Row::new(vec![
                Value::String("b".into()),
                Value::Int64(1),
                Value::Int64(21),
            ]),
        ]
    );
}

#[test]
fn test_insert_select_into_partitioned_target() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE source (id BIGINT PRIMARY KEY, day INT NOT NULL, v INT);")
        .unwrap();
    server
        .execute(
            "CREATE TABLE target (id BIGINT, day INT, v INT, PRIMARY KEY (id, day)) \
             PARTITION BY RANGE (day) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO source (id, day, v) VALUES \
             (1, 1, 10), (2, 15, 20), (3, 25, 30);",
        )
        .unwrap();

    assert_eq!(
        dml(
            &server,
            "INSERT INTO target (id, day, v) SELECT id, day, v FROM source;"
        )
        .0,
        3
    );
    assert_eq!(
        rows(&server, "SELECT id, day, v FROM target WHERE day < 10;"),
        vec![Row::new(vec![
            Value::Int64(1),
            Value::Int32(1),
            Value::Int32(10),
        ])]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT id, day, v FROM target WHERE day >= 10 AND day < 20;"
        ),
        vec![Row::new(vec![
            Value::Int64(2),
            Value::Int32(15),
            Value::Int32(20),
        ])]
    );
    assert_eq!(
        rows(&server, "SELECT id, day, v FROM target WHERE day >= 20;"),
        vec![Row::new(vec![
            Value::Int64(3),
            Value::Int32(25),
            Value::Int32(30),
        ])]
    );
}

#[test]
fn test_insert_select_from_column_format_source() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE source (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE target (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO source (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    assert!(server
        .convert_table_to_column("source")
        .unwrap()
        .is_success());

    assert_eq!(
        dml(
            &server,
            "INSERT INTO target (id, v) SELECT id, v FROM source;"
        )
        .0,
        3
    );
    assert_eq!(
        rows(&server, "SELECT id, v FROM target ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            Row::new(vec![Value::Int64(3), Value::Int32(30)]),
        ]
    );
}

#[test]
fn test_insert_select_duplicate_pk_aborts_atomically() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE source (id BIGINT PRIMARY KEY, target_id BIGINT);")
        .unwrap();
    server
        .execute("CREATE TABLE target (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();
    server
        .execute("INSERT INTO source (id, target_id) VALUES (1, 7), (2, 7);")
        .unwrap();

    // Duplicate target keys produced within this one SELECT reject the whole statement.
    let duplicate_err = server
        .execute(
            "INSERT INTO target (id, v) \
             SELECT target_id, id FROM source;",
        )
        .unwrap_err();
    assert!(matches!(duplicate_err, HtapError::InvalidArgument(_)));
    assert!(rows(&server, "SELECT id, v FROM target;").is_empty());

    // A source key conflicting with an already committed target key also leaves no partial rows.
    server
        .execute("INSERT INTO target (id, v) VALUES (7, 700);")
        .unwrap();
    let conflict_err = server
        .execute(
            "INSERT INTO target (id, v) \
             SELECT target_id, id FROM source WHERE id = 1;",
        )
        .unwrap_err();
    assert!(matches!(conflict_err, HtapError::InvalidArgument(_)));
    assert_eq!(
        rows(&server, "SELECT id, v FROM target;"),
        vec![Row::new(vec![Value::Int64(7), Value::Int64(700)])]
    );
}

#[test]
fn test_correlated_exists_in_where() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE parent (id INT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT);")
        .unwrap();
    server
        .execute("INSERT INTO parent (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();

    assert!(rows(
        &server,
        "SELECT p.id FROM parent p \
         WHERE EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) \
         ORDER BY p.id;",
    )
    .is_empty());
    assert_eq!(
        rows(
            &server,
            "SELECT p.id FROM parent p \
             WHERE NOT EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) \
             ORDER BY p.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(3)]),
        ]
    );

    server
        .execute("INSERT INTO child (id, parent_id) VALUES (10, 1), (11, 1), (12, 3);")
        .unwrap();
    assert_eq!(
        rows(
            &server,
            "SELECT p.id FROM parent p \
             WHERE EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) \
             ORDER BY p.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(3)]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT p.id FROM parent p \
             WHERE NOT EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) \
             ORDER BY p.id;",
        ),
        vec![Row::new(vec![Value::Int32(2)])]
    );
}

#[test]
fn test_correlated_in_subquery() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE o (id INT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE i (id INT PRIMARY KEY, oid INT, value INT);")
        .unwrap();
    server
        .execute("INSERT INTO o (id, value) VALUES (1, 10), (2, 20), (3, 10), (4, 40);")
        .unwrap();
    server
        .execute(
            "INSERT INTO i (id, oid, value) VALUES \
             (10, 1, 10), (11, 2, 99), (12, 3, 10), (13, 4, 20), (14, 99, 40);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT * FROM o \
             WHERE id IN (SELECT oid FROM i WHERE i.value = o.value) \
             ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(3), Value::Int32(10)]),
        ]
    );
}

#[test]
fn test_correlated_subquery_nested_two_levels() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE o (id INT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE i (id INT PRIMARY KEY, oid INT, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE j (id INT PRIMARY KEY, jid INT, value INT);")
        .unwrap();
    server
        .execute("INSERT INTO o (id, value) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    server
        .execute(
            "INSERT INTO i (id, oid, value) VALUES \
             (10, 1, 20), (20, 2, 10), (30, 3, 30), (40, 99, 40);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO j (id, jid, value) VALUES \
             (100, 10, 1), (200, 20, 0), (300, 30, 7), (400, 40, 9);",
        )
        .unwrap();

    // Each subquery correlates only to its immediate parent. Using the outer o row
    // while evaluating the innermost predicate changes which parent rows survive.
    assert_eq!(
        rows(
            &server,
            "SELECT * FROM o \
             WHERE EXISTS ( \
                 SELECT 1 FROM i WHERE i.oid = o.id AND EXISTS ( \
                     SELECT 1 FROM j WHERE j.jid = i.id AND j.value > 0 \
                 ) \
             ) ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10)]),
            Row::new(vec![Value::Int32(3), Value::Int32(30)]),
        ]
    );
}

#[test]
fn test_correlated_subquery_caps_fire_during_execution() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE cap_outer (id INT PRIMARY KEY);")
        .unwrap();
    server
        .execute("CREATE TABLE cap_inner (id INT PRIMARY KEY);")
        .unwrap();
    server
        .execute("CREATE TABLE cap_leaf (id INT PRIMARY KEY);")
        .unwrap();

    let values = (1..=101)
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!("INSERT INTO cap_outer (id) VALUES {values};"))
        .unwrap();
    server
        .execute(&format!("INSERT INTO cap_inner (id) VALUES {values};"))
        .unwrap();

    // Every outer row scans all 101 inner rows. The nested correlated EXISTS is
    // always false, preventing short-circuiting and forcing more than 10,000 runs.
    let err = server
        .execute(
            "SELECT o.id FROM cap_outer o \
             WHERE EXISTS ( \
                 SELECT 1 FROM cap_inner i \
                 WHERE i.id >= o.id - 1000 \
                   AND EXISTS ( \
                       SELECT 1 FROM cap_leaf l \
                       WHERE l.id = i.id AND l.id < 0 \
                   ) \
             );",
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("invocation cap (10000) exceeded"),
        "{err}"
    );
}

#[test]
fn test_correlated_subquery_column_outer_pruning_and_having() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE outer_col ( \
                 id BIGINT PRIMARY KEY, grp VARCHAR(8), correlation_key INT, amount INT \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE inner_rows ( \
                 id BIGINT PRIMARY KEY, correlation_key INT, minimum BIGINT \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO outer_col (id, grp, correlation_key, amount) VALUES \
             (1, 'a', 10, 4), (2, 'a', 20, 7), \
             (3, 'b', 30, 3), (4, 'b', 40, 8);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO inner_rows (id, correlation_key, minimum) VALUES \
             (10, 10, 0), (20, 20, 12), (30, 30, 0), (40, 99, 0);",
        )
        .unwrap();
    assert!(server
        .convert_table_to_column("outer_col")
        .unwrap()
        .is_success());

    // correlation_key is needed only by the correlated predicate. Column pruning
    // must retain it even though it is absent from the projection and outer filter.
    assert_eq!(
        rows(
            &server,
            "SELECT o.id FROM outer_col o \
             WHERE EXISTS ( \
                 SELECT 1 FROM inner_rows i \
                 WHERE i.correlation_key = o.correlation_key \
             ) ORDER BY o.id;",
        ),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
        ]
    );

    // Correlated references in HAVING must likewise survive pruning of a
    // Column-format outer table.
    assert_eq!(
        rows(
            &server,
            "SELECT o.correlation_key, SUM(o.amount) AS total \
             FROM outer_col o \
             GROUP BY o.correlation_key \
             HAVING SUM(o.amount) >= ( \
                 SELECT MAX(i.minimum) FROM inner_rows i \
                 WHERE i.correlation_key = o.correlation_key \
             ) \
             ORDER BY o.correlation_key;",
        ),
        vec![
            Row::new(vec![Value::Int32(10), Value::Int64(4)]),
            Row::new(vec![Value::Int32(30), Value::Int64(3)]),
        ]
    );
}

#[test]
fn test_correlated_scalar_subquery_in_select() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(8));")
        .unwrap();
    server
        .execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO parent (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');")
        .unwrap();
    server
        .execute(
            "INSERT INTO child (id, parent_id, v) VALUES \
             (10, 1, 5), (11, 1, 9), (12, 2, 7);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT p.id, \
                 (SELECT COUNT(*) FROM child c WHERE c.parent_id = p.id), \
                 (SELECT MAX(c.v) FROM child c WHERE c.parent_id = p.id), \
                 (SELECT MAX(v) FROM child) \
             FROM parent p ORDER BY p.id;",
        ),
        vec![
            Row::new(vec![
                Value::Int32(1),
                Value::Int64(2),
                Value::Int32(9),
                Value::Int32(9),
            ]),
            Row::new(vec![
                Value::Int32(2),
                Value::Int64(1),
                Value::Int32(7),
                Value::Int32(9),
            ]),
            Row::new(vec![
                Value::Int32(3),
                Value::Int64(0),
                Value::Null,
                Value::Int32(9),
            ]),
        ]
    );
}

#[test]
fn test_correlated_subquery_in_having_grouped() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE sales (id INT PRIMARY KEY, grp VARCHAR(8), amount INT);")
        .unwrap();
    server
        .execute("CREATE TABLE thresholds (grp VARCHAR(8) PRIMARY KEY, minimum BIGINT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO sales (id, grp, amount) VALUES \
             (1, 'a', 10), (2, 'a', 20), (3, 'b', 7), (4, 'c', 4), (5, 'c', 5);",
        )
        .unwrap();
    server
        .execute("INSERT INTO thresholds (grp, minimum) VALUES ('a', 25), ('b', 10), ('c', 9);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT s.grp, SUM(s.amount) AS total \
             FROM sales s GROUP BY s.grp \
             HAVING SUM(s.amount) >= ( \
                 SELECT MAX(t.minimum) FROM thresholds t WHERE t.grp = s.grp \
             ) ORDER BY s.grp;",
        ),
        vec![
            Row::new(vec![Value::String("a".into()), Value::Int64(30)]),
            Row::new(vec![Value::String("c".into()), Value::Int64(9)]),
        ]
    );
}

#[test]
fn test_correlated_subquery_self_reference() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE scores (id INT PRIMARY KEY, grp VARCHAR(8), score INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO scores (id, grp, score) VALUES \
             (1, 'a', 10), (2, 'a', 30), (3, 'a', 20), \
             (4, 'b', 7), (5, 'b', 5), (6, 'c', 1);",
        )
        .unwrap();

    // Hand-computed higher-score counts by row:
    // (1, 'a', 10) -> [30, 20] = 2; (2, 'a', 30) -> [] = 0;
    // (3, 'a', 20) -> [30] = 1; (4, 'b', 7) -> [] = 0;
    // (5, 'b', 5) -> [7] = 1; (6, 'c', 1) -> [] = 0.
    // Expected counts: [2, 0, 1, 0, 1, 0].
    assert_eq!(
        rows(
            &server,
            "SELECT outer_s.id, outer_s.score, \
                 (SELECT COUNT(*) FROM scores inner_s \
                  WHERE inner_s.grp = outer_s.grp \
                    AND inner_s.score > outer_s.score) AS higher \
             FROM scores outer_s ORDER BY outer_s.id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int32(10), Value::Int64(2)]),
            Row::new(vec![Value::Int32(2), Value::Int32(30), Value::Int64(0)]),
            Row::new(vec![Value::Int32(3), Value::Int32(20), Value::Int64(1)]),
            Row::new(vec![Value::Int32(4), Value::Int32(7), Value::Int64(0)]),
            Row::new(vec![Value::Int32(5), Value::Int32(5), Value::Int64(1)]),
            Row::new(vec![Value::Int32(6), Value::Int32(1), Value::Int64(0)]),
        ]
    );
}

#[test]
fn test_correlated_subquery_across_row_column_converting_and_partitions() {
    let dir = TempDir::new().unwrap();
    let server = setup_three_engines(&dir);
    server
        .execute(
            "CREATE TABLE events (id BIGINT, bucket INT, v INT, PRIMARY KEY (id, bucket)) \
             PARTITION BY RANGE (bucket) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO events (id, bucket, v) VALUES \
             (1, 1, 10), (2, 12, 20), (3, 25, 30), (5, 15, 50), (9, 5, 90);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT r.id, \
                 (SELECT COUNT(*) FROM c WHERE c.id = r.id), \
                 (SELECT MAX(k.v) FROM k WHERE k.id = r.id), \
                 (SELECT COUNT(*) FROM events e \
                  WHERE e.id = r.id AND e.bucket >= 10) \
             FROM r \
             WHERE EXISTS (SELECT 1 FROM c WHERE c.id = r.id) \
               AND EXISTS (SELECT 1 FROM k WHERE k.id = r.id) \
               AND EXISTS (SELECT 1 FROM events e WHERE e.id = r.id) \
             ORDER BY r.id;",
        ),
        vec![
            Row::new(vec![
                Value::Int64(1),
                Value::Int64(1),
                Value::Int32(10),
                Value::Int64(0),
            ]),
            Row::new(vec![
                Value::Int64(2),
                Value::Int64(1),
                Value::Int32(20),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int64(3),
                Value::Int64(1),
                Value::Int32(30),
                Value::Int64(1),
            ]),
            Row::new(vec![
                Value::Int64(5),
                Value::Int64(1),
                Value::Int32(50),
                Value::Int64(1),
            ]),
        ]
    );
}

#[test]
fn test_recursive_cte_counting_and_hierarchy_traversal() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE counter(n) AS ( \
                 SELECT 1 \
                 UNION ALL \
                 SELECT n + 1 FROM counter WHERE n < 10 \
             ) SELECT n FROM counter ORDER BY n;",
        ),
        (1..=10)
            .map(|n| Row::new(vec![Value::Int64(n)]))
            .collect::<Vec<_>>()
    );

    server
        .execute("CREATE TABLE employees (id INT PRIMARY KEY, manager_id INT, name VARCHAR(16));")
        .unwrap();
    server
        .execute(
            "INSERT INTO employees (id, manager_id, name) VALUES \
             (1, NULL, 'ceo'), (2, 1, 'eng'), (3, 1, 'sales'), \
             (4, 2, 'backend'), (5, 2, 'frontend'), (6, 3, 'field');",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE tree(id, depth) AS ( \
                 SELECT id, 0 FROM employees WHERE manager_id IS NULL \
                 UNION ALL \
                 SELECT e.id, tree.depth + 1 \
                 FROM tree JOIN employees e ON e.manager_id = tree.id \
             ) SELECT id, depth FROM tree ORDER BY depth, id;",
        ),
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(0)]),
            Row::new(vec![Value::Int32(2), Value::Int64(1)]),
            Row::new(vec![Value::Int32(3), Value::Int64(1)]),
            Row::new(vec![Value::Int32(4), Value::Int64(2)]),
            Row::new(vec![Value::Int32(5), Value::Int64(2)]),
            Row::new(vec![Value::Int32(6), Value::Int64(2)]),
        ]
    );
}

#[test]
fn test_recursive_cte_union_distinct_vs_all_semantics() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE edges (src INT, dst INT, PRIMARY KEY (src, dst));")
        .unwrap();
    server
        .execute("INSERT INTO edges (src, dst) VALUES (1, 2), (2, 3), (3, 1), (3, 4);")
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE reachable(node) AS ( \
                 SELECT 1 \
                 UNION \
                 SELECT e.dst FROM reachable r JOIN edges e ON e.src = r.node \
             ) SELECT node FROM reachable ORDER BY node;",
        ),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
            Row::new(vec![Value::Int64(4)]),
        ]
    );

    let err = server
        .execute(
            "WITH RECURSIVE reachable(node) AS ( \
                 SELECT 1 \
                 UNION ALL \
                 SELECT e.dst FROM reachable r JOIN edges e ON e.src = r.node \
             ) SELECT node FROM reachable;",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("recursive CTE iteration cap of 1000 iterations exceeded"),
        "{err}"
    );
}

#[test]
fn test_recursive_cte_iteration_and_row_cap_bounded_time() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let iteration_err = server
        .execute(
            "WITH RECURSIVE counter(n) AS ( \
                 SELECT 1 \
                 UNION ALL \
                 SELECT n + 1 FROM counter \
             ) SELECT n FROM counter;",
        )
        .unwrap_err();
    assert!(
        iteration_err
            .to_string()
            .contains("recursive CTE iteration cap of 1000 iterations exceeded"),
        "{iteration_err}"
    );

    server
        .execute("CREATE TABLE fan (id INT PRIMARY KEY);")
        .unwrap();
    server
        .execute("INSERT INTO fan (id) VALUES (1), (2);")
        .unwrap();
    let row_err = server
        .execute(
            "WITH RECURSIVE expanding(n) AS ( \
                 SELECT 0 FROM fan \
                 UNION ALL \
                 SELECT expanding.n + 1 FROM expanding CROSS JOIN fan \
                 WHERE expanding.n < 20 \
             ) SELECT n FROM expanding;",
        )
        .unwrap_err();
    assert!(
        row_err
            .to_string()
            .contains("recursive CTE accumulated-row-count cap of 1000000 rows exceeded"),
        "{row_err}"
    );
}

#[test]
fn test_recursive_cte_large_working_set() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE seed (id INT PRIMARY KEY);")
        .unwrap();

    let values = (1..=20_000)
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!("INSERT INTO seed (id) VALUES {values};"))
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE work(id, depth) AS ( \
                 SELECT id, 0 FROM seed \
                 UNION ALL \
                 SELECT id, depth + 1 FROM work WHERE depth = 0 \
             ) SELECT COUNT(*) FROM work;",
        ),
        vec![Row::new(vec![Value::Int64(40_000)])]
    );
}

#[test]
fn test_recursive_cte_over_column_and_partitioned_tables() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE edges (src INT, dst INT, PRIMARY KEY (src, dst));")
        .unwrap();
    server
        .execute("INSERT INTO edges (src, dst) VALUES (1, 2), (2, 3), (3, 4);")
        .unwrap();
    assert!(server
        .convert_table_to_column("edges")
        .unwrap()
        .is_success());
    server
        .execute(
            "CREATE TABLE quota (id INT, day INT, max_depth INT, PRIMARY KEY (id, day)) \
             PARTITION BY RANGE (day) ( \
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO quota (id, day, max_depth) VALUES \
             (2, 1, 1), (3, 15, 2), (4, 15, 2);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE walk(id, depth) AS ( \
                 SELECT 1, 0 \
                 UNION ALL \
                 SELECT e.dst, walk.depth + 1 \
                 FROM walk \
                 JOIN edges e ON e.src = walk.id \
                 JOIN quota q ON q.id = e.dst \
                 WHERE walk.depth < q.max_depth \
             ) SELECT id, depth FROM walk ORDER BY id;",
        ),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(0)]),
            Row::new(vec![Value::Int64(2), Value::Int64(1)]),
            Row::new(vec![Value::Int64(3), Value::Int64(2)]),
        ]
    );
}

#[test]
fn test_recursive_cte_with_type_widening() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(
        &server,
        "WITH RECURSIVE nums(n) AS ( \
             SELECT CAST(1 AS INT) \
             UNION ALL \
             SELECT CAST(n + 1 AS BIGINT) FROM nums WHERE n < 3 \
         ) SELECT n FROM nums ORDER BY n;",
    );
    assert_eq!(result.columns[0].data_type, DataType::Int64);
    assert_eq!(
        result.rows,
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
        ]
    );
}

#[test]
fn test_recursive_cte_outer_query_uses_cte() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE labels (n INT PRIMARY KEY, label VARCHAR(4));")
        .unwrap();
    server
        .execute(
            "INSERT INTO labels (n, label) VALUES \
             (1, 'odd'), (2, 'even'), (3, 'odd'), (4, 'even'), (5, 'odd');",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "WITH RECURSIVE nums(n) AS ( \
                 SELECT 1 \
                 UNION ALL \
                 SELECT n + 1 FROM nums WHERE n < 5 \
             ) \
             SELECT l.label, COUNT(*), SUM(nums.n) \
             FROM nums JOIN labels l ON l.n = nums.n \
             WHERE nums.n >= 2 \
             GROUP BY l.label ORDER BY l.label;",
        ),
        vec![
            Row::new(vec![
                Value::String("even".into()),
                Value::Int64(2),
                Value::Int64(6),
            ]),
            Row::new(vec![
                Value::String("odd".into()),
                Value::Int64(2),
                Value::Int64(8),
            ]),
        ]
    );
}

#[test]
fn test_insert_select_type_mismatch_leaves_target_untouched() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE source (id BIGINT PRIMARY KEY, v VARCHAR(8));")
        .unwrap();
    server
        .execute("CREATE TABLE target (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO source (id, v) VALUES (1, 'one'), (2, 'two');")
        .unwrap();

    let err = server
        .execute("INSERT INTO target (id, v) SELECT id, v FROM source;")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
    assert!(err.to_string().contains("type mismatch"), "{err}");
    assert_eq!(
        rows(&server, "SELECT COUNT(*) FROM target;"),
        vec![Row::new(vec![Value::Int64(0)])]
    );
    assert!(server
        .convert_table_to_column("target")
        .unwrap()
        .is_success());
}

#[test]
fn test_nested_join_tree_on_non_pk_columns() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    for table in ["a", "b", "c"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id INT PRIMARY KEY, k INT, x INT);"
            ))
            .unwrap();
    }
    server
        .execute("INSERT INTO a (id, k, x) VALUES (1, 7, 10), (2, 8, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, k, x) VALUES (1, 7, 30);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, k, x) VALUES (1, 7, 40);")
        .unwrap();

    let inner_sql = "SELECT a.x FROM a JOIN (b JOIN c ON b.k = c.k) ON a.k = b.k;";
    let left_sql = "SELECT a.id, a.x, b.x, c.x \
                    FROM a LEFT JOIN (b JOIN c ON b.k = c.k) ON a.k = b.k \
                    ORDER BY a.id;";

    let expected_inner = vec![Row::new(vec![Value::Int32(10)])];
    let expected_left = vec![
        Row::new(vec![
            Value::Int32(1),
            Value::Int32(10),
            Value::Int32(30),
            Value::Int32(40),
        ]),
        Row::new(vec![
            Value::Int32(2),
            Value::Int32(20),
            Value::Null,
            Value::Null,
        ]),
    ];

    for (storage, convert) in [
        ("row-format tables", None),
        ("column-converted b", Some("b")),
        ("column-converted b and c", Some("c")),
        ("all column-converted tables", Some("a")),
    ] {
        if let Some(table) = convert {
            let report = server.convert_table_to_column(table).unwrap();
            assert!(report.is_success(), "failed to convert {table}: {report:?}");
        }
        assert_eq!(
            rows(&server, inner_sql),
            expected_inner,
            "nested inner join on non-PK columns failed with {storage}"
        );
        assert_eq!(
            rows(&server, left_sql),
            expected_left,
            "nested left join on non-PK columns failed with {storage}"
        );
    }
}

#[test]
fn test_join_mixed_integer_float_keys_both_orientations() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE f (id BIGINT PRIMARY KEY, k DOUBLE);")
        .unwrap();
    server
        .execute("CREATE TABLE i (id BIGINT PRIMARY KEY, k BIGINT);")
        .unwrap();
    server
        .execute("INSERT INTO f (id, k) VALUES (1, 1.0), (3, 1125899906842625.0);")
        .unwrap();
    server
        .execute("INSERT INTO i (id, k) VALUES (2, 1), (4, 1125899906842624);")
        .unwrap();

    let expected = vec![Row::new(vec![Value::Int64(1), Value::Int64(2)])];

    assert_eq!(
        rows(
            &server,
            "SELECT f.id, i.id FROM f JOIN i ON f.k = i.k ORDER BY f.id;",
        ),
        expected,
        "float-left/integer-right join must match equal keys without rounding the distinct large keys together"
    );
    assert_eq!(
        rows(
            &server,
            "SELECT f.id, i.id FROM i JOIN f ON i.k = f.k ORDER BY f.id;",
        ),
        expected,
        "integer-left/float-right join must preserve the same mixed-key equality and no-collision guarantee"
    );
}

#[test]
fn test_join_multi_column_mixed_numeric_keys() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE mixed_left ( \
                 id BIGINT PRIMARY KEY, int_key BIGINT, float_key DOUBLE \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE mixed_right ( \
                 id BIGINT PRIMARY KEY, float_key DOUBLE, int_key BIGINT \
             );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO mixed_left (id, int_key, float_key) VALUES \
             (1, 1, 2.0), \
             (2, 2, 1.5), \
             (3, 9007199254740991, 4.0), \
             (4, 1125899906842625, 5.0);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO mixed_right (id, float_key, int_key) VALUES \
             (10, 1.0, 2), \
             (11, 2.0, 1), \
             (12, 9007199254740992.0, 4), \
             (14, 1125899906842625.0, 5);",
        )
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT l.id, r.id \
             FROM mixed_left l JOIN mixed_right r \
               ON l.int_key = r.float_key AND l.float_key = r.int_key \
             ORDER BY l.id;",
        ),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(10)]),
            Row::new(vec![Value::Int64(4), Value::Int64(14)]),
        ]
    );
    assert_eq!(
        rows(
            &server,
            "SELECT l.id, r.id \
             FROM mixed_left l LEFT JOIN mixed_right r \
               ON l.int_key = r.float_key AND l.float_key = r.int_key \
             ORDER BY l.id;",
        ),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(10)]),
            Row::new(vec![Value::Int64(2), Value::Null]),
            Row::new(vec![Value::Int64(3), Value::Null]),
            Row::new(vec![Value::Int64(4), Value::Int64(14)]),
        ]
    );
}

#[test]
fn test_correlated_subquery_through_window_spec() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE p (id BIGINT PRIMARY KEY, x BIGINT);")
        .unwrap();
    server
        .execute("CREATE TABLE ch (id BIGINT PRIMARY KEY);")
        .unwrap();
    server
        .execute("INSERT INTO p (id, x) VALUES (1, 42);")
        .unwrap();
    server.execute("INSERT INTO ch (id) VALUES (1);").unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT p.id, (SELECT ROW_NUMBER() OVER (ORDER BY p.x) FROM ch) FROM p;",
        ),
        vec![Row::new(vec![Value::Int64(1), Value::Int64(1)])]
    );
}

#[test]
fn test_correlated_subquery_is_optimized_once_per_statement() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE outer_rows (id INT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE inner_rows (id INT PRIMARY KEY, value INT);")
        .unwrap();

    let values = (1..=32)
        .map(|id| format!("({id}, {})", id * 10))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO outer_rows (id, value) VALUES {values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO inner_rows (id, value) VALUES {values};"
        ))
        .unwrap();

    assert_eq!(
        rows(
            &server,
            "SELECT o.id FROM outer_rows o \
             WHERE EXISTS (SELECT 1 FROM inner_rows i WHERE i.id = o.id) \
             ORDER BY o.id;",
        )
        .len(),
        32
    );
    assert_eq!(server.last_query_optimizer_invocations(), 2);
}

#[test]
fn test_float_sum_overflow_returns_out_of_range_error() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE float_sum_left (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();
    server
        .execute("CREATE TABLE float_sum_right (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();

    let values = (1..=512)
        .map(|id| format!("({id}, 1e308)"))
        .collect::<Vec<_>>()
        .join(", ");
    for table in ["float_sum_left", "float_sum_right"] {
        server
            .execute(&format!("INSERT INTO {table} (id, value) VALUES {values};"))
            .unwrap();
    }

    for sql in [
        "SELECT SUM(l.value) FROM float_sum_left l \
         JOIN float_sum_right r ON l.id = r.id;",
        "SELECT l.id % 2, SUM(l.value) FROM float_sum_left l \
         JOIN float_sum_right r ON l.id = r.id \
         GROUP BY l.id % 2;",
    ] {
        let err = server.execute(sql).unwrap_err();
        assert!(
            err.to_string()
                .contains("DOUBLE value is out of range in 'SUM'"),
            "{err}"
        );
    }
}

#[test]
fn test_float_avg_overflow_returns_out_of_range_error() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE float_avg_left (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();
    server
        .execute("CREATE TABLE float_avg_right (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();

    let values = (1..=512)
        .map(|id| format!("({id}, 1e308)"))
        .collect::<Vec<_>>()
        .join(", ");
    for table in ["float_avg_left", "float_avg_right"] {
        server
            .execute(&format!("INSERT INTO {table} (id, value) VALUES {values};"))
            .unwrap();
    }

    for sql in [
        "SELECT AVG(l.value) FROM float_avg_left l \
         JOIN float_avg_right r ON l.id = r.id;",
        "SELECT l.id % 2, AVG(l.value) FROM float_avg_left l \
         JOIN float_avg_right r ON l.id = r.id \
         GROUP BY l.id % 2;",
    ] {
        let err = server.execute(sql).unwrap_err();
        assert!(
            err.to_string()
                .contains("DOUBLE value is out of range in 'AVG'"),
            "{err}"
        );
    }
}

#[test]
fn test_non_finite_double_casts_and_literals_are_rejected() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    for sql in [
        "SELECT CAST('nan' AS DOUBLE);",
        "SELECT CAST('inf' AS DOUBLE);",
        "SELECT CAST('1e999' AS DOUBLE);",
    ] {
        let err = server.execute(sql).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
        assert!(
            err.to_string()
                .contains("DOUBLE value is out of range in 'CAST'"),
            "{err}"
        );
    }

    let err = server.execute("SELECT 1e400;").unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
    assert!(
        err.to_string()
            .contains("DOUBLE value is out of range in literal"),
        "{err}"
    );
}

#[test]
fn test_non_finite_cast_update_fails_at_statement_and_leaves_value_unchanged() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE float_values (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO float_values (id, value) VALUES (1, 1.0);")
        .unwrap();

    // UPDATE evaluates and validates the cast before committing its write.
    let err = server
        .execute("UPDATE float_values SET value = CAST('1e999' AS DOUBLE) WHERE id = 1;")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
    assert!(
        err.to_string()
            .contains("DOUBLE value is out of range in 'CAST'"),
        "{err}"
    );

    // The failed statement did not write a non-finite value or partially update the row.
    assert_eq!(
        rows(&server, "SELECT value FROM float_values WHERE id = 1;"),
        vec![Row::new(vec![Value::Float64(1.0)])]
    );
}

#[test]
fn test_analytic_float_sum_overflow_returns_out_of_range_error() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE analytic_float_sum (id INT PRIMARY KEY, value DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO analytic_float_sum (id, value) VALUES (1, 1e308), (2, 1e308);")
        .unwrap();

    let err = server
        .execute("SELECT SUM(value) FROM analytic_float_sum;")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)), "{err}");
    assert!(
        err.to_string()
            .contains("DOUBLE value is out of range in 'SUM'"),
        "{err}"
    );
}
