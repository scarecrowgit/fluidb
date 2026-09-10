//! SQL statement binder and semantic validation against the catalog.

use htap_catalog::{CatalogSnapshot, TableDescriptor};
use htap_common::error::{HtapError, Result};
use htap_common::types::{
    ColumnDef as CommonColumnDef, DataType as CommonDataType, Row, Schema, Value,
};
use sqlparser::ast::{
    BinaryOperator, ColumnOption, CreateTable as SqlCreateTable, CreateTableOptions,
    Delete as SqlDelete, Expr, FromTable, GroupByExpr, IndexColumn, Insert as SqlInsert,
    ObjectName, ObjectNamePart, OrderByExpr, PrimaryKeyConstraint, Query, SelectItem, SetExpr,
    Statement, TableConstraint, TableFactor, TableObject, UnaryOperator,
};

use crate::ast::{BoundStatement, CreateTable, DeleteByPrimaryKey, Insert, PointSelect};

/// Binds an AST [`Statement`] against the [`CatalogSnapshot`], performing strict semantic
/// validation and type checking, and returning a [`BoundStatement`].
///
/// # Errors
///
/// Returns:
/// - [`HtapError::NotFound`] if a referenced table does not exist in the catalog.
/// - [`HtapError::InvalidArgument`] if statement arguments, types, constraints, or predicates
///   are invalid or violate table schemas.
/// - [`HtapError::Unsupported`] if the statement type, clause, or feature is not supported in
///   this strict execution slice.
pub fn bind(statement: &Statement, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    match statement {
        Statement::CreateTable(create_table) => bind_create_table(create_table, catalog),
        Statement::Insert(insert) => bind_insert(insert, catalog),
        Statement::Delete(delete) => bind_delete(delete, catalog),
        Statement::Query(query) => bind_select(query, catalog),
        other => Err(HtapError::Unsupported(format!(
            "unsupported statement: {other}"
        ))),
    }
}

fn extract_unqualified_name(name: &ObjectName) -> Result<String> {
    if name.0.len() != 1 {
        return Err(HtapError::Unsupported(format!(
            "qualified names not supported: '{name}'"
        )));
    }
    match &name.0[0] {
        ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
        ObjectNamePart::Function(_) => Err(HtapError::Unsupported(format!(
            "function names not supported: '{name}'"
        ))),
    }
}

fn map_sql_data_type(data_type: &sqlparser::ast::DataType) -> Result<CommonDataType> {
    match data_type {
        sqlparser::ast::DataType::Boolean | sqlparser::ast::DataType::Bool => {
            Ok(CommonDataType::Bool)
        }
        sqlparser::ast::DataType::Int(_) | sqlparser::ast::DataType::Integer(_) => {
            Ok(CommonDataType::Int32)
        }
        sqlparser::ast::DataType::BigInt(_) => Ok(CommonDataType::Int64),
        sqlparser::ast::DataType::Double(_) | sqlparser::ast::DataType::DoublePrecision => {
            Ok(CommonDataType::Float64)
        }
        sqlparser::ast::DataType::Varchar(_) | sqlparser::ast::DataType::Text => {
            Ok(CommonDataType::String)
        }
        sqlparser::ast::DataType::Varbinary(_) | sqlparser::ast::DataType::Blob(_) => {
            Ok(CommonDataType::Bytes)
        }
        sqlparser::ast::DataType::Timestamp(_, _) => Ok(CommonDataType::Timestamp),
        other => Err(HtapError::Unsupported(format!(
            "unsupported data type: {other}"
        ))),
    }
}

fn validate_table_descriptor_primary_key(table_desc: &TableDescriptor) -> Result<()> {
    if table_desc.primary_key.is_empty() {
        return Err(HtapError::Internal(format!(
            "table '{}' has empty primary key in catalog",
            table_desc.name
        )));
    }
    let schema_len = table_desc.schema.len();
    let mut seen = std::collections::HashSet::with_capacity(table_desc.primary_key.len());
    for &idx in &table_desc.primary_key {
        if idx >= schema_len {
            return Err(HtapError::Internal(format!(
                "table '{}' primary key index {} out of bounds (schema length {})",
                table_desc.name, idx, schema_len
            )));
        }
        if !seen.insert(idx) {
            return Err(HtapError::Internal(format!(
                "table '{}' has duplicate primary key index {}",
                table_desc.name, idx
            )));
        }
        if let Some(col) = table_desc.schema.column(idx) {
            if !col.primary_key {
                return Err(HtapError::Internal(format!(
                    "table '{}' primary key index {} points to column '{}' not marked primary_key",
                    table_desc.name, idx, col.name
                )));
            }
        }
    }
    Ok(())
}

fn bind_create_table(stmt: &SqlCreateTable, _catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if stmt.or_replace {
        return Err(HtapError::Unsupported("OR REPLACE is not supported".into()));
    }
    if stmt.temporary {
        return Err(HtapError::Unsupported(
            "TEMPORARY tables are not supported".into(),
        ));
    }
    if stmt.external {
        return Err(HtapError::Unsupported(
            "EXTERNAL tables are not supported".into(),
        ));
    }
    if stmt.global.is_some() {
        return Err(HtapError::Unsupported(
            "GLOBAL tables are not supported".into(),
        ));
    }
    if stmt.if_not_exists {
        return Err(HtapError::Unsupported(
            "IF NOT EXISTS is not supported".into(),
        ));
    }
    if stmt.transient || stmt.volatile || stmt.iceberg || stmt.snapshot {
        return Err(HtapError::Unsupported("unsupported table modifier".into()));
    }
    if stmt.query.is_some() {
        return Err(HtapError::Unsupported(
            "CREATE TABLE AS SELECT is not supported".into(),
        ));
    }
    if stmt.without_rowid {
        return Err(HtapError::Unsupported(
            "WITHOUT ROWID is not supported".into(),
        ));
    }
    if stmt.like.is_some() || stmt.clone.is_some() {
        return Err(HtapError::Unsupported(
            "LIKE / CLONE is not supported".into(),
        ));
    }
    if stmt.version.is_some()
        || stmt.comment.is_some()
        || stmt.on_commit.is_some()
        || stmt.on_cluster.is_some()
    {
        return Err(HtapError::Unsupported("unsupported table clause".into()));
    }
    if stmt.order_by.is_some()
        || stmt.partition_by.is_some()
        || stmt.cluster_by.is_some()
        || stmt.clustered_by.is_some()
    {
        return Err(HtapError::Unsupported(
            "ORDER BY / PARTITION BY / CLUSTER BY not supported in CREATE TABLE".into(),
        ));
    }
    if stmt.inherits.is_some() || stmt.partition_of.is_some() || stmt.for_values.is_some() {
        return Err(HtapError::Unsupported(
            "inheritance / partitioning not supported".into(),
        ));
    }
    if stmt.strict || stmt.copy_grants {
        return Err(HtapError::Unsupported("unsupported table flags".into()));
    }

    match &stmt.table_options {
        CreateTableOptions::None => {}
        CreateTableOptions::With(opts)
        | CreateTableOptions::Options(opts)
        | CreateTableOptions::Plain(opts)
        | CreateTableOptions::TableProperties(opts) => {
            if !opts.is_empty() {
                return Err(HtapError::Unsupported(
                    "table options / engine not supported in CREATE TABLE".into(),
                ));
            }
        }
    }

    let table_name = extract_unqualified_name(&stmt.name)?;
    if table_name.is_empty() {
        return Err(HtapError::InvalidArgument(
            "table name cannot be empty".into(),
        ));
    }

    if stmt.columns.is_empty() {
        return Err(HtapError::InvalidArgument(
            "table must have at least one column".into(),
        ));
    }

    // Process table constraints
    let mut table_pk_constraint: Option<std::borrow::Cow<'_, PrimaryKeyConstraint>> = None;
    for constraint in &stmt.constraints {
        match constraint {
            TableConstraint::PrimaryKey(pk) => {
                if table_pk_constraint.is_some() {
                    return Err(HtapError::InvalidArgument(
                        "multiple primary key declarations".into(),
                    ));
                }
                table_pk_constraint = Some(std::borrow::Cow::Borrowed(pk));
            }
            TableConstraint::Unique(_) => {
                return Err(HtapError::Unsupported(
                    "UNIQUE constraint not supported".into(),
                ));
            }
            TableConstraint::ForeignKey(_) => {
                return Err(HtapError::Unsupported(
                    "FOREIGN KEY constraint not supported".into(),
                ));
            }
            TableConstraint::Check(_) => {
                return Err(HtapError::Unsupported(
                    "CHECK constraint not supported".into(),
                ));
            }
            TableConstraint::Index(_) | TableConstraint::FulltextOrSpatial(_) => {
                return Err(HtapError::Unsupported(
                    "INDEX constraint not supported".into(),
                ));
            }
            TableConstraint::PrimaryKeyUsingIndex(_) | TableConstraint::UniqueUsingIndex(_) => {
                return Err(HtapError::Unsupported("USING INDEX not supported".into()));
            }
        }
    }

    if let Some(ref pk_expr) = stmt.primary_key {
        if table_pk_constraint.is_some() {
            return Err(HtapError::InvalidArgument(
                "multiple primary key declarations".into(),
            ));
        }
        let pk = expr_to_primary_key_constraint(pk_expr)?;
        table_pk_constraint = Some(std::borrow::Cow::Owned(pk));
    }

    let mut column_defs = Vec::with_capacity(stmt.columns.len());
    let mut column_pk_indices = Vec::new();

    for (col_idx, col) in stmt.columns.iter().enumerate() {
        let col_name = col.name.value.clone();
        if col_name.is_empty() {
            return Err(HtapError::InvalidArgument(
                "column name cannot be empty".into(),
            ));
        }
        let data_type = map_sql_data_type(&col.data_type)?;

        let mut is_pk = false;
        let mut explicit_null: Option<bool> = None;

        for opt_def in &col.options {
            match &opt_def.option {
                ColumnOption::Null => {
                    if explicit_null.is_some() {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate or conflicting nullability for column '{col_name}'"
                        )));
                    }
                    explicit_null = Some(true);
                }
                ColumnOption::NotNull => {
                    if explicit_null.is_some() {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate or conflicting nullability for column '{col_name}'"
                        )));
                    }
                    explicit_null = Some(false);
                }
                ColumnOption::PrimaryKey(pk) => {
                    if is_pk {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate primary key on column '{col_name}'"
                        )));
                    }
                    if !pk.columns.is_empty() {
                        if pk.columns.len() != 1 {
                            return Err(HtapError::InvalidArgument(
                                "column primary key cannot reference multiple columns".into(),
                            ));
                        }
                        if let Expr::Identifier(ref ident) = pk.columns[0].column.expr {
                            if ident.value != col_name {
                                return Err(HtapError::InvalidArgument(
                                    "column primary key references different column".into(),
                                ));
                            }
                        } else {
                            return Err(HtapError::InvalidArgument(
                                "invalid column primary key expression".into(),
                            ));
                        }
                    }
                    if pk.index_type.is_some()
                        || !pk.index_options.is_empty()
                        || pk.characteristics.is_some()
                    {
                        return Err(HtapError::Unsupported(
                            "unsupported primary key options".into(),
                        ));
                    }
                    is_pk = true;
                }
                ColumnOption::Default(_) => {
                    return Err(HtapError::Unsupported(
                        "DEFAULT clause not supported".into(),
                    ));
                }
                ColumnOption::Generated { .. } => {
                    return Err(HtapError::Unsupported(
                        "GENERATED columns not supported".into(),
                    ));
                }
                ColumnOption::Unique(_) => {
                    return Err(HtapError::Unsupported(
                        "UNIQUE column option not supported".into(),
                    ));
                }
                ColumnOption::ForeignKey(_) => {
                    return Err(HtapError::Unsupported(
                        "FOREIGN KEY column option not supported".into(),
                    ));
                }
                ColumnOption::Check(_) => {
                    return Err(HtapError::Unsupported(
                        "CHECK column option not supported".into(),
                    ));
                }
                other => {
                    return Err(HtapError::Unsupported(format!(
                        "unsupported column option: {other:?}"
                    )));
                }
            }
        }

        if is_pk {
            if explicit_null == Some(true) {
                return Err(HtapError::InvalidArgument(format!(
                    "primary key column '{col_name}' cannot be nullable"
                )));
            }
            column_pk_indices.push(col_idx);
        }

        let nullable = if is_pk {
            false
        } else {
            explicit_null.unwrap_or(true)
        };

        column_defs.push(CommonColumnDef {
            name: col_name,
            data_type,
            nullable,
            primary_key: is_pk,
        });
    }

    // Determine authoritative primary key
    let final_pk_indices = match (column_pk_indices.as_slice(), table_pk_constraint.as_deref()) {
        ([single_pk], None) => vec![*single_pk],
        ([], Some(table_pk)) => {
            if table_pk.columns.is_empty() {
                return Err(HtapError::InvalidArgument(
                    "primary key cannot be empty".into(),
                ));
            }
            if table_pk.index_type.is_some()
                || !table_pk.index_options.is_empty()
                || table_pk.characteristics.is_some()
            {
                return Err(HtapError::Unsupported(
                    "unsupported table primary key options".into(),
                ));
            }
            let mut pk_indices = Vec::with_capacity(table_pk.columns.len());
            let mut seen_pk_names = std::collections::HashSet::new();

            for index_col in &table_pk.columns {
                if index_col.operator_class.is_some() {
                    return Err(HtapError::Unsupported(
                        "operator classes not supported".into(),
                    ));
                }
                let pk_col_name = match &index_col.column.expr {
                    Expr::Identifier(ident) => &ident.value,
                    _ => {
                        return Err(HtapError::InvalidArgument(
                            "primary key column must be a simple identifier".into(),
                        ));
                    }
                };
                if !seen_pk_names.insert(pk_col_name.as_str()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate column '{pk_col_name}' in primary key"
                    )));
                }
                let col_idx = column_defs
                    .iter()
                    .position(|c| c.name == *pk_col_name)
                    .ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "unknown column '{pk_col_name}' in table primary key"
                        ))
                    })?;

                // Check if this column was explicitly declared NULL
                if column_defs[col_idx].nullable
                    && stmt.columns[col_idx]
                        .options
                        .iter()
                        .any(|o| matches!(o.option, ColumnOption::Null))
                {
                    return Err(HtapError::InvalidArgument(format!(
                        "primary key column '{pk_col_name}' cannot be nullable"
                    )));
                }

                column_defs[col_idx].primary_key = true;
                column_defs[col_idx].nullable = false;
                pk_indices.push(col_idx);
            }
            pk_indices
        }
        ([], None) => {
            return Err(HtapError::InvalidArgument(
                "table must have a primary key".into(),
            ));
        }
        (_, _) => {
            return Err(HtapError::InvalidArgument(
                "multiple primary key declarations".into(),
            ));
        }
    };

    let schema = Schema::new(column_defs)?;
    Ok(BoundStatement::CreateTable(CreateTable::new(
        table_name,
        schema,
        final_pk_indices,
    )))
}

fn expr_to_primary_key_constraint(expr: &Expr) -> Result<PrimaryKeyConstraint> {
    fn unwrap_nested(expr: &Expr) -> &Expr {
        match expr {
            Expr::Nested(inner) => unwrap_nested(inner),
            other => other,
        }
    }

    let unwrapped = unwrap_nested(expr);
    let exprs = match unwrapped {
        Expr::Tuple(exprs) => exprs.clone(),
        other => vec![other.clone()],
    };

    let columns = exprs
        .into_iter()
        .map(|e| IndexColumn {
            column: OrderByExpr {
                expr: e,
                options: sqlparser::ast::OrderByOptions::default(),
                with_fill: None,
            },
            operator_class: None,
        })
        .collect();

    Ok(PrimaryKeyConstraint {
        name: None,
        index_name: None,
        index_type: None,
        columns,
        index_options: vec![],
        characteristics: None,
    })
}

fn extract_number_parts<'a>(expr: &'a Expr, col_name: &str) -> Result<(&'static str, &'a str)> {
    match expr {
        Expr::Value(v) => match &v.value {
            sqlparser::ast::Value::Number(s, _) => Ok(("", s.as_str())),
            sqlparser::ast::Value::Placeholder(_) => Err(HtapError::InvalidArgument(
                "placeholders not supported".into(),
            )),
            _ => Err(HtapError::InvalidArgument(format!(
                "type mismatch for column '{col_name}': expected number"
            ))),
        },
        Expr::UnaryOp { op, expr: inner } => {
            let prefix = match op {
                UnaryOperator::Plus => "+",
                UnaryOperator::Minus => "-",
                _ => {
                    return Err(HtapError::InvalidArgument(format!(
                        "unsupported unary operator '{op}' in numeric literal"
                    )));
                }
            };
            if let Expr::Value(v) = &**inner {
                if let sqlparser::ast::Value::Number(s, _) = &v.value {
                    return Ok((prefix, s.as_str()));
                }
            }
            Err(HtapError::InvalidArgument(format!(
                "type mismatch or expression in numeric literal for column '{col_name}'"
            )))
        }
        _ => Err(HtapError::InvalidArgument(format!(
            "expressions not supported in values for column '{col_name}'"
        ))),
    }
}

fn parse_integer_string(prefix: &str, num_str: &str, col_name: &str) -> Result<String> {
    if num_str.contains('.') || num_str.contains('e') || num_str.contains('E') {
        return Err(HtapError::InvalidArgument(format!(
            "cannot convert fractional or exponent number '{prefix}{num_str}' to integer for column '{col_name}'"
        )));
    }
    if !num_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(HtapError::InvalidArgument(format!(
            "invalid integer literal with suffix or non-digit characters for column '{col_name}': '{prefix}{num_str}'"
        )));
    }
    Ok(format!("{prefix}{num_str}"))
}

fn parse_float_string(prefix: &str, num_str: &str, col_name: &str) -> Result<f64> {
    let full = format!("{prefix}{num_str}");
    let parts: Vec<&str> = num_str.split(['e', 'E']).collect();
    if parts.is_empty() || parts.len() > 2 {
        return Err(HtapError::InvalidArgument(format!(
            "invalid float literal for column '{col_name}': '{full}'"
        )));
    }
    let mantissa = parts[0];
    if mantissa.is_empty()
        || mantissa.chars().filter(|&c| c == '.').count() > 1
        || !mantissa.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return Err(HtapError::InvalidArgument(format!(
            "invalid float literal for column '{col_name}': '{full}'"
        )));
    }
    if parts.len() == 2 {
        let exp = parts[1];
        let exp_digits = exp
            .strip_prefix('+')
            .or_else(|| exp.strip_prefix('-'))
            .unwrap_or(exp);
        if exp_digits.is_empty() || !exp_digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(HtapError::InvalidArgument(format!(
                "invalid float exponent for column '{col_name}': '{full}'"
            )));
        }
    }
    let val: f64 = full.parse().map_err(|e| {
        HtapError::InvalidArgument(format!(
            "invalid float literal for column '{col_name}': {e}"
        ))
    })?;
    if val.is_nan() || val.is_infinite() {
        return Err(HtapError::InvalidArgument(format!(
            "float overflow or NaN for column '{col_name}'"
        )));
    }
    Ok(val)
}

fn parse_hex_bytes(s: &str, col_name: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(HtapError::InvalidArgument(format!(
            "hex literal for column '{col_name}' must have an even number of digits, got length {}",
            s.len()
        )));
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| {
            HtapError::InvalidArgument(format!(
                "invalid hex character in literal for column '{col_name}'"
            ))
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn parse_literal_value(expr: &Expr, col_def: &CommonColumnDef) -> Result<Value> {
    let mut current = expr;
    while let Expr::Nested(inner) = current {
        current = inner;
    }

    if let Expr::Value(v) = current {
        if matches!(v.value, sqlparser::ast::Value::Null) {
            if col_def.primary_key {
                return Err(HtapError::InvalidArgument(format!(
                    "NULL value not allowed for primary key column '{}'",
                    col_def.name
                )));
            }
            if !col_def.nullable {
                return Err(HtapError::InvalidArgument(format!(
                    "column '{}' is NOT NULL",
                    col_def.name
                )));
            }
            return Ok(Value::Null);
        }
        if matches!(v.value, sqlparser::ast::Value::Placeholder(_)) {
            return Err(HtapError::InvalidArgument(
                "placeholders not supported".into(),
            ));
        }
    }

    match col_def.data_type {
        CommonDataType::Bool => {
            if let Expr::Value(v) = current {
                if let sqlparser::ast::Value::Boolean(b) = v.value {
                    return Ok(Value::Bool(b));
                }
            }
            Err(HtapError::InvalidArgument(format!(
                "type mismatch for column '{}': expected boolean",
                col_def.name
            )))
        }
        CommonDataType::Int32 => {
            let (prefix, num_str) = extract_number_parts(current, &col_def.name)?;
            let s = parse_integer_string(prefix, num_str, &col_def.name)?;
            let val: i32 = s.parse().map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "integer overflow or error for column '{}' (INT32): {e}",
                    col_def.name
                ))
            })?;
            Ok(Value::Int32(val))
        }
        CommonDataType::Int64 => {
            let (prefix, num_str) = extract_number_parts(current, &col_def.name)?;
            let s = parse_integer_string(prefix, num_str, &col_def.name)?;
            let val: i64 = s.parse().map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "integer overflow or error for column '{}' (INT64): {e}",
                    col_def.name
                ))
            })?;
            Ok(Value::Int64(val))
        }
        CommonDataType::Timestamp => {
            let (prefix, num_str) = extract_number_parts(current, &col_def.name)?;
            let s = parse_integer_string(prefix, num_str, &col_def.name)?;
            let val: i64 = s.parse().map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "integer overflow or error for column '{}' (TIMESTAMP): {e}",
                    col_def.name
                ))
            })?;
            Ok(Value::Timestamp(val))
        }
        CommonDataType::Float64 => {
            let (prefix, num_str) = extract_number_parts(current, &col_def.name)?;
            let val = parse_float_string(prefix, num_str, &col_def.name)?;
            Ok(Value::Float64(val))
        }
        CommonDataType::String => {
            if let Expr::Value(v) = current {
                match &v.value {
                    sqlparser::ast::Value::SingleQuotedString(s)
                    | sqlparser::ast::Value::DoubleQuotedString(s) => {
                        return Ok(Value::String(s.clone()));
                    }
                    _ => {}
                }
            }
            Err(HtapError::InvalidArgument(format!(
                "type mismatch for column '{}': expected quoted string",
                col_def.name
            )))
        }
        CommonDataType::Bytes => {
            if let Expr::Value(v) = current {
                if let sqlparser::ast::Value::HexStringLiteral(s) = &v.value {
                    let bytes = parse_hex_bytes(s, &col_def.name)?;
                    return Ok(Value::Bytes(bytes));
                }
            }
            Err(HtapError::InvalidArgument(format!(
                "type mismatch for column '{}': expected hex byte literal",
                col_def.name
            )))
        }
    }
}

fn bind_insert(insert: &SqlInsert, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if insert.ignore {
        return Err(HtapError::Unsupported(
            "INSERT IGNORE is not supported".into(),
        ));
    }
    if insert.replace_into {
        return Err(HtapError::Unsupported(
            "REPLACE INTO is not supported".into(),
        ));
    }
    if insert.on.is_some() {
        return Err(HtapError::Unsupported(
            "ON DUPLICATE KEY UPDATE is not supported".into(),
        ));
    }
    if insert.returning.is_some() {
        return Err(HtapError::Unsupported(
            "RETURNING clause is not supported in INSERT".into(),
        ));
    }
    if !insert.assignments.is_empty() {
        return Err(HtapError::Unsupported(
            "INSERT ... SET is not supported".into(),
        ));
    }
    if insert.table_alias.is_some() || insert.insert_alias.is_some() {
        return Err(HtapError::Unsupported(
            "table aliases in INSERT not supported".into(),
        ));
    }

    let table_name = match &insert.table {
        TableObject::TableName(name) => extract_unqualified_name(name)?,
        _ => {
            return Err(HtapError::Unsupported(
                "unsupported table factor in INSERT".into(),
            ));
        }
    };

    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))?;

    validate_table_descriptor_primary_key(table_desc)?;

    if insert.columns.is_empty() {
        return Err(HtapError::InvalidArgument(
            "INSERT statement requires an explicit column list".into(),
        ));
    }

    let query = insert.source.as_ref().ok_or_else(|| {
        HtapError::InvalidArgument("INSERT statement missing source query".into())
    })?;

    if query.with.is_some() {
        return Err(HtapError::Unsupported(
            "CTEs not supported in INSERT".into(),
        ));
    }
    if query.order_by.is_some() || query.limit_clause.is_some() {
        return Err(HtapError::Unsupported(
            "ORDER BY / LIMIT not supported in INSERT".into(),
        ));
    }

    let values = match &*query.body {
        SetExpr::Values(v) => v,
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::SetOperation { .. } => {
            return Err(HtapError::Unsupported(
                "INSERT ... SELECT not supported".into(),
            ));
        }
        _ => {
            return Err(HtapError::Unsupported("unsupported INSERT source".into()));
        }
    };

    if values.rows.is_empty() {
        return Err(HtapError::InvalidArgument(
            "INSERT VALUES cannot be empty".into(),
        ));
    }

    // Check missing columns: every schema column must be present in insert.columns
    if insert.columns.len() != table_desc.schema.len() {
        return Err(HtapError::InvalidArgument(format!(
            "INSERT column count mismatch: expected all {} schema columns, got {}",
            table_desc.schema.len(),
            insert.columns.len()
        )));
    }

    let mut col_index_in_schema = Vec::with_capacity(insert.columns.len());
    let mut seen_cols = std::collections::HashSet::with_capacity(insert.columns.len());

    for col_obj in &insert.columns {
        let col_name = if col_obj.0.len() != 1 {
            return Err(HtapError::Unsupported(format!(
                "qualified column names not supported in INSERT: {col_obj}"
            )));
        } else {
            match &col_obj.0[0] {
                ObjectNamePart::Identifier(ident) => &ident.value,
                _ => {
                    return Err(HtapError::Unsupported(format!(
                        "unsupported column name: {col_obj}"
                    )));
                }
            }
        };

        let schema_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "unknown column '{col_name}' in table '{table_name}'"
            ))
        })?;

        if !seen_cols.insert(schema_idx) {
            return Err(HtapError::InvalidArgument(format!(
                "duplicate column '{col_name}' in INSERT column list"
            )));
        }

        col_index_in_schema.push(schema_idx);
    }

    let schema_len = table_desc.schema.len();
    let mut rows = Vec::with_capacity(values.rows.len());

    for (row_idx, row_exprs) in values.rows.iter().enumerate() {
        if row_exprs.len() != insert.columns.len() {
            return Err(HtapError::InvalidArgument(format!(
                "row {} arity mismatch: expected {} values, got {}",
                row_idx,
                insert.columns.len(),
                row_exprs.len()
            )));
        }

        let mut row_values: Vec<Option<Value>> = vec![None; schema_len];

        for (i, expr) in row_exprs.iter().enumerate() {
            let schema_idx = col_index_in_schema[i];
            let col_def = table_desc
                .schema
                .column(schema_idx)
                .expect("index validated");
            let val = parse_literal_value(expr, col_def)?;
            row_values[schema_idx] = Some(val);
        }

        let ordered_values: Vec<Value> = row_values
            .into_iter()
            .map(|v| v.expect("all columns must be assigned"))
            .collect();

        rows.push(Row::new(ordered_values));
    }

    Ok(BoundStatement::Insert(Insert::new(table_name, rows)))
}

fn collect_and_leaves<'a>(expr: &'a Expr, leaves: &mut Vec<&'a Expr>) -> Result<()> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_and_leaves(left, leaves)?;
            collect_and_leaves(right, leaves)?;
            Ok(())
        }
        Expr::Nested(inner) => collect_and_leaves(inner, leaves),
        other => {
            leaves.push(other);
            Ok(())
        }
    }
}

fn bind_pk_where_predicate(
    table_desc: &TableDescriptor,
    selection: Option<&Expr>,
) -> Result<Vec<Value>> {
    let expr = selection.ok_or_else(|| {
        HtapError::InvalidArgument(format!(
            "statement requires a WHERE clause on table '{}'",
            table_desc.name
        ))
    })?;

    let mut leaves = Vec::new();
    collect_and_leaves(expr, &mut leaves)?;

    if leaves.is_empty() {
        return Err(HtapError::InvalidArgument(
            "empty WHERE clause predicate".into(),
        ));
    }

    let mut pk_values = std::collections::HashMap::new();

    for leaf in leaves {
        let mut unnested_leaf = leaf;
        while let Expr::Nested(inner) = unnested_leaf {
            unnested_leaf = inner;
        }

        match unnested_leaf {
            Expr::BinaryOp { left, op, right } => {
                if *op != BinaryOperator::Eq {
                    return Err(HtapError::InvalidArgument(format!(
                        "unsupported operator in WHERE clause: expected '=', found '{op}'"
                    )));
                }

                let mut l = &**left;
                while let Expr::Nested(inner) = l {
                    l = inner;
                }

                let mut r = &**right;
                while let Expr::Nested(inner) = r {
                    r = inner;
                }

                // Check for reversed operands (e.g. 1 = id)
                if matches!(l, Expr::Value(_)) || matches!(r, Expr::Identifier(_)) {
                    return Err(HtapError::InvalidArgument(
                        "reversed operands in equality predicate: expected column = value".into(),
                    ));
                }

                let col_name = match l {
                    Expr::Identifier(ident) => &ident.value,
                    Expr::CompoundIdentifier(_) => {
                        return Err(HtapError::Unsupported(
                            "qualified column names not supported in WHERE clause".into(),
                        ));
                    }
                    _ => {
                        return Err(HtapError::InvalidArgument(
                            "left side of equality predicate must be a column identifier".into(),
                        ));
                    }
                };

                let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "unknown column '{col_name}' in table '{}'",
                        table_desc.name
                    ))
                })?;

                if !table_desc.primary_key.contains(&col_idx) {
                    return Err(HtapError::InvalidArgument(format!(
                        "predicate on non-primary-key column '{col_name}' in table '{}'",
                        table_desc.name
                    )));
                }

                if pk_values.contains_key(&col_idx) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate predicate on primary key column '{col_name}'",
                    )));
                }

                if matches!(r, Expr::Value(ref v) if matches!(v.value, sqlparser::ast::Value::Null))
                {
                    return Err(HtapError::InvalidArgument(format!(
                        "NULL literal not permitted in primary key predicate for column '{col_name}'"
                    )));
                }

                let col_def = table_desc
                    .schema
                    .column(col_idx)
                    .expect("column must exist");
                let parsed_value = parse_literal_value(r, col_def)?;
                pk_values.insert(col_idx, parsed_value);
            }
            _ => {
                return Err(HtapError::InvalidArgument(
                    "WHERE clause must be a conjunction of equality predicates".into(),
                ));
            }
        }
    }

    if pk_values.len() != table_desc.primary_key.len() {
        return Err(HtapError::InvalidArgument(format!(
            "partial primary key predicate: expected {} columns, got {}",
            table_desc.primary_key.len(),
            pk_values.len()
        )));
    }

    // Emit key in catalog primary_key order
    let mut key = Vec::with_capacity(table_desc.primary_key.len());
    for &pk_idx in &table_desc.primary_key {
        let val = pk_values
            .remove(&pk_idx)
            .expect("all PK columns verified present");
        key.push(val);
    }
    Ok(key)
}

fn bind_delete(delete: &SqlDelete, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if !delete.tables.is_empty() {
        return Err(HtapError::Unsupported(
            "multi-table DELETE not supported".into(),
        ));
    }
    if delete.using.is_some() {
        return Err(HtapError::Unsupported(
            "USING clause not supported in DELETE".into(),
        ));
    }
    if delete.returning.is_some() {
        return Err(HtapError::Unsupported(
            "RETURNING clause not supported in DELETE".into(),
        ));
    }
    if !delete.order_by.is_empty() {
        return Err(HtapError::Unsupported(
            "ORDER BY not supported in DELETE".into(),
        ));
    }
    if delete.limit.is_some() {
        return Err(HtapError::Unsupported(
            "LIMIT not supported in DELETE".into(),
        ));
    }

    let tables = match &delete.from {
        FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
    };
    if tables.len() != 1 {
        return Err(HtapError::Unsupported(
            "joins or multiple tables in DELETE not supported".into(),
        ));
    }
    let table_with_joins = &tables[0];
    if !table_with_joins.joins.is_empty() {
        return Err(HtapError::Unsupported(
            "JOIN not supported in DELETE".into(),
        ));
    }

    let table_name = match &table_with_joins.relation {
        TableFactor::Table {
            name,
            alias,
            partitions,
            args,
            with_hints,
            version,
            ..
        } => {
            if alias.is_some() {
                return Err(HtapError::Unsupported(
                    "table aliases not supported in DELETE".into(),
                ));
            }
            if !partitions.is_empty()
                || args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
            {
                return Err(HtapError::Unsupported(
                    "unsupported table factor options in DELETE".into(),
                ));
            }
            extract_unqualified_name(name)?
        }
        _ => {
            return Err(HtapError::Unsupported(
                "unsupported table relation in DELETE".into(),
            ));
        }
    };

    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))?;

    validate_table_descriptor_primary_key(table_desc)?;

    let key = bind_pk_where_predicate(table_desc, delete.selection.as_ref())?;

    Ok(BoundStatement::Delete(DeleteByPrimaryKey::new(
        table_name, key,
    )))
}

fn bind_select(query: &Query, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if query.with.is_some() {
        return Err(HtapError::Unsupported(
            "CTEs (WITH clause) not supported".into(),
        ));
    }
    if query.order_by.is_some() {
        return Err(HtapError::Unsupported(
            "ORDER BY clause not supported in SELECT".into(),
        ));
    }
    if query.limit_clause.is_some() {
        return Err(HtapError::Unsupported(
            "LIMIT clause not supported in SELECT".into(),
        ));
    }
    if query.fetch.is_some() {
        return Err(HtapError::Unsupported(
            "FETCH clause not supported in SELECT".into(),
        ));
    }
    if !query.locks.is_empty() {
        return Err(HtapError::Unsupported(
            "locking clauses not supported in SELECT".into(),
        ));
    }
    if query.for_clause.is_some() {
        return Err(HtapError::Unsupported(
            "FOR clause not supported in SELECT".into(),
        ));
    }

    let select = match &*query.body {
        SetExpr::Select(s) => s,
        SetExpr::SetOperation { .. } => {
            return Err(HtapError::Unsupported(
                "set operations (UNION/EXCEPT/INTERSECT) not supported".into(),
            ));
        }
        _ => {
            return Err(HtapError::Unsupported(
                "unsupported query body in SELECT".into(),
            ));
        }
    };

    if select.distinct.is_some() {
        return Err(HtapError::Unsupported(
            "DISTINCT not supported in SELECT".into(),
        ));
    }
    if select.top.is_some() {
        return Err(HtapError::Unsupported("TOP not supported in SELECT".into()));
    }
    if select.into.is_some() {
        return Err(HtapError::Unsupported(
            "INTO not supported in SELECT".into(),
        ));
    }
    if !select.lateral_views.is_empty() {
        return Err(HtapError::Unsupported(
            "LATERAL VIEW not supported in SELECT".into(),
        ));
    }
    if select.prewhere.is_some() {
        return Err(HtapError::Unsupported(
            "PREWHERE not supported in SELECT".into(),
        ));
    }
    if matches!(select.group_by, GroupByExpr::Expressions(ref exprs, _) if !exprs.is_empty())
        || matches!(select.group_by, GroupByExpr::All(_))
    {
        return Err(HtapError::Unsupported(
            "GROUP BY not supported in SELECT".into(),
        ));
    }
    if select.having.is_some() {
        return Err(HtapError::Unsupported(
            "HAVING not supported in SELECT".into(),
        ));
    }
    if !select.named_window.is_empty() {
        return Err(HtapError::Unsupported(
            "WINDOW clauses not supported in SELECT".into(),
        ));
    }
    if select.qualify.is_some() {
        return Err(HtapError::Unsupported(
            "QUALIFY not supported in SELECT".into(),
        ));
    }

    if select.from.len() != 1 {
        return Err(HtapError::Unsupported(
            "joins or multiple tables in FROM not supported".into(),
        ));
    }
    let table_with_joins = &select.from[0];
    if !table_with_joins.joins.is_empty() {
        return Err(HtapError::Unsupported(
            "JOIN not supported in SELECT".into(),
        ));
    }

    let table_name = match &table_with_joins.relation {
        TableFactor::Table {
            name,
            alias,
            partitions,
            args,
            with_hints,
            version,
            ..
        } => {
            if alias.is_some() {
                return Err(HtapError::Unsupported(
                    "table aliases not supported in SELECT".into(),
                ));
            }
            if !partitions.is_empty()
                || args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
            {
                return Err(HtapError::Unsupported(
                    "unsupported table factor options in SELECT".into(),
                ));
            }
            extract_unqualified_name(name)?
        }
        _ => {
            return Err(HtapError::Unsupported(
                "unsupported table relation in SELECT".into(),
            ));
        }
    };

    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))?;

    validate_table_descriptor_primary_key(table_desc)?;

    // Validate projection
    if select.projection.is_empty() {
        return Err(HtapError::InvalidArgument(
            "SELECT projection cannot be empty".into(),
        ));
    }

    let projection = if select.projection.len() == 1
        && matches!(select.projection[0], SelectItem::Wildcard(_))
    {
        (0..table_desc.schema.len()).collect()
    } else {
        let mut proj = Vec::with_capacity(select.projection.len());
        for item in &select.projection {
            match item {
                SelectItem::Wildcard(_) => {
                    return Err(HtapError::InvalidArgument(
                        "wildcard '*' cannot be combined with specific columns".into(),
                    ));
                }
                SelectItem::QualifiedWildcard(..) => {
                    return Err(HtapError::Unsupported(
                        "qualified wildcard not supported in projection".into(),
                    ));
                }
                SelectItem::ExprWithAlias { .. } | SelectItem::ExprWithAliases { .. } => {
                    return Err(HtapError::Unsupported(
                        "aliases not supported in projection".into(),
                    ));
                }
                SelectItem::UnnamedExpr(expr) => match expr {
                    Expr::Identifier(ident) => {
                        let col_name = &ident.value;
                        let col_idx =
                            table_desc.schema.column_index(col_name).ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{col_name}' in table '{table_name}'"
                                ))
                            })?;
                        proj.push(col_idx);
                    }
                    Expr::CompoundIdentifier(_) => {
                        return Err(HtapError::Unsupported(
                            "qualified column names not supported in projection".into(),
                        ));
                    }
                    _ => {
                        return Err(HtapError::Unsupported(
                            "expressions not supported in projection".into(),
                        ));
                    }
                },
            }
        }
        proj
    };

    let key = bind_pk_where_predicate(table_desc, select.selection.as_ref())?;

    Ok(BoundStatement::Select(PointSelect::new(
        table_name, projection, key,
    )))
}
