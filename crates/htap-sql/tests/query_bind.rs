//! Binding of the general query path: joins, expressions, subqueries, set operations,
//! UPDATE, DROP TABLE and SHOW/DESCRIBE.

use htap_catalog::{CatalogSnapshot, TableDescriptor, TableId};
use htap_common::error::{HtapError, Result};
use htap_common::types::{DataType, Row, Value};
use htap_sql::{
    bind, parse_one, BinOp, BoundQuery, BoundStatement, EvalContext, Expr, JoinKind, QueryBody,
    ShowStatement, TableSlot, UpdateTarget, VariableLookup,
};

fn catalog() -> CatalogSnapshot {
    let ddls = [
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL, age INT, \
         score DOUBLE)",
        "CREATE TABLE orders (order_id BIGINT PRIMARY KEY, user_id INT NOT NULL, \
         amount DOUBLE, note VARCHAR(64))",
        "CREATE TABLE docs (id INT PRIMARY KEY, data BLOB)",
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
        Ok(BoundStatement::Query(q)) => q,
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
    assert_eq!(body.joins.len(), 2);
    assert_eq!(body.joins[0].kind, JoinKind::Inner);
    assert_eq!(body.joins[0].right_slot, 1);
    assert_eq!(body.joins[1].kind, JoinKind::Left);
    assert!(body.joins[1].on.is_some());
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
    assert_eq!(select_body(&star).joins[0].kind, JoinKind::Cross);
    let qualified_star = bind_query("SELECT o.*, u.name FROM users u CROSS JOIN orders o");
    assert_eq!(
        names(&qualified_star),
        ["order_id", "user_id", "amount", "note", "name"]
    );
    // `JOIN` without ON is a cross join.
    let bare = bind_query("SELECT COUNT(*) FROM users JOIN orders");
    assert_eq!(select_body(&bare).joins[0].kind, JoinKind::Cross);
    // Same output name twice is allowed at the top level.
    let dup =
        bind_query("SELECT u.id, o.user_id AS id FROM users u JOIN orders o ON u.id = o.user_id");
    assert_eq!(names(&dup), ["id", "id"]);
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
    assert!(matches!(
        bind_err("SELECT * FROM users FULL OUTER JOIN orders ON users.id = orders.user_id"),
        HtapError::Unsupported(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM users NATURAL JOIN orders"),
        HtapError::Unsupported(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM users JOIN orders USING (id)"),
        HtapError::Unsupported(_)
    ));
    assert!(matches!(
        bind_err("SELECT * FROM users LEFT JOIN (orders o JOIN users v ON o.user_id = v.id) ON users.id = v.id"),
        HtapError::Unsupported(_)
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
        "SELECT id * 2 + 1 AS twice, age / 2, -age, CAST(age AS DOUBLE), UPPER(name), \
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
    assert_eq!(cols[3].data_type, DataType::Float64);
    assert_eq!(cols[4].data_type, DataType::String);
    assert_eq!(cols[5].data_type, DataType::String);
    assert_eq!(cols[6].name, "bucket");
    assert_eq!(cols[6].data_type, DataType::String);
    assert_eq!(
        cols[7].data_type,
        DataType::Int64,
        "COALESCE widens Int32 with an Int64 literal"
    );
    assert!(!cols[7].nullable);
    for c in &cols[8..] {
        assert_eq!(c.data_type, DataType::Bool, "{}", c.name);
    }
    assert_eq!(cols[11].name, "age IS NULL");

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
        "SELECT COUNT(*) OVER () FROM users",
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
        "SELECT age FROM users GROUP BY 1",
    ] {
        let e = bind_err(sql);
        assert!(
            matches!(e, HtapError::InvalidArgument(_) | HtapError::Unsupported(_)),
            "{sql}: {e}"
        );
    }
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
    assert!(matches!(q.order_by[2].expr, Expr::Literal(Value::Int64(1))));

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

    // Correlated subquery is rejected clearly.
    let e =
        bind_err("SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = u.id)");
    assert!(
        matches!(e, HtapError::Unsupported(ref m) if m.contains("correlated")),
        "{e}"
    );
    let e =
        bind_err("SELECT id FROM users WHERE age > (SELECT amount FROM orders WHERE user_id = id)");
    assert!(
        matches!(e, HtapError::Unsupported(ref m) if m.contains("correlated")),
        "{e}"
    );
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
    assert!(matches!(
        bind_err("WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r"),
        HtapError::Unsupported(_)
    ));
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
    assert!(matches!(
        bind_err("SELECT id FROM users EXCEPT SELECT user_id FROM orders"),
        HtapError::Unsupported(_)
    ));
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
        row: &row,
        aggregates: &[],
        output: None,
        subqueries: &[],
        variables: Some(&vars),
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
            row: &[],
            aggregates: &[],
            output: None,
            subqueries: &[],
            variables: Some(&vars),
        })
        .unwrap(),
        Value::Int64(1)
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
