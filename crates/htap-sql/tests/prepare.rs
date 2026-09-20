//! Tests for prepared-statement support: placeholder discovery (`count_placeholders` /
//! `checked_placeholder_count`), AST-level substitution (`substitute_placeholders`), and output
//! schema resolution (`resolve_prepare_output_schema`).

use htap_catalog::{CatalogSnapshot, TableDescriptor, TableId};
use htap_common::error::HtapError;
use htap_common::types::{ColumnDef, DataType, Value};
use htap_sql::{
    bind, checked_placeholder_count, count_placeholders, infer_placeholder_type_hints, parse_one,
    resolve_prepare_output_schema, substitute_placeholders, substitute_placeholders_ext,
    tokenizer_placeholder_count, BoundStatement, ParamLiteral,
};

/// `t`'s full column list, in schema order. Every INSERT into `t` must list all of these (this
/// binder requires INSERT to name every schema column; there is no partial-column / DEFAULT
/// support), so tests that only care about one or two columns still list all eight and pad the
/// rest with `Value::Null` (every non-PK column below is nullable).
const T_COLUMNS: &str = "id, n, big, amt, name, data, ts, active";

fn catalog() -> CatalogSnapshot {
    let ddls = [
        "CREATE TABLE t (\
            id BIGINT PRIMARY KEY, \
            n INT, \
            big BIGINT, \
            amt DOUBLE, \
            name VARCHAR(64), \
            data VARBINARY(64), \
            ts TIMESTAMP, \
            active BOOL\
        )",
        "CREATE TABLE orders (\
            order_id BIGINT PRIMARY KEY, \
            user_id BIGINT, \
            amount DOUBLE\
        )",
    ];
    let mut tables = Vec::new();
    for (i, ddl) in ddls.iter().enumerate() {
        let bound = bind(&parse_one(ddl).unwrap(), &CatalogSnapshot::empty()).unwrap();
        let create = match bound {
            BoundStatement::CreateTable(c) => c,
            other => panic!("expected CreateTable for {ddl}, got {other:?}"),
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

fn bind_sql(sql: &str, cat: &CatalogSnapshot) -> htap_common::Result<BoundStatement> {
    bind(&parse_one(sql).unwrap(), cat)
}

/// Builds `INSERT INTO t (id, n, big, amt, name, data, ts, active) VALUES (?, ...), ...` with
/// `rows` groups of 8 placeholders.
fn full_insert_template(rows: usize) -> String {
    let row = "(?, ?, ?, ?, ?, ?, ?, ?)";
    let rows = vec![row; rows].join(", ");
    format!("INSERT INTO t ({T_COLUMNS}) VALUES {rows}")
}

/// Escapes `s` the way the MySQL dialect's tokenizer expects inside a single-quoted string
/// literal (backslash-escaping both `\` and `'`), so hand-built literal SQL text round-trips
/// through the real tokenizer/parser identically to the source string.
fn mysql_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            other => out.push(other),
        }
    }
    out
}

fn string_literal(s: &str) -> String {
    format!("'{}'", mysql_escape(s))
}

fn hex_literal(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("X'{hex}'")
}

/// Renders `v` the same way real SQL text would tokenize a signed integer literal: plain digits
/// for non-negative values, and `-`-prefixed digits for negative ones (the real parser turns the
/// latter into `UnaryOp { Minus, Number(digits) }`, exactly what `substitute_placeholders` builds
/// internally).
fn int_literal(v: i64) -> String {
    v.to_string()
}

/// Renders `v` using `{:?}` (not `{}`), guaranteeing the text always contains a `.` or `e` so it
/// is never misread as an integer literal, mirroring `signed_float_expr`'s magnitude formatting.
fn float_literal(v: f64) -> String {
    format!("{v:?}")
}

fn value_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string().to_uppercase(),
        Value::Int32(i) => int_literal(i64::from(*i)),
        Value::Int64(i) => int_literal(*i),
        Value::Timestamp(i) => int_literal(*i),
        Value::Float64(f) => float_literal(*f),
        Value::String(s) => string_literal(s),
        Value::Bytes(b) => hex_literal(b),
    }
}

/// Builds the literal-SQL equivalent of `template` (which contains one `?` per entry of
/// `values`, in order) by textually substituting each `?` with `value_literal`. This is used
/// only to construct the *comparison target* for the "substitution binds identically to literal
/// SQL" tests; `substitute_placeholders` itself never does text substitution.
fn literal_sql(template: &str, values: &[Value]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut values = values.iter();
    for part in template.split('?') {
        out.push_str(part);
        if let Some(v) = values.next() {
            out.push_str(&value_literal(v));
        }
    }
    assert!(values.next().is_none(), "more values than '?' in template");
    out
}

/// Asserts that binding `template` after `substitute_placeholders` produces the exact same
/// `BoundStatement` as parsing and binding the equivalent literal SQL text directly, and that
/// the placeholder count agrees with the raw tokenizer count.
fn assert_substitution_matches_literal(template: &str, values: &[Value]) {
    let cat = catalog();
    let mut stmt = parse_one(template).unwrap();
    let checked = checked_placeholder_count(template, &stmt)
        .unwrap_or_else(|e| panic!("checked_placeholder_count failed for {template}: {e:?}"));
    assert_eq!(
        checked,
        values.len(),
        "placeholder count mismatch for {template}"
    );

    substitute_placeholders(&mut stmt, values)
        .unwrap_or_else(|e| panic!("substitute_placeholders failed for {template}: {e:?}"));
    let substituted = bind(&stmt, &cat)
        .unwrap_or_else(|e| panic!("bind(substituted) failed for {template}: {e:?}"));

    let literal = literal_sql(template, values);
    let direct = bind_sql(&literal, &cat).unwrap_or_else(|e| {
        panic!("bind(literal) failed for {literal:?} (from {template}): {e:?}")
    });

    assert_eq!(
        substituted, direct,
        "substitution vs literal SQL mismatch for {template} (literal: {literal:?})"
    );
}

// ---------------------------------------------------------------------------------------------
// count/tokenizer agreement + substitution-equals-literal-SQL, across >=20 supported shapes.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_supported_shapes_count_agrees_and_substitution_matches_literal_sql() {
    let cases: Vec<(String, Vec<Value>)> = vec![
        (
            full_insert_template(1),
            vec![
                Value::Int64(1),
                Value::Int32(42),
                Value::Int64(9_000_000_000),
                Value::Float64(3.5),
                Value::String("hello".into()),
                Value::Bytes(vec![1, 2, 3]),
                Value::Timestamp(1_700_000_000_000_000),
                Value::Bool(true),
            ],
        ),
        (
            full_insert_template(1),
            vec![
                Value::Int64(-5),
                Value::Int32(-42),
                Value::Int64(i64::MIN),
                Value::Float64(-2.5),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        (
            full_insert_template(1),
            vec![
                Value::Int64(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        (
            full_insert_template(3),
            vec![
                Value::Int64(10),
                Value::Int32(1),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Int64(11),
                Value::Int32(2),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Int64(12),
                Value::Int32(3),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        (
            "UPDATE t SET n = ?, amt = ? WHERE id = ?".to_string(),
            vec![Value::Int32(7), Value::Float64(1.25), Value::Int64(1)],
        ),
        (
            "UPDATE t SET active = ? WHERE id = ?".to_string(),
            vec![Value::Bool(true), Value::Int64(1)],
        ),
        (
            "DELETE FROM t WHERE id = ?".to_string(),
            vec![Value::Int64(7)],
        ),
        (
            "SELECT id, name FROM t WHERE id = ?".to_string(),
            vec![Value::Int64(3)],
        ),
        (
            "SELECT id FROM t WHERE n = ? AND amt > ?".to_string(),
            vec![Value::Int32(10), Value::Float64(1.5)],
        ),
        (
            "SELECT n, COUNT(*) FROM t GROUP BY n HAVING COUNT(*) > ?".to_string(),
            vec![Value::Int64(2)],
        ),
        (
            "SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?".to_string(),
            vec![Value::Int64(5), Value::Int64(2)],
        ),
        (
            "SELECT id FROM t ORDER BY id LIMIT ?, ?".to_string(),
            vec![Value::Int64(3), Value::Int64(7)],
        ),
        (
            "SELECT t.id, o.order_id FROM t JOIN orders o ON t.id = o.user_id WHERE o.amount > ?"
                .to_string(),
            vec![Value::Float64(10.0)],
        ),
        (
            "SELECT id FROM t WHERE id = (SELECT order_id FROM orders WHERE order_id = ?)"
                .to_string(),
            vec![Value::Int64(5)],
        ),
        (
            "SELECT id FROM t WHERE id IN (SELECT user_id FROM orders WHERE amount > ?)"
                .to_string(),
            vec![Value::Float64(1.0)],
        ),
        (
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.amount > ?)"
                .to_string(),
            vec![Value::Float64(2.0)],
        ),
        (
            "SELECT sub.id FROM (SELECT id FROM t WHERE id = ?) AS sub".to_string(),
            vec![Value::Int64(9)],
        ),
        (
            "WITH c AS (SELECT id FROM t WHERE id = ?) SELECT id FROM c WHERE id = ?".to_string(),
            vec![Value::Int64(4), Value::Int64(4)],
        ),
        (
            "SELECT id FROM t WHERE id = ? UNION SELECT id FROM t WHERE id = ?".to_string(),
            vec![Value::Int64(1), Value::Int64(2)],
        ),
        (
            "WITH RECURSIVE seq(n) AS ( \
                 SELECT ? UNION ALL SELECT n + ? FROM seq WHERE n < 3 \
             ) SELECT n FROM seq"
                .to_string(),
            vec![Value::Int64(1), Value::Int64(1)],
        ),
        (
            "SELECT id FROM t WHERE id IN (?, ?, ?)".to_string(),
            vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
        ),
        (
            "SELECT id FROM t WHERE n BETWEEN ? AND ?".to_string(),
            vec![Value::Int32(1), Value::Int32(10)],
        ),
        (
            "SELECT id FROM t WHERE name LIKE ?".to_string(),
            vec![Value::String("a%".into())],
        ),
        (
            "SELECT CASE WHEN n > ? THEN ? ELSE ? END AS label FROM t WHERE id = ?".to_string(),
            vec![
                Value::Int32(5),
                Value::String("big".into()),
                Value::String("small".into()),
                Value::Int64(1),
            ],
        ),
        (
            "SELECT UPPER(?) AS u FROM t WHERE id = ?".to_string(),
            vec![Value::String("hi".into()), Value::Int64(1)],
        ),
        (
            "SELECT id FROM t WHERE ts = ?".to_string(),
            vec![Value::Timestamp(1_700_000_000_000_000)],
        ),
    ];

    assert!(
        cases.len() >= 20,
        "expected at least 20 cases, found {}",
        cases.len()
    );

    for (template, values) in &cases {
        assert_substitution_matches_literal(template, values);
    }
}

// ---------------------------------------------------------------------------------------------
// Finding 5 of the Phase 11 fix pass: ORDER BY / LIMIT type inference must cover every expression
// shape `walk_expr` does, so the hint vector's length never falls out of sync with
// `count_placeholders`.
// ---------------------------------------------------------------------------------------------

/// The motivating example from the fix-pass brief: a placeholder nested inside a `CASE` in
/// `ORDER BY` used to be invisible to `infer_expr_no_catalog` (it only recognized
/// `Value`/`Nested`/`BinaryOp`/`UnaryOp`), so the hint vector came out shorter than
/// `count_placeholders`'s count. `resolve_prepare_output_schema` must never surface that as an
/// error: either a resolved schema or `None` is acceptable, but not `Err`.
#[test]
fn test_case_in_order_by_resolves_output_schema_without_error() {
    let cat = catalog();
    let stmt = parse_one("SELECT id FROM t ORDER BY CASE WHEN ? = 1 THEN 0 ELSE 1 END").unwrap();
    assert_eq!(count_placeholders(&stmt), 1);
    let schema = resolve_prepare_output_schema(&stmt, &cat)
        .expect("must never be an Err, whether or not the placeholder's type is inferable");
    // The `?`'s ambient type is not inferable from `= 1` (a literal, not a column), so this
    // resolves to `None` today; either outcome is acceptable, `Err` is not.
    let _ = schema;
}

/// Property-style check: for every statement shape this module claims to support (INSERT VALUES;
/// UPDATE SET/WHERE; DELETE WHERE; SELECT WHERE/HAVING/GROUP BY/JOIN ON; ORDER BY containing a
/// bare column, a `CASE`, a `LIKE`, an `IN` list, a `BETWEEN`, or a function call; every `LIMIT`/
/// `OFFSET` spelling; scalar/`IN`/`EXISTS` subqueries; a derived table; a CTE body; a `UNION`
/// branch), `infer_placeholder_type_hints` must produce exactly one entry per placeholder
/// `count_placeholders` finds — never more, never fewer — and `resolve_prepare_output_schema`
/// must never error.
#[test]
fn test_hint_count_matches_placeholder_count_for_every_supported_shape() {
    let cat = catalog();
    let templates = [
        "INSERT INTO orders (order_id, user_id, amount) VALUES (?, ?, ?)",
        "UPDATE t SET n = ? WHERE id = ?",
        "DELETE FROM t WHERE id = ?",
        "SELECT id FROM t WHERE id = ?",
        "SELECT n, COUNT(*) FROM t GROUP BY n HAVING COUNT(*) > ?",
        "SELECT t.id FROM t JOIN orders o ON t.id = o.user_id AND o.amount > ?",
        "SELECT id FROM t ORDER BY id",
        "SELECT id FROM t ORDER BY CASE WHEN ? = 1 THEN 0 ELSE 1 END",
        "SELECT id FROM t ORDER BY CASE WHEN n > 0 THEN ? ELSE ? END",
        "SELECT id FROM t ORDER BY name LIKE ?",
        "SELECT id FROM t ORDER BY n IN (?, ?, ?)",
        "SELECT id FROM t ORDER BY n BETWEEN ? AND ?",
        "SELECT id FROM t ORDER BY UPPER(?)",
        "SELECT id FROM t ORDER BY id LIMIT ?",
        "SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?",
        "SELECT id FROM t ORDER BY id LIMIT ?, ?",
        "SELECT id FROM t WHERE id = (SELECT order_id FROM orders WHERE order_id = ?)",
        "SELECT id FROM t WHERE id IN (SELECT user_id FROM orders WHERE amount > ?)",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.amount > ?)",
        "SELECT sub.id FROM (SELECT id FROM t WHERE id = ?) AS sub",
        "WITH c AS (SELECT id FROM t WHERE id = ?) SELECT id FROM c WHERE id = ?",
        "SELECT id FROM t WHERE id = ? UNION SELECT id FROM t WHERE id = ?",
        "WITH RECURSIVE seq(n) AS ( \
             SELECT ? UNION ALL SELECT n + ? FROM seq WHERE n < 3 \
         ) SELECT n FROM seq",
        "SELECT t.id FROM t \
         JOIN (orders b JOIN orders c ON b.order_id = c.order_id AND c.amount > ?) \
         ON t.id = b.user_id",
        "DELETE FROM t WHERE n > ?",
        "INSERT INTO orders (order_id, user_id, amount) \
         SELECT id, id, amt FROM t WHERE n > ?",
        "SELECT a.id FROM t a JOIN t b USING(id) WHERE b.n > ?",
        "SELECT a.id FROM t a NATURAL JOIN t b WHERE b.n > ?",
    ];
    for template in templates {
        let stmt = parse_one(template).unwrap();
        let walk_count = count_placeholders(&stmt);
        let hints = infer_placeholder_type_hints(&stmt, &cat)
            .unwrap_or_else(|e| panic!("hint inference failed for {template:?}: {e:?}"));
        assert_eq!(
            hints.len(),
            walk_count,
            "hint count must equal placeholder count for {template:?}"
        );
        resolve_prepare_output_schema(&stmt, &cat).unwrap_or_else(|e| {
            panic!("resolve_prepare_output_schema must never error for {template:?}: {e:?}")
        });
    }

    let truncate = "TRUNCATE TABLE t";
    let stmt = parse_one(truncate).unwrap();
    assert_eq!(checked_placeholder_count(truncate, &stmt).unwrap(), 0);
}

// ---------------------------------------------------------------------------------------------
// Value-type edge cases (kept separate from the table above for clearer failure messages).
// ---------------------------------------------------------------------------------------------

#[test]
fn test_negative_numbers_and_i64_min() {
    assert_substitution_matches_literal(
        "SELECT id FROM t WHERE big = ?",
        &[Value::Int64(i64::MIN)],
    );
    assert_substitution_matches_literal(
        &full_insert_template(1),
        &[
            Value::Int64(1),
            Value::Null,
            Value::Int64(i64::MIN),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    );
    assert_substitution_matches_literal("SELECT id FROM t WHERE n = ?", &[Value::Int32(i32::MIN)]);
    assert_substitution_matches_literal("SELECT id FROM t WHERE n = ?", &[Value::Int32(-1)]);
}

#[test]
fn test_f64_edge_values() {
    for v in [
        0.0_f64,
        -0.0_f64,
        1.0,
        -1.0,
        3.25,
        f64::MIN,
        f64::MAX,
        f64::EPSILON,
        f64::MIN_POSITIVE,
    ] {
        assert_substitution_matches_literal(
            &full_insert_template(1),
            &[
                Value::Int64(1),
                Value::Null,
                Value::Null,
                Value::Float64(v),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        );
    }
}

#[test]
fn test_non_finite_float_is_rejected() {
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut stmt = parse_one("SELECT id FROM t WHERE amt = ?").unwrap();
        let err = substitute_placeholders(&mut stmt, &[Value::Float64(v)])
            .expect_err("non-finite float must be rejected");
        assert!(matches!(err, HtapError::Unsupported(_)), "got {err:?}");
    }
}

#[test]
fn test_strings_with_quotes_backslashes_and_unicode() {
    for s in [
        "plain",
        "with 'single quotes'",
        "with \\backslashes\\",
        "mixed: it's a \\test\\ 😀 café",
        "",
    ] {
        assert_substitution_matches_literal(
            &full_insert_template(1),
            &[
                Value::Int64(1),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::String(s.to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        );
        assert_substitution_matches_literal(
            "SELECT id FROM t WHERE name = ?",
            &[Value::String(s.to_string())],
        );
    }
}

#[test]
fn test_bytes_values() {
    for b in [
        vec![],
        vec![0x00],
        vec![0xff, 0x00, 0xab, 0xcd],
        (0u8..=255).collect::<Vec<u8>>(),
    ] {
        assert_substitution_matches_literal(
            &full_insert_template(1),
            &[
                Value::Int64(1),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Bytes(b),
                Value::Null,
                Value::Null,
            ],
        );
    }
}

#[test]
fn test_null_values() {
    assert_substitution_matches_literal(
        &full_insert_template(1),
        &[
            Value::Int64(1),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    );
    assert_substitution_matches_literal(
        "SELECT id FROM t WHERE n IS NULL AND id = ?",
        &[Value::Int64(1)],
    );
}

// ---------------------------------------------------------------------------------------------
// Unsupported placeholder positions.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_placeholder_in_unsupported_position_returns_unsupported_error() {
    for sql in [
        "SET @x = ?",
        "SET sql_mode = ?",
        "CREATE TABLE t2 (id INT DEFAULT ?)",
    ] {
        let stmt = parse_one(sql).unwrap();
        let result = checked_placeholder_count(sql, &stmt);
        match result {
            Err(HtapError::Unsupported(_)) => {}
            other => panic!("expected Unsupported for {sql}, got {other:?}"),
        }
    }
}

#[test]
fn test_no_supported_shape_ever_mismatches_tokenizer_count() {
    // Regression guard: every case in the big table above (and the edge-value tests) must have
    // count_placeholders(...) == tokenizer_placeholder_count(...), i.e. checked_placeholder_count
    // never errors for a statement shape this module claims to support.
    let shapes = [
        "INSERT INTO t (id, n) VALUES (?, ?)".to_string(),
        "INSERT INTO t (id, n) VALUES (?, ?), (?, ?)".to_string(),
        "UPDATE t SET n = ? WHERE id = ?".to_string(),
        "DELETE FROM t WHERE id = ?".to_string(),
        "SELECT id FROM t WHERE id = ? OR n = ?".to_string(),
        "SELECT id FROM t WHERE id IN (?, ?, ?)".to_string(),
        "SELECT id FROM t WHERE n BETWEEN ? AND ?".to_string(),
        "SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?".to_string(),
        "WITH c AS (SELECT id FROM t WHERE id = ?) SELECT id FROM c WHERE id = ?".to_string(),
        "SELECT id FROM t WHERE id = ? UNION SELECT id FROM t WHERE id = ?".to_string(),
    ];
    for sql in shapes {
        let stmt = parse_one(&sql).unwrap();
        let walk = count_placeholders(&stmt);
        let raw = tokenizer_placeholder_count(&sql).unwrap();
        assert_eq!(walk, raw, "mismatch for {sql}");
    }
}

// ---------------------------------------------------------------------------------------------
// resolve_prepare_output_schema
// ---------------------------------------------------------------------------------------------

#[test]
fn test_resolve_prepare_output_schema_point_select() {
    let cat = catalog();
    let stmt = parse_one("SELECT id, name FROM t WHERE id = ?").unwrap();
    let schema = resolve_prepare_output_schema(&stmt, &cat)
        .expect("resolution should succeed")
        .expect("point select schema should be resolvable");
    assert_eq!(
        schema,
        vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "name".into(),
                data_type: DataType::String,
                nullable: true,
                primary_key: false,
            },
        ]
    );
}

#[test]
fn test_resolve_prepare_output_schema_filter() {
    // Not a complete-PK point read (filters on `n`, not `id`): binds through the narrow
    // analytic-filter path (`AnalyticSelect`), whose projected columns are always reported with
    // `primary_key: false` regardless of the source column (see `AnalyticExpr::to_column_def`).
    let cat = catalog();
    let stmt = parse_one("SELECT id FROM t WHERE n = ? AND amt > ?").unwrap();
    let schema = resolve_prepare_output_schema(&stmt, &cat)
        .expect("resolution should succeed")
        .expect("filtered scan schema should be resolvable");
    assert_eq!(
        schema,
        vec![ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        }]
    );
}

#[test]
fn test_resolve_prepare_output_schema_general_query_path_filter() {
    // A `LIMIT` clause forces the general query binder (`BoundQuery`) rather than the narrow
    // analytic-filter path, while remaining a single-table, no-join filter whose placeholder
    // type is still inferable from the `n` column.
    let cat = catalog();
    let stmt = parse_one("SELECT id FROM t WHERE n = ? LIMIT 10").unwrap();
    let schema = resolve_prepare_output_schema(&stmt, &cat)
        .expect("resolution should succeed")
        .expect("general-path filter schema should be resolvable");
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name, "id");
}

#[test]
fn test_resolve_prepare_output_schema_joined_filter_is_none() {
    // Multi-table (joined) placeholder type inference is out of scope for this best-effort
    // resolver: a placeholder compared against a qualified column in a joined query has no
    // locally-inferable type, so this conservatively returns `None` rather than guessing.
    let cat = catalog();
    let stmt = parse_one("SELECT t.id FROM t JOIN orders o ON t.id = o.user_id WHERE o.amount > ?")
        .unwrap();
    let schema = resolve_prepare_output_schema(&stmt, &cat).expect("resolution should succeed");
    assert_eq!(schema, None);
}

#[test]
fn test_resolve_prepare_output_schema_dml_is_empty() {
    let cat = catalog();
    for sql in [
        "INSERT INTO t (id, n) VALUES (?, ?)".to_string(),
        "UPDATE t SET n = ? WHERE id = ?".to_string(),
        "DELETE FROM t WHERE id = ?".to_string(),
    ] {
        let stmt = parse_one(&sql).unwrap();
        let schema = resolve_prepare_output_schema(&stmt, &cat)
            .unwrap_or_else(|e| panic!("resolution should succeed for {sql}: {e:?}"));
        assert_eq!(schema, Some(Vec::new()), "expected empty schema for {sql}");
    }
}

#[test]
fn test_resolve_prepare_output_schema_unresolvable_placeholder_type_is_none() {
    let cat = catalog();
    let stmt = parse_one("SELECT ? AS x").unwrap();
    let schema = resolve_prepare_output_schema(&stmt, &cat).expect("resolution should succeed");
    assert_eq!(schema, None);
}

#[test]
fn test_resolve_prepare_output_schema_show_and_ddl_are_none() {
    let cat = catalog();
    for sql in ["SHOW TABLES", "CREATE TABLE t3 (id INT PRIMARY KEY)"] {
        let stmt = parse_one(sql).unwrap();
        let schema = resolve_prepare_output_schema(&stmt, &cat)
            .unwrap_or_else(|e| panic!("resolution should succeed for {sql}: {e:?}"));
        assert_eq!(schema, None, "expected None for {sql}");
    }
}

// ---------------------------------------------------------------------------------------------
// Finding 2 of the Phase 11 fix pass: `ParamLiteral::NumericText` substitutes DECIMAL parameter
// text directly as a numeric literal, with no `f64`/`i64` round trip.
// ---------------------------------------------------------------------------------------------

/// A `BIGINT` value one past `2^53`, the largest integer an `f64` can represent exactly: routing
/// this text through `f64` (as the old `decimal_text_to_value` did) would silently corrupt it.
/// Both a `ParamLiteral::NumericText` substitution and equivalent literal SQL must bind it exactly.
#[test]
fn test_numeric_text_decimal_round_trips_exactly_into_bigint_column() {
    let cat = catalog();
    let text = "9007199254740993"; // 2^53 + 1
    assert_ne!(
        text.parse::<f64>().unwrap() as i64,
        text.parse::<i64>().unwrap(),
        "sanity check: this value must actually lose precision through f64"
    );

    let mut stmt = parse_one("SELECT id FROM t WHERE big = ?").unwrap();
    substitute_placeholders_ext(&mut stmt, &[ParamLiteral::NumericText(text.to_string())]).unwrap();
    let substituted = bind(&stmt, &cat).unwrap();

    let direct = bind_sql(&format!("SELECT id FROM t WHERE big = {text}"), &cat).unwrap();
    assert_eq!(substituted, direct);
}

#[test]
fn test_numeric_text_negative_decimal_with_fraction_round_trips() {
    let cat = catalog();
    let text = "-123.45";
    let mut stmt = parse_one("SELECT id FROM t WHERE amt = ?").unwrap();
    substitute_placeholders_ext(&mut stmt, &[ParamLiteral::NumericText(text.to_string())]).unwrap();
    let substituted = bind(&stmt, &cat).unwrap();

    let direct = bind_sql(&format!("SELECT id FROM t WHERE amt = {text}"), &cat).unwrap();
    assert_eq!(substituted, direct);
}

#[test]
fn test_numeric_text_validates_strictly() {
    let valid = [
        "0", "7", "-1", "+1", "123.456", "-0.5", "1e10", "1E-10", "-1.5e+3",
    ];
    for text in valid {
        let mut stmt = parse_one("SELECT id FROM t WHERE amt = ?").unwrap();
        substitute_placeholders_ext(&mut stmt, &[ParamLiteral::NumericText(text.to_string())])
            .unwrap_or_else(|e| panic!("{text:?} should be accepted as numeric text: {e:?}"));
    }

    let invalid = [
        "", "-", "+", ".", "12.3.4", "abc", "1e", "1..2", "1 2", "1_000", "0x1",
    ];
    for text in invalid {
        let mut stmt = parse_one("SELECT id FROM t WHERE amt = ?").unwrap();
        let err =
            substitute_placeholders_ext(&mut stmt, &[ParamLiteral::NumericText(text.to_string())])
                .expect_err(&format!("{text:?} should be rejected as numeric text"));
        assert!(matches!(err, HtapError::InvalidArgument(_)), "got {err:?}");
    }
}

#[test]
fn test_window_over_placeholder_counts_agree() {
    for sql in [
        "SELECT SUM(n) OVER (PARTITION BY ?) FROM t",
        "SELECT SUM(n) OVER (ORDER BY ?) FROM t",
        "SELECT SUM(n) OVER (ORDER BY id ROWS BETWEEN ? PRECEDING AND CURRENT ROW) FROM t",
    ] {
        let stmt = parse_one(sql).unwrap();
        assert_eq!(count_placeholders(&stmt), 1, "count mismatch for {sql}");
        assert_eq!(
            checked_placeholder_count(sql, &stmt).unwrap(),
            1,
            "checked count mismatch for {sql}"
        );
        assert_eq!(
            infer_placeholder_type_hints(&stmt, &catalog())
                .unwrap()
                .len(),
            1,
            "hint count mismatch for {sql}"
        );
    }
}

#[test]
fn test_window_partition_placeholder_substitution_matches_literal_sql() {
    assert_substitution_matches_literal(
        "SELECT SUM(n) OVER (PARTITION BY ?) FROM t",
        &[Value::Int64(7)],
    );
}

#[test]
fn test_truncate_partition_placeholder_counts_agree() {
    let sql = "TRUNCATE TABLE t PARTITION (?)";
    let stmt = parse_one(sql).unwrap();

    assert_eq!(count_placeholders(&stmt), 1);
    assert_eq!(checked_placeholder_count(sql, &stmt).unwrap(), 1);
    assert_eq!(
        infer_placeholder_type_hints(&stmt, &catalog()).unwrap(),
        vec![None]
    );
}
