use htap_common::error::HtapError;
use htap_sql::parse_one;
use sqlparser::ast::{
    BinaryOperator, Expr, Ident, ObjectName, ObjectNamePart, SelectItem, SetExpr, Statement,
    TableConstraint, TableFactor, TableObject, TableWithJoins, Value,
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
fn test_mysql_partition_ddl_parsed_and_bound() {
    let cases = [
        "CREATE TABLE t (id INT PRIMARY KEY, val INT) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (20))",
        "CREATE TABLE t (id INT PRIMARY KEY, val INT) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN MAXVALUE)",
        "CREATE TABLE t (id INT PRIMARY KEY, val INT) PARTITION BY RANGE COLUMNS (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (20))",
        "CREATE TABLE t (id INT PRIMARY KEY, val INT) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (3, 4))",
        "CREATE TABLE t (id INT PRIMARY KEY, val INT) PARTITION BY LIST COLUMNS (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (3, 4))",
    ];

    let cat = CatalogSnapshot::empty();
    for case in cases {
        let stmt = parse_one(case).expect("should parse successfully");
        let bound = bind(&stmt, &cat).expect("should bind successfully");
        match bound {
            BoundStatement::CreateTable(create) => {
                assert!(
                    create.partitioning.is_some(),
                    "expected partitioning for {case}"
                );
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }
}

#[test]
fn test_mysql_partition_ddl_negative_parser_and_binder() {
    let catalog = CatalogSnapshot::empty();

    // 1. Partition options (ENGINE, COMMENT, TABLESPACE) are rejected at parse time
    let option_cases = [
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10) ENGINE = InnoDB)",
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10) COMMENT = 'test comment')",
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10) TABLESPACE = ts1)",
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10) DATA DIRECTORY = '/data')",
    ];
    for case in option_cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected parse error for option case {case:?}, got {res:?}"
        );
    }

    // 2. SUBPARTITION syntax is rejected at parse time
    let subpartition_cases = [
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) SUBPARTITION BY HASH(id) (PARTITION p0 VALUES LESS THAN (10))",
        "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10) (SUBPARTITION s0))",
    ];
    for case in subpartition_cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected parse error for subpartition case {case:?}, got {res:?}"
        );
    }

    // 3. LIST DEFAULT is rejected (syntax without parens fails parser; with parens fails binder)
    let list_default_noparens = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1), PARTITION p1 VALUES IN DEFAULT)";
    let res = parse_one(list_default_noparens);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected parse error for LIST DEFAULT without parens, got {res:?}"
    );

    let list_default_case = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1), PARTITION p1 VALUES IN (DEFAULT))";
    let stmt = parse_one(list_default_case).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected binder error for LIST DEFAULT, got {res:?}"
    );

    // 4. Malformed and non-final MAXVALUE
    // Non-final MAXVALUE parses, but is rejected by binder
    let nonfinal_maxvalue = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN MAXVALUE, PARTITION p1 VALUES LESS THAN (20))";
    let stmt = parse_one(nonfinal_maxvalue).expect("should parse non-final MAXVALUE");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("defined after MAXVALUE partition")),
        "expected binder error for non-final MAXVALUE, got {res:?}"
    );

    // Malformed MAXVALUE in tuple
    let malformed_maxvalue_tuple = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (MAXVALUE, 10))";
    let res = parse_one(malformed_maxvalue_tuple);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected parse error for tuple MAXVALUE, got {res:?}"
    );

    // 5. Multi-column COLUMNS (parses, but rejected by binder as Unsupported)
    let multi_col_range = "CREATE TABLE t (id INT, k INT, PRIMARY KEY (id, k)) PARTITION BY RANGE COLUMNS (id, k) (PARTITION p0 VALUES LESS THAN (10))";
    let stmt = parse_one(multi_col_range).expect("should parse multi-column range");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::Unsupported(ref msg)) if msg.contains("multi-column partitioning is not supported")),
        "expected Unsupported for multi-column range, got {res:?}"
    );

    let multi_col_list = "CREATE TABLE t (id INT, k INT, PRIMARY KEY (id, k)) PARTITION BY LIST COLUMNS (id, k) (PARTITION p0 VALUES IN (1, 2))";
    let stmt = parse_one(multi_col_list).expect("should parse multi-column list");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::Unsupported(ref msg)) if msg.contains("multi-column partitioning is not supported")),
        "expected Unsupported for multi-column list, got {res:?}"
    );

    // 6. Expressions in PARTITION BY (parses, but rejected by binder as Unsupported)
    let expr_range = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id + 1) (PARTITION p0 VALUES LESS THAN (10))";
    let stmt = parse_one(expr_range).expect("should parse range expression");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::Unsupported(ref msg)) if msg.contains("expressions in PARTITION BY are not supported")),
        "expected Unsupported for range expression, got {res:?}"
    );

    let expr_list = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id * 2) (PARTITION p0 VALUES IN (1, 2))";
    let stmt = parse_one(expr_list).expect("should parse list expression");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::Unsupported(ref msg)) if msg.contains("expressions in PARTITION BY are not supported")),
        "expected Unsupported for list expression, got {res:?}"
    );

    // 7. Duplicate names / duplicate list values (parses, but rejected by binder as InvalidArgument)
    // Duplicate partition name in RANGE
    let dup_name_range = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p0 VALUES LESS THAN (20))";
    let stmt = parse_one(dup_name_range).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("duplicate partition name 'p0'")),
        "expected InvalidArgument for dup partition name in range, got {res:?}"
    );

    // Duplicate partition name in LIST
    let dup_name_list = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1), PARTITION p0 VALUES IN (2))";
    let stmt = parse_one(dup_name_list).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("duplicate partition name 'p0'")),
        "expected InvalidArgument for dup partition name in list, got {res:?}"
    );

    // Duplicate list value within same partition
    let dup_val_intra = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 1))";
    let stmt = parse_one(dup_val_intra).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("duplicate list value '1'")),
        "expected InvalidArgument for duplicate value within partition, got {res:?}"
    );

    // Duplicate list value across partitions
    let dup_val_inter = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (2, 3))";
    let stmt = parse_one(dup_val_inter).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("duplicate list value '2' across partitions")),
        "expected InvalidArgument for duplicate value across partitions, got {res:?}"
    );

    // 8. Invalid range order (parses, but rejected by binder as InvalidArgument)
    // Decreasing order
    let decreasing_range = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (20), PARTITION p1 VALUES LESS THAN (10))";
    let stmt = parse_one(decreasing_range).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("strictly increasing")),
        "expected InvalidArgument for decreasing range order, got {res:?}"
    );

    // Equal adjacent bounds
    let equal_range = "CREATE TABLE t (id INT PRIMARY KEY) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (10))";
    let stmt = parse_one(equal_range).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("strictly increasing")),
        "expected InvalidArgument for equal range bounds, got {res:?}"
    );
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

#[test]
fn test_parse_many_splits_and_drops_empty_statements() {
    let sql = "SELECT 1; SELECT 2";
    let stmts = htap_sql::parse_many(sql).unwrap();
    assert_eq!(stmts.len(), 2);

    // Extra/trailing `;` produce no empty statements.
    for sql in ["SELECT 1;;", "SELECT 1; ; SELECT 2;", "SELECT 1;"] {
        let stmts = htap_sql::parse_many(sql).unwrap();
        assert!(!stmts.is_empty());
    }
    assert_eq!(
        htap_sql::parse_many("SELECT 1; ; SELECT 2;").unwrap().len(),
        2
    );
    assert_eq!(htap_sql::parse_many("SELECT 1;").unwrap().len(), 1);

    // A single statement round-trips identically to `parse_one`.
    assert_eq!(
        htap_sql::parse_many("SELECT 1").unwrap(),
        vec![parse_one("SELECT 1").unwrap()]
    );
}

#[test]
fn test_parse_many_rejects_empty_and_semicolons_only() {
    for case in ["", "   ", ";", "  ;  ;  "] {
        let res = htap_sql::parse_many(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }
}

#[test]
fn test_parse_many_propagates_syntax_errors() {
    let res = htap_sql::parse_many("SELECT 1; NOT VALID SQL AT ALL");
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));
}

#[test]
fn test_bind_account_management_statements() {
    use htap_catalog::PrivilegeSet;
    use htap_sql::{
        AlterUserStatement, CreateUserStatement, DropUserStatement, GrantScope, GrantStatement,
        RevokeStatement, ShowGrantsStatement,
    };

    let catalog = make_test_catalog();

    let cases = [
        (
            "CREATE USER 'u'@'%' IDENTIFIED BY 'p'",
            BoundStatement::CreateUser(CreateUserStatement {
                username: "u".into(),
                if_not_exists: false,
                password: Some("p".into()),
            }),
        ),
        (
            "CREATE USER IF NOT EXISTS u IDENTIFIED BY 'p'",
            BoundStatement::CreateUser(CreateUserStatement {
                username: "u".into(),
                if_not_exists: true,
                password: Some("p".into()),
            }),
        ),
        (
            "ALTER USER 'u'@'%' IDENTIFIED BY 'q'",
            BoundStatement::AlterUser(AlterUserStatement {
                username: "u".into(),
                if_exists: false,
                password: "q".into(),
            }),
        ),
        (
            "ALTER USER IF EXISTS u IDENTIFIED BY 'q'",
            BoundStatement::AlterUser(AlterUserStatement {
                username: "u".into(),
                if_exists: true,
                password: "q".into(),
            }),
        ),
        (
            "DROP USER IF EXISTS a, 'b'@'%'",
            BoundStatement::DropUser(DropUserStatement {
                usernames: vec!["a".into(), "b".into()],
                if_exists: true,
            }),
        ),
        (
            "GRANT SELECT, INSERT ON *.* TO u",
            BoundStatement::GrantPrivileges(GrantStatement {
                privileges: PrivilegeSet(PrivilegeSet::SELECT.0 | PrivilegeSet::INSERT.0),
                scope: GrantScope::Global,
                grantee: "u".into(),
            }),
        ),
        (
            "GRANT ALL ON htap.* TO u",
            BoundStatement::GrantPrivileges(GrantStatement {
                privileges: PrivilegeSet::ALL,
                scope: GrantScope::Global,
                grantee: "u".into(),
            }),
        ),
        (
            "GRANT SELECT ON t TO 'u'@'%'",
            BoundStatement::GrantPrivileges(GrantStatement {
                privileges: PrivilegeSet::SELECT,
                scope: GrantScope::Table("t".into()),
                grantee: "u".into(),
            }),
        ),
        (
            "REVOKE DELETE ON htap.t FROM u",
            BoundStatement::RevokePrivileges(RevokeStatement {
                privileges: PrivilegeSet::DELETE,
                scope: GrantScope::Table("t".into()),
                grantee: "u".into(),
            }),
        ),
        (
            "SHOW GRANTS",
            BoundStatement::ShowGrants(ShowGrantsStatement { for_username: None }),
        ),
        (
            "SHOW GRANTS FOR u",
            BoundStatement::ShowGrants(ShowGrantsStatement {
                for_username: Some("u".into()),
            }),
        ),
    ];

    for (sql, expected) in cases {
        assert_eq!(
            parse_and_bind(sql, &catalog).unwrap(),
            expected,
            "unexpected binding for {sql}"
        );
    }
}

#[test]
fn test_bind_account_management_rejections() {
    let catalog = make_test_catalog();

    for sql in [
        "CREATE USER 'u'@'localhost' IDENTIFIED BY 'p'",
        "GRANT SELECT ON t TO u WITH GRANT OPTION",
        "GRANT SELECT (c) ON t TO u",
        "GRANT REFERENCES ON t TO u",
        "GRANT SELECT ON t TO a, b",
    ] {
        assert!(
            matches!(
                parse_and_bind(sql, &catalog),
                Err(HtapError::Unsupported(_))
            ),
            "expected Unsupported for {sql}"
        );
    }

    assert!(matches!(
        parse_and_bind("GRANT SELECT ON missing TO u", &catalog),
        Err(HtapError::NotFound(_))
    ));
    assert!(matches!(
        parse_and_bind("GRANT SELECT ON otherdb.* TO u", &catalog),
        Err(HtapError::NotFound(_))
    ));

    let sql = "CREATE USER u PASSWORD = 'x'";
    if let Ok(statement) = parse_one(sql) {
        assert!(
            matches!(bind(&statement, &catalog), Err(HtapError::Unsupported(_))),
            "expected Unsupported for {sql}"
        );
    }
}

use htap_catalog::{CatalogSnapshot, TableDescriptor, TableId};
use htap_common::types::{
    ColumnDef as CommonColumnDef, DataType as CommonDataType, Row, Schema, Value as CommonValue,
};
use htap_sql::{bind, BoundStatement, InsertSource};

#[test]
fn test_grant_table_vs_global_grantee_binding() {
    let catalog = make_test_catalog();

    for sql in [
        "GRANT SELECT ON widgets TO alice",
        "GRANT SELECT ON *.* TO alice",
    ] {
        let stmt = parse_one(sql).expect("GRANT should parse");
        println!("parsed {sql:?}: {stmt:#?}");

        let bound = bind(&stmt, &catalog).expect("GRANT should bind");
        println!("bound {sql:?}: {bound:#?}");

        match bound {
            BoundStatement::GrantPrivileges(grant) => assert_eq!(grant.grantee, "alice"),
            other => panic!("expected GrantPrivileges, got {other:?}"),
        }
    }
}

#[test]
fn test_grant_grantee_parsing_regression() {
    let catalog = make_test_catalog();

    let unquoted = parse_and_bind("GRANT SELECT ON widgets TO alice", &catalog)
        .expect("unquoted GRANT should bind");
    let quoted = parse_and_bind("GRANT SELECT ON htap.widgets TO 'alice'@'%'", &catalog)
        .expect("quoted GRANT should bind");

    let unquoted_grantee = match unquoted {
        BoundStatement::GrantPrivileges(grant) => grant.grantee,
        other => panic!("expected GrantPrivileges, got {other:?}"),
    };
    let quoted_grantee = match quoted {
        BoundStatement::GrantPrivileges(grant) => grant.grantee,
        other => panic!("expected GrantPrivileges, got {other:?}"),
    };

    assert_eq!(unquoted_grantee, "alice");
    assert_eq!(quoted_grantee, "alice");
    assert_eq!(unquoted_grantee, quoted_grantee);
}

fn make_test_catalog() -> CatalogSnapshot {
    let users_schema = Schema::new(vec![
        CommonColumnDef {
            name: "id".to_string(),
            data_type: CommonDataType::Int32,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "name".to_string(),
            data_type: CommonDataType::String,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "age".to_string(),
            data_type: CommonDataType::Int32,
            nullable: true,
            primary_key: false,
        },
        CommonColumnDef {
            name: "bio".to_string(),
            data_type: CommonDataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();
    let users_table = TableDescriptor::new(TableId(1), "users", users_schema, vec![0], vec![], 1);

    let orders_schema = Schema::new(vec![
        CommonColumnDef {
            name: "tenant_id".to_string(),
            data_type: CommonDataType::Int32,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "order_id".to_string(),
            data_type: CommonDataType::Int64,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "amount".to_string(),
            data_type: CommonDataType::Float64,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "created_at".to_string(),
            data_type: CommonDataType::Timestamp,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();
    let orders_table =
        TableDescriptor::new(TableId(2), "orders", orders_schema, vec![0, 1], vec![], 1);

    let bytes_schema = Schema::new(vec![
        CommonColumnDef {
            name: "id".to_string(),
            data_type: CommonDataType::Int32,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "data".to_string(),
            data_type: CommonDataType::Bytes,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();
    let bytes_table =
        TableDescriptor::new(TableId(3), "bytes_table", bytes_schema, vec![0], vec![], 1);

    let all_types_schema = Schema::new(vec![
        CommonColumnDef {
            name: "c_bool".to_string(),
            data_type: CommonDataType::Bool,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_int".to_string(),
            data_type: CommonDataType::Int32,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "c_bigint".to_string(),
            data_type: CommonDataType::Int64,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_double".to_string(),
            data_type: CommonDataType::Float64,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_varchar".to_string(),
            data_type: CommonDataType::String,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_text".to_string(),
            data_type: CommonDataType::String,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_varbinary".to_string(),
            data_type: CommonDataType::Bytes,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_blob".to_string(),
            data_type: CommonDataType::Bytes,
            nullable: false,
            primary_key: false,
        },
        CommonColumnDef {
            name: "c_timestamp".to_string(),
            data_type: CommonDataType::Timestamp,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();
    let all_types_table = TableDescriptor::new(
        TableId(4),
        "all_types",
        all_types_schema,
        vec![1],
        vec![],
        1,
    );

    let t_schema = Schema::new(vec![CommonColumnDef {
        name: "id".to_string(),
        data_type: CommonDataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();
    let t_table = TableDescriptor::new(TableId(5), "t", t_schema, vec![0], vec![], 1);

    let widgets_schema = Schema::new(vec![CommonColumnDef {
        name: "id".to_string(),
        data_type: CommonDataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();
    let widgets_table =
        TableDescriptor::new(TableId(6), "widgets", widgets_schema, vec![0], vec![], 1);

    CatalogSnapshot::new(
        1,
        vec![
            users_table,
            orders_table,
            bytes_table,
            all_types_table,
            t_table,
            widgets_table,
        ],
        vec![],
        vec![],
        vec![],
    )
}

fn parse_and_bind(sql: &str, catalog: &CatalogSnapshot) -> htap_common::Result<BoundStatement> {
    let stmt = parse_one(sql)?;
    bind(&stmt, catalog)
}

#[test]
fn test_bind_truncate() {
    let catalog = make_test_catalog();

    for sql in ["TRUNCATE t", "TRUNCATE TABLE t"] {
        match parse_and_bind(sql, &catalog).unwrap() {
            BoundStatement::Delete(delete) => {
                assert_eq!(delete.table, "t");
                assert_eq!(delete.target, htap_sql::DeleteTarget::Filter(None));
                assert!(!delete.if_exists);
            }
            other => panic!("expected Delete for {sql}, got {other:?}"),
        }
    }

    let statement = parse_one("TRUNCATE t, u").expect("multi-table TRUNCATE should parse");
    let error = bind(&statement, &catalog).unwrap_err();
    assert!(
        matches!(error, HtapError::Unsupported(ref message) if message.contains("multiple")),
        "expected multi-table TRUNCATE rejection, got {error:?}"
    );

    if let Ok(statement) = parse_one("TRUNCATE TABLE t PARTITION (p0)") {
        let error = bind(&statement, &catalog).unwrap_err();
        assert!(
            matches!(error, HtapError::Unsupported(ref message) if message.contains("partitions")),
            "expected partition TRUNCATE rejection, got {error:?}"
        );
    }
}

#[test]
fn test_bind_create_table_valid() {
    let catalog = CatalogSnapshot::empty();

    // 1. Quoted backtick CREATE TABLE
    let sql = "CREATE TABLE `accounts` (`acc_id` INT PRIMARY KEY, `balance` DOUBLE NOT NULL)";
    let bound = parse_and_bind(sql, &catalog).expect("valid quoted CREATE TABLE");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.name, "accounts");
            assert_eq!(create.primary_key, vec![0]);
            assert_eq!(create.schema.len(), 2);
            assert_eq!(create.schema.column(0).unwrap().name, "acc_id");
            assert_eq!(
                create.schema.column(0).unwrap().data_type,
                CommonDataType::Int32
            );
            assert!(!create.schema.column(0).unwrap().nullable);
            assert!(create.schema.column(0).unwrap().primary_key);

            assert_eq!(create.schema.column(1).unwrap().name, "balance");
            assert_eq!(
                create.schema.column(1).unwrap().data_type,
                CommonDataType::Float64
            );
            assert!(!create.schema.column(1).unwrap().nullable);
            assert!(!create.schema.column(1).unwrap().primary_key);
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }

    // 2. All scalar types mapped to htap_common DataType
    let sql = "CREATE TABLE all_types (\
        c_bool BOOL, \
        c_int INT, \
        c_bigint BIGINT, \
        c_double DOUBLE, \
        c_varchar VARCHAR(255), \
        c_text TEXT, \
        c_varbinary VARBINARY(16), \
        c_blob BLOB, \
        c_timestamp TIMESTAMP, \
        PRIMARY KEY (c_int)\
    )";
    let bound = parse_and_bind(sql, &catalog).expect("valid CREATE TABLE with all scalar types");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.name, "all_types");
            assert_eq!(create.primary_key, vec![1]);
            let types: Vec<_> = create
                .schema
                .columns()
                .iter()
                .map(|c| c.data_type)
                .collect();
            assert_eq!(
                types,
                vec![
                    CommonDataType::Bool,
                    CommonDataType::Int32,
                    CommonDataType::Int64,
                    CommonDataType::Float64,
                    CommonDataType::String,
                    CommonDataType::String,
                    CommonDataType::Bytes,
                    CommonDataType::Bytes,
                    CommonDataType::Timestamp,
                ]
            );
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }

    // 3. Column PK
    let sql = "CREATE TABLE t1 (id BIGINT PRIMARY KEY, name VARCHAR(255))";
    let bound = parse_and_bind(sql, &catalog).expect("valid column PK");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.primary_key, vec![0]);
            assert!(!create.schema.column(0).unwrap().nullable);
            assert!(create.schema.column(0).unwrap().primary_key);
            assert!(create.schema.column(1).unwrap().nullable);
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }

    // 4. Table composite PK
    let sql = "CREATE TABLE t2 (tenant_id INT, user_id BIGINT, name TEXT, PRIMARY KEY (tenant_id, user_id))";
    let bound = parse_and_bind(sql, &catalog).expect("valid table composite PK");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.primary_key, vec![0, 1]);
            assert!(create.schema.column(0).unwrap().primary_key);
            assert!(!create.schema.column(0).unwrap().nullable);
            assert!(create.schema.column(1).unwrap().primary_key);
            assert!(!create.schema.column(1).unwrap().nullable);
            assert!(!create.schema.column(2).unwrap().primary_key);
            assert!(create.schema.column(2).unwrap().nullable);
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }

    // 5. Table composite PK (exact requirement: CREATE TABLE t (a INT, b BIGINT, PRIMARY KEY (a,b)))
    let sql = "CREATE TABLE t (a INT, b BIGINT, PRIMARY KEY (a,b))";
    let bound = parse_and_bind(sql, &catalog).expect("valid table composite PK");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.name, "t");
            assert_eq!(create.primary_key, vec![0, 1]);
            assert!(create.schema.column(0).unwrap().primary_key);
            assert!(!create.schema.column(0).unwrap().nullable);
            assert!(create.schema.column(1).unwrap().primary_key);
            assert!(!create.schema.column(1).unwrap().nullable);
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_bind_insert_valid() {
    let catalog = make_test_catalog();

    // 1. Multi-row reordered INSERT with nullable NULL
    let sql = "INSERT INTO users (bio, age, name, id) VALUES ('first bio', 30, 'alice', 1), ('second bio', NULL, 'bob', 2)";
    let bound = parse_and_bind(sql, &catalog).expect("valid multi-row reordered INSERT");
    match bound {
        BoundStatement::Insert(insert) => {
            assert_eq!(insert.table, "users");
            let rows = match &insert.source {
                InsertSource::Values(rows) => rows,
                other => panic!("expected InsertSource::Values, got {other:?}"),
            };
            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows[0],
                Row::new(vec![
                    CommonValue::Int32(1),
                    CommonValue::String("alice".into()),
                    CommonValue::Int32(30),
                    CommonValue::String("first bio".into()),
                ])
            );
            assert_eq!(
                rows[1],
                Row::new(vec![
                    CommonValue::Int32(2),
                    CommonValue::String("bob".into()),
                    CommonValue::Null,
                    CommonValue::String("second bio".into()),
                ])
            );
        }
        other => panic!("expected Insert, got {other:?}"),
    }

    // 2. All supported literal values
    let sql = "INSERT INTO all_types (\
        c_bool, c_int, c_bigint, c_double, c_varchar, c_text, c_varbinary, c_blob, c_timestamp\
    ) VALUES (\
        true, -42, 9223372036854775807, 3.25, 'quoted_str', \"double_quoted\", X'01020304', 0xabcd, 1700000000\
    )";
    let bound = parse_and_bind(sql, &catalog).expect("valid literals INSERT");
    match bound {
        BoundStatement::Insert(insert) => {
            assert_eq!(insert.table, "all_types");
            let rows = match &insert.source {
                InsertSource::Values(rows) => rows,
                other => panic!("expected InsertSource::Values, got {other:?}"),
            };
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0],
                Row::new(vec![
                    CommonValue::Bool(true),
                    CommonValue::Int32(-42),
                    CommonValue::Int64(9223372036854775807),
                    CommonValue::Float64(3.25),
                    CommonValue::String("quoted_str".into()),
                    CommonValue::String("double_quoted".into()),
                    CommonValue::Bytes(vec![1, 2, 3, 4]),
                    CommonValue::Bytes(vec![0xab, 0xcd]),
                    CommonValue::Timestamp(1700000000),
                ])
            );
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_bind_insert_select() {
    let catalog = make_test_catalog();

    // 1. Explicit column list with SELECT succeeds and maps the SELECT output to table columns.
    let bound = parse_and_bind(
        "INSERT INTO users (id, name, age, bio) SELECT id, name, age, bio FROM users",
        &catalog,
    )
    .expect("INSERT ... SELECT with explicit columns should bind");
    match bound {
        BoundStatement::Insert(insert) => {
            assert_eq!(insert.table, "users");
            match insert.source {
                InsertSource::Query {
                    query,
                    column_mapping,
                } => {
                    assert_eq!(column_mapping, vec![0, 1, 2, 3]);
                    assert!(
                        !query.output_columns.is_empty(),
                        "INSERT ... SELECT query should have bound output columns"
                    );
                }
                other => panic!("expected InsertSource::Query, got {other:?}"),
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }

    // 2. The SELECT output must match the four target users columns.
    let res = parse_and_bind(
        "INSERT INTO users (id, name, age, bio) SELECT id, name, age FROM users",
        &catalog,
    );
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected InvalidArgument for SELECT/INSERT arity mismatch, got {res:?}"
    );

    // 4. users.id is INT32, so a BIGINT SELECT expression cannot be inserted into it.
    let res = parse_and_bind(
        "INSERT INTO users (id, name, age, bio) \
         SELECT order_id, name, age, bio FROM orders, users",
        &catalog,
    );
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected InvalidArgument for BIGINT-to-INT INSERT ... SELECT mismatch, got {res:?}"
    );

    // 5. Nullable source columns still require an exact type match.
    let res = parse_and_bind(
        "INSERT INTO orders (tenant_id, order_id, amount, created_at) \
         SELECT id, age, amount, created_at FROM users, orders",
        &catalog,
    );
    assert!(
        matches!(
            res,
            Err(HtapError::InvalidArgument(ref message))
                if message.contains("expected bigint") && message.contains("found int")
        ),
        "expected nullable INT-to-BIGINT mismatch, got {res:?}"
    );

    let res = parse_and_bind(
        "INSERT INTO orders (tenant_id, order_id, amount, created_at) \
         SELECT id, order_id, amount, bio FROM users, orders",
        &catalog,
    );
    assert!(
        matches!(
            res,
            Err(HtapError::InvalidArgument(ref message))
                if message.contains("expected timestamp") && message.contains("found varchar")
        ),
        "expected nullable VARCHAR-to-TIMESTAMP mismatch, got {res:?}"
    );

    // 6. NULL is valid for a nullable target column.
    let bound = parse_and_bind(
        "INSERT INTO users (id, name, age, bio) \
         SELECT id, name, NULL, bio FROM users",
        &catalog,
    )
    .expect("INSERT ... SELECT NULL into nullable age should bind");
    match bound {
        BoundStatement::Insert(insert) => match insert.source {
            InsertSource::Query {
                query,
                column_mapping,
            } => {
                assert_eq!(column_mapping, vec![0, 1, 2, 3]);
                assert!(
                    !query.output_columns.is_empty(),
                    "INSERT ... SELECT query should have bound output columns"
                );
            }
            other => panic!("expected InsertSource::Query, got {other:?}"),
        },
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_bind_select_valid() {
    let catalog = make_test_catalog();

    // 1. SELECT *
    let sql = "SELECT * FROM users WHERE id = 1";
    let bound = parse_and_bind(sql, &catalog).expect("valid SELECT *");
    match bound {
        BoundStatement::Select(select) => {
            assert_eq!(select.table, "users");
            assert_eq!(select.projection, vec![0, 1, 2, 3]);
            assert_eq!(select.key, vec![CommonValue::Int32(1)]);
        }
        other => panic!("expected PointSelect, got {other:?}"),
    }

    // 2. Specific projection order preserved
    let sql = "SELECT bio, id, name FROM users WHERE id = 10";
    let bound = parse_and_bind(sql, &catalog).expect("valid projection order SELECT");
    match bound {
        BoundStatement::Select(select) => {
            assert_eq!(select.table, "users");
            assert_eq!(select.projection, vec![3, 0, 1]);
            assert_eq!(select.key, vec![CommonValue::Int32(10)]);
        }
        other => panic!("expected PointSelect, got {other:?}"),
    }

    // 3. Composite PK predicate in reverse order: key emitted in catalog primary_key order
    let sql = "SELECT amount, order_id FROM orders WHERE order_id = 1000 AND tenant_id = 42";
    let bound = parse_and_bind(sql, &catalog).expect("valid composite PK reverse order SELECT");
    match bound {
        BoundStatement::Select(select) => {
            assert_eq!(select.table, "orders");
            assert_eq!(select.projection, vec![2, 1]);
            assert_eq!(
                select.key,
                vec![CommonValue::Int32(42), CommonValue::Int64(1000)]
            );
        }
        other => panic!("expected PointSelect, got {other:?}"),
    }
}

#[test]
fn test_bind_delete_valid() {
    use htap_sql::DeleteTarget;

    let catalog = make_test_catalog();

    // 1. Single-column PK DELETE
    let sql = "DELETE FROM users WHERE id = 99";
    let bound = parse_and_bind(sql, &catalog).expect("valid single PK DELETE");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.table, "users");
            match delete.target {
                DeleteTarget::PrimaryKey(key_values) => {
                    assert_eq!(key_values, vec![CommonValue::Int32(99)]);
                }
                other => panic!("expected DeleteTarget::PrimaryKey, got {other:?}"),
            }
        }
        other => panic!("expected DeleteByPrimaryKey, got {other:?}"),
    }

    // 2. Composite PK DELETE with reverse predicate order: emits in catalog primary_key order
    let sql = "DELETE FROM orders WHERE order_id = 500 AND tenant_id = 12";
    let bound = parse_and_bind(sql, &catalog).expect("valid composite PK reverse order DELETE");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.table, "orders");
            match delete.target {
                DeleteTarget::PrimaryKey(key_values) => {
                    assert_eq!(
                        key_values,
                        vec![CommonValue::Int32(12), CommonValue::Int64(500)]
                    );
                }
                other => panic!("expected DeleteTarget::PrimaryKey, got {other:?}"),
            }
        }
        other => panic!("expected DeleteByPrimaryKey, got {other:?}"),
    }
}

#[test]
fn test_bind_delete_general_filters() {
    use htap_sql::DeleteTarget;

    let catalog = make_test_catalog();

    let bound = parse_and_bind("DELETE FROM users WHERE id = 1", &catalog)
        .expect("DELETE with complete primary key should bind");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.table, "users");
            assert_eq!(
                delete.target,
                DeleteTarget::PrimaryKey(vec![CommonValue::Int32(1)])
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let bound = parse_and_bind("DELETE FROM users WHERE 1 = id", &catalog)
        .expect("DELETE with reversed equality should bind");
    match bound {
        BoundStatement::Delete(delete) => {
            assert!(matches!(delete.target, DeleteTarget::Filter(Some(_))));
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let bound = parse_and_bind("DELETE FROM users WHERE age > 30", &catalog)
        .expect("DELETE with non-primary-key filter should bind");
    match bound {
        BoundStatement::Delete(delete) => {
            assert!(matches!(delete.target, DeleteTarget::Filter(Some(_))));
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let bound =
        parse_and_bind("DELETE FROM users", &catalog).expect("DELETE without WHERE should bind");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.target, DeleteTarget::Filter(None));
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let bound = parse_and_bind("DELETE FROM users u WHERE id = 1", &catalog)
        .expect("DELETE with table alias should bind");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(
                delete.target,
                DeleteTarget::PrimaryKey(vec![CommonValue::Int32(1)])
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    assert!(matches!(
        parse_and_bind("DELETE FROM users WHERE COUNT(*) > 5", &catalog),
        Err(HtapError::InvalidArgument(_))
    ));
}

#[test]
fn test_negative_create_table() {
    let catalog = CatalogSnapshot::empty();

    let invalid_arg_cases = [
        // No PK
        "CREATE TABLE t (id INT, val TEXT)",
        // Duplicate column in composite PK
        "CREATE TABLE t (id INT, val TEXT, PRIMARY KEY (id, id))",
        // Multiple PK declarations (multiple column PKs)
        "CREATE TABLE t (id1 INT PRIMARY KEY, id2 INT PRIMARY KEY)",
        // Multiple PK declarations (column PK and table PK)
        "CREATE TABLE t (id INT PRIMARY KEY, val TEXT, PRIMARY KEY (id))",
        // Unknown column in composite PK
        "CREATE TABLE t (id INT, PRIMARY KEY (non_existent))",
        // PK declared NULL
        "CREATE TABLE t (id INT NULL PRIMARY KEY, val TEXT)",
        "CREATE TABLE t (id INT NULL, val TEXT, PRIMARY KEY (id))",
        // Empty columns
        "CREATE TABLE t ()",
    ];

    for case in invalid_arg_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }

    let unsupported_cases = [
        // IF NOT EXISTS
        "CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY)",
        // TEMPORARY
        "CREATE TEMPORARY TABLE t (id INT PRIMARY KEY)",
        // CTAS
        "CREATE TABLE t AS SELECT 1",
        // Engine / table options
        "CREATE TABLE t (id INT PRIMARY KEY) ENGINE=InnoDB",
        // Default clause
        "CREATE TABLE t (id INT PRIMARY KEY, val INT DEFAULT 0)",
        // Unique clause
        "CREATE TABLE t (id INT PRIMARY KEY, val INT UNIQUE)",
        // Check clause
        "CREATE TABLE t (id INT PRIMARY KEY, val INT CHECK (val > 0))",
        // Foreign key
        "CREATE TABLE t (id INT PRIMARY KEY, fid INT REFERENCES other(id))",
        // Unsupported types
        "CREATE TABLE t (id INT PRIMARY KEY, d DATE)",
        "CREATE TABLE t (id INT PRIMARY KEY, g GEOMETRY)",
        "CREATE TABLE t (id INT PRIMARY KEY, dec DECIMAL(10, 2))",
        // Qualified table name
        "CREATE TABLE db.t (id INT PRIMARY KEY)",
        // Table PK USING index type
        "CREATE TABLE t (id INT, PRIMARY KEY (id) USING BTREE)",
    ];

    for case in unsupported_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for {case:?}, got {res:?}"
        );
    }

    // Test partition_by rejection directly in binder
    let mut stmt = match parse_one("CREATE TABLE t (id INT PRIMARY KEY)").unwrap() {
        Statement::CreateTable(ct) => ct,
        _ => unreachable!(),
    };
    stmt.partition_by = Some(Box::new(Expr::Identifier(Ident::new("id"))));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::Unsupported(_))));

    // Test table PK with index_type rejection directly in binder
    let mut stmt = match parse_one("CREATE TABLE t (id INT, PRIMARY KEY (id))").unwrap() {
        Statement::CreateTable(ct) => ct,
        _ => unreachable!(),
    };
    if let TableConstraint::PrimaryKey(ref mut pk) = stmt.constraints[0] {
        pk.index_type = Some(sqlparser::ast::IndexType::BTree);
    }
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::Unsupported(_))));
}

#[test]
fn test_bind_create_table_primary_key_field() {
    let catalog = CatalogSnapshot::empty();

    // 1. Positive: Statement::CreateTable with stmt.primary_key set (e.g. AST)
    let mut stmt = match parse_one("CREATE TABLE t (a INT, b BIGINT)").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::Tuple(vec![
        Expr::Identifier(Ident::new("a")),
        Expr::Identifier(Ident::new("b")),
    ])));
    let bound =
        bind(&Statement::CreateTable(stmt), &catalog).expect("valid table PK via stmt.primary_key");
    match bound {
        BoundStatement::CreateTable(create) => {
            assert_eq!(create.name, "t");
            assert_eq!(create.primary_key, vec![0, 1]);
            assert!(create.schema.column(0).unwrap().primary_key);
            assert!(!create.schema.column(0).unwrap().nullable);
            assert!(create.schema.column(1).unwrap().primary_key);
            assert!(!create.schema.column(1).unwrap().nullable);
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }

    // 2. Negative: multiple PK declarations (column PK + stmt.primary_key)
    let mut stmt = match parse_one("CREATE TABLE t (a INT PRIMARY KEY, b BIGINT)").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::Identifier(Ident::new("b"))));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));

    // 3. Negative: multiple PK declarations (table constraint PK + stmt.primary_key)
    let mut stmt = match parse_one("CREATE TABLE t (a INT, b BIGINT, PRIMARY KEY (a))").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::Identifier(Ident::new("b"))));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));

    // 4. Negative: duplicate column in stmt.primary_key
    let mut stmt = match parse_one("CREATE TABLE t (a INT, b BIGINT)").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::Tuple(vec![
        Expr::Identifier(Ident::new("a")),
        Expr::Identifier(Ident::new("a")),
    ])));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));

    // 5. Negative: unknown column in stmt.primary_key
    let mut stmt = match parse_one("CREATE TABLE t (a INT, b BIGINT)").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::Identifier(Ident::new("non_existent"))));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));

    // 6. Negative: invalid non-identifier expression in stmt.primary_key
    let mut stmt = match parse_one("CREATE TABLE t (a INT, b BIGINT)").unwrap() {
        Statement::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    stmt.primary_key = Some(Box::new(Expr::BinaryOp {
        left: Box::new(Expr::Identifier(Ident::new("a"))),
        op: BinaryOperator::Plus,
        right: Box::new(Expr::Identifier(Ident::new("b"))),
    }));
    let res = bind(&Statement::CreateTable(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::InvalidArgument(_))));
}

#[test]
fn test_negative_insert() {
    let catalog = make_test_catalog();

    // Missing table lookup -> NotFound
    let res = parse_and_bind(
        "INSERT INTO missing_table (id, name) VALUES (1, 'alice')",
        &catalog,
    );
    assert!(
        matches!(res, Err(HtapError::NotFound(_))),
        "expected NotFound, got {res:?}"
    );

    let invalid_arg_cases = [
        // Column not in table schema
        "INSERT INTO users (id, name, age, bio, non_existent) VALUES (1, 'a', 20, 'b', 'c')",
        // Omitted columns (must provide all schema columns)
        "INSERT INTO users (id, name) VALUES (1, 'alice')",
        // Duplicate column in column list
        "INSERT INTO users (id, id, name, age) VALUES (1, 2, 'alice', 20)",
        // Missing explicit column list
        "INSERT INTO users VALUES (1, 'alice', 20, 'b')",
        // Arity mismatch: fewer values than columns
        "INSERT INTO users (id, name, age, bio) VALUES (1, 'alice', 20)",
        // Arity mismatch: more values than columns
        "INSERT INTO users (id, name, age, bio) VALUES (1, 'alice', 20, 'bio', 'extra')",
        // Multi-row arity mismatch on second row
        "INSERT INTO users (id, name, age, bio) VALUES (1, 'alice', 20, 'b'), (2, 'bob')",
        // Expressions in VALUES
        "INSERT INTO users (id, name, age, bio) VALUES (1 + 1, 'alice', 20, 'b')",
        "INSERT INTO users (id, name, age, bio) VALUES (1, LOWER('alice'), 20, 'b')",
        // Placeholders in VALUES
        "INSERT INTO users (id, name, age, bio) VALUES (?, 'alice', 20, 'b')",
        // Type mismatch: string to int
        "INSERT INTO users (id, name, age, bio) VALUES ('not_an_int', 'alice', 20, 'b')",
        // Type mismatch: number to string
        "INSERT INTO users (id, name, age, bio) VALUES (1, 12345, 20, 'b')",
        // Fractional to integer
        "INSERT INTO users (id, name, age, bio) VALUES (1.5, 'alice', 20, 'b')",
        // Exponent to integer
        "INSERT INTO users (id, name, age, bio) VALUES (1e3, 'alice', 20, 'b')",
        // Integer with suffix
        "INSERT INTO users (id, name, age, bio) VALUES (1L, 'alice', 20, 'b')",
        // Integer overflow for INT32
        "INSERT INTO users (id, name, age, bio) VALUES (3000000000, 'alice', 20, 'b')",
        // NULL in primary key
        "INSERT INTO users (id, name, age, bio) VALUES (NULL, 'alice', 20, 'b')",
        // NULL in non-nullable column (name is NOT NULL)
        "INSERT INTO users (id, name, age, bio) VALUES (1, NULL, 20, 'b')",
        // Odd length hex bytes
        "INSERT INTO bytes_table (id, data) VALUES (1, X'123')",
        // Invalid hex character
        "INSERT INTO bytes_table (id, data) VALUES (1, X'12ZZ')",
    ];

    for case in invalid_arg_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }

    let unsupported_cases = [
        // INSERT SET
        "INSERT INTO users SET id = 1, name = 'a', age = 20, bio = 'b'",
        // INSERT IGNORE
        "INSERT IGNORE INTO users (id, name, age, bio) VALUES (1, 'a', 20, 'b')",
        // REPLACE INTO
        "REPLACE INTO users (id, name, age, bio) VALUES (1, 'a', 20, 'b')",
        // RETURNING
        "INSERT INTO users (id, name, age, bio) VALUES (1, 'a', 20, 'b') RETURNING id",
        // Qualified table name
        "INSERT INTO db.users (id, name, age, bio) VALUES (1, 'a', 20, 'b')",
    ];

    for case in unsupported_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for {case:?}, got {res:?}"
        );
    }

    // Test table alias rejection directly in binder
    let mut stmt =
        match parse_one("INSERT INTO users (id, name, age, bio) VALUES (1, 'a', 20, 'b')").unwrap()
        {
            Statement::Insert(ins) => ins,
            _ => unreachable!(),
        };
    stmt.table_alias = Some(sqlparser::ast::TableAliasWithoutColumns {
        explicit: true,
        alias: Ident::new("u"),
    });
    let res = bind(&Statement::Insert(stmt), &catalog);
    assert!(matches!(res, Err(HtapError::Unsupported(_))));
}

#[test]
fn test_negative_select_and_delete() {
    let catalog = make_test_catalog();

    // Missing table lookup -> NotFound
    let res = parse_and_bind("SELECT * FROM missing WHERE id = 1", &catalog);
    assert!(
        matches!(res, Err(HtapError::NotFound(_))),
        "expected NotFound, got {res:?}"
    );
    let res = parse_and_bind("DELETE FROM missing WHERE id = 1", &catalog);
    assert!(
        matches!(res, Err(HtapError::NotFound(_))),
        "expected NotFound, got {res:?}"
    );

    let invalid_arg_cases = [
        // Unknown column in projection
        "SELECT nonexistent FROM users WHERE id = 1",
        // Unknown column in WHERE
        "SELECT * FROM users WHERE nonexistent = 1",
        // Duplicate PK predicate
        "SELECT * FROM users WHERE id = 1 AND id = 2",
        // NULL literal in WHERE
        "SELECT * FROM users WHERE id = NULL",
        // Placeholder in WHERE value
        "SELECT * FROM users WHERE id = ?",
        // Type mismatch in WHERE value
        "SELECT * FROM users WHERE id = 'not_an_int'",
        // Fractional to integer in WHERE value
        "SELECT * FROM users WHERE id = 1.5",
        // Wildcard combined with column
        "SELECT *, id FROM users WHERE id = 1",
    ];

    for case in invalid_arg_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }

    let unsupported_cases = [
        // DELETE qualified names / ORDER BY / LIMIT
        "DELETE FROM db.users WHERE id = 1",
        "DELETE FROM users WHERE id = 1 ORDER BY id",
        "DELETE FROM users WHERE id = 1 LIMIT 1",
        // Qualified (schema.table) names
        "SELECT * FROM db.users WHERE id = 1",
        // GROUP BY ALL
        "SELECT * FROM users GROUP BY ALL",
        // Non-partition ALTER
        "ALTER TABLE users ADD COLUMN foo INT",
    ];

    for case in unsupported_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for {case:?}, got {res:?}"
        );
    }

    // Formerly rejected shapes now bind through the general query path (never as a
    // point read, so no clause is silently dropped).
    let general_cases = [
        "SELECT * FROM users WHERE id = 1 OR id = 2",
        "SELECT * FROM users WHERE 1 = id",
        "SELECT * FROM users WHERE id = 1 + 1",
        "SELECT * FROM users JOIN orders ON users.id = orders.tenant_id WHERE users.id = 1",
        "SELECT * FROM users, orders WHERE users.id = 1",
        "SELECT * FROM users u WHERE id = 1",
        "SELECT * FROM users AS u WHERE id = 1",
        "SELECT id AS my_id FROM users WHERE id = 1",
        "SELECT users.id FROM users WHERE id = 1",
        "SELECT * FROM users WHERE users.id = 1",
        "SELECT * FROM users WHERE id = 1 ORDER BY id + 1",
        "SELECT * FROM users WHERE id = 1 ORDER BY users.id",
        "SELECT id, COUNT(*) FROM users GROUP BY id ORDER BY COUNT(*)",
        "SELECT * FROM users WHERE id = 1 LIMIT 1",
        "SELECT * FROM users WHERE id = 1 HAVING id = 1",
        "SELECT DISTINCT id FROM users WHERE id = 1",
        "WITH cte AS (SELECT 1) SELECT * FROM users WHERE id = 1",
        "SELECT * FROM users WHERE id = 1 UNION SELECT * FROM users WHERE id = 2",
    ];
    for case in general_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Ok(BoundStatement::Query(_))),
            "expected general Query for {case:?}, got {res:?}"
        );
    }
    assert!(matches!(
        parse_and_bind("UPDATE users SET name = 'bob' WHERE id = 1", &catalog),
        Ok(BoundStatement::Update(_))
    ));
    assert!(matches!(
        parse_and_bind("DROP TABLE users", &catalog),
        Ok(BoundStatement::DropTable(_))
    ));
}

#[test]
fn test_bind_analytic_select_valid() {
    use htap_sql::{AggregateFunction, AnalyticExpr, AnalyticFilter, ComparisonOp};

    let catalog = make_test_catalog();

    // 1. Full table scan with SELECT *
    let sql = "SELECT * FROM users";
    let bound = parse_and_bind(sql, &catalog).expect("valid full table scan");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.table, "users");
            assert_eq!(select.projection.len(), 4);
            assert!(select.filter.is_none());
            assert!(select.group_by.is_empty());
            assert_eq!(select.output_schema.len(), 4);
            assert_eq!(select.output_schema.column(0).unwrap().name, "id");
            assert_eq!(
                select.output_schema.column(0).unwrap().data_type,
                CommonDataType::Int32
            );
            assert!(!select.output_schema.column(0).unwrap().nullable);
            assert_eq!(select.output_schema.column(2).unwrap().name, "age");
            assert!(select.output_schema.column(2).unwrap().nullable);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 2. Specific column projection with exact metadata
    let sql = "SELECT bio, name, age FROM users";
    let bound = parse_and_bind(sql, &catalog).expect("valid specific column projection");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.table, "users");
            assert_eq!(select.projection.len(), 3);
            assert_eq!(
                select.projection,
                vec![
                    AnalyticExpr::Column {
                        index: 3,
                        name: "bio".into(),
                        data_type: CommonDataType::String,
                        nullable: true,
                    },
                    AnalyticExpr::Column {
                        index: 1,
                        name: "name".into(),
                        data_type: CommonDataType::String,
                        nullable: false,
                    },
                    AnalyticExpr::Column {
                        index: 2,
                        name: "age".into(),
                        data_type: CommonDataType::Int32,
                        nullable: true,
                    },
                ]
            );
            assert_eq!(
                select
                    .output_schema
                    .columns()
                    .iter()
                    .map(|c| (c.name.as_str(), c.data_type, c.nullable))
                    .collect::<Vec<_>>(),
                vec![
                    ("bio", CommonDataType::String, true),
                    ("name", CommonDataType::String, false),
                    ("age", CommonDataType::Int32, true),
                ]
            );
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 3. Filter tree with Eq, NotEq, Lt, Lte, Gt, Gte
    let sql = "SELECT id FROM users WHERE age >= 18 AND age <= 65 AND id != 99 AND name = 'alice' AND age > 20 AND id < 100";
    let bound = parse_and_bind(sql, &catalog).expect("valid range and comparison filter");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            let filter = select.filter.expect("filter should be present");
            let leaves = filter.leaves();
            assert_eq!(leaves.len(), 6);
            assert_eq!(
                leaves[0],
                &AnalyticFilter::Comparison {
                    column: 2,
                    op: ComparisonOp::Gte,
                    value: CommonValue::Int32(18),
                }
            );
            assert_eq!(
                leaves[1],
                &AnalyticFilter::Comparison {
                    column: 2,
                    op: ComparisonOp::Lte,
                    value: CommonValue::Int32(65),
                }
            );
            assert_eq!(
                leaves[2],
                &AnalyticFilter::Comparison {
                    column: 0,
                    op: ComparisonOp::NotEq,
                    value: CommonValue::Int32(99),
                }
            );
            assert_eq!(
                leaves[3],
                &AnalyticFilter::Comparison {
                    column: 1,
                    op: ComparisonOp::Eq,
                    value: CommonValue::String("alice".into()),
                }
            );
            assert_eq!(
                leaves[4],
                &AnalyticFilter::Comparison {
                    column: 2,
                    op: ComparisonOp::Gt,
                    value: CommonValue::Int32(20),
                }
            );
            assert_eq!(
                leaves[5],
                &AnalyticFilter::Comparison {
                    column: 0,
                    op: ComparisonOp::Lt,
                    value: CommonValue::Int32(100),
                }
            );
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 4. Filter with IsNull and IsNotNull
    let sql = "SELECT * FROM users WHERE bio IS NULL AND age IS NOT NULL";
    let bound = parse_and_bind(sql, &catalog).expect("valid IS NULL / IS NOT NULL filter");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            let filter = select.filter.expect("filter present");
            let leaves = filter.leaves();
            assert_eq!(leaves.len(), 2);
            assert_eq!(leaves[0], &AnalyticFilter::IsNull { column: 3 });
            assert_eq!(leaves[1], &AnalyticFilter::IsNotNull { column: 2 });
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 5. Approved aggregates without GROUP BY (scalar aggregation)
    let sql = "SELECT COUNT(*), COUNT(bio), SUM(age), MIN(age), MAX(age) FROM users";
    let bound = parse_and_bind(sql, &catalog).expect("valid scalar aggregates");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.projection.len(), 5);
            assert_eq!(
                select.projection[0],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::CountStar,
                    column_index: None,
                    name: "COUNT(*)".into(),
                    data_type: CommonDataType::Int64,
                    nullable: false,
                }
            );
            assert_eq!(
                select.projection[1],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::Count,
                    column_index: Some(3),
                    name: "COUNT(bio)".into(),
                    data_type: CommonDataType::Int64,
                    nullable: false,
                }
            );
            assert_eq!(
                select.projection[2],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::Sum,
                    column_index: Some(2),
                    name: "SUM(age)".into(),
                    data_type: CommonDataType::Int64,
                    nullable: true,
                }
            );
            assert_eq!(
                select.projection[3],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::Min,
                    column_index: Some(2),
                    name: "MIN(age)".into(),
                    data_type: CommonDataType::Int32,
                    nullable: true,
                }
            );
            assert_eq!(
                select.projection[4],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::Max,
                    column_index: Some(2),
                    name: "MAX(age)".into(),
                    data_type: CommonDataType::Int32,
                    nullable: true,
                }
            );
            assert_eq!(
                select
                    .output_schema
                    .columns()
                    .iter()
                    .map(|c| (c.name.as_str(), c.data_type, c.nullable))
                    .collect::<Vec<_>>(),
                vec![
                    ("COUNT(*)", CommonDataType::Int64, false),
                    ("COUNT(bio)", CommonDataType::Int64, false),
                    ("SUM(age)", CommonDataType::Int64, true),
                    ("MIN(age)", CommonDataType::Int32, true),
                    ("MAX(age)", CommonDataType::Int32, true),
                ]
            );
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 6. SUM on Float64 column
    let sql = "SELECT SUM(amount) FROM orders";
    let bound = parse_and_bind(sql, &catalog).expect("valid Float64 SUM");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(
                select.projection[0],
                AnalyticExpr::Aggregate {
                    function: AggregateFunction::Sum,
                    column_index: Some(2),
                    name: "SUM(amount)".into(),
                    data_type: CommonDataType::Float64,
                    nullable: true,
                }
            );
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 7. Aggregates with GROUP BY
    let sql = "SELECT tenant_id, COUNT(*), SUM(amount) FROM orders GROUP BY tenant_id";
    let bound = parse_and_bind(sql, &catalog).expect("valid GROUP BY aggregation");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.group_by, vec![0]);
            assert_eq!(select.projection.len(), 3);
            assert_eq!(
                select.output_schema.columns()[0],
                CommonColumnDef {
                    name: "tenant_id".into(),
                    data_type: CommonDataType::Int32,
                    nullable: false,
                    primary_key: false,
                }
            );
            assert_eq!(
                select.output_schema.columns()[1],
                CommonColumnDef {
                    name: "COUNT(*)".into(),
                    data_type: CommonDataType::Int64,
                    nullable: false,
                    primary_key: false,
                }
            );
            assert_eq!(
                select.output_schema.columns()[2],
                CommonColumnDef {
                    name: "SUM(amount)".into(),
                    data_type: CommonDataType::Float64,
                    nullable: true,
                    primary_key: false,
                }
            );
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 8. Multi-column GROUP BY
    let sql = "SELECT tenant_id, order_id, COUNT(*) FROM orders GROUP BY order_id, tenant_id";
    let bound = parse_and_bind(sql, &catalog).expect("valid multi-column GROUP BY");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.group_by, vec![1, 0]);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 9. GROUP BY without projecting grouping column
    let sql = "SELECT COUNT(*) FROM orders GROUP BY tenant_id";
    let bound = parse_and_bind(sql, &catalog).expect("valid GROUP BY without projecting group col");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.group_by, vec![0]);
            assert_eq!(select.projection.len(), 1);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 10. Complete PK filter with aggregate binds as AnalyticSelect (not PointSelect)
    let sql = "SELECT COUNT(*) FROM users WHERE id = 1";
    let bound = parse_and_bind(sql, &catalog).expect("valid aggregate with PK filter");
    assert!(matches!(bound, BoundStatement::AnalyticSelect(_)));

    // 11. Partial PK filter binds as AnalyticSelect
    let sql = "SELECT amount FROM orders WHERE tenant_id = 42";
    let bound = parse_and_bind(sql, &catalog).expect("valid partial PK query");
    assert!(matches!(bound, BoundStatement::AnalyticSelect(_)));

    // 12. Non-PK filter with complete PK still binds as AnalyticSelect
    let sql = "SELECT * FROM users WHERE id = 1 AND name = 'alice'";
    let bound = parse_and_bind(sql, &catalog).expect("valid PK + non-PK filter query");
    assert!(matches!(bound, BoundStatement::AnalyticSelect(_)));

    // 13. Narrow ORDER BY defaults to ASC, NULLS FIRST
    let sql = "SELECT name, age FROM users ORDER BY age";
    let bound = parse_and_bind(sql, &catalog).expect("valid ORDER BY ASC default");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.order_by.len(), 1);
            assert_eq!(select.order_by[0].column, 2); // age is column index 2
            assert!(select.order_by[0].asc);
            assert!(select.order_by[0].nulls_first);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 14. Narrow ORDER BY DESC defaults to NULLS LAST
    let sql = "SELECT name, age FROM users ORDER BY age DESC";
    let bound = parse_and_bind(sql, &catalog).expect("valid ORDER BY DESC default");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.order_by.len(), 1);
            assert_eq!(select.order_by[0].column, 2);
            assert!(!select.order_by[0].asc);
            assert!(!select.order_by[0].nulls_first);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 15. Explicit NULLS LAST with ASC and NULLS FIRST with DESC
    let sql = "SELECT name, age FROM users ORDER BY age ASC NULLS LAST, id DESC NULLS FIRST";
    let bound = parse_and_bind(sql, &catalog).expect("valid explicit NULLS ordering");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.order_by.len(), 2);
            assert_eq!(select.order_by[0].column, 2);
            assert!(select.order_by[0].asc);
            assert!(!select.order_by[0].nulls_first);
            assert_eq!(select.order_by[1].column, 0);
            assert!(!select.order_by[1].asc);
            assert!(select.order_by[1].nulls_first);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }

    // 16. ORDER BY with GROUP BY
    let sql = "SELECT age, COUNT(*) FROM users GROUP BY age ORDER BY age DESC";
    let bound = parse_and_bind(sql, &catalog).expect("valid grouped ORDER BY");
    match bound {
        BoundStatement::AnalyticSelect(select) => {
            assert_eq!(select.group_by, vec![2]);
            assert_eq!(select.order_by.len(), 1);
            assert_eq!(select.order_by[0].column, 2);
            assert!(!select.order_by[0].asc);
        }
        other => panic!("expected AnalyticSelect, got {other:?}"),
    }
}

#[test]
fn test_bind_analytic_select_negative() {
    let catalog = make_test_catalog();

    let invalid_cases = [
        // Non-aggregate projected column not in GROUP BY (when aggregate present)
        "SELECT id, name, COUNT(*) FROM users GROUP BY id",
        // Non-aggregate projected column without GROUP BY when aggregate present
        "SELECT id, COUNT(*) FROM users",
        // Non-aggregate projected column not in GROUP BY (without aggregates)
        "SELECT id, name FROM users GROUP BY id",
        // Unknown column in GROUP BY
        "SELECT COUNT(*) FROM users GROUP BY nonexistent",
        // Duplicate column in GROUP BY
        "SELECT COUNT(*) FROM users GROUP BY id, id",
        // Duplicate output column names
        "SELECT id, id FROM users",
        // SUM on non-numeric column (String)
        "SELECT SUM(name) FROM users",
        // SUM on non-numeric column (Bytes)
        "SELECT SUM(data) FROM bytes_table",
        // Aggregate with multiple arguments
        "SELECT COUNT(id, name) FROM users",
        // Unknown column in ORDER BY
        "SELECT * FROM users ORDER BY nonexistent",
        // Column not in GROUP BY in ORDER BY
        "SELECT age, COUNT(*) FROM users GROUP BY age ORDER BY id",
    ];

    for case in invalid_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }

    // Shapes outside the narrow analytic slice bind through the general query path.
    let general_cases = [
        "SELECT COUNT(*) FROM users GROUP BY id + 1",
        "SELECT * FROM users WHERE NOT (id = 1)",
        "SELECT * FROM users WHERE id = age",
        "SELECT * FROM users WHERE 10 < age",
        "SELECT * FROM users WHERE 'alice' = name",
        "SELECT AVG(age) FROM users",
        "SELECT AVG(amount) FROM orders",
        "SELECT COUNT(DISTINCT id) FROM users",
        "SELECT SUM(DISTINCT amount) FROM orders",
        "SELECT id AS user_id FROM users",
        "SELECT COUNT(*) AS total FROM users",
        "SELECT * FROM users ORDER BY id + 1",
        "SELECT * FROM users ORDER BY users.id",
        "SELECT id, COUNT(*) FROM users GROUP BY id ORDER BY COUNT(*)",
    ];

    for case in general_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Ok(BoundStatement::Query(_))),
            "expected general Query for {case:?}, got {res:?}"
        );
    }
}

fn make_partitioned_test_catalog() -> CatalogSnapshot {
    use htap_catalog::*;
    let schema = Schema::new(vec![
        CommonColumnDef {
            name: "id".to_string(),
            data_type: CommonDataType::Int64,
            nullable: false,
            primary_key: true,
        },
        CommonColumnDef {
            name: "val".to_string(),
            data_type: CommonDataType::Int64,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let p0 = PartitionDescriptor::new(
        PartitionId::new(1),
        TableId(1),
        "p0",
        StorageDescriptor::Row,
        vec![],
        1,
    )
    .with_range(RangeBound::new_opt(None, Some(CommonValue::Int64(10))));
    let p1 = PartitionDescriptor::new(
        PartitionId::new(2),
        TableId(1),
        "p1",
        StorageDescriptor::Row,
        vec![],
        1,
    )
    .with_range(RangeBound::new_opt(
        Some(CommonValue::Int64(10)),
        Some(CommonValue::Int64(20)),
    ));
    let p2 = PartitionDescriptor::new(
        PartitionId::new(3),
        TableId(1),
        "p2",
        StorageDescriptor::Row,
        vec![],
        1,
    )
    .with_range(RangeBound::new_opt(
        Some(CommonValue::Int64(20)),
        Some(CommonValue::Int64(30)),
    ));
    let range_table = TableDescriptor::new(
        TableId(1),
        "t_range",
        schema.clone(),
        vec![0],
        vec![
            PartitionId::new(1),
            PartitionId::new(2),
            PartitionId::new(3),
        ],
        1,
    )
    .with_partitioning(PartitioningDescriptor::new(0, PartitioningMethod::Range));

    let lp0 = PartitionDescriptor::new(
        PartitionId::new(4),
        TableId(2),
        "p0",
        StorageDescriptor::Row,
        vec![],
        1,
    )
    .with_list_values(vec![CommonValue::Int64(1), CommonValue::Int64(2)]);
    let lp1 = PartitionDescriptor::new(
        PartitionId::new(5),
        TableId(2),
        "p1",
        StorageDescriptor::Row,
        vec![],
        1,
    )
    .with_list_values(vec![CommonValue::Int64(3), CommonValue::Int64(4)]);
    let list_table = TableDescriptor::new(
        TableId(2),
        "t_list",
        schema.clone(),
        vec![0],
        vec![PartitionId::new(4), PartitionId::new(5)],
        1,
    )
    .with_partitioning(PartitioningDescriptor::new(0, PartitioningMethod::List));

    let unpart_table = TableDescriptor::new(TableId(3), "t_unpart", schema, vec![0], vec![], 1);

    CatalogSnapshot::new(
        1,
        vec![range_table, list_table, unpart_table],
        vec![p0, p1, p2, lp0, lp1],
        vec![],
        vec![],
    )
}

#[test]
fn test_mysql_alter_partition_parsed_and_bound() {
    let catalog = make_partitioned_test_catalog();

    // 1. ADD PARTITION with LESS THAN bound
    let sql_add_range = "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40))";
    let stmt = parse_one(sql_add_range).expect("should parse ADD PARTITION range");
    let bound = bind(&stmt, &catalog).expect("should bind ADD PARTITION range");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_range");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Add { partitions } => {
                    assert_eq!(partitions.len(), 1);
                    assert_eq!(partitions[0].name(), "p3");
                    let r = partitions[0].range().expect("range partition");
                    assert_eq!(r.lower_opt, Some(CommonValue::Int64(30)));
                    assert_eq!(r.upper_opt, Some(CommonValue::Int64(40)));
                }
                other => panic!("expected Alteration::Add, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 2. ADD PARTITION with MAXVALUE bound
    let sql_add_max =
        "ALTER TABLE t_range ADD PARTITION (PARTITION p_max VALUES LESS THAN MAXVALUE)";
    let stmt = parse_one(sql_add_max).expect("should parse ADD PARTITION maxvalue");
    let bound = bind(&stmt, &catalog).expect("should bind ADD PARTITION maxvalue");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_range");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Add { partitions } => {
                    assert_eq!(partitions.len(), 1);
                    assert_eq!(partitions[0].name(), "p_max");
                    let r = partitions[0].range().expect("range partition");
                    assert_eq!(r.lower_opt, Some(CommonValue::Int64(30)));
                    assert_eq!(r.upper_opt, None);
                }
                other => panic!("expected Alteration::Add, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 3. ADD PARTITION with LIST VALUES IN
    let sql_add_list = "ALTER TABLE t_list ADD PARTITION (PARTITION p2 VALUES IN (5, 6))";
    let stmt = parse_one(sql_add_list).expect("should parse ADD PARTITION list");
    let bound = bind(&stmt, &catalog).expect("should bind ADD PARTITION list");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_list");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Add { partitions } => {
                    assert_eq!(partitions.len(), 1);
                    assert_eq!(partitions[0].name(), "p2");
                    let l = partitions[0].list().expect("list partition");
                    assert_eq!(l.values, vec![CommonValue::Int64(5), CommonValue::Int64(6)]);
                }
                other => panic!("expected Alteration::Add, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 4. DROP PARTITION single
    let sql_drop_single = "ALTER TABLE t_range DROP PARTITION p2";
    let stmt = parse_one(sql_drop_single).expect("should parse DROP PARTITION single");
    let bound = bind(&stmt, &catalog).expect("should bind DROP PARTITION single");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_range");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Drop { partitions } => {
                    assert_eq!(partitions, vec!["p2".to_string()]);
                }
                other => panic!("expected Alteration::Drop, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 5. DROP PARTITION multiple
    let sql_drop_multi = "ALTER TABLE t_range DROP PARTITION p1, p2";
    let stmt = parse_one(sql_drop_multi).expect("should parse DROP PARTITION multi");
    let bound = bind(&stmt, &catalog).expect("should bind DROP PARTITION multi");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_range");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Drop { partitions } => {
                    assert_eq!(partitions, vec!["p1".to_string(), "p2".to_string()]);
                }
                other => panic!("expected Alteration::Drop, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 6. REORGANIZE PARTITION range
    let sql_reorg_range = "ALTER TABLE t_range REORGANIZE PARTITION p0, p1 INTO (PARTITION p01a VALUES LESS THAN (15), PARTITION p01b VALUES LESS THAN (20))";
    let stmt = parse_one(sql_reorg_range).expect("should parse REORGANIZE range");
    let bound = bind(&stmt, &catalog).expect("should bind REORGANIZE range");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_range");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Reorganize { sources, targets } => {
                    assert_eq!(sources, vec!["p0".to_string(), "p1".to_string()]);
                    assert_eq!(targets.len(), 2);
                    assert_eq!(targets[0].name(), "p01a");
                    assert_eq!(targets[0].range().unwrap().lower_opt, None);
                    assert_eq!(
                        targets[0].range().unwrap().upper_opt,
                        Some(CommonValue::Int64(15))
                    );
                    assert_eq!(targets[1].name(), "p01b");
                    assert_eq!(
                        targets[1].range().unwrap().lower_opt,
                        Some(CommonValue::Int64(15))
                    );
                    assert_eq!(
                        targets[1].range().unwrap().upper_opt,
                        Some(CommonValue::Int64(20))
                    );
                }
                other => panic!("expected Alteration::Reorganize, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }

    // 7. REORGANIZE PARTITION list
    let sql_reorg_list = "ALTER TABLE t_list REORGANIZE PARTITION p0, p1 INTO (PARTITION p01a VALUES IN (1, 3), PARTITION p01b VALUES IN (2, 4))";
    let stmt = parse_one(sql_reorg_list).expect("should parse REORGANIZE list");
    let bound = bind(&stmt, &catalog).expect("should bind REORGANIZE list");
    match bound {
        BoundStatement::AlterPartitions(alter) => {
            assert_eq!(alter.table, "t_list");
            match alter.alteration {
                htap_catalog::PartitionAlteration::Reorganize { sources, targets } => {
                    assert_eq!(sources, vec!["p0".to_string(), "p1".to_string()]);
                    assert_eq!(targets.len(), 2);
                    assert_eq!(targets[0].name(), "p01a");
                    assert_eq!(
                        targets[0].list().unwrap().values,
                        vec![CommonValue::Int64(1), CommonValue::Int64(3)]
                    );
                    assert_eq!(targets[1].name(), "p01b");
                    assert_eq!(
                        targets[1].list().unwrap().values,
                        vec![CommonValue::Int64(2), CommonValue::Int64(4)]
                    );
                }
                other => panic!("expected Alteration::Reorganize, got {other:?}"),
            }
        }
        other => panic!("expected BoundStatement::AlterPartitions, got {other:?}"),
    }
}

#[test]
fn test_mysql_alter_partition_negative() {
    let catalog = make_partitioned_test_catalog();

    // 1. IF EXISTS on ALTER TABLE
    let if_exists =
        "ALTER TABLE IF EXISTS t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40))";
    let stmt = parse_one(if_exists).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("IF EXISTS is not supported")),
        "expected InvalidArgument for IF EXISTS, got {res:?}"
    );

    // 2. IF NOT EXISTS on ADD PARTITION (rejected at parse time)
    let add_if_not_exists = [
        "ALTER TABLE t_range ADD PARTITION IF NOT EXISTS (PARTITION p3 VALUES LESS THAN (40))",
        "ALTER TABLE t_range ADD IF NOT EXISTS PARTITION (PARTITION p3 VALUES LESS THAN (40))",
    ];
    for case in add_if_not_exists {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected parse error for ADD IF NOT EXISTS, got {res:?}"
        );
    }

    // 3. IF EXISTS on DROP PARTITION (rejected at parse time)
    let drop_if_exists = [
        "ALTER TABLE t_range DROP PARTITION IF EXISTS p1",
        "ALTER TABLE t_range DROP IF EXISTS PARTITION p1",
    ];
    for case in drop_if_exists {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected parse error for DROP IF EXISTS, got {res:?}"
        );
    }

    // 4. Options: ENGINE, COMMENT, TABLESPACE
    let option_cases = [
        "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40) ENGINE = InnoDB)",
        "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40) COMMENT = 'test')",
        "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40) TABLESPACE = ts1)",
    ];
    for case in option_cases {
        let res = parse_one(case);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected parse error for partition options, got {res:?}"
        );
    }

    // 5. Subpartitioning
    let subpart =
        "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (40) (SUBPARTITION s0))";
    let res = parse_one(subpart);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected parse error for subpartition, got {res:?}"
    );

    // 6. Expressions in bound
    let expr_bound = "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (30 + 10))";
    let stmt = parse_one(expr_bound).expect("should parse expression");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected binder error for expr in bound, got {res:?}"
    );

    // 7. Multi-column in bounds
    let multi_col = "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (35, 40))";
    let res = parse_one(multi_col);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(_))),
        "expected parse error for multi-column bound, got {res:?}"
    );

    // 8. Unrelated ALTER operations
    let unrelated = [
        "ALTER TABLE t_range ADD COLUMN c INT",
        "ALTER TABLE t_range DROP COLUMN val",
        "ALTER TABLE t_range RENAME TO t_renamed",
    ];
    for case in unrelated {
        let stmt = parse_one(case).expect("should parse unrelated ALTER");
        let res = bind(&stmt, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for unrelated ALTER {case:?}, got {res:?}"
        );
    }

    // 9. Non-partitioned table
    let unpart = "ALTER TABLE t_unpart ADD PARTITION (PARTITION p1 VALUES LESS THAN (10))";
    let stmt = parse_one(unpart).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("is not partitioned")),
        "expected InvalidArgument for unpartitioned table, got {res:?}"
    );

    // 10. Non-existent table
    let ghost = "ALTER TABLE t_ghost ADD PARTITION (PARTITION p1 VALUES LESS THAN (10))";
    let stmt = parse_one(ghost).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::NotFound(_))),
        "expected NotFound for non-existent table, got {res:?}"
    );

    // 11. Range table with VALUES IN
    let range_with_in = "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES IN (1, 2))";
    let stmt = parse_one(range_with_in).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("must use VALUES LESS THAN")),
        "expected InvalidArgument for RANGE with VALUES IN, got {res:?}"
    );

    // 12. List table with VALUES LESS THAN
    let list_with_less_than =
        "ALTER TABLE t_list ADD PARTITION (PARTITION p2 VALUES LESS THAN (10))";
    let stmt = parse_one(list_with_less_than).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("must use VALUES IN")),
        "expected InvalidArgument for LIST with VALUES LESS THAN, got {res:?}"
    );

    // 13. Non-increasing range bound
    let non_increasing = "ALTER TABLE t_range ADD PARTITION (PARTITION p3 VALUES LESS THAN (25))";
    let stmt = parse_one(non_increasing).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("strictly increasing")),
        "expected InvalidArgument for non-increasing bound, got {res:?}"
    );

    // 14. Duplicate partition name
    let dup_name = "ALTER TABLE t_range ADD PARTITION (PARTITION p1 VALUES LESS THAN (40))";
    let stmt = parse_one(dup_name).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("duplicate partition name")),
        "expected InvalidArgument for duplicate name, got {res:?}"
    );

    // 15. Non-contiguous sources in REORGANIZE
    let non_contiguous = "ALTER TABLE t_range REORGANIZE PARTITION p0, p2 INTO (PARTITION p02 VALUES LESS THAN (30))";
    let stmt = parse_one(non_contiguous).expect("should parse");
    let res = bind(&stmt, &catalog);
    assert!(
        matches!(res, Err(HtapError::InvalidArgument(ref msg)) if msg.contains("contiguous")),
        "expected InvalidArgument for non-contiguous sources, got {res:?}"
    );
}
