use htap_common::error::HtapError;
use htap_sql::parse_one;
use sqlparser::ast::{
    BinaryOperator, Expr, Ident, ObjectName, ObjectNamePart, SelectItem, SetExpr, Statement,
    TableFactor, TableObject, TableWithJoins, Value,
};

fn object_name_to_ident(name: &ObjectName) -> &Ident {
    match &name.0[0] {
        ObjectNamePart::Identifier(ident) => ident,
        other => panic!("expected ObjectNamePart::Identifier, got {other:?}"),
    }
}

fn table_object_to_ident(table: &TableObject) -> &Ident {
    match table {
        TableObject::TableName(name) => object_name_to_ident(name),
        other => panic!("expected TableObject::TableName, got {other:?}"),
    }
}

#[test]
fn test_one_valid_statement_succeeds() {
    // SELECT without trailing semicolon
    let stmt = parse_one("SELECT id, name FROM users WHERE id = 1").expect("valid SELECT");
    assert!(matches!(stmt, Statement::Query(_)));

    // SELECT with trailing semicolon
    let stmt = parse_one("SELECT 1;").expect("valid SELECT with semicolon");
    assert!(matches!(stmt, Statement::Query(_)));

    // SELECT with trailing whitespace and newline after semicolon
    let stmt = parse_one("SELECT 1;   \n\t").expect("valid SELECT with trailing whitespace");
    assert!(matches!(stmt, Statement::Query(_)));

    // Complex SELECT with ORDER BY and LIMIT
    let stmt = parse_one("SELECT id, name FROM users WHERE id = 1 ORDER BY id DESC LIMIT 10")
        .expect("valid complex SELECT");
    assert!(matches!(stmt, Statement::Query(_)));

    // INSERT statement
    let stmt = parse_one("INSERT INTO users (id, name) VALUES (1, 'alice')").expect("valid INSERT");
    assert!(matches!(stmt, Statement::Insert(_)));

    // DELETE statement
    let stmt = parse_one("DELETE FROM users WHERE id = 1;").expect("valid DELETE");
    assert!(matches!(stmt, Statement::Delete(_)));

    // CREATE TABLE statement
    let stmt = parse_one("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255))")
        .expect("valid CREATE TABLE");
    assert!(matches!(stmt, Statement::CreateTable(_)));
}

#[test]
fn test_backtick_identifiers_parsed_under_mysql_dialect() {
    // 1. SELECT query with backticks for table, projection columns, and WHERE column
    let stmt = parse_one("SELECT `id`, `user_name` FROM `users` WHERE `id` = 42")
        .expect("valid backtick select");
    match stmt {
        Statement::Query(query) => match *query.body {
            SetExpr::Select(select) => {
                // Assert projection column identifiers
                assert_eq!(select.projection.len(), 2);
                match &select.projection[0] {
                    SelectItem::UnnamedExpr(Expr::Identifier(Ident {
                        value, quote_style, ..
                    })) => {
                        assert_eq!(value, "id");
                        assert_eq!(*quote_style, Some('`'));
                    }
                    other => panic!("expected UnnamedExpr(Identifier), got {other:?}"),
                }
                match &select.projection[1] {
                    SelectItem::UnnamedExpr(Expr::Identifier(Ident {
                        value, quote_style, ..
                    })) => {
                        assert_eq!(value, "user_name");
                        assert_eq!(*quote_style, Some('`'));
                    }
                    other => panic!("expected UnnamedExpr(Identifier), got {other:?}"),
                }

                // Assert FROM table identifier
                assert_eq!(select.from.len(), 1);
                match &select.from[0] {
                    TableWithJoins {
                        relation: TableFactor::Table { name, .. },
                        ..
                    } => {
                        let ident = object_name_to_ident(name);
                        assert_eq!(ident.value, "users");
                        assert_eq!(ident.quote_style, Some('`'));
                    }
                    other => panic!("expected TableFactor::Table, got {other:?}"),
                }

                // Assert WHERE column identifier
                match select.selection {
                    Some(Expr::BinaryOp { left, .. }) => match *left {
                        Expr::Identifier(Ident {
                            value, quote_style, ..
                        }) => {
                            assert_eq!(value, "id");
                            assert_eq!(quote_style, Some('`'));
                        }
                        other => panic!("expected Expr::Identifier, got {other:?}"),
                    },
                    other => panic!("expected Expr::BinaryOp, got {other:?}"),
                }
            }
            other => panic!("expected SetExpr::Select, got {other:?}"),
        },
        other => panic!("expected Statement::Query, got {other:?}"),
    }

    // 2. Reserved keywords used as identifiers via backtick quotes
    let stmt = parse_one("SELECT `select`, `from`, `order` FROM `table`")
        .expect("valid keywords in backticks");
    match stmt {
        Statement::Query(query) => match *query.body {
            SetExpr::Select(select) => {
                let expected_cols = ["select", "from", "order"];
                assert_eq!(select.projection.len(), expected_cols.len());
                for (i, expected) in expected_cols.iter().enumerate() {
                    match &select.projection[i] {
                        SelectItem::UnnamedExpr(Expr::Identifier(Ident {
                            value,
                            quote_style,
                            ..
                        })) => {
                            assert_eq!(value, expected);
                            assert_eq!(*quote_style, Some('`'));
                        }
                        other => panic!("expected UnnamedExpr(Identifier), got {other:?}"),
                    }
                }
                match &select.from[0] {
                    TableWithJoins {
                        relation: TableFactor::Table { name, .. },
                        ..
                    } => {
                        let ident = object_name_to_ident(name);
                        assert_eq!(ident.value, "table");
                        assert_eq!(ident.quote_style, Some('`'));
                    }
                    other => panic!("expected TableFactor::Table, got {other:?}"),
                }
            }
            other => panic!("expected SetExpr::Select, got {other:?}"),
        },
        other => panic!("expected Statement::Query, got {other:?}"),
    }

    // 3. CREATE TABLE with backtick identifiers
    let stmt = parse_one("CREATE TABLE `orders` (`order_id` BIGINT, `price` DOUBLE)")
        .expect("valid backtick create table");
    match stmt {
        Statement::CreateTable(create_table) => {
            let table_ident = object_name_to_ident(&create_table.name);
            assert_eq!(table_ident.value, "orders");
            assert_eq!(table_ident.quote_style, Some('`'));
            assert_eq!(create_table.columns[0].name.value, "order_id");
            assert_eq!(create_table.columns[0].name.quote_style, Some('`'));
            assert_eq!(create_table.columns[1].name.value, "price");
            assert_eq!(create_table.columns[1].name.quote_style, Some('`'));
        }
        other => panic!("expected Statement::CreateTable, got {other:?}"),
    }

    // 4. INSERT INTO with backtick table and column identifiers
    let stmt = parse_one("INSERT INTO `users` (`id`, `name`) VALUES (1, 'bob')")
        .expect("valid backtick insert");
    match stmt {
        Statement::Insert(insert) => {
            let table_ident = table_object_to_ident(&insert.table);
            assert_eq!(table_ident.value, "users");
            assert_eq!(table_ident.quote_style, Some('`'));
            let col0_ident = object_name_to_ident(&insert.columns[0]);
            assert_eq!(col0_ident.value, "id");
            assert_eq!(col0_ident.quote_style, Some('`'));
            let col1_ident = object_name_to_ident(&insert.columns[1]);
            assert_eq!(col1_ident.value, "name");
            assert_eq!(col1_ident.quote_style, Some('`'));
        }
        other => panic!("expected Statement::Insert, got {other:?}"),
    }

    // 5. DELETE with backtick table and WHERE identifier
    let stmt = parse_one("DELETE FROM `users` WHERE `id` = 99").expect("valid backtick delete");
    match stmt {
        Statement::Delete(delete) => {
            match &delete.from {
                sqlparser::ast::FromTable::WithFromKeyword(tables) => match &tables[0] {
                    TableWithJoins {
                        relation: TableFactor::Table { name, .. },
                        ..
                    } => {
                        let ident = object_name_to_ident(name);
                        assert_eq!(ident.value, "users");
                        assert_eq!(ident.quote_style, Some('`'));
                    }
                    other => panic!("expected TableFactor::Table, got {other:?}"),
                },
                other => panic!("expected FromTable::WithFromKeyword, got {other:?}"),
            }
            match delete.selection {
                Some(Expr::BinaryOp { left, .. }) => match *left {
                    Expr::Identifier(Ident {
                        value, quote_style, ..
                    }) => {
                        assert_eq!(value, "id");
                        assert_eq!(quote_style, Some('`'));
                    }
                    other => panic!("expected Expr::Identifier, got {other:?}"),
                },
                other => panic!("expected Expr::BinaryOp, got {other:?}"),
            }
        }
        other => panic!("expected Statement::Delete, got {other:?}"),
    }
}

fn extract_select_first_value(sql: &str) -> Value {
    let stmt = parse_one(sql).unwrap_or_else(|e| panic!("failed to parse {sql:?}: {e}"));
    match stmt {
        Statement::Query(query) => match *query.body {
            SetExpr::Select(select) => match &select.projection[0] {
                SelectItem::UnnamedExpr(Expr::Value(v)) => v.value.clone(),
                other => panic!("expected UnnamedExpr(Expr::Value), got {other:?}"),
            },
            other => panic!("expected SetExpr::Select, got {other:?}"),
        },
        other => panic!("expected Statement::Query, got {other:?}"),
    }
}

#[test]
fn test_mysql_backslash_escaped_string_parsing() {
    // Assert actual parser AST values as supported by sqlparser 0.62 under MySqlDialect

    // Single quote escaped via backslash: \' -> '
    assert_eq!(
        extract_select_first_value(r"SELECT 'O\'Connor'"),
        Value::SingleQuotedString("O'Connor".to_string())
    );

    // Escaped backslash: \\ -> \
    assert_eq!(
        extract_select_first_value(r"SELECT 'path\\to\\file'"),
        Value::SingleQuotedString(r"path\to\file".to_string())
    );

    // Escaped newline: \n -> newline character
    assert_eq!(
        extract_select_first_value(r"SELECT 'hello\nworld'"),
        Value::SingleQuotedString("hello\nworld".to_string())
    );

    // Escaped tab: \t -> tab character
    assert_eq!(
        extract_select_first_value(r"SELECT 'col1\tcol2'"),
        Value::SingleQuotedString("col1\tcol2".to_string())
    );

    // Escaped carriage return: \r -> carriage return character
    assert_eq!(
        extract_select_first_value(r"SELECT 'line1\rline2'"),
        Value::SingleQuotedString("line1\rline2".to_string())
    );

    // Escaped null byte: \0 -> null character
    assert_eq!(
        extract_select_first_value(r"SELECT 'null\0byte'"),
        Value::SingleQuotedString("null\0byte".to_string())
    );

    // Escaped double quote inside single-quoted string: \" -> "
    assert_eq!(
        extract_select_first_value(r#"SELECT 'he said \"hello\"'"#),
        Value::SingleQuotedString("he said \"hello\"".to_string())
    );

    // Multiple projections with different escapes in one query
    let stmt = parse_one(r"SELECT 'col1\tcol2', 'path\\to\\file'").expect("valid multiple escapes");
    match stmt {
        Statement::Query(query) => match *query.body {
            SetExpr::Select(select) => {
                assert_eq!(select.projection.len(), 2);
                match &select.projection[0] {
                    SelectItem::UnnamedExpr(Expr::Value(v)) => {
                        assert_eq!(v.value, Value::SingleQuotedString("col1\tcol2".to_string()));
                    }
                    other => panic!("expected Value, got {other:?}"),
                }
                match &select.projection[1] {
                    SelectItem::UnnamedExpr(Expr::Value(v)) => {
                        assert_eq!(
                            v.value,
                            Value::SingleQuotedString(r"path\to\file".to_string())
                        );
                    }
                    other => panic!("expected Value, got {other:?}"),
                }
            }
            other => panic!("expected SetExpr::Select, got {other:?}"),
        },
        other => panic!("expected Statement::Query, got {other:?}"),
    }

    // MySQL double-quoted string with backslash escaping
    assert_eq!(
        extract_select_first_value(r#"SELECT "hello\nworld""#),
        Value::DoubleQuotedString("hello\nworld".to_string())
    );
    assert_eq!(
        extract_select_first_value(r#"SELECT "he said \"hello\"""#),
        Value::DoubleQuotedString("he said \"hello\"".to_string())
    );

    // Escaped string in INSERT VALUES AST
    let stmt = parse_one(r"INSERT INTO users (name) VALUES ('O\'Connor')")
        .expect("valid escaped single quote in INSERT");
    match stmt {
        Statement::Insert(insert) => {
            let query = insert.source.expect("insert has source query");
            match *query.body {
                SetExpr::Values(values) => {
                    assert_eq!(values.rows.len(), 1);
                    assert_eq!(values.rows[0].content.len(), 1);
                    match &values.rows[0].content[0] {
                        Expr::Value(v) => {
                            assert_eq!(v.value, Value::SingleQuotedString("O'Connor".to_string()));
                        }
                        other => panic!("expected Expr::Value, got {other:?}"),
                    }
                }
                other => panic!("expected SetExpr::Values, got {other:?}"),
            }
        }
        other => panic!("expected Statement::Insert, got {other:?}"),
    }

    // Escaped string in WHERE clause AST
    let stmt = parse_one(r"SELECT id FROM users WHERE name = 'O\'Connor'")
        .expect("valid escaped single quote in WHERE");
    match stmt {
        Statement::Query(query) => match *query.body {
            SetExpr::Select(select) => match select.selection {
                Some(Expr::BinaryOp { left, op, right }) => {
                    assert_eq!(*left, Expr::Identifier(Ident::new("name")));
                    assert_eq!(op, BinaryOperator::Eq);
                    match *right {
                        Expr::Value(v) => {
                            assert_eq!(v.value, Value::SingleQuotedString("O'Connor".to_string()));
                        }
                        other => panic!("expected Expr::Value, got {other:?}"),
                    }
                }
                other => panic!("expected Expr::BinaryOp, got {other:?}"),
            },
            other => panic!("expected SetExpr::Select, got {other:?}"),
        },
        other => panic!("expected Statement::Query, got {other:?}"),
    }
}

#[test]
fn test_malformed_sql_returns_invalid_argument() {
    let cases = [
        "SELECT",
        "SELECT FROM",
        "NOT A SQL STATEMENT",
        "INSERT INTO",
        "CREATE TABLE",
        "DELETE WHERE",
        "SELECT * FROM table WHERE",
        "INSERT INTO t VALUES (",
        "SELECT 'unclosed string",
        "SELECT `unclosed_backtick FROM users",
        "SELECT 1 1",
        "SELECT WHERE id = 1",
        "SELECT @",
        "UPDATE",
        "DROP",
        "ALTER TABLE",
    ];

    for case in cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for malformed SQL {:?}, got {:?}",
            case,
            res
        );
        if let Err(HtapError::InvalidArgument(msg)) = res {
            assert!(
                msg.contains("SQL parse error") || msg.contains("statement is empty"),
                "expected parse error message for {case:?}, got: {msg}"
            );
        }
    }
}

#[test]
fn test_empty_whitespace_sql_returns_invalid_argument() {
    let cases = [
        "",
        " ",
        "   ",
        "\t",
        "\n",
        "\r\n",
        "  \t \n \r  ",
        ";",
        "  ;  ",
        ";;",
        "; ; ;",
        "\t ; \n ; \r ",
    ];

    for case in cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for empty SQL {:?}, got {:?}",
            case,
            res
        );
        if let Err(HtapError::InvalidArgument(msg)) = res {
            assert!(
                msg.contains("statement is empty"),
                "expected 'statement is empty' in error message for {case:?}, got: {msg}"
            );
        }
    }
}

#[test]
fn test_multi_statement_sql_returns_invalid_argument() {
    let cases = [
        "SELECT 1; SELECT 2;",
        "SELECT 1; SELECT 2",
        "INSERT INTO t VALUES (1); DROP TABLE t;",
        "CREATE TABLE a (id INT); CREATE TABLE b (id INT);",
        "SELECT * FROM a; SELECT * FROM b; SELECT * FROM c",
        "DELETE FROM users WHERE id = 1; DELETE FROM users WHERE id = 2;",
    ];

    for case in cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for multi-statement SQL {:?}, got {:?}",
            case,
            res
        );
        if let Err(HtapError::InvalidArgument(msg)) = res {
            assert!(
                msg.contains("expected exactly one SQL statement"),
                "expected statement count error message for {case:?}, got: {msg}"
            );
        }
    }
}
