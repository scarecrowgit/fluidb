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

use htap_catalog::{CatalogSnapshot, TableDescriptor, TableId};
use htap_common::types::{
    ColumnDef as CommonColumnDef, DataType as CommonDataType, Row, Schema, Value as CommonValue,
};
use htap_sql::{bind, BoundStatement};

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

    CatalogSnapshot::new(
        1,
        vec![users_table, orders_table, bytes_table, all_types_table],
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
            assert_eq!(insert.rows.len(), 2);
            assert_eq!(
                insert.rows[0],
                Row::new(vec![
                    CommonValue::Int32(1),
                    CommonValue::String("alice".into()),
                    CommonValue::Int32(30),
                    CommonValue::String("first bio".into()),
                ])
            );
            assert_eq!(
                insert.rows[1],
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
            assert_eq!(insert.rows.len(), 1);
            assert_eq!(
                insert.rows[0],
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
    let catalog = make_test_catalog();

    // 1. Single-column PK DELETE
    let sql = "DELETE FROM users WHERE id = 99";
    let bound = parse_and_bind(sql, &catalog).expect("valid single PK DELETE");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.table, "users");
            assert_eq!(delete.key, vec![CommonValue::Int32(99)]);
        }
        other => panic!("expected DeleteByPrimaryKey, got {other:?}"),
    }

    // 2. Composite PK DELETE with reverse predicate order: emits in catalog primary_key order
    let sql = "DELETE FROM orders WHERE order_id = 500 AND tenant_id = 12";
    let bound = parse_and_bind(sql, &catalog).expect("valid composite PK DELETE");
    match bound {
        BoundStatement::Delete(delete) => {
            assert_eq!(delete.table, "orders");
            assert_eq!(
                delete.key,
                vec![CommonValue::Int32(12), CommonValue::Int64(500)]
            );
        }
        other => panic!("expected DeleteByPrimaryKey, got {other:?}"),
    }
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
        // INSERT SELECT
        "INSERT INTO users (id, name, age, bio) SELECT 1, 'a', 20, 'b'",
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
        // OR in WHERE predicate
        "SELECT * FROM users WHERE id = 1 OR id = 2",
        // NULL literal in WHERE
        "SELECT * FROM users WHERE id = NULL",
        // Reversed operands (value = col)
        "SELECT * FROM users WHERE 1 = id",
        "DELETE FROM users WHERE 1 = id",
        // Missing WHERE clause in DELETE
        "DELETE FROM users",
        // Predicate on non-PK column in DELETE
        "DELETE FROM users WHERE name = 'alice'",
        // Non-PK column combined with PK in WHERE in DELETE
        "DELETE FROM users WHERE id = 1 AND name = 'alice'",
        // Partial PK predicate on composite PK table in DELETE
        "DELETE FROM orders WHERE tenant_id = 1",
        // Non-equality operators in DELETE
        "DELETE FROM users WHERE id > 1",
        "DELETE FROM users WHERE id != 1",
        "DELETE FROM users WHERE id IS NULL",
        // Expression in WHERE value
        "SELECT * FROM users WHERE id = 1 + 1",
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
        // Joins
        "SELECT * FROM users JOIN orders ON users.id = orders.tenant_id WHERE users.id = 1",
        "SELECT * FROM users, orders WHERE users.id = 1",
        // Aliases
        "SELECT * FROM users u WHERE id = 1",
        "SELECT * FROM users AS u WHERE id = 1",
        "DELETE FROM users u WHERE id = 1",
        "DELETE FROM users AS u WHERE id = 1",
        "SELECT id AS my_id FROM users WHERE id = 1",
        // Qualified names
        "SELECT * FROM db.users WHERE id = 1",
        "DELETE FROM db.users WHERE id = 1",
        "SELECT users.id FROM users WHERE id = 1",
        "SELECT * FROM users WHERE users.id = 1",
        // ORDER BY
        "SELECT * FROM users WHERE id = 1 ORDER BY id",
        "DELETE FROM users WHERE id = 1 ORDER BY id",
        // LIMIT
        "SELECT * FROM users WHERE id = 1 LIMIT 1",
        "DELETE FROM users WHERE id = 1 LIMIT 1",
        // GROUP BY ALL / HAVING / DISTINCT
        "SELECT * FROM users GROUP BY ALL",
        "SELECT * FROM users WHERE id = 1 HAVING id = 1",
        "SELECT DISTINCT id FROM users WHERE id = 1",
        // CTEs / Set ops
        "WITH cte AS (SELECT 1) SELECT * FROM users WHERE id = 1",
        "SELECT * FROM users WHERE id = 1 UNION SELECT * FROM users WHERE id = 2",
        // Unsupported statement types
        "UPDATE users SET name = 'bob' WHERE id = 1",
        "DROP TABLE users",
        "ALTER TABLE users ADD COLUMN foo INT",
    ];

    for case in unsupported_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for {case:?}, got {res:?}"
        );
    }
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
        // Expression in GROUP BY
        "SELECT COUNT(*) FROM users GROUP BY id + 1",
        // Duplicate output column names
        "SELECT id, id FROM users",
        // SUM on non-numeric column (String)
        "SELECT SUM(name) FROM users",
        // SUM on non-numeric column (Bytes)
        "SELECT SUM(data) FROM bytes_table",
        // Aggregate with multiple arguments
        "SELECT COUNT(id, name) FROM users",
        // NOT operator in WHERE
        "SELECT * FROM users WHERE NOT (id = 1)",
        // Cross-column comparison in WHERE
        "SELECT * FROM users WHERE id = age",
        // Reversed operands in inequality
        "SELECT * FROM users WHERE 10 < age",
        // Reversed operands in equality
        "SELECT * FROM users WHERE 'alice' = name",
    ];

    for case in invalid_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::InvalidArgument(_))),
            "expected InvalidArgument for {case:?}, got {res:?}"
        );
    }

    let unsupported_cases = [
        // AVG function
        "SELECT AVG(age) FROM users",
        "SELECT AVG(amount) FROM orders",
        // DISTINCT in aggregate
        "SELECT COUNT(DISTINCT id) FROM users",
        "SELECT SUM(DISTINCT amount) FROM orders",
        // Column alias
        "SELECT id AS user_id FROM users",
        // Aggregate alias
        "SELECT COUNT(*) AS total FROM users",
    ];

    for case in unsupported_cases {
        let res = parse_and_bind(case, &catalog);
        assert!(
            matches!(res, Err(HtapError::Unsupported(_))),
            "expected Unsupported for {case:?}, got {res:?}"
        );
    }
}
