//! Binding of the general query path: joins, expressions, subqueries, set operations,
//! UPDATE, DROP TABLE and SHOW/DESCRIBE.

use htap_catalog::{CatalogSnapshot, TableDescriptor, TableId};
use htap_common::error::{HtapError, Result};
use htap_common::types::{DataType, Row, Value};
use htap_sql::{
    bind, parse_one, BinOp, BoundQuery, BoundStatement, EvalContext, Expr, JoinKind, QueryBody,
    ShowStatement, TableSlot, UpdateTarget, VariableLookup, WindowFunctionKind,
};

fn catalog() -> CatalogSnapshot {
    let ddls = [
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL, age INT, \
         score DOUBLE)",
        "CREATE TABLE orders (order_id BIGINT PRIMARY KEY, user_id INT NOT NULL, \
         amount DOUBLE, note VARCHAR(64))",
        "CREATE TABLE docs (id INT PRIMARY KEY, data BLOB)",
        "CREATE TABLE a (id INT PRIMARY KEY, v INT)",
        "CREATE TABLE b (id INT PRIMARY KEY, a_id INT)",
        "CREATE TABLE c (id INT PRIMARY KEY, b_id INT)",
        "CREATE TABLE p (id INT PRIMARY KEY, x BIGINT)",
        "CREATE TABLE ch (id INT PRIMARY KEY)",
    ];
    let mut tables = Vec::new();
    for (i, ddl) in ddls.iter().enumerate() {
        let bound = bind(&parse_one(ddl).unwrap(), &CatalogSnapshot::empty()).unwrap();
        let create = match bound {
            BoundStatement::CreateTable(c) => c,
            other => panic!("{other:?}"),
        };
        tables.push(TableDescriptor::new(
            TableId((i + 1) as u64),
            create.name,
            create.schema,
            create.primary_key,
            vec![],
            1,
        ));
    }
    CatalogSnapshot::new(1, tables, vec![], vec![], vec![])
}

fn bind_query(sql: &str) -> BoundQuery {
    match bind(&parse_one(sql).unwrap(), &catalog()) {
        Ok(BoundStatement::Query(q)) => *q,
        other => panic!("expected Query for {sql}: {other:?}"),
    }
}

fn bind_err(sql: &str) -> HtapError {
    match bind(&parse_one(sql).unwrap(), &catalog()) {
        Err(e) => e,
        Ok(other) => panic!("expected error for {sql}, got {other:?}"),
    }
}

fn select_body(q: &BoundQuery) -> &htap_sql::SelectBody {
    match &q.body {
        QueryBody::Select(s) => s,
        other => panic!("expected select body, got {other:?}"),
    }
}

fn names(q: &BoundQuery) -> Vec<&str> {
    q.output_columns.iter().map(|c| c.name.as_str()).collect()
}

#[test]
fn test_join_binding_kinds_aliases_and_wildcards() {
    let q = bind_query(
        "SELECT u.id, o.amount, name FROM users u JOIN orders o ON u.id = o.user_id \
         LEFT JOIN orders o2 ON o2.user_id = u.id AND o2.amount > 1.5",
    );
    let body = select_body(&q);
    assert_eq!(body.slots.len(), 3);
    assert_eq!(body.slots[0].alias(), "u");
    assert!(matches!(&body.slots[1], TableSlot::Base { table, .. } if table == "orders"));
    match &body.join_tree {
        htap_sql::JoinTree::Join {
            left,
            right,
            kind: JoinKind::Left,
            on: Some(_),
        } => {
            assert!(matches!(right.as_ref(), htap_sql::JoinTree::Leaf(2)));
            match left.as_ref() {
                htap_sql::JoinTree::Join {
                    left,
                    right,
                    kind: JoinKind::Inner,
                    on: Some(_),
                } => {
                    assert!(matches!(left.as_ref(), htap_sql::JoinTree::Leaf(0)));
                    assert!(matches!(right.as_ref(), htap_sql::JoinTree::Leaf(1)));
                }
                other => panic!("expected inner join on left, got {other:?}"),
            }
        }
        other => panic!("expected left join at root, got {other:?}"),
    }
    assert_eq!(body.row_width(), 4 + 4 + 4);
    assert_eq!(body.slot_offset(2), 8);
    assert_eq!(names(&q), ["id", "amount", "name"]);
    // o.amount lives at offset 4 + 2.
    match &body.projection[1].expr {
        Expr::ColumnRef { slot, offset, .. } => assert_eq!((*slot, *offset), (1, 6)),
        other => panic!("{other:?}"),
    }
    // Columns of the LEFT-joined slot are nullable even when NOT NULL in the table.
    let q2 = bind_query("SELECT o.user_id FROM users LEFT JOIN orders o ON users.id = o.user_id");
    assert!(q2.output_columns[0].nullable);
    let q3 = bind_query("SELECT users.name FROM users RIGHT JOIN orders o ON users.id = o.user_id");
    assert!(q3.output_columns[0].nullable);

    let star = bind_query("SELECT * FROM users, orders");
    assert_eq!(star.output_columns.len(), 8);
    assert!(matches!(
        &select_body(&star).join_tree,
        htap_sql::JoinTree::Join {
            kind: JoinKind::Cross,
            ..
        }
    ));
    let qualified_star = bind_query("SELECT o.*, u.name FROM users u CROSS JOIN orders o");
    assert_eq!(
        names(&qualified_star),
        ["order_id", "user_id", "amount", "note", "name"]
    );
    // `JOIN` without ON is a cross join.
    let bare = bind_query("SELECT COUNT(*) FROM users JOIN orders");
    assert!(matches!(
        &select_body(&bare).join_tree,
        htap_sql::JoinTree::Join {
            kind: JoinKind::Cross,
            ..
        }
    ));
    // Same output name twice is allowed at the top level.
    let dup =
        bind_query("SELECT u.id, o.user_id AS id FROM users u JOIN orders o ON u.id = o.user_id");
    assert_eq!(names(&dup), ["id", "id"]);
}

#[test]
fn test_join_tree_lowering_and_nested_groups() {
    let chain = bind_query(
        "SELECT a.id, b.id, c.id FROM a JOIN b ON a.id = b.a_id \
         LEFT JOIN c ON b.id = c.b_id",
    );
    let body = select_body(&chain);
    match &body.join_tree {
        htap_sql::JoinTree::Join {
            left,
            right,
            kind: JoinKind::Left,
            ..
        } => {
            assert!(matches!(right.as_ref(), htap_sql::JoinTree::Leaf(2)));
            assert!(matches!(
                left.as_ref(),
                htap_sql::JoinTree::Join {
                    kind: JoinKind::Inner,
                    ..
                }
            ));
        }
        other => panic!("expected left join over an inner join, got {other:?}"),
    }
    assert!(!chain.output_columns[0].nullable);
    assert!(!chain.output_columns[1].nullable);
    assert!(chain.output_columns[2].nullable);

    let nested = bind_query(
        "SELECT a.id, b.id, c.id FROM a LEFT JOIN (b JOIN c ON b.id = c.b_id) \
         ON a.id = b.a_id",
    );
    let body = select_body(&nested);
    assert!(nested.output_columns[1].nullable);
    assert!(nested.output_columns[2].nullable);
    match &body.join_tree {
        htap_sql::JoinTree::Join { right, .. } => match right.as_ref() {
            htap_sql::JoinTree::Join { on: Some(on), .. } => match on {
                Expr::BinaryOp { left, right, .. } => {
                    assert!(matches!(left.as_ref(), Expr::ColumnRef { offset: 0, .. }));
                    assert!(matches!(right.as_ref(), Expr::ColumnRef { offset: 3, .. }));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }

    let full = bind_query(
        "SELECT a.id, b.id, c.id FROM (a LEFT JOIN b ON a.id = b.a_id) \
         FULL JOIN c ON b.id = c.b_id",
    );
    let body = select_body(&full);
    match &body.join_tree {
        htap_sql::JoinTree::Join {
            left,
            right,
            kind: JoinKind::Full,
            ..
        } => {
            assert!(matches!(right.as_ref(), htap_sql::JoinTree::Leaf(2)));
            assert!(matches!(
                left.as_ref(),
                htap_sql::JoinTree::Join {
                    kind: JoinKind::Left,
                    ..
                }
            ));
        }
        other => panic!("expected full join over a left join, got {other:?}"),
    }
    assert!(full.output_columns.iter().all(|c| c.nullable));

    let comma = bind_query("SELECT a.id, b.id, c.id FROM a, b, c");
    let body = select_body(&comma);
    match &body.join_tree {
        htap_sql::JoinTree::Join {
            left,
            right,
            kind: JoinKind::Cross,
            ..
        } => {
            assert!(matches!(right.as_ref(), htap_sql::JoinTree::Leaf(2)));
            assert!(matches!(
                left.as_ref(),
                htap_sql::JoinTree::Join {
                    kind: JoinKind::Cross,
                    ..
                }
            ));
        }
        other => panic!("expected a chain of cross joins, got {other:?}"),
    }
}

#[test]
fn test_nested_join_on_cannot_reference_outer_comma_item() {
    let err = bind_err("SELECT a.id FROM a, (b JOIN c ON a.id = c.id)");
    assert!(
        matches!(
            err,
            HtapError::InvalidArgument(ref message)
                if message.contains("unknown table or alias 'a'")
        ),
        "{err}"
    );
}

#[test]
fn test_nested_join_on_with_valid_outer_join() {
    let q = bind_query(
        "SELECT a.id, b.id, c.id \
         FROM a JOIN (b JOIN c ON b.id = c.id) ON a.id = b.id",
    );
    let body = select_body(&q);
    match &body.join_tree {
        htap_sql::JoinTree::Join {
            left,
            right,
            on: Some(outer_on),
            ..
        } => {
            assert!(matches!(left.as_ref(), htap_sql::JoinTree::Leaf(0)));
            match outer_on {
                Expr::BinaryOp { left, right, .. } => {
                    assert!(matches!(
                        left.as_ref(),
                        Expr::ColumnRef {
                            slot: 0,
                            offset: 0,
                            ..
                        }
                    ));
                    assert!(matches!(
                        right.as_ref(),
                        Expr::ColumnRef {
                            slot: 1,
                            offset: 2,
                            ..
                        }
                    ));
                }
                other => panic!("{other:?}"),
            }

            match right.as_ref() {
                htap_sql::JoinTree::Join {
                    on: Some(inner_on), ..
                } => match inner_on {
                    Expr::BinaryOp { left, right, .. } => {
                        assert!(matches!(
                            left.as_ref(),
                            Expr::ColumnRef {
                                slot: 1,
                                offset: 0,
                                ..
                            }
                        ));
                        assert!(matches!(
                            right.as_ref(),
                            Expr::ColumnRef {
                                slot: 2,
                                offset: 2,
                                ..
                            }
                        ));
                    }
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_join_binding_errors() {
    // 'id' exists only in users, so it is unambiguous across the join.
    bind_query(
        "SELECT id FROM users JOIN orders ON users.id = orders.user_id WHERE amount > 1 AND id = 1",
    );
    let e = bind_err("SELECT note FROM users u JOIN users v ON u.id = v.id");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("unknown column")),
        "{e}"
    );
    let e = bind_err("SELECT name FROM users u JOIN users v ON u.id = v.id");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("ambiguous")),
        "{e}"
    );
    let e = bind_err("SELECT * FROM users, users");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("not unique")),
        "{e}"
    );

    let natural = bind_query("SELECT * FROM a NATURAL JOIN b");
    assert_eq!(names(&natural), ["id", "v", "a_id"]);
    assert!(matches!(
        &select_body(&natural).join_tree,
        htap_sql::JoinTree::Join {
            kind: JoinKind::Inner,
            ..
        }
    ));

    let using = bind_query("SELECT * FROM a JOIN b USING (id)");
    assert_eq!(names(&using), ["id", "v", "a_id"]);
    assert!(matches!(
        &select_body(&using).join_tree,
        htap_sql::JoinTree::Join {
            kind: JoinKind::Inner,
            ..
        }
    ));

    assert!(matches!(
        bind_err("SELECT * FROM users LEFT JOIN orders"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM users u JOIN orders o ON u.name = o.amount"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM nope JOIN users ON nope.id = users.id"),
        HtapError::NotFound(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM (SELECT id FROM users)"),
        HtapError::InvalidArgument(_)
    ));
}

#[test]
fn test_expressions_functions_and_type_checks() {
    let q = bind_query(
        "SELECT id * 2 + 1 AS twice, age / 2, age DIV 2, -age, CAST(age AS DOUBLE), UPPER(name), \
         CONCAT(name, '!'), CASE WHEN age > 30 THEN 'old' ELSE 'young' END AS bucket, \
         COALESCE(age, 0), name LIKE 'a%', age BETWEEN 1 AND 5, id IN (1, 2), \
         age IS NULL, NOT (age > 1) FROM users",
    );
    let cols = &q.output_columns;
    assert_eq!(cols[0].name, "twice");
    assert_eq!(cols[0].data_type, DataType::Int64);
    assert!(!cols[0].nullable);
    assert_eq!(cols[1].data_type, DataType::Float64);
    assert!(cols[1].nullable);
    assert_eq!(cols[2].data_type, DataType::Int64);
    assert_eq!(cols[3].data_type, DataType::Int64);
    assert_eq!(cols[4].data_type, DataType::Float64);
    assert_eq!(cols[5].data_type, DataType::String);
    assert_eq!(cols[6].data_type, DataType::String);
    assert_eq!(cols[7].name, "bucket");
    assert_eq!(cols[7].data_type, DataType::String);
    assert_eq!(
        cols[8].data_type,
        DataType::Int64,
        "COALESCE widens Int32 with an Int64 literal"
    );
    assert!(!cols[8].nullable);
    for c in &cols[9..] {
        assert_eq!(c.data_type, DataType::Bool, "{}", c.name);
    }
    assert_eq!(cols[12].name, "age IS NULL");

    // Evaluate the projection against a row (id=3, name='ann', age=40, score=NULL).
    let row = vec![
        Value::Int32(3),
        Value::String("ann".into()),
        Value::Int32(40),
        Value::Null,
    ];
    let ctx = EvalContext::row_only(&row);
    let body = select_body(&q);
    let out: Vec<Value> = body
        .projection
        .iter()
        .map(|p| p.expr.eval(&ctx).unwrap())
        .collect();
    assert_eq!(
        out,
        vec![
            Value::Int64(7),
            Value::Float64(20.0),
            Value::Int64(20),
            Value::Int64(-40),
            Value::Float64(40.0),
            Value::String("ANN".into()),
            Value::String("ann!".into()),
            Value::String("old".into()),
            Value::Int64(40),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );

    // Negative literals fold; CAST of literals folds.
    let q = bind_query("SELECT -5, CAST('7' AS BIGINT), 1.5e3 FROM users");
    let body = select_body(&q);
    assert_eq!(body.projection[0].expr, Expr::Literal(Value::Int64(-5)));
    assert_eq!(body.projection[1].expr, Expr::Literal(Value::Int64(7)));
    assert_eq!(
        body.projection[2].expr,
        Expr::Literal(Value::Float64(1500.0))
    );

    // FROM-less select.
    let q = bind_query("SELECT 1 + 1 AS two, 'x'");
    assert!(select_body(&q).slots.is_empty());
    assert_eq!(names(&q), ["two", "'x'"]);

    for sql in [
        "SELECT name + 1 FROM users",
        "SELECT * FROM users WHERE name = 1",
        "SELECT * FROM users WHERE age",
        "SELECT * FROM users WHERE name AND age > 1",
        "SELECT UPPER(age) FROM users",
        "SELECT CASE WHEN age > 1 THEN 'a' ELSE 2 END FROM users",
        "SELECT name LIKE 1 FROM users",
        "SELECT COALESCE(name, age) FROM users",
        "SELECT ABS(name) FROM users",
        "SELECT * FROM users WHERE id = ?",
        "SELECT * FROM users LIMIT -1",
    ] {
        assert!(
            matches!(bind_err(sql), HtapError::InvalidArgument(_)),
            "{sql}"
        );
    }
    for sql in [
        "SELECT NOW() FROM users",
        "SELECT id FROM users WHERE id = ROW(1)",
        "SELECT CAST(id AS JSON) FROM users",
    ] {
        assert!(matches!(bind_err(sql), HtapError::Unsupported(_)), "{sql}");
    }
}

#[test]
fn test_where_bytes_column_compared_against_string_literal_coerces() {
    // A String *literal* compared against a BYTES-typed expression coerces to a BYTES literal
    // (encoded as UTF-8), matching the coercion already applied to INSERT/UPDATE assignment
    // targets: a bound parameter whose bytes happen to be valid UTF-8 decodes as `Value::String`
    // (see `htap_wire::binary_codec::decode_execute`), so `WHERE data = ?` must accept it too.
    // An alias on the sole FROM table forces binding through the general query path
    // (`binder_query.rs`), which is what this fix targets: an unaliased single-table filter binds
    // through a separate fast path (`AnalyticSelect`) that already applied this coercion.
    let q = bind_query("SELECT id FROM docs AS d WHERE data = 'abc'");
    match select_body(&q).filter.clone().unwrap() {
        Expr::BinaryOp { op, left, right } => {
            assert_eq!(op, BinOp::Eq);
            assert!(matches!(*left, Expr::ColumnRef { ref name, .. } if name == "data"));
            assert_eq!(*right, Expr::Literal(Value::Bytes(b"abc".to_vec())));
        }
        other => panic!("{other:?}"),
    }

    // The literal may be on either side of the comparison.
    let q2 = bind_query("SELECT id FROM docs AS d WHERE 'abc' = data");
    match select_body(&q2).filter.clone().unwrap() {
        Expr::BinaryOp { op, left, right } => {
            assert_eq!(op, BinOp::Eq);
            assert_eq!(*left, Expr::Literal(Value::Bytes(b"abc".to_vec())));
            assert!(matches!(*right, Expr::ColumnRef { ref name, .. } if name == "data"));
        }
        other => panic!("{other:?}"),
    }

    // Also works for other comparison operators, and a genuine type mismatch (BYTES vs a
    // non-coercible type) is still rejected.
    assert!(bind(
        &parse_one("SELECT id FROM docs AS d WHERE data <> 'xyz'").unwrap(),
        &catalog()
    )
    .is_ok());
    assert!(matches!(
        bind_err("SELECT id FROM docs AS d WHERE data = 1"),
        HtapError::InvalidArgument(_)
    ));
}

#[test]
fn test_aggregates_group_by_having_and_grouping_rules() {
    let q = bind_query(
        "SELECT age, COUNT(*), COUNT(DISTINCT name), AVG(score) AS avg_score, SUM(id), \
         MAX(name), MIN(score) FROM users WHERE id > 0 GROUP BY age HAVING COUNT(*) > 1 \
         AND avg_score > 2",
    );
    let body = select_body(&q);
    assert_eq!(body.aggregates.len(), 6);
    assert!(body.aggregates[1].distinct);
    assert_eq!(body.aggregates[2].data_type, DataType::Float64);
    assert_eq!(body.aggregates[3].data_type, DataType::Int64);
    assert_eq!(body.aggregates[4].data_type, DataType::String);
    assert_eq!(names(&q)[3], "avg_score");
    assert!(body.having.is_some());
    assert!(body.is_aggregate());
    // COUNT(*) is shared between projection and HAVING.
    assert_eq!(
        body.aggregates
            .iter()
            .filter(|a| a.name == "COUNT(*)")
            .count(),
        1
    );

    // Grouping by an expression; projecting the same expression is allowed.
    let q = bind_query("SELECT age + 1, COUNT(*) FROM users GROUP BY age + 1");
    assert_eq!(select_body(&q).group_by.len(), 1);
    let q = bind_query("SELECT age, COUNT(*) FROM users GROUP BY 1");
    assert_eq!(select_body(&q).group_by.len(), 1);
    // Aggregate without GROUP BY (narrow shape stays on the analytic path).
    assert!(matches!(
        bind(
            &parse_one("SELECT COUNT(*), MAX(age) FROM users").unwrap(),
            &catalog()
        ),
        Ok(BoundStatement::AnalyticSelect(_))
    ));
    bind_query("SELECT COUNT(*) AS n, MAX(age) FROM users");

    for sql in [
        "SELECT name, COUNT(*) FROM users GROUP BY age",
        "SELECT name, MAX(age) FROM users",
        "SELECT age FROM users GROUP BY age HAVING name = 'x'",
        "SELECT COUNT(*) FROM users WHERE COUNT(*) > 1",
        "SELECT SUM(name) FROM users",
        "SELECT AVG(name) FROM users",
        "SELECT MAX(*) FROM users",
        "SELECT COUNT(COUNT(*)) FROM users",
        "SELECT age, COUNT(*) FROM users GROUP BY age, age",
        "SELECT age FROM users GROUP BY age ORDER BY name",
        "SELECT age FROM users GROUP BY 2",
        "SELECT * FROM users GROUP BY 1",
    ] {
        let e = bind_err(sql);
        assert!(
            matches!(e, HtapError::InvalidArgument(_) | HtapError::Unsupported(_)),
            "{sql}: {e}"
        );
    }
}

#[test]
fn test_order_by_uses_merged_join_columns() {
    let using = bind_query("SELECT a.v FROM a JOIN b USING (id) ORDER BY id");
    assert!(matches!(
        &using.order_by[0].expr,
        Expr::ScalarFunction {
            func: htap_sql::ScalarFn::Coalesce,
            ..
        }
    ));

    let natural = bind_query("SELECT a.v FROM a NATURAL JOIN b ORDER BY id");
    assert!(matches!(
        &natural.order_by[0].expr,
        Expr::ScalarFunction {
            func: htap_sql::ScalarFn::Coalesce,
            ..
        }
    ));

    let err = bind_err("SELECT a.v FROM a JOIN b ON a.id = b.id ORDER BY id");
    assert!(
        matches!(
            err,
            HtapError::InvalidArgument(ref message) if message.contains("ambiguous")
        ),
        "{err}"
    );
}

#[test]
fn test_order_by_limit_distinct() {
    let q =
        bind_query("SELECT id, age AS a FROM users ORDER BY a DESC, id + 1, 1 LIMIT 5 OFFSET 2");
    assert_eq!(q.limit, Some(5));
    assert_eq!(q.offset, Some(2));
    assert_eq!(q.order_by.len(), 3);
    assert!(matches!(
        q.order_by[0].expr,
        Expr::OutputColumn { index: 1, .. }
    ));
    assert!(!q.order_by[0].asc);
    assert!(!q.order_by[0].nulls_first, "DESC defaults to NULLS LAST");
    assert!(matches!(q.order_by[1].expr, Expr::BinaryOp { .. }));
    assert!(q.order_by[1].asc);
    assert!(q.order_by[1].nulls_first, "ASC defaults to NULLS FIRST");
    assert!(matches!(
        q.order_by[2].expr,
        Expr::OutputColumn { index: 0, .. }
    ));

    // Alias shadows a source column of the same name in ORDER BY.
    let q = bind_query("SELECT age AS name FROM users ORDER BY name");
    assert!(matches!(
        q.order_by[0].expr,
        Expr::OutputColumn { index: 0, .. }
    ));
    // Source column not in the projection is still orderable.
    let q = bind_query("SELECT id FROM users u ORDER BY age NULLS LAST");
    assert!(matches!(
        q.order_by[0].expr,
        Expr::ColumnRef { column: 2, .. }
    ));
    assert!(!q.order_by[0].nulls_first);

    let q = bind_query("SELECT * FROM users LIMIT 3, 4");
    assert_eq!((q.limit, q.offset), (Some(4), Some(3)));

    let q = bind_query("SELECT DISTINCT age FROM users ORDER BY age");
    assert!(select_body(&q).distinct);
    assert!(matches!(
        bind_err("SELECT DISTINCT age FROM users ORDER BY id"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT id FROM users ORDER BY nope"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT id FROM users ORDER BY COUNT(*)"),
        HtapError::InvalidArgument(_)
    ));
}

#[test]
fn test_window_ranking_and_offset_function_binding() {
    let q = bind_query(
        "SELECT \
             ROW_NUMBER() OVER (PARTITION BY age ORDER BY id DESC), \
             RANK() OVER (PARTITION BY age ORDER BY id DESC), \
             DENSE_RANK() OVER (PARTITION BY age ORDER BY id DESC), \
             NTILE(4) OVER (PARTITION BY age ORDER BY id DESC), \
             LAG(name, 2, 'missing') OVER (PARTITION BY age ORDER BY id DESC), \
             LEAD(age) OVER (PARTITION BY age ORDER BY id DESC) \
         FROM users",
    );
    let body = select_body(&q);
    assert_eq!(body.windows.len(), 6);
    assert_eq!(
        body.windows
            .iter()
            .map(|window| window.func)
            .collect::<Vec<_>>(),
        vec![
            WindowFunctionKind::RowNumber,
            WindowFunctionKind::Rank,
            WindowFunctionKind::DenseRank,
            WindowFunctionKind::Ntile,
            WindowFunctionKind::Lag,
            WindowFunctionKind::Lead,
        ]
    );

    for window in &body.windows {
        assert_eq!(window.partition_by.len(), 1);
        assert!(matches!(
            window.partition_by[0],
            Expr::ColumnRef {
                ref name,
                column: 2,
                ..
            } if name == "age"
        ));
        assert_eq!(window.order_by.len(), 1);
        assert!(matches!(
            window.order_by[0].expr,
            Expr::ColumnRef {
                ref name,
                column: 0,
                ..
            } if name == "id"
        ));
        assert!(!window.order_by[0].asc);
        assert!(!window.order_by[0].nulls_first);
    }

    assert_eq!(body.windows[3].args, vec![Expr::Literal(Value::Int64(4))]);
    assert_eq!(body.windows[4].args.len(), 3);
    assert!(matches!(
        body.windows[4].args[0],
        Expr::ColumnRef {
            ref name,
            column: 1,
            ..
        } if name == "name"
    ));
    assert_eq!(body.windows[4].args[1], Expr::Literal(Value::Int64(2)));
    assert_eq!(
        body.windows[4].args[2],
        Expr::Literal(Value::String("missing".into()))
    );
    assert_eq!(body.windows[5].args.len(), 3);
    assert!(matches!(
        body.windows[5].args[0],
        Expr::ColumnRef {
            ref name,
            column: 2,
            ..
        } if name == "age"
    ));
    assert_eq!(body.windows[5].args[1], Expr::Literal(Value::Int64(1)));
    assert_eq!(body.windows[5].args[2], Expr::Literal(Value::Null));

    for (sql, clause) in [
        (
            "SELECT id FROM users WHERE ROW_NUMBER() OVER (ORDER BY id) = 1",
            "WHERE",
        ),
        (
            "SELECT age, COUNT(*) FROM users \
             GROUP BY age, ROW_NUMBER() OVER (ORDER BY id)",
            "GROUP BY",
        ),
        (
            "SELECT age, COUNT(*) FROM users GROUP BY age \
             HAVING ROW_NUMBER() OVER (ORDER BY age) = 1",
            "HAVING",
        ),
        (
            "SELECT u.id FROM users u JOIN orders o \
             ON ROW_NUMBER() OVER (ORDER BY u.id) = 1",
            "JOIN ON",
        ),
        (
            "SELECT u.id FROM users u JOIN \
                 (orders o JOIN a ON ROW_NUMBER() OVER (ORDER BY o.order_id) = 1) \
             ON u.id = o.user_id",
            "JOIN ON",
        ),
    ] {
        let err = bind_err(sql);
        assert!(
            matches!(
                err,
                HtapError::InvalidArgument(ref message)
                    if message.contains(&format!(
                        "window functions are not allowed in {clause}"
                    ))
            ),
            "{sql}: {err}"
        );
    }

    for (sql, message) in [
        (
            "SELECT LAG(ROW_NUMBER() OVER (), 1) OVER () FROM users",
            "cannot be nested",
        ),
        (
            "SELECT SUM(ROW_NUMBER() OVER ()) FROM users",
            "inside aggregate arguments",
        ),
        (
            "SELECT NTILE(0) OVER (ORDER BY id) FROM users",
            "positive integer literal",
        ),
        (
            "SELECT NTILE(age) OVER (ORDER BY id) FROM users",
            "positive integer literal",
        ),
        (
            "SELECT LAG(name, age) OVER (ORDER BY id) FROM users",
            "non-negative integer literal",
        ),
        (
            "SELECT LEAD(name, 1, age) OVER (ORDER BY id) FROM users",
            "incompatible type",
        ),
        (
            "SELECT ROW_NUMBER() OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) FROM users",
            "frame",
        ),
        (
            "SELECT ROW_NUMBER() OVER named_window FROM users",
            "named windows",
        ),
    ] {
        let err = bind_err(sql);
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains(&message.to_ascii_lowercase()),
            "{sql}: {err}"
        );
    }
}

#[test]
fn test_subqueries_ctes_derived_tables_and_union() {
    let q = bind_query(
        "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders) \
         AND EXISTS (SELECT 1 FROM orders WHERE amount > 1) \
         AND age > (SELECT MAX(amount) FROM orders)",
    );
    assert_eq!(q.subqueries.len(), 3);
    let filter = select_body(&q).filter.as_ref().unwrap();
    let mut kinds = Vec::new();
    filter.walk(&mut |e| match e {
        Expr::InSubquery { index, .. } => kinds.push(("in", *index)),
        Expr::Exists { index, .. } => kinds.push(("exists", *index)),
        Expr::ScalarSubquery { index, .. } => kinds.push(("scalar", *index)),
        _ => {}
    });
    assert_eq!(kinds, [("in", 0), ("exists", 1), ("scalar", 2)]);

    // Correlated subqueries bind successfully and record their outer references.
    let q = bind_query(
        "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = u.id)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    let q = bind_query(
        "SELECT id FROM users WHERE age > (SELECT amount FROM orders WHERE user_id = id)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());
    assert!(matches!(
        bind_err("SELECT id FROM users WHERE age > (SELECT order_id, amount FROM orders)"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT id FROM users WHERE name IN (SELECT amount FROM orders)"),
        HtapError::InvalidArgument(_)
    ));

    // Derived table and CTE.
    let q = bind_query(
        "SELECT s.user_id, s.total FROM (SELECT user_id, SUM(amount) AS total FROM orders \
         GROUP BY user_id) AS s WHERE s.total > 10",
    );
    assert!(matches!(&select_body(&q).slots[0], TableSlot::Derived { alias, .. } if alias == "s"));
    assert_eq!(q.output_columns[1].data_type, DataType::Float64);
    let q = bind_query(
        "WITH big AS (SELECT user_id FROM orders WHERE amount > 100), \
         named AS (SELECT b.user_id, u.name FROM big b JOIN users u ON u.id = b.user_id) \
         SELECT name FROM named",
    );
    assert!(
        matches!(&select_body(&q).slots[0], TableSlot::Derived { alias, .. } if alias == "named")
    );
    // A non-self-referencing recursive CTE behaves like a plain CTE.
    let q = bind_query("WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r");
    assert!(matches!(
        &select_body(&q).slots[0],
        TableSlot::Derived { alias, .. } if alias == "r"
    ));
    assert_eq!(names(&q), ["1"]);
    assert!(matches!(
        bind_err("SELECT * FROM (SELECT id, id FROM users) AS d"),
        HtapError::InvalidArgument(_)
    ));

    // UNION / UNION ALL with numeric widening.
    let q = bind_query("SELECT id, age FROM users UNION ALL SELECT order_id, amount FROM orders");
    match &q.body {
        QueryBody::SetOp { kind, .. } => assert_eq!(*kind, htap_sql::SetOpKind::UnionAll),
        other => panic!("{other:?}"),
    }
    assert_eq!(names(&q), ["id", "age"]);
    assert_eq!(q.output_columns[0].data_type, DataType::Int64);
    assert_eq!(q.output_columns[1].data_type, DataType::Float64);
    let q = bind_query("SELECT id FROM users UNION SELECT user_id FROM orders ORDER BY id LIMIT 3");
    assert!(matches!(
        q.order_by[0].expr,
        Expr::OutputColumn { index: 0, .. }
    ));
    assert_eq!(q.limit, Some(3));
    assert!(matches!(
        bind_err("SELECT id, name FROM users UNION SELECT user_id FROM orders"),
        HtapError::InvalidArgument(_)
    ));
    assert!(matches!(
        bind_err("SELECT name FROM users UNION SELECT amount FROM orders"),
        HtapError::InvalidArgument(_)
    ));
    let q = bind_query("SELECT id FROM users EXCEPT SELECT user_id FROM orders");
    match &q.body {
        QueryBody::SetOp { kind, .. } => assert_eq!(*kind, htap_sql::SetOpKind::ExceptDistinct),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        bind_err("SELECT id FROM users UNION SELECT user_id FROM orders ORDER BY age"),
        HtapError::InvalidArgument(_)
    ));
}

#[test]
fn test_update_drop_show_binding() {
    let stmt = bind(
        &parse_one("UPDATE users SET age = age + 1, name = 'x', score = 3 WHERE id = 7").unwrap(),
        &catalog(),
    )
    .unwrap();
    let update = match stmt {
        BoundStatement::Update(u) => u,
        other => panic!("{other:?}"),
    };
    assert_eq!(update.table, "users");
    assert_eq!(update.assignments.len(), 3);
    assert_eq!(update.assignments[0].0, 2);
    assert!(matches!(
        update.assignments[0].1,
        Expr::Cast {
            to: DataType::Int32,
            ..
        }
    ));
    assert_eq!(update.assignments[2].1, Expr::Literal(Value::Float64(3.0)));
    assert_eq!(
        update.target,
        UpdateTarget::PrimaryKey(vec![Value::Int32(7)])
    );

    let stmt = bind(
        &parse_one("UPDATE users u SET u.age = NULL WHERE u.age > 10 OR name = 'z'").unwrap(),
        &catalog(),
    )
    .unwrap();
    match stmt {
        BoundStatement::Update(u) => assert!(matches!(u.target, UpdateTarget::Filter(Some(_)))),
        other => panic!("{other:?}"),
    }
    let stmt = bind(&parse_one("UPDATE users SET age = 1").unwrap(), &catalog()).unwrap();
    match stmt {
        BoundStatement::Update(u) => assert_eq!(u.target, UpdateTarget::Filter(None)),
        other => panic!("{other:?}"),
    }
    for (sql, unsupported) in [
        ("UPDATE users SET id = 2 WHERE id = 1", true),
        ("UPDATE users SET name = NULL WHERE id = 1", false),
        ("UPDATE users SET age = 'x' WHERE id = 1", false),
        ("UPDATE users SET age = 1, age = 2 WHERE id = 1", false),
        ("UPDATE users SET nope = 1", false),
        ("UPDATE users SET age = (SELECT MAX(age) FROM users)", true),
        ("UPDATE users SET age = 1 WHERE id = 1 LIMIT 1", true),
        (
            "UPDATE users u JOIN orders o ON u.id = o.user_id SET u.age = 1",
            true,
        ),
        ("UPDATE users SET age = 1 WHERE age", false),
    ] {
        let e = bind_err(sql);
        if unsupported {
            assert!(matches!(e, HtapError::Unsupported(_)), "{sql}: {e}");
        } else {
            assert!(matches!(e, HtapError::InvalidArgument(_)), "{sql}: {e}");
        }
    }
    assert!(matches!(
        bind_err("UPDATE nope SET a = 1"),
        HtapError::NotFound(_)
    ));

    let stmt = bind(
        &parse_one("DROP TABLE IF EXISTS users").unwrap(),
        &catalog(),
    )
    .unwrap();
    match stmt {
        BoundStatement::DropTable(d) => {
            assert_eq!(d.table, "users");
            assert!(d.if_exists);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        bind_err("DROP VIEW users"),
        HtapError::Unsupported(_)
    ));
    assert!(matches!(
        bind_err("DROP TABLE users, orders"),
        HtapError::Unsupported(_)
    ));

    let cat = catalog();
    let show = |sql: &str| match bind(&parse_one(sql).unwrap(), &cat).unwrap() {
        BoundStatement::Show(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(show("SHOW TABLES"), ShowStatement::Tables { like: None });
    assert_eq!(
        show("SHOW TABLES LIKE 'u%'"),
        ShowStatement::Tables {
            like: Some("u%".into())
        }
    );
    assert_eq!(show("SHOW DATABASES"), ShowStatement::Databases);
    assert_eq!(
        show("SHOW COLUMNS FROM orders"),
        ShowStatement::Columns {
            table: "orders".into()
        }
    );
    assert_eq!(
        show("DESCRIBE users"),
        ShowStatement::Describe {
            table: "users".into()
        }
    );
    assert_eq!(
        show("DESC users"),
        ShowStatement::Describe {
            table: "users".into()
        }
    );
    assert!(matches!(bind_err("DESCRIBE nope"), HtapError::NotFound(_)));
    assert!(matches!(
        bind_err("SHOW FULL TABLES"),
        HtapError::Unsupported(_)
    ));
}

#[test]
fn test_bound_predicate_evaluation_with_joined_rows() {
    let q = bind_query(
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id \
         WHERE o.amount IS NULL OR o.amount > 5 AND u.name <> 'bob'",
    );
    let filter = select_body(&q).filter.clone().unwrap();
    let joined = |amount: Value, name: &str| -> Vec<Value> {
        vec![
            Value::Int32(1),
            Value::String(name.into()),
            Value::Int32(20),
            Value::Null,
            Value::Int64(9),
            Value::Int32(1),
            amount,
            Value::Null,
        ]
    };
    let row = joined(Value::Null, "ann");
    assert!(filter.eval_predicate(&EvalContext::row_only(&row)).unwrap());
    let row = joined(Value::Float64(6.0), "bob");
    assert!(!filter.eval_predicate(&EvalContext::row_only(&row)).unwrap());
    let row = joined(Value::Float64(6.0), "cid");
    assert!(filter.eval_predicate(&EvalContext::row_only(&row)).unwrap());
    let _ = Row::new(vec![]);
}

struct FakeVars {
    x: Option<Value>,
    autocommit: Value,
}

impl VariableLookup for FakeVars {
    fn lookup(&self, name: &str, is_system: bool) -> Result<Value> {
        match (is_system, name) {
            (false, "x") => Ok(self.x.clone().unwrap_or(Value::Null)),
            (false, _) => Ok(Value::Null),
            (true, "autocommit") => Ok(self.autocommit.clone()),
            (true, _) => Err(HtapError::Unsupported(format!(
                "unknown system variable '{name}'"
            ))),
        }
    }
}

#[test]
fn test_user_and_system_variable_binding() {
    // A bare user variable with no FROM clause binds through the zero-table general path.
    let q = bind_query("SELECT @x");
    let proj = &select_body(&q).projection;
    assert_eq!(proj.len(), 1);
    assert_eq!(
        proj[0].expr,
        Expr::Variable {
            name: "x".into(),
            is_system: false,
        }
    );
    assert_eq!(proj[0].name, "@x");
    let t = proj[0].expr.expr_type();
    assert!(t.nullable);
    assert!(t.is_dynamic);

    // `@@name` (unscoped) is a session-scoped system variable.
    let q2 = bind_query("SELECT @@autocommit");
    assert_eq!(
        select_body(&q2).projection[0].expr,
        Expr::Variable {
            name: "autocommit".into(),
            is_system: true,
        }
    );

    // `@@session.name` strips the explicit scope qualifier.
    let q3 = bind_query("SELECT @@session.autocommit");
    assert_eq!(
        select_body(&q3).projection[0].expr,
        Expr::Variable {
            name: "autocommit".into(),
            is_system: true,
        }
    );

    // A variable also binds inside a WHERE clause, comparing against a real column.
    let q4 = bind_query("SELECT name FROM users WHERE age = @x");
    let filter = select_body(&q4).filter.clone().unwrap();
    let row = vec![
        Value::Int32(1),
        Value::String("ann".into()),
        Value::Int32(30),
        Value::Float64(1.0),
    ];
    let vars = FakeVars {
        x: None,
        autocommit: Value::Int64(1),
    };
    // No lookup provided: an unset user variable is NULL, so `age = NULL` is never true.
    assert!(!filter.eval_predicate(&EvalContext::row_only(&row)).unwrap());
    let ctx = EvalContext {
        variables: Some(&vars),
        ..EvalContext::row_only(&row)
    };
    assert!(!filter.eval_predicate(&ctx).unwrap());
    let vars_match = FakeVars {
        x: Some(Value::Int32(30)),
        autocommit: Value::Int64(1),
    };
    let ctx_match = EvalContext {
        variables: Some(&vars_match),
        ..ctx
    };
    assert!(filter.eval_predicate(&ctx_match).unwrap());

    // A system variable with no lookup provided is a clear error, not a panic.
    let sys = Expr::Variable {
        name: "autocommit".into(),
        is_system: true,
    };
    assert!(matches!(
        sys.eval(&EvalContext::row_only(&[])),
        Err(HtapError::Unsupported(_))
    ));
    // The same variable resolves once a lookup is available.
    assert_eq!(
        sys.eval(&EvalContext {
            variables: Some(&vars),
            ..EvalContext::row_only(&[])
        })
        .unwrap(),
        Value::Int64(1)
    );
}

#[test]
fn test_natural_using_coalescing_all_join_kinds() {
    let assert_coalesced = |sql: &str, row: Vec<Value>, expected: Vec<Value>| {
        let q = bind_query(sql);
        assert_eq!(names(&q), ["id", "v", "a_id"], "{sql}");
        let body = select_body(&q);
        assert_eq!(body.projection.len(), 3, "{sql}");

        let ctx = EvalContext::row_only(&row);
        let actual = body
            .projection
            .iter()
            .map(|p| p.expr.eval(&ctx).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{sql}");
    };

    let matched = vec![
        Value::Int32(1),
        Value::Int32(10),
        Value::Int32(1),
        Value::Int32(100),
    ];
    let left_only = vec![Value::Int32(2), Value::Int32(20), Value::Null, Value::Null];
    let right_only = vec![Value::Null, Value::Null, Value::Int32(3), Value::Int32(300)];

    for join in ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"] {
        let using_sql = format!("SELECT * FROM a {join} b USING (id)");
        let natural_sql = format!("SELECT * FROM a NATURAL {join} b");

        let (row, expected) = match join {
            "JOIN" => (
                matched.clone(),
                vec![Value::Int32(1), Value::Int32(10), Value::Int32(100)],
            ),
            "LEFT JOIN" => (
                left_only.clone(),
                vec![Value::Int32(2), Value::Int32(20), Value::Null],
            ),
            "RIGHT JOIN" => (
                right_only.clone(),
                vec![Value::Int32(3), Value::Null, Value::Int32(300)],
            ),
            "FULL JOIN" => (
                right_only.clone(),
                vec![Value::Int32(3), Value::Null, Value::Int32(300)],
            ),
            other => unreachable!("{other}"),
        };

        assert_coalesced(&using_sql, row.clone(), expected.clone());
        assert_coalesced(&natural_sql, row, expected);
    }

    // FULL JOIN must also preserve the left key for an unmatched left row.
    assert_coalesced(
        "SELECT * FROM a FULL JOIN b USING (id)",
        left_only.clone(),
        vec![Value::Int32(2), Value::Int32(20), Value::Null],
    );
    assert_coalesced(
        "SELECT * FROM a NATURAL FULL JOIN b",
        left_only,
        vec![Value::Int32(2), Value::Int32(20), Value::Null],
    );
}

#[test]
fn test_natural_using_merged_column_nullability() {
    let full_not_null = bind_query("SELECT id FROM a FULL JOIN b USING (id)");
    assert!(
        !full_not_null.output_columns[0].nullable,
        "FULL JOIN of two NOT NULL keys must produce a NOT NULL merged key"
    );

    let full_nullable = bind_query(
        "SELECT id FROM a FULL JOIN \
         (SELECT age AS id FROM users) AS nullable_ids USING (id)",
    );
    assert!(
        full_nullable.output_columns[0].nullable,
        "FULL JOIN with a nullable input key must produce a nullable merged key"
    );

    let left = bind_query(
        "SELECT id FROM a LEFT JOIN \
         (SELECT age AS id FROM users) AS nullable_ids USING (id)",
    );
    assert!(
        !left.output_columns[0].nullable,
        "LEFT JOIN merged-key nullability must follow the left input"
    );

    let right = bind_query(
        "SELECT id FROM \
         (SELECT age AS id FROM users) AS nullable_ids \
         RIGHT JOIN a USING (id)",
    );
    assert!(
        !right.output_columns[0].nullable,
        "RIGHT JOIN merged-key nullability must follow the right input"
    );
}

#[test]
fn test_global_scope_rejected() {
    assert!(matches!(
        bind_err("SELECT @@global.autocommit"),
        HtapError::Unsupported(_)
    ));
    assert!(matches!(
        bind_err("SELECT @@GLOBAL.autocommit FROM users"),
        HtapError::Unsupported(_)
    ));
}

#[test]
fn test_natural_using_ambiguity_and_errors() {
    // USING removes the duplicate join key from the unqualified namespace.
    let q = bind_query("SELECT id, a.id, b.id FROM a JOIN b USING (id)");
    assert_eq!(names(&q), ["id", "id", "id"]);
    assert_eq!(select_body(&q).projection.len(), 3);

    // The merged key remains unambiguous across another USING/NATURAL join.
    let q = bind_query("SELECT id FROM a JOIN b USING (id) JOIN c USING (id)");
    assert_eq!(names(&q), ["id"]);
    let q = bind_query("SELECT id FROM a NATURAL JOIN b NATURAL JOIN c");
    assert_eq!(names(&q), ["id"]);

    // An ordinary ON join retains both keys, so an unqualified reference is ambiguous.
    let e = bind_err("SELECT id FROM a JOIN b ON a.id = b.id");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("ambiguous")),
        "{e}"
    );

    // USING cannot choose between duplicate columns already present on one side.
    let e = bind_err("SELECT * FROM (a JOIN b ON a.id = b.id) JOIN c USING (id)");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("ambiguous")),
        "{e}"
    );
    let e = bind_err("SELECT * FROM (a JOIN b ON a.id = b.id) NATURAL JOIN c");
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("ambiguous")),
        "{e}"
    );

    for sql in [
        "SELECT * FROM a JOIN b USING (nope)",
        "SELECT * FROM a JOIN b USING (id, id)",
    ] {
        assert!(
            matches!(bind_err(sql), HtapError::InvalidArgument(_)),
            "{sql}"
        );
    }

    // Same-named USING columns still need compatible types.
    let e = bind_err(
        "SELECT * FROM (SELECT id AS k FROM a) x \
         JOIN (SELECT name AS k FROM users) y USING (k)",
    );
    assert!(matches!(e, HtapError::InvalidArgument(_)), "{e}");
}

#[test]
fn test_correlated_subquery_binding_and_depth_limit() {
    // EXISTS can reference columns from its immediately enclosing query.
    let q = bind_query(
        "SELECT id FROM users u \
         WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    // Scalar subqueries in the projection can also be correlated.
    let q = bind_query(
        "SELECT (SELECT amount FROM orders o WHERE o.user_id = u.id) \
         FROM users u",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    // IN subqueries retain correlation information as well.
    let q = bind_query(
        "SELECT id FROM users u \
         WHERE id IN (SELECT user_id FROM orders o WHERE o.amount > u.score)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    // A nested subquery cannot skip its immediate parent and reference an outer query.
    let e = bind_err(
        "SELECT id FROM users u WHERE EXISTS (\
             SELECT 1 FROM orders o WHERE EXISTS (\
                 SELECT 1 FROM docs d WHERE d.id = u.id\
             )\
         )",
    );
    assert!(
        matches!(
            e,
            HtapError::InvalidArgument(ref m)
                if m.contains("correlated subqueries may only reference the immediately enclosing query")
        ),
        "{e}"
    );

    // The same depth restriction applies to unqualified references.
    let e = bind_err(
        "SELECT id FROM users u WHERE EXISTS (\
             SELECT 1 FROM orders o WHERE EXISTS (\
                 SELECT 1 FROM docs d WHERE d.id = score\
             )\
         )",
    );
    assert!(
        matches!(
            e,
            HtapError::InvalidArgument(ref m)
                if m.contains("correlated subqueries may only reference the immediately enclosing query")
        ),
        "{e}"
    );

    // A name missing from every scope retains the ordinary unknown-column diagnostic.
    let e = bind_err(
        "SELECT id FROM users u WHERE EXISTS (\
             SELECT 1 FROM orders o WHERE EXISTS (\
                 SELECT 1 FROM docs d WHERE d.id = nowhere\
             )\
         )",
    );
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("unknown column 'nowhere'")),
        "{e}"
    );

    // The inner query may reference its immediate parent even if that parent is uncorrelated.
    bind_query(
        "SELECT id FROM users u WHERE EXISTS (\
             SELECT 1 FROM orders o WHERE EXISTS (\
                 SELECT 1 FROM docs d WHERE d.id = o.user_id\
             )\
         )",
    );

    // Ordinary subqueries do not report correlation or outer references.
    let q = bind_query(
        "SELECT id FROM users \
         WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 1)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(!q.subqueries[0].correlated);
    assert!(q.subqueries[0].correlated_outer_refs.is_empty());
}

#[test]
fn test_correlated_subquery_grouped_context() {
    // A correlated reference to a grouping key is valid in HAVING.
    let q = bind_query(
        "SELECT u.age, COUNT(*) FROM users u GROUP BY u.age \
         HAVING EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.age)",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);

    // HAVING cannot correlate through a non-grouped parent column.
    let e = bind_err(
        "SELECT u.age, COUNT(*) FROM users u GROUP BY u.age \
         HAVING EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)",
    );
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("id")),
        "{e}"
    );

    // The same grouping rule applies when a correlated scalar subquery is an aggregate argument.
    let e = bind_err(
        "SELECT SUM((SELECT amount FROM orders o WHERE o.user_id = u.id)) \
         FROM users u GROUP BY u.age",
    );
    assert!(
        matches!(e, HtapError::InvalidArgument(ref m) if m.contains("id")),
        "{e}"
    );
}

#[test]
fn test_correlated_subquery_binding_is_case_insensitive() {
    let q = bind_query(
        "SELECT u.Id FROM users AS u \
         WHERE EXISTS ( \
             SELECT 1 FROM orders AS o \
             WHERE o.USER_ID = u.Id \
         )",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    let q = bind_query(
        "SELECT u.AGE, COUNT(*) FROM users AS u GROUP BY u.AgE \
         HAVING EXISTS ( \
             SELECT 1 FROM orders AS o \
             WHERE o.USER_ID = u.aGe \
         )",
    );
    assert_eq!(q.subqueries.len(), 1);
    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());
}

#[test]
fn test_recursive_cte_binding_and_output_schema() {
    let q = bind_query(
        "WITH RECURSIVE seq(n) AS ( \
             SELECT 1 \
             UNION ALL \
             SELECT n + 1 FROM seq WHERE n < 5 \
         ) \
         SELECT n FROM seq",
    );

    assert_eq!(names(&q), ["n"]);
    assert_eq!(q.output_columns[0].data_type, DataType::Int64);

    let q = bind_query(
        "WITH RECURSIVE descendants(id) AS ( \
             SELECT id FROM a WHERE id = 1 \
             UNION ALL \
             SELECT b.id FROM b JOIN descendants d ON b.a_id = d.id \
         ) \
         SELECT id FROM descendants",
    );
    assert_eq!(names(&q), ["id"]);
}

#[test]
fn test_recursive_cte_binding_rejects_invalid_shapes() {
    for sql in [
        "WITH RECURSIVE r(n) AS (SELECT n + 1 FROM r) SELECT n FROM r",
        "WITH RECURSIVE r(n) AS ( \
             SELECT 1 UNION ALL \
             SELECT x.n + y.n FROM r x JOIN r y ON x.n = y.n \
         ) SELECT n FROM r",
        "WITH RECURSIVE r(a, b) AS ( \
             SELECT 1 UNION ALL SELECT a + 1 FROM r \
         ) SELECT * FROM r",
    ] {
        let err = bind_err(sql);
        assert!(
            err.to_string().to_ascii_lowercase().contains("recursive"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn test_recursive_cte_binding_restrictions_and_non_recursive_forms() {
    for (sql, message) in [
        (
            "WITH RECURSIVE a AS (SELECT 1 FROM b), \
                 b AS (SELECT 1 FROM a) \
             SELECT * FROM a",
            "nested or mutually recursive CTEs not supported",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL \
                 SELECT x.n + y.n FROM r AS x JOIN r AS y ON x.n = y.n \
             ) SELECT n FROM r",
            "exactly once",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL \
                 SELECT (SELECT n FROM r) FROM r \
             ) SELECT n FROM r",
            "subquery",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL \
                 SELECT r.n FROM (SELECT 1 AS n) AS x LEFT JOIN r ON x.n = r.n \
             ) SELECT n FROM r",
            "outer join",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL \
                 SELECT r.n FROM (a LEFT JOIN r ON a.id = r.n) \
             ) SELECT n FROM r",
            "null-supplying side of an outer join",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL SELECT SUM(n) FROM r \
             ) SELECT n FROM r",
            "aggregate",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL SELECT n FROM r GROUP BY n \
             ) SELECT n FROM r",
            "GROUP BY",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL SELECT DISTINCT n FROM r \
             ) SELECT n FROM r",
            "DISTINCT",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL (SELECT n FROM r ORDER BY n) \
             ) SELECT n FROM r",
            "ORDER BY",
        ),
        (
            "WITH RECURSIVE r(n) AS ( \
                 SELECT 1 UNION ALL (SELECT n FROM r LIMIT 1) \
             ) SELECT n FROM r",
            "LIMIT",
        ),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT n FROM r) \
             SELECT n FROM r",
            "body",
        ),
    ] {
        let err = bind_err(sql);
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains(&message.to_ascii_lowercase()),
            "{sql}: {err}"
        );
    }

    bind_query("WITH r AS (SELECT id FROM users) SELECT id FROM r");
    bind_query("WITH RECURSIVE r AS (SELECT id FROM users) SELECT id FROM r");
    bind_query("WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r");
}

#[test]
fn test_window_aggregate_and_value_functions_with_frames() {
    use htap_sql::{
        PeerFrameBound, RowFrameBound, ValueFrameBound, WindowFrame, WindowFrameDirection,
    };

    let q = bind_query(
        "SELECT \
             COUNT(*) OVER (), \
             COUNT(age) OVER (), \
             SUM(id) OVER (), \
             AVG(age) OVER (), \
             MIN(name) OVER (), \
             MAX(score) OVER (), \
             FIRST_VALUE(name) OVER (), \
             LAST_VALUE(age) OVER () \
         FROM users",
    );
    let body = select_body(&q);
    assert_eq!(body.windows.len(), 8);

    let expected = [
        (DataType::Int64, false),
        (DataType::Int64, false),
        (DataType::Int64, true),
        (DataType::Float64, true),
        (DataType::String, true),
        (DataType::Float64, true),
        (DataType::String, true),
        (DataType::Int32, true),
    ];
    for (column, (data_type, nullable)) in q.output_columns.iter().zip(expected) {
        assert_eq!(column.data_type, data_type);
        assert_eq!(column.nullable, nullable);
    }
    assert!(body
        .windows
        .iter()
        .all(|window| window.frame == WindowFrame::None));

    let q = bind_query("SELECT SUM(id) OVER (ORDER BY age) FROM users");
    assert_eq!(
        select_body(&q).windows[0].frame,
        WindowFrame::PeerRange {
            start: PeerFrameBound::Unbounded(WindowFrameDirection::Preceding),
            end: PeerFrameBound::CurrentRow,
        }
    );

    let q = bind_query(
        "SELECT \
             SUM(id) OVER (ORDER BY age ROWS BETWEEN 2 PRECEDING AND CURRENT ROW), \
             AVG(age) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 3 FOLLOWING), \
             COUNT(*) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
         FROM users",
    );
    let windows = &select_body(&q).windows;
    assert_eq!(
        windows[0].frame,
        WindowFrame::Rows {
            start: RowFrameBound::Offset {
                value: 2,
                direction: WindowFrameDirection::Preceding,
            },
            end: RowFrameBound::CurrentRow,
        }
    );
    assert_eq!(
        windows[1].frame,
        WindowFrame::Rows {
            start: RowFrameBound::CurrentRow,
            end: RowFrameBound::Offset {
                value: 3,
                direction: WindowFrameDirection::Following,
            },
        }
    );
    assert_eq!(
        windows[2].frame,
        WindowFrame::Rows {
            start: RowFrameBound::Unbounded(WindowFrameDirection::Preceding),
            end: RowFrameBound::Unbounded(WindowFrameDirection::Following),
        }
    );

    let q = bind_query(
        "SELECT SUM(id) OVER (ORDER BY age \
         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM users",
    );
    assert_eq!(
        select_body(&q).windows[0].frame,
        WindowFrame::PeerRange {
            start: PeerFrameBound::Unbounded(WindowFrameDirection::Preceding),
            end: PeerFrameBound::CurrentRow,
        }
    );

    for (sql, message) in [
        (
            "SELECT SUM(id) OVER (ORDER BY age ROWS BETWEEN \
             UNBOUNDED FOLLOWING AND UNBOUNDED FOLLOWING) FROM users",
            "UNBOUNDED FOLLOWING",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY age ROWS BETWEEN \
             UNBOUNDED PRECEDING AND UNBOUNDED PRECEDING) FROM users",
            "UNBOUNDED PRECEDING",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY age ROWS BETWEEN \
             1 FOLLOWING AND 1 PRECEDING) FROM users",
            "frame start",
        ),
    ] {
        let err = bind_err(sql);
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains(&message.to_ascii_lowercase()),
            "{sql}: {err}"
        );
    }

    // Keep ValueFrameBound imported alongside the other concrete frame-bound types.
    let _: Option<ValueFrameBound> = None;
}

#[test]
fn test_value_offset_range_frame_binding_and_key_restrictions() {
    use htap_sql::{
        PeerFrameBound, RowFrameBound, ValueFrameBound, WindowFrame, WindowFrameDirection,
    };

    let q = bind_query(
        "SELECT SUM(id) OVER (ORDER BY age \
         RANGE BETWEEN 2 PRECEDING AND 3 FOLLOWING) FROM users",
    );
    assert_eq!(
        select_body(&q).windows[0].frame,
        WindowFrame::ValueRange {
            start: ValueFrameBound::Offset {
                value: Expr::Literal(Value::Int64(2)),
                direction: WindowFrameDirection::Preceding,
            },
            end: ValueFrameBound::Offset {
                value: Expr::Literal(Value::Int64(3)),
                direction: WindowFrameDirection::Following,
            },
        }
    );

    for (sql, message) in [
        (
            "SELECT SUM(id) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM users",
            "exactly one ORDER BY",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY age, id \
             RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM users",
            "exactly one ORDER BY",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY name \
             RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM users",
            "numeric or timestamp",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY age \
             RANGE BETWEEN -1 PRECEDING AND CURRENT ROW) FROM users",
            "non-negative",
        ),
        (
            "SELECT SUM(id) OVER (ORDER BY age \
             RANGE BETWEEN age PRECEDING AND CURRENT ROW) FROM users",
            "literal",
        ),
    ] {
        let err = bind_err(sql);
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains(&message.to_ascii_lowercase()),
            "{sql}: {err}"
        );
    }

    // Keep the other concrete bound types imported with the frame API.
    let _: Option<PeerFrameBound> = None;
    let _: Option<RowFrameBound> = None;
}

#[test]
fn test_window_functions_over_group_by_and_aggregate_discovery() {
    // A window can order by a GROUP BY key.
    let q = bind_query(
        "SELECT age, ROW_NUMBER() OVER (ORDER BY age) \
         FROM users GROUP BY age",
    );
    assert_eq!(select_body(&q).group_by.len(), 1);
    assert_eq!(select_body(&q).windows.len(), 1);

    // Aggregates used only by a window specification must still be discovered.
    let q = bind_query(
        "SELECT age, ROW_NUMBER() OVER (ORDER BY SUM(id)) \
         FROM users GROUP BY age",
    );
    let body = select_body(&q);
    assert_eq!(body.aggregates.len(), 1);
    assert_eq!(body.aggregates[0].name, "SUM(id)");
    assert_eq!(body.windows.len(), 1);

    // Window arguments are subject to the parent query's grouping rules.
    let err = bind_err(
        "SELECT age, FIRST_VALUE(name) OVER (ORDER BY age) \
         FROM users GROUP BY age",
    );
    assert!(
        matches!(err, HtapError::InvalidArgument(ref message) if message.contains("name")),
        "{err}"
    );

    // An aggregate referenced only by a window creates an implicit single group.
    let q = bind_query("SELECT ROW_NUMBER() OVER (ORDER BY SUM(id)) FROM users");
    let body = select_body(&q);
    assert!(body.is_aggregate());
    assert_eq!(body.aggregates.len(), 1);
    assert_eq!(body.aggregates[0].name, "SUM(id)");

    // A window may reuse an ordinary aggregate from the same query.
    let q = bind_query(
        "SELECT SUM(id), ROW_NUMBER() OVER (ORDER BY SUM(id)) \
         FROM users",
    );
    let body = select_body(&q);
    assert_eq!(body.aggregates.len(), 1);
    assert_eq!(body.aggregates[0].name, "SUM(id)");
    assert_eq!(body.windows.len(), 1);
}

#[test]
fn test_having_cannot_reference_window_result() {
    // HAVING cannot reference an alias whose expression is a window function.
    let err = bind_err(
        "SELECT age, ROW_NUMBER() OVER (ORDER BY age) AS rn \
         FROM users GROUP BY age HAVING rn = 1",
    );
    assert!(
        matches!(
            err,
            HtapError::InvalidArgument(ref message)
                if message.contains("window function results cannot be referenced in HAVING clause")
        ),
        "{err}"
    );

    // An ordinary aggregate alias remains valid in HAVING.
    let q = bind_query(
        "SELECT age, COUNT(*) AS n \
         FROM users GROUP BY age HAVING n > 1",
    );
    assert!(select_body(&q).having.is_some());
}

#[test]
fn test_correlated_subquery_window_order_by() {
    let q = bind_query("SELECT p.id, (SELECT ROW_NUMBER() OVER (ORDER BY p.id) FROM ch) FROM p");

    assert!(q.subqueries[0].correlated);
    assert!(!q.subqueries[0].correlated_outer_refs.is_empty());

    let body = select_body(&q.subqueries[0]);
    assert_eq!(body.windows.len(), 1);
    assert!(matches!(
        body.windows[0].order_by[0].expr,
        Expr::CorrelatedColumnRef { ref name, .. } if name == "id"
    ));
    assert!(q.subqueries[0].correlated_outer_refs.iter().any(|expr| {
        matches!(
            expr,
            Expr::CorrelatedColumnRef { name, .. } if name == "id"
        )
    }));
}
