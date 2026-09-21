//! SQL statement binder and semantic validation against the catalog.

use htap_catalog::{
    CatalogSnapshot, ListPartitionDefinition, PartitionAlteration, PartitionDefinition,
    PartitioningMethod, RangePartitionDefinition, TableDescriptor,
};
use htap_common::error::{HtapError, Result};
use htap_common::types::{
    ColumnDef as CommonColumnDef, DataType as CommonDataType, Row, Schema, Value,
};
use sqlparser::ast::{
    Action, AlterTable as SqlAlterTable, AlterTableOperation, BinaryOperator, ColumnOption,
    CreateTable as SqlCreateTable, CreateTableOptions, DuplicateTreatment, Expr, FunctionArg,
    FunctionArgExpr, FunctionArguments, GrantObjects, Grantee, GranteeName, GranteesType,
    GroupByExpr, Ident, IndexColumn, Insert as SqlInsert, MysqlLessThanBound, MysqlPartitionBy,
    MysqlPartitionDef, MysqlPartitionValues, ObjectName, ObjectNamePart, OrderByExpr,
    PrimaryKeyConstraint, Privileges, Query, SelectItem, SetExpr, Statement, TableConstraint,
    TableFactor, TableObject, UnaryOperator,
};

use crate::{
    ast::{
        AggregateFunction, AlterPartitions, AlterUserStatement, AnalyticExpr, AnalyticFilter,
        AnalyticOrderBy, AnalyticSelect, BoundListPartition, BoundPartitioning,
        BoundRangePartition, BoundStatement, ComparisonOp, CreateTable, CreateUserStatement,
        DropUserStatement, GrantScope, GrantStatement, Insert, PointSelect, RevokeStatement,
        ShowGrantsStatement,
    },
    table_not_found,
};
use htap_catalog::PrivilegeSet;

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
        Statement::Delete(delete) => crate::binder_query::bind_delete(delete, catalog),
        Statement::Truncate(truncate) => crate::binder_query::bind_truncate(truncate, catalog),
        Statement::Query(query) => bind_select(query, catalog),
        Statement::AlterTable(alter_table) => bind_alter_table(alter_table, catalog),
        Statement::Update(update) => crate::binder_query::bind_update(update, catalog),
        Statement::CreateUser(create) => bind_create_user(create),
        Statement::AlterUser(alter) => bind_alter_user(alter),
        Statement::DropUser { if_exists, names } => bind_drop_user(*if_exists, names),
        Statement::Grant(grant) => {
            if grant.with_grant_option
                || grant.as_grantor.is_some()
                || grant.granted_by.is_some()
                || grant.current_grants.is_some()
            {
                return Err(HtapError::Unsupported("unsupported GRANT option".into()));
            }
            bind_grant(
                &grant.privileges,
                grant.objects.as_ref(),
                &grant.grantees,
                catalog,
            )
        }
        Statement::Revoke(revoke) => {
            if revoke.granted_by.is_some() || revoke.cascade.is_some() {
                return Err(HtapError::Unsupported("unsupported REVOKE option".into()));
            }
            bind_revoke(
                &revoke.privileges,
                revoke.objects.as_ref(),
                &revoke.grantees,
                catalog,
            )
        }
        Statement::ShowGrants { for_ } => bind_show_grants(for_.as_ref()),
        Statement::Analyze(analyze) => bind_analyze_table(analyze, catalog),
        Statement::Explain {
            analyze,
            verbose,
            query_plan,
            estimate,
            statement: inner,
            format,
            options,
            ..
        } => {
            if *verbose || *query_plan || *estimate || format.is_some() || options.is_some() {
                return Err(HtapError::Unsupported("unsupported EXPLAIN option".into()));
            }
            Ok(BoundStatement::Explain {
                inner: Box::new(bind(inner, catalog)?),
                analyze: *analyze,
            })
        }
        Statement::Drop { .. } => crate::binder_query::bind_drop(statement),
        Statement::ShowTables { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowColumns { .. }
        | Statement::ExplainTable { .. } => crate::binder_query::bind_show(statement, catalog),
        other => Err(HtapError::Unsupported(format!(
            "unsupported statement: {other}"
        ))),
    }
}

fn bind_grantee_name(name: &GranteeName) -> Result<String> {
    match name {
        GranteeName::ObjectName(name) => extract_unqualified_name(name),
        GranteeName::UserHost { user, host } => {
            if host.value != "%" {
                return Err(HtapError::Unsupported("only '%' host is supported".into()));
            }
            Ok(user.value.clone())
        }
    }
}

fn bind_create_user(stmt: &sqlparser::ast::CreateUser) -> Result<BoundStatement> {
    if stmt.or_replace
        || !stmt.options.options.is_empty()
        || stmt.with_tags
        || !stmt.tags.options.is_empty()
    {
        return Err(HtapError::Unsupported(
            "unsupported CREATE USER options".into(),
        ));
    }
    let password = stmt
        .identified_by
        .clone()
        .ok_or_else(|| HtapError::InvalidArgument("CREATE USER requires IDENTIFIED BY".into()))?;
    Ok(BoundStatement::CreateUser(CreateUserStatement {
        username: bind_grantee_name(&stmt.name)?,
        if_not_exists: stmt.if_not_exists,
        password: Some(password),
    }))
}

fn bind_alter_user(stmt: &sqlparser::ast::AlterUser) -> Result<BoundStatement> {
    let password = stmt
        .password
        .as_ref()
        .and_then(|password| {
            if password.encrypted {
                None
            } else {
                password.password.clone()
            }
        })
        .ok_or_else(|| HtapError::InvalidArgument("ALTER USER requires IDENTIFIED BY".into()))?;
    Ok(BoundStatement::AlterUser(AlterUserStatement {
        username: bind_grantee_name(&stmt.name)?,
        if_exists: stmt.if_exists,
        password,
    }))
}

fn bind_drop_user(if_exists: bool, names: &[GranteeName]) -> Result<BoundStatement> {
    if names.is_empty() {
        return Err(HtapError::InvalidArgument(
            "DROP USER requires at least one user".into(),
        ));
    }
    Ok(BoundStatement::DropUser(DropUserStatement {
        usernames: names
            .iter()
            .map(bind_grantee_name)
            .collect::<Result<Vec<_>>>()?,
        if_exists,
    }))
}

fn bind_privileges(privileges: &Privileges) -> Result<PrivilegeSet> {
    match privileges {
        Privileges::All { .. } => Ok(PrivilegeSet::ALL),
        Privileges::Actions(actions) => {
            let mut bits: u16 = 0;
            for action in actions {
                bits |= match action {
                    Action::Select { columns: None } => PrivilegeSet::SELECT.0,
                    Action::Insert { columns: None } => PrivilegeSet::INSERT.0,
                    Action::Update { columns: None } => PrivilegeSet::UPDATE.0,
                    Action::Delete => PrivilegeSet::DELETE.0,
                    Action::Create { .. } => PrivilegeSet::CREATE.0,
                    Action::Drop => PrivilegeSet::DROP.0,
                    Action::Select { columns: Some(_) }
                    | Action::Insert { columns: Some(_) }
                    | Action::Update { columns: Some(_) } => {
                        return Err(HtapError::Unsupported(
                            "column-level privileges are not supported".into(),
                        ));
                    }
                    other => {
                        return Err(HtapError::Unsupported(format!(
                            "unsupported privilege: {other}"
                        )));
                    }
                };
            }
            Ok(PrivilegeSet(bits))
        }
    }
}

fn bind_grantee(grantees: &[Grantee]) -> Result<String> {
    let [grantee] = grantees else {
        return Err(HtapError::Unsupported(
            "exactly one USER grantee is required".into(),
        ));
    };
    if !matches!(
        grantee.grantee_type,
        GranteesType::None | GranteesType::User
    ) {
        return Err(HtapError::Unsupported(
            "only USER grantees are supported".into(),
        ));
    }
    bind_grantee_name(
        grantee
            .name
            .as_ref()
            .ok_or_else(|| HtapError::InvalidArgument("USER grantee requires a username".into()))?,
    )
}

fn bind_grant_scope(
    objects: Option<&GrantObjects>,
    catalog: &CatalogSnapshot,
) -> Result<GrantScope> {
    match objects {
        None => Ok(GrantScope::Global),
        Some(GrantObjects::Databases(databases)) if databases.is_empty() => Ok(GrantScope::Global),
        Some(GrantObjects::AllTablesInSchema { schemas }) if schemas.len() == 1 => {
            let schema = extract_unqualified_name(&schemas[0])?;
            if schema == "htap" {
                Ok(GrantScope::Global)
            } else {
                Err(HtapError::NotFound(format!("schema '{schema}' not found")))
            }
        }
        Some(GrantObjects::Tables(tables)) if tables.len() == 1 => {
            if matches!(
                tables[0].0.as_slice(),
                [
                    ObjectNamePart::Identifier(schema),
                    ObjectNamePart::Identifier(table)
                ] if schema.value == "*" && table.value == "*"
            ) {
                return Ok(GrantScope::Global);
            }

            if let [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(table)] =
                tables[0].0.as_slice()
            {
                if table.value == "*" {
                    return if schema.value == "htap" {
                        Ok(GrantScope::Global)
                    } else {
                        Err(HtapError::NotFound(format!(
                            "schema '{}' not found",
                            schema.value
                        )))
                    };
                }

                if schema.value != "htap" {
                    return Err(HtapError::NotFound(format!(
                        "schema '{}' not found",
                        schema.value
                    )));
                }

                let table_name = table.value.clone();
                if catalog.table_by_name(&table_name).is_none() {
                    return Err(table_not_found(&table_name));
                }
                return Ok(GrantScope::Table(table_name));
            }

            let table = extract_unqualified_name(&tables[0])?;
            if catalog.table_by_name(&table).is_none() {
                return Err(table_not_found(&table));
            }
            Ok(GrantScope::Table(table))
        }
        Some(GrantObjects::Tables(_)) => Err(HtapError::Unsupported(
            "multiple tables in GRANT scope are not supported".into(),
        )),
        _ => Err(HtapError::Unsupported("unsupported GRANT scope".into())),
    }
}

fn bind_grant(
    privileges: &Privileges,
    objects: Option<&GrantObjects>,
    grantees: &[Grantee],
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    Ok(BoundStatement::GrantPrivileges(GrantStatement {
        privileges: bind_privileges(privileges)?,
        scope: bind_grant_scope(objects, catalog)?,
        grantee: bind_grantee(grantees)?,
    }))
}

fn bind_revoke(
    privileges: &Privileges,
    objects: Option<&GrantObjects>,
    grantees: &[Grantee],
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    Ok(BoundStatement::RevokePrivileges(RevokeStatement {
        privileges: bind_privileges(privileges)?,
        scope: bind_grant_scope(objects, catalog)?,
        grantee: bind_grantee(grantees)?,
    }))
}

fn bind_show_grants(for_username: Option<&GranteeName>) -> Result<BoundStatement> {
    Ok(BoundStatement::ShowGrants(ShowGrantsStatement {
        for_username: for_username.map(bind_grantee_name).transpose()?,
    }))
}

fn bind_analyze_table(
    stmt: &sqlparser::ast::Analyze,
    catalog: &CatalogSnapshot,
) -> Result<BoundStatement> {
    let table_name = stmt
        .table_name
        .as_ref()
        .ok_or_else(|| HtapError::InvalidArgument("ANALYZE requires a table name".into()))?;
    let table_name = extract_unqualified_name(table_name)?;

    if catalog.table_by_name(&table_name).is_none() {
        return Err(table_not_found(&table_name));
    }
    if stmt.for_columns {
        return Err(HtapError::Unsupported(
            "column lists in ANALYZE TABLE are not supported".into(),
        ));
    }
    if stmt.noscan {
        return Err(HtapError::Unsupported(
            "NOSCAN in ANALYZE TABLE is not supported".into(),
        ));
    }
    if stmt.partitions.is_some() {
        return Err(HtapError::Unsupported(
            "partition-scoped ANALYZE TABLE is not supported".into(),
        ));
    }

    Ok(BoundStatement::AnalyzeTable(table_name))
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

pub(crate) fn validate_table_descriptor_primary_key(table_desc: &TableDescriptor) -> Result<()> {
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

    let bound_partitioning = if let Some(ref mysql_part) = stmt.mysql_partition_by {
        Some(bind_mysql_partitioning(
            mysql_part,
            &schema,
            &final_pk_indices,
        )?)
    } else {
        None
    };

    let mut create = CreateTable::new(table_name, schema, final_pk_indices);
    if let Some(part) = bound_partitioning {
        create = create.with_partitioning(part);
    }
    Ok(BoundStatement::CreateTable(create))
}

fn bind_mysql_partitioning(
    mysql_part: &MysqlPartitionBy,
    schema: &Schema,
    pk_indices: &[usize],
) -> Result<BoundPartitioning> {
    match mysql_part {
        MysqlPartitionBy::Range {
            columns,
            expr,
            partitions,
        } => {
            let key_column = resolve_partition_key_column(
                columns.as_deref(),
                expr.as_ref(),
                schema,
                pk_indices,
            )?;
            let key_col_def = &schema.columns()[key_column];

            if partitions.is_empty() {
                return Err(HtapError::InvalidArgument(
                    "PARTITION BY RANGE must define at least one partition".into(),
                ));
            }

            let mut seen_names = std::collections::HashSet::new();
            let mut bound_partitions = Vec::with_capacity(partitions.len());
            let mut cur_lower: Option<Value> = None;
            let mut had_maxvalue = false;

            for part_def in partitions {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "partition name cannot be empty".into(),
                    ));
                }
                if !seen_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate partition name '{part_name}'"
                    )));
                }

                if had_maxvalue {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' defined after MAXVALUE partition"
                    )));
                }

                let bound = match &part_def.values {
                    MysqlPartitionValues::LessThan(b) => b,
                    MysqlPartitionValues::In(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in PARTITION BY RANGE must use VALUES LESS THAN"
                        )));
                    }
                };

                let cur_upper = match bound {
                    MysqlLessThanBound::MaxValue => {
                        had_maxvalue = true;
                        None
                    }
                    MysqlLessThanBound::Expr(bound_expr) => {
                        let val = parse_literal_value(bound_expr, key_col_def)?;
                        if val.is_null() {
                            return Err(HtapError::InvalidArgument(format!(
                                "range partition '{part_name}' bound cannot be NULL"
                            )));
                        }
                        if let Some(ref prev_val) = cur_lower {
                            if &val <= prev_val {
                                return Err(HtapError::InvalidArgument(format!(
                                    "VALUES LESS THAN value must be strictly increasing: {val} <= {prev_val}"
                                )));
                            }
                        }
                        Some(val)
                    }
                };

                bound_partitions.push(BoundRangePartition {
                    name: part_name,
                    lower: cur_lower.clone(),
                    upper: cur_upper.clone(),
                });

                cur_lower = cur_upper;
            }

            Ok(BoundPartitioning::Range {
                key_column,
                partitions: bound_partitions,
            })
        }
        MysqlPartitionBy::List {
            columns,
            expr,
            partitions,
        } => {
            let key_column = resolve_partition_key_column(
                columns.as_deref(),
                expr.as_ref(),
                schema,
                pk_indices,
            )?;
            let key_col_def = &schema.columns()[key_column];

            if partitions.is_empty() {
                return Err(HtapError::InvalidArgument(
                    "PARTITION BY LIST must define at least one partition".into(),
                ));
            }

            let mut seen_names = std::collections::HashSet::new();
            let mut seen_values = std::collections::HashSet::new();
            let mut bound_partitions = Vec::with_capacity(partitions.len());

            for part_def in partitions {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "partition name cannot be empty".into(),
                    ));
                }
                if !seen_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate partition name '{part_name}'"
                    )));
                }

                let exprs = match &part_def.values {
                    MysqlPartitionValues::In(exprs) => exprs,
                    MysqlPartitionValues::LessThan(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in PARTITION BY LIST must use VALUES IN"
                        )));
                    }
                };

                if exprs.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' has empty VALUES IN list"
                    )));
                }

                let mut part_values = Vec::with_capacity(exprs.len());
                let mut seen_in_part = std::collections::HashSet::new();
                for e in exprs {
                    let val = parse_literal_value(e, key_col_def)?;
                    if val.is_null() {
                        return Err(HtapError::InvalidArgument(format!(
                            "list partition '{part_name}' value cannot be NULL"
                        )));
                    }
                    if !seen_in_part.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' contains duplicate list value '{val}'"
                        )));
                    }
                    if !seen_values.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate list value '{val}' across partitions (found in partition '{part_name}')"
                        )));
                    }
                    part_values.push(val);
                }

                bound_partitions.push(BoundListPartition {
                    name: part_name,
                    values: part_values,
                });
            }

            Ok(BoundPartitioning::List {
                key_column,
                partitions: bound_partitions,
            })
        }
    }
}

fn resolve_partition_key_column(
    columns: Option<&[Ident]>,
    expr: Option<&Expr>,
    schema: &Schema,
    pk_indices: &[usize],
) -> Result<usize> {
    let col_name = if let Some(cols) = columns {
        if cols.len() != 1 {
            return Err(HtapError::Unsupported(
                "multi-column partitioning is not supported".into(),
            ));
        }
        &cols[0].value
    } else if let Some(e) = expr {
        match e {
            Expr::Identifier(ident) => &ident.value,
            _ => {
                return Err(HtapError::Unsupported(
                    "expressions in PARTITION BY are not supported, only column identifiers".into(),
                ));
            }
        }
    } else {
        return Err(HtapError::InvalidArgument(
            "PARTITION BY requires a column or expression".into(),
        ));
    };

    let col_idx = schema
        .columns()
        .iter()
        .position(|c| c.name == *col_name)
        .ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "partition key column '{col_name}' not found in schema"
            ))
        })?;

    if !pk_indices.contains(&col_idx) {
        return Err(HtapError::InvalidArgument(format!(
            "partition key column '{col_name}' must be part of the primary key"
        )));
    }

    if schema.columns()[col_idx].nullable {
        return Err(HtapError::InvalidArgument(format!(
            "partition key column '{col_name}' cannot be nullable"
        )));
    }

    Ok(col_idx)
}

fn bind_alter_table(stmt: &SqlAlterTable, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if stmt.if_exists {
        return Err(HtapError::InvalidArgument(
            "IF EXISTS is not supported for ALTER TABLE".into(),
        ));
    }
    if stmt.only {
        return Err(HtapError::Unsupported("ONLY is not supported".into()));
    }
    if stmt.on_cluster.is_some() {
        return Err(HtapError::Unsupported("ON CLUSTER is not supported".into()));
    }
    if stmt.location.is_some() {
        return Err(HtapError::Unsupported("LOCATION is not supported".into()));
    }
    if stmt.table_type.is_some() {
        return Err(HtapError::Unsupported("unsupported table type".into()));
    }

    let table_name = extract_unqualified_name(&stmt.name)?;
    let table_desc = catalog
        .table_by_name(&table_name)
        .ok_or_else(|| table_not_found(&table_name))?;

    if stmt.operations.is_empty() {
        return Err(HtapError::Unsupported(
            "ALTER TABLE requires an operation".into(),
        ));
    }
    if stmt.operations.len() != 1 {
        return Err(HtapError::Unsupported(
            "ALTER TABLE supports exactly one operation".into(),
        ));
    }

    let alteration = match &stmt.operations[0] {
        AlterTableOperation::AddPartition { partitions } => {
            bind_alter_add_partition(partitions, table_desc, catalog)?
        }
        AlterTableOperation::DropPartition { partitions } => {
            bind_alter_drop_partition(partitions, table_desc)?
        }
        AlterTableOperation::ReorganizePartition {
            partitions,
            into_partitions,
        } => bind_alter_reorganize_partition(partitions, into_partitions, table_desc, catalog)?,
        other => {
            return Err(HtapError::Unsupported(format!(
                "unsupported ALTER TABLE operation: {other}"
            )));
        }
    };

    Ok(BoundStatement::AlterPartitions(AlterPartitions::new(
        table_name, alteration,
    )))
}

fn bind_alter_add_partition(
    partitions: &[MysqlPartitionDef],
    table_desc: &TableDescriptor,
    catalog: &CatalogSnapshot,
) -> Result<PartitionAlteration> {
    let partitioning = table_desc.partitioning.as_ref().ok_or_else(|| {
        HtapError::InvalidArgument(format!("table '{}' is not partitioned", table_desc.name))
    })?;

    if partitions.is_empty() {
        return Err(HtapError::InvalidArgument(
            "ADD PARTITION requires at least one partition definition".into(),
        ));
    }

    let key_col = &table_desc.schema.columns()[partitioning.key_column];
    let mut part_defs = Vec::with_capacity(partitions.len());
    let mut seen_names = std::collections::HashSet::new();

    for &pid in &table_desc.partitions {
        if let Some(p) = catalog.partition(pid) {
            seen_names.insert(p.name.clone());
        }
    }

    match partitioning.method {
        PartitioningMethod::Range => {
            let mut cur_lower: Option<Value> = None;
            let mut had_maxvalue = false;

            for &pid in &table_desc.partitions {
                if let Some(part) = catalog.partition(pid) {
                    if let Some(range) = &part.range {
                        match &range.upper {
                            None => had_maxvalue = true,
                            Some(u) => {
                                if let Some(ref cur) = cur_lower {
                                    if u > cur {
                                        cur_lower = Some(u.clone());
                                    }
                                } else {
                                    cur_lower = Some(u.clone());
                                }
                            }
                        }
                    }
                }
            }

            if had_maxvalue {
                return Err(HtapError::InvalidArgument(format!(
                    "cannot add partition to table '{}': a MAXVALUE partition already exists",
                    table_desc.name
                )));
            }

            for part_def in partitions {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "partition name cannot be empty".into(),
                    ));
                }
                if !seen_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate partition name '{part_name}'"
                    )));
                }

                if had_maxvalue {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' defined after MAXVALUE partition"
                    )));
                }

                let bound = match &part_def.values {
                    MysqlPartitionValues::LessThan(b) => b,
                    MysqlPartitionValues::In(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in table '{}' with RANGE partitioning must use VALUES LESS THAN",
                            table_desc.name
                        )));
                    }
                };

                let cur_upper = match bound {
                    MysqlLessThanBound::MaxValue => {
                        had_maxvalue = true;
                        None
                    }
                    MysqlLessThanBound::Expr(bound_expr) => {
                        let val = parse_literal_value(bound_expr, key_col)?;
                        if val.is_null() {
                            return Err(HtapError::InvalidArgument(format!(
                                "range partition '{part_name}' bound cannot be NULL"
                            )));
                        }
                        if let Some(ref prev_val) = cur_lower {
                            if &val <= prev_val {
                                return Err(HtapError::InvalidArgument(format!(
                                    "VALUES LESS THAN value must be strictly increasing: {val} <= {prev_val}"
                                )));
                            }
                        }
                        Some(val)
                    }
                };

                part_defs.push(PartitionDefinition::Range(
                    RangePartitionDefinition::new_opt(
                        part_name,
                        cur_lower.clone(),
                        cur_upper.clone(),
                    ),
                ));

                cur_lower = cur_upper;
            }
        }
        PartitioningMethod::List => {
            let mut seen_values = std::collections::HashSet::new();
            for &pid in &table_desc.partitions {
                if let Some(part) = catalog.partition(pid) {
                    for v in &part.list_values {
                        seen_values.insert(v.clone());
                    }
                }
            }

            for part_def in partitions {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "partition name cannot be empty".into(),
                    ));
                }
                if !seen_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate partition name '{part_name}'"
                    )));
                }

                let exprs = match &part_def.values {
                    MysqlPartitionValues::In(exprs) => exprs,
                    MysqlPartitionValues::LessThan(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in table '{}' with LIST partitioning must use VALUES IN",
                            table_desc.name
                        )));
                    }
                };

                if exprs.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' has empty VALUES IN list"
                    )));
                }

                let mut part_values = Vec::with_capacity(exprs.len());
                let mut seen_in_part = std::collections::HashSet::new();
                for e in exprs {
                    let val = parse_literal_value(e, key_col)?;
                    if val.is_null() {
                        return Err(HtapError::InvalidArgument(format!(
                            "list partition '{part_name}' value cannot be NULL"
                        )));
                    }
                    if !seen_in_part.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' contains duplicate list value '{val}'"
                        )));
                    }
                    if !seen_values.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate list value '{val}' across partitions (found in partition '{part_name}')"
                        )));
                    }
                    part_values.push(val);
                }

                part_defs.push(PartitionDefinition::List(ListPartitionDefinition::new(
                    part_name,
                    part_values,
                )));
            }
        }
    }

    Ok(PartitionAlteration::Add {
        partitions: part_defs,
    })
}

fn bind_alter_drop_partition(
    partitions: &[Ident],
    table_desc: &TableDescriptor,
) -> Result<PartitionAlteration> {
    let _partitioning = table_desc.partitioning.as_ref().ok_or_else(|| {
        HtapError::InvalidArgument(format!("table '{}' is not partitioned", table_desc.name))
    })?;

    if partitions.is_empty() {
        return Err(HtapError::InvalidArgument(
            "DROP PARTITION requires at least one partition name".into(),
        ));
    }

    let mut names = Vec::with_capacity(partitions.len());
    let mut seen = std::collections::HashSet::new();
    for ident in partitions {
        let name = ident.value.clone();
        if name.is_empty() {
            return Err(HtapError::InvalidArgument(
                "partition name cannot be empty".into(),
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(HtapError::InvalidArgument(format!(
                "duplicate partition name '{name}' in DROP PARTITION"
            )));
        }
        names.push(name);
    }

    Ok(PartitionAlteration::Drop { partitions: names })
}

fn bind_alter_reorganize_partition(
    sources: &[Ident],
    targets: &[MysqlPartitionDef],
    table_desc: &TableDescriptor,
    catalog: &CatalogSnapshot,
) -> Result<PartitionAlteration> {
    let partitioning = table_desc.partitioning.as_ref().ok_or_else(|| {
        HtapError::InvalidArgument(format!("table '{}' is not partitioned", table_desc.name))
    })?;

    if sources.is_empty() {
        return Err(HtapError::InvalidArgument(
            "REORGANIZE PARTITION requires at least one source partition".into(),
        ));
    }
    if targets.is_empty() {
        return Err(HtapError::InvalidArgument(
            "REORGANIZE PARTITION requires at least one target partition definition".into(),
        ));
    }

    let key_col = &table_desc.schema.columns()[partitioning.key_column];

    let mut source_names = Vec::with_capacity(sources.len());
    let mut seen_source_names = std::collections::HashSet::new();
    for ident in sources {
        let name = ident.value.clone();
        if name.is_empty() {
            return Err(HtapError::InvalidArgument(
                "source partition name cannot be empty".into(),
            ));
        }
        if !seen_source_names.insert(name.clone()) {
            return Err(HtapError::InvalidArgument(format!(
                "duplicate source partition name '{name}' in REORGANIZE PARTITION"
            )));
        }
        source_names.push(name);
    }

    let mut source_positions: Vec<usize> = Vec::with_capacity(source_names.len());
    for s in &source_names {
        let pos = table_desc
            .partitions
            .iter()
            .position(|&pid| {
                catalog
                    .partition(pid)
                    .map(|p| p.name == *s)
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                HtapError::NotFound(format!(
                    "source partition '{s}' not found in table '{}'",
                    table_desc.name
                ))
            })?;
        source_positions.push(pos);
    }

    source_positions.sort_unstable();
    let min_pos = source_positions[0];
    let max_pos = *source_positions.last().unwrap();
    if max_pos - min_pos + 1 != source_positions.len() {
        return Err(HtapError::InvalidArgument(format!(
            "reorganize partition sources must be contiguous in table '{}', got non-contiguous positions {:?}",
            table_desc.name, source_positions
        )));
    }

    let mut target_defs = Vec::with_capacity(targets.len());
    let mut seen_target_names = std::collections::HashSet::new();

    for &pid in &table_desc.partitions {
        if let Some(p) = catalog.partition(pid) {
            if !seen_source_names.contains(&p.name) {
                seen_target_names.insert(p.name.clone());
            }
        }
    }

    match partitioning.method {
        PartitioningMethod::Range => {
            let first_pos = source_positions[0];
            let first_pid = table_desc.partitions[first_pos];
            let first_part = catalog.partition(first_pid).unwrap();
            let initial_lower = first_part.range.as_ref().and_then(|r| r.lower.clone());

            let mut cur_lower = initial_lower;
            let mut had_maxvalue = false;

            for part_def in targets {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "target partition name cannot be empty".into(),
                    ));
                }
                if !seen_target_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate target partition name '{part_name}'"
                    )));
                }

                if had_maxvalue {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' defined after MAXVALUE partition"
                    )));
                }

                let bound = match &part_def.values {
                    MysqlPartitionValues::LessThan(b) => b,
                    MysqlPartitionValues::In(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in table '{}' with RANGE partitioning must use VALUES LESS THAN",
                            table_desc.name
                        )));
                    }
                };

                let cur_upper = match bound {
                    MysqlLessThanBound::MaxValue => {
                        had_maxvalue = true;
                        None
                    }
                    MysqlLessThanBound::Expr(bound_expr) => {
                        let val = parse_literal_value(bound_expr, key_col)?;
                        if val.is_null() {
                            return Err(HtapError::InvalidArgument(format!(
                                "range partition '{part_name}' bound cannot be NULL"
                            )));
                        }
                        if let Some(ref prev_val) = cur_lower {
                            if &val <= prev_val {
                                return Err(HtapError::InvalidArgument(format!(
                                    "VALUES LESS THAN value must be strictly increasing: {val} <= {prev_val}"
                                )));
                            }
                        }
                        Some(val)
                    }
                };

                target_defs.push(PartitionDefinition::Range(
                    RangePartitionDefinition::new_opt(
                        part_name,
                        cur_lower.clone(),
                        cur_upper.clone(),
                    ),
                ));

                cur_lower = cur_upper;
            }
        }
        PartitioningMethod::List => {
            let mut seen_values = std::collections::HashSet::new();
            for &pid in &table_desc.partitions {
                if let Some(part) = catalog.partition(pid) {
                    if !seen_source_names.contains(&part.name) {
                        for v in &part.list_values {
                            seen_values.insert(v.clone());
                        }
                    }
                }
            }

            for part_def in targets {
                let part_name = part_def.name.value.clone();
                if part_name.is_empty() {
                    return Err(HtapError::InvalidArgument(
                        "target partition name cannot be empty".into(),
                    ));
                }
                if !seen_target_names.insert(part_name.clone()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate target partition name '{part_name}'"
                    )));
                }

                let exprs = match &part_def.values {
                    MysqlPartitionValues::In(exprs) => exprs,
                    MysqlPartitionValues::LessThan(_) => {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' in table '{}' with LIST partitioning must use VALUES IN",
                            table_desc.name
                        )));
                    }
                };

                if exprs.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{part_name}' has empty VALUES IN list"
                    )));
                }

                let mut part_values = Vec::with_capacity(exprs.len());
                let mut seen_in_part = std::collections::HashSet::new();
                for e in exprs {
                    let val = parse_literal_value(e, key_col)?;
                    if val.is_null() {
                        return Err(HtapError::InvalidArgument(format!(
                            "list partition '{part_name}' value cannot be NULL"
                        )));
                    }
                    if !seen_in_part.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{part_name}' contains duplicate list value '{val}'"
                        )));
                    }
                    if !seen_values.insert(val.clone()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate list value '{val}' across partitions (found in partition '{part_name}')"
                        )));
                    }
                    part_values.push(val);
                }

                target_defs.push(PartitionDefinition::List(ListPartitionDefinition::new(
                    part_name,
                    part_values,
                )));
            }
        }
    }

    Ok(PartitionAlteration::Reorganize {
        sources: source_names,
        targets: target_defs,
    })
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

pub(crate) fn parse_hex_bytes(s: &str, col_name: &str) -> Result<Vec<u8>> {
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
                // Narrow coercion: MySQL clients commonly send BOOL columns as the integer
                // literals 0/1 (TINYINT(1) convention); accept exactly those two spellings.
                if let sqlparser::ast::Value::Number(s, _) = &v.value {
                    match s.as_str() {
                        "0" => return Ok(Value::Bool(false)),
                        "1" => return Ok(Value::Bool(true)),
                        _ => {}
                    }
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
                match &v.value {
                    sqlparser::ast::Value::HexStringLiteral(s) => {
                        let bytes = parse_hex_bytes(s, &col_def.name)?;
                        return Ok(Value::Bytes(bytes));
                    }
                    // Narrow coercion (bytes-vs-string codec fix, Phase 11): a bound parameter
                    // whose raw bytes happen to be valid UTF-8 decodes as `Value::String` and
                    // substitutes as a single-quoted string literal, not a hex literal (see
                    // `htap_wire::binary_codec::decode_execute`'s doc comment on the client-driven
                    // `String`-vs-`Vec<u8>` ambiguity). Accept it here too, encoded as UTF-8, so
                    // such a parameter can still bind into a BYTES column exactly as if the
                    // client had sent the equivalent hex literal.
                    sqlparser::ast::Value::SingleQuotedString(s)
                    | sqlparser::ast::Value::DoubleQuotedString(s) => {
                        return Ok(Value::Bytes(s.as_bytes().to_vec()));
                    }
                    _ => {}
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
        .ok_or_else(|| table_not_found(&table_name))?;

    validate_table_descriptor_primary_key(table_desc)?;

    if insert.columns.is_empty() {
        return Err(HtapError::InvalidArgument(
            "INSERT statement requires an explicit column list".into(),
        ));
    }

    let query = insert.source.as_ref().ok_or_else(|| {
        HtapError::InvalidArgument("INSERT statement missing source query".into())
    })?;

    let values = match &*query.body {
        SetExpr::Values(values) => Some(values),
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::SetOperation { .. } => None,
        _ => {
            return Err(HtapError::Unsupported("unsupported INSERT source".into()));
        }
    };

    if let Some(values) = values {
        if query.with.is_some() {
            return Err(HtapError::Unsupported(
                "CTEs not supported in INSERT VALUES".into(),
            ));
        }
        if query.order_by.is_some() || query.limit_clause.is_some() {
            return Err(HtapError::Unsupported(
                "ORDER BY / LIMIT not supported in INSERT VALUES".into(),
            ));
        }
        if values.rows.is_empty() {
            return Err(HtapError::InvalidArgument(
                "INSERT VALUES cannot be empty".into(),
            ));
        }
    }

    // Check missing columns: every schema column must be present in insert.columns.
    // This is validated before binding an INSERT ... SELECT source.
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

    if values.is_none() {
        let bound_query = crate::binder_query::bind_query(query, catalog)?;
        let targets = col_index_in_schema
            .iter()
            .map(|&index| {
                table_desc
                    .schema
                    .column(index)
                    .expect("INSERT column index validated")
                    .clone()
            })
            .collect::<Vec<_>>();
        crate::binder_query::validate_insert_query_types(&bound_query, &targets)?;

        return Ok(BoundStatement::Insert(Insert::from_query(
            table_name,
            bound_query,
            col_index_in_schema,
        )));
    }

    let values = values.expect("literal INSERT source validated");
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

pub(crate) fn bind_pk_where_predicate(
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

/// Binds a `SELECT`.
///
/// Statements whose shape fits the narrow single-table slice (see
/// [`is_narrow_select_shape`]) keep using the strict point/analytic binders, so complete-PK
/// point reads still bind to [`PointSelect`] and narrow scans to [`AnalyticSelect`] with
/// partition pruning and predicate pushdown. Everything else binds through the general
/// query binder. The shape test is purely syntactic and evaluated before any deep binding;
/// a narrow-shaped statement that fails deep binding reports that error rather than
/// falling through.
fn bind_select(query: &Query, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if is_narrow_select_shape(query) {
        return bind_narrow_select(query, catalog);
    }
    crate::binder_query::bind_query(query, catalog)
        .map(|query| BoundStatement::Query(Box::new(query)))
}

/// Whether a query has the shape handled by the narrow point/analytic binders:
/// one unaliased table, no joins/CTEs/subqueries/set operations, no LIMIT/HAVING/DISTINCT,
/// a projection of plain columns or `COUNT/SUM/MIN/MAX` over a plain column, an AND-only
/// filter of `column op literal` / `IS [NOT] NULL` leaves, and GROUP BY / ORDER BY of plain
/// unqualified columns. A `@`/`@@`-prefixed identifier anywhere in this shape (user or system
/// variable) always forces `false`: the narrow binders resolve every plain identifier as a
/// column and have no notion of [`crate::expr::Expr::Variable`].
pub fn is_narrow_select_shape(query: &Query) -> bool {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return false;
    }
    let select = match &*query.body {
        SetExpr::Select(s) => s,
        _ => return false,
    };
    if select.distinct.is_some()
        || select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.exclude.is_some()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.connect_by.is_empty()
    {
        return false;
    }
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return false;
    }
    match &select.from[0].relation {
        TableFactor::Table { alias: None, .. } => {}
        _ => return false,
    }
    // A leading `@`/`@@` marks a user/system variable (`@x`, `@@autocommit`), not a column;
    // such statements always fall through to the general query path, which knows how to
    // evaluate `Expr::Variable`.
    let is_ident =
        |e: &Expr| matches!(unnest(e), Expr::Identifier(ident) if !ident.value.starts_with('@'));
    let is_literal = |e: &Expr| match unnest(e) {
        Expr::Value(_) => true,
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } => matches!(unnest(expr), Expr::Value(_)),
        _ => false,
    };
    if select.projection.is_empty() {
        return false;
    }
    for item in &select.projection {
        let ok = match item {
            SelectItem::Wildcard(_) => true,
            SelectItem::UnnamedExpr(expr) => match unnest(expr) {
                Expr::Identifier(ident) => !ident.value.starts_with('@'),
                Expr::Function(func) => {
                    let name = match func.name.0.as_slice() {
                        [ObjectNamePart::Identifier(ident)] => ident.value.to_ascii_uppercase(),
                        _ => return false,
                    };
                    let list = match &func.args {
                        FunctionArguments::List(list) => list,
                        _ => return false,
                    };
                    func.over.is_none()
                        && func.filter.is_none()
                        && list.duplicate_treatment.is_none()
                        && list.clauses.is_empty()
                        && list.args.len() == 1
                        && match (&list.args[0], name.as_str()) {
                            (FunctionArg::Unnamed(FunctionArgExpr::Wildcard), "COUNT") => true,
                            (
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)),
                                "COUNT" | "SUM" | "MIN" | "MAX",
                            ) => is_ident(arg),
                            _ => false,
                        }
                }
                _ => false,
            },
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    fn unnest(e: &Expr) -> &Expr {
        let mut cur = e;
        while let Expr::Nested(inner) = cur {
            cur = inner;
        }
        cur
    }
    fn is_narrow_filter(
        e: &Expr,
        is_ident: &dyn Fn(&Expr) -> bool,
        is_literal: &dyn Fn(&Expr) -> bool,
    ) -> bool {
        match unnest(e) {
            Expr::BinaryOp {
                left,
                op: sqlparser::ast::BinaryOperator::And,
                right,
            } => {
                is_narrow_filter(left, is_ident, is_literal)
                    && is_narrow_filter(right, is_ident, is_literal)
            }
            Expr::BinaryOp { left, op, right } => {
                matches!(
                    op,
                    sqlparser::ast::BinaryOperator::Eq
                        | sqlparser::ast::BinaryOperator::NotEq
                        | sqlparser::ast::BinaryOperator::Lt
                        | sqlparser::ast::BinaryOperator::LtEq
                        | sqlparser::ast::BinaryOperator::Gt
                        | sqlparser::ast::BinaryOperator::GtEq
                ) && is_ident(left)
                    && is_literal(right)
            }
            Expr::IsNull(inner) | Expr::IsNotNull(inner) => is_ident(inner),
            _ => false,
        }
    }
    if let Some(sel) = &select.selection {
        if !is_narrow_filter(sel, &is_ident, &is_literal) {
            return false;
        }
    }
    match &select.group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() || !exprs.iter().all(&is_ident) {
                return false;
            }
        }
        GroupByExpr::All(_) => return false,
    }
    if let Some(order_by) = &query.order_by {
        if order_by.interpolate.is_some() {
            return false;
        }
        match &order_by.kind {
            sqlparser::ast::OrderByKind::Expressions(items) => {
                if !items
                    .iter()
                    .all(|i| i.with_fill.is_none() && is_ident(&i.expr))
                {
                    return false;
                }
            }
            sqlparser::ast::OrderByKind::All(_) => return false,
        }
    }
    true
}

fn bind_narrow_select(query: &Query, catalog: &CatalogSnapshot) -> Result<BoundStatement> {
    if query.with.is_some() {
        return Err(HtapError::Unsupported(
            "CTEs (WITH clause) not supported".into(),
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
        .ok_or_else(|| table_not_found(&table_name))?;

    validate_table_descriptor_primary_key(table_desc)?;

    // Validate projection
    if select.projection.is_empty() {
        return Err(HtapError::InvalidArgument(
            "SELECT projection cannot be empty".into(),
        ));
    }

    let has_group_by = match &select.group_by {
        GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
        GroupByExpr::All(_) => true,
    };

    let has_order_by = query.order_by.is_some();

    let is_candidate = !has_order_by
        && !has_group_by
        && select.selection.is_some()
        && is_simple_or_wildcard_projection(&select.projection)
        && is_pk_equality_where(select.selection.as_ref().unwrap(), table_desc);

    if is_candidate {
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
        return Ok(BoundStatement::Select(PointSelect::new(
            table_name, projection, key,
        )));
    }

    // Otherwise, bind as AnalyticSelect:
    let projection = bind_analytic_projection(&select.projection, table_desc, &table_name)?;
    let group_by = bind_analytic_group_by(&select.group_by, table_desc)?;

    // Validate grouping rules
    let has_aggregates = projection.iter().any(|e| e.is_aggregate());
    if has_aggregates {
        for expr in &projection {
            if let AnalyticExpr::Column { index, name, .. } = expr {
                if !group_by.contains(index) {
                    return Err(HtapError::InvalidArgument(format!(
                        "column '{name}' must appear in the GROUP BY clause or be used in an aggregate function"
                    )));
                }
            }
        }
    } else if !group_by.is_empty() {
        for expr in &projection {
            if let AnalyticExpr::Column { index, name, .. } = expr {
                if !group_by.contains(index) {
                    return Err(HtapError::InvalidArgument(format!(
                        "column '{name}' must appear in the GROUP BY clause"
                    )));
                }
            }
        }
    }

    let order_by = bind_analytic_order_by(
        query.order_by.as_ref(),
        table_desc,
        &group_by,
        has_aggregates,
    )?;

    let filter = bind_analytic_filter(select.selection.as_ref(), table_desc)?;

    let output_columns: Vec<CommonColumnDef> =
        projection.iter().map(|e| e.to_column_def()).collect();
    let output_schema = Schema::new(output_columns)?;

    Ok(BoundStatement::AnalyticSelect(AnalyticSelect::new(
        table_name,
        projection,
        filter,
        group_by,
        order_by,
        output_schema,
    )))
}

fn is_simple_or_wildcard_projection(projection: &[SelectItem]) -> bool {
    if projection.is_empty() {
        return false;
    }
    if projection.len() == 1 && matches!(projection[0], SelectItem::Wildcard(_)) {
        return true;
    }
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(Expr::Identifier(_)) => {}
            _ => return false,
        }
    }
    true
}

pub(crate) fn is_pk_equality_where(expr: &Expr, table_desc: &TableDescriptor) -> bool {
    let mut leaves = Vec::new();
    if collect_and_leaves(expr, &mut leaves).is_err() {
        return false;
    }
    if leaves.len() != table_desc.primary_key.len() {
        return false;
    }
    let mut seen_pk = std::collections::HashSet::new();
    for leaf in leaves {
        let mut unnested = leaf;
        while let Expr::Nested(inner) = unnested {
            unnested = inner;
        }
        match unnested {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } => {
                let mut l = &**left;
                while let Expr::Nested(inner) = l {
                    l = inner;
                }
                let mut r = &**right;
                while let Expr::Nested(inner) = r {
                    r = inner;
                }
                if matches!(l, Expr::Value(_)) || matches!(r, Expr::Identifier(_)) {
                    return false;
                }
                match l {
                    Expr::Identifier(ident) => {
                        if let Some(idx) = table_desc.schema.column_index(&ident.value) {
                            if table_desc.primary_key.contains(&idx) && seen_pk.insert(idx) {
                                continue;
                            }
                        }
                        return false;
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    seen_pk.len() == table_desc.primary_key.len()
}

fn bind_analytic_projection(
    items: &[SelectItem],
    table_desc: &TableDescriptor,
    table_name: &str,
) -> Result<Vec<AnalyticExpr>> {
    if items.len() == 1 && matches!(items[0], SelectItem::Wildcard(_)) {
        return Ok((0..table_desc.schema.len())
            .map(|i| {
                let col = table_desc.schema.column(i).unwrap();
                AnalyticExpr::Column {
                    index: i,
                    name: col.name.clone(),
                    data_type: col.data_type,
                    nullable: col.nullable,
                }
            })
            .collect());
    }

    let mut projection = Vec::with_capacity(items.len());
    for item in items {
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
                    let col_idx =
                        table_desc
                            .schema
                            .column_index(&ident.value)
                            .ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{}' in table '{table_name}'",
                                    ident.value
                                ))
                            })?;
                    let col = table_desc.schema.column(col_idx).unwrap();
                    projection.push(AnalyticExpr::Column {
                        index: col_idx,
                        name: col.name.clone(),
                        data_type: col.data_type,
                        nullable: col.nullable,
                    });
                }
                Expr::CompoundIdentifier(_) => {
                    return Err(HtapError::Unsupported(
                        "qualified column names not supported in projection".into(),
                    ));
                }
                Expr::Function(func) => {
                    let expr = bind_analytic_function(func, table_desc, table_name)?;
                    projection.push(expr);
                }
                _ => {
                    return Err(HtapError::Unsupported(
                        "expressions not supported in projection".into(),
                    ));
                }
            },
        }
    }
    Ok(projection)
}

fn bind_analytic_function(
    func: &sqlparser::ast::Function,
    table_desc: &TableDescriptor,
    table_name: &str,
) -> Result<AnalyticExpr> {
    let func_name = extract_unqualified_name(&func.name)?.to_ascii_uppercase();
    if func.over.is_some() {
        return Err(HtapError::Unsupported(
            "window functions (OVER clause) not supported".into(),
        ));
    }
    if func.filter.is_some() {
        return Err(HtapError::Unsupported(
            "aggregate FILTER clause not supported".into(),
        ));
    }
    let list = match &func.args {
        FunctionArguments::List(list) => list,
        FunctionArguments::None => {
            return Err(HtapError::InvalidArgument(format!(
                "function '{func_name}' requires arguments"
            )));
        }
        FunctionArguments::Subquery(_) => {
            return Err(HtapError::Unsupported(
                "subqueries in function arguments not supported".into(),
            ));
        }
    };
    if list.duplicate_treatment == Some(DuplicateTreatment::Distinct) {
        return Err(HtapError::Unsupported(
            "DISTINCT in aggregate functions not supported".into(),
        ));
    }
    if !list.clauses.is_empty() {
        return Err(HtapError::Unsupported(
            "clauses in aggregate functions not supported".into(),
        ));
    }

    match func_name.as_str() {
        "COUNT" => {
            if list.args.len() != 1 {
                return Err(HtapError::InvalidArgument(format!(
                    "COUNT requires exactly 1 argument, found {}",
                    list.args.len()
                )));
            }
            match &list.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Ok(AnalyticExpr::Aggregate {
                    function: AggregateFunction::CountStar,
                    column_index: None,
                    name: "COUNT(*)".to_string(),
                    data_type: CommonDataType::Int64,
                    nullable: false,
                }),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(ident))) => {
                    let col_idx =
                        table_desc
                            .schema
                            .column_index(&ident.value)
                            .ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{}' in table '{table_name}'",
                                    ident.value
                                ))
                            })?;
                    Ok(AnalyticExpr::Aggregate {
                        function: AggregateFunction::Count,
                        column_index: Some(col_idx),
                        name: format!("COUNT({})", ident.value),
                        data_type: CommonDataType::Int64,
                        nullable: false,
                    })
                }
                _ => Err(HtapError::InvalidArgument(
                    "COUNT argument must be '*' or a column identifier".into(),
                )),
            }
        }
        "SUM" => {
            if list.args.len() != 1 {
                return Err(HtapError::InvalidArgument(format!(
                    "SUM requires exactly 1 argument, found {}",
                    list.args.len()
                )));
            }
            match &list.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(ident))) => {
                    let col_idx =
                        table_desc
                            .schema
                            .column_index(&ident.value)
                            .ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{}' in table '{table_name}'",
                                    ident.value
                                ))
                            })?;
                    let col = table_desc.schema.column(col_idx).unwrap();
                    let (data_type, name) = match col.data_type {
                        CommonDataType::Int32 | CommonDataType::Int64 => {
                            (CommonDataType::Int64, format!("SUM({})", ident.value))
                        }
                        CommonDataType::Float64 => {
                            (CommonDataType::Float64, format!("SUM({})", ident.value))
                        }
                        other => {
                            return Err(HtapError::InvalidArgument(format!(
                                "SUM cannot be applied to non-numeric column '{}' of type {other:?}",
                                ident.value
                            )));
                        }
                    };
                    Ok(AnalyticExpr::Aggregate {
                        function: AggregateFunction::Sum,
                        column_index: Some(col_idx),
                        name,
                        data_type,
                        nullable: true,
                    })
                }
                _ => Err(HtapError::InvalidArgument(
                    "SUM argument must be a column identifier".into(),
                )),
            }
        }
        "MIN" => {
            if list.args.len() != 1 {
                return Err(HtapError::InvalidArgument(format!(
                    "MIN requires exactly 1 argument, found {}",
                    list.args.len()
                )));
            }
            match &list.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(ident))) => {
                    let col_idx =
                        table_desc
                            .schema
                            .column_index(&ident.value)
                            .ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{}' in table '{table_name}'",
                                    ident.value
                                ))
                            })?;
                    let col = table_desc.schema.column(col_idx).unwrap();
                    Ok(AnalyticExpr::Aggregate {
                        function: AggregateFunction::Min,
                        column_index: Some(col_idx),
                        name: format!("MIN({})", ident.value),
                        data_type: col.data_type,
                        nullable: true,
                    })
                }
                _ => Err(HtapError::InvalidArgument(
                    "MIN argument must be a column identifier".into(),
                )),
            }
        }
        "MAX" => {
            if list.args.len() != 1 {
                return Err(HtapError::InvalidArgument(format!(
                    "MAX requires exactly 1 argument, found {}",
                    list.args.len()
                )));
            }
            match &list.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(ident))) => {
                    let col_idx =
                        table_desc
                            .schema
                            .column_index(&ident.value)
                            .ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "unknown column '{}' in table '{table_name}'",
                                    ident.value
                                ))
                            })?;
                    let col = table_desc.schema.column(col_idx).unwrap();
                    Ok(AnalyticExpr::Aggregate {
                        function: AggregateFunction::Max,
                        column_index: Some(col_idx),
                        name: format!("MAX({})", ident.value),
                        data_type: col.data_type,
                        nullable: true,
                    })
                }
                _ => Err(HtapError::InvalidArgument(
                    "MAX argument must be a column identifier".into(),
                )),
            }
        }
        "AVG" => Err(HtapError::Unsupported(
            "AVG aggregate function is not supported".into(),
        )),
        other => Err(HtapError::Unsupported(format!(
            "unsupported aggregate function: '{other}'"
        ))),
    }
}

fn bind_analytic_group_by(
    group_by: &GroupByExpr,
    table_desc: &TableDescriptor,
) -> Result<Vec<usize>> {
    match group_by {
        GroupByExpr::All(_) => Err(HtapError::Unsupported(
            "GROUP BY ALL is not supported".into(),
        )),
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(HtapError::Unsupported(
                    "GROUP BY modifiers not supported".into(),
                ));
            }
            let mut indices = Vec::with_capacity(exprs.len());
            let mut seen = std::collections::HashSet::with_capacity(exprs.len());
            for expr in exprs {
                let col_name = match expr {
                    Expr::Identifier(ident) => &ident.value,
                    Expr::CompoundIdentifier(_) => {
                        return Err(HtapError::Unsupported(
                            "qualified column names not supported in GROUP BY".into(),
                        ));
                    }
                    _ => {
                        return Err(HtapError::InvalidArgument(format!(
                            "GROUP BY expression must be a simple column identifier, found: {expr}"
                        )));
                    }
                };
                let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
                    HtapError::InvalidArgument(format!("unknown column '{col_name}' in GROUP BY"))
                })?;
                if !seen.insert(col_idx) {
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate column '{col_name}' in GROUP BY"
                    )));
                }
                indices.push(col_idx);
            }
            Ok(indices)
        }
    }
}

fn bind_analytic_order_by(
    order_by: Option<&sqlparser::ast::OrderBy>,
    table_desc: &TableDescriptor,
    group_by: &[usize],
    has_aggregates: bool,
) -> Result<Vec<AnalyticOrderBy>> {
    let order_by = match order_by {
        Some(ob) => ob,
        None => return Ok(Vec::new()),
    };

    if order_by.interpolate.is_some() {
        return Err(HtapError::Unsupported(
            "INTERPOLATE not supported in ORDER BY".into(),
        ));
    }

    let exprs = match &order_by.kind {
        sqlparser::ast::OrderByKind::Expressions(exprs) => exprs,
        sqlparser::ast::OrderByKind::All(_) => {
            return Err(HtapError::Unsupported("ORDER BY ALL not supported".into()));
        }
    };

    let mut result = Vec::with_capacity(exprs.len());
    for ob_expr in exprs {
        if ob_expr.with_fill.is_some() {
            return Err(HtapError::Unsupported(
                "WITH FILL not supported in ORDER BY".into(),
            ));
        }

        let col_name = match &ob_expr.expr {
            Expr::Identifier(ident) => &ident.value,
            Expr::CompoundIdentifier(_) => {
                return Err(HtapError::Unsupported(
                    "qualified column names not supported in ORDER BY".into(),
                ));
            }
            Expr::Function(_) => {
                return Err(HtapError::Unsupported(
                    "aggregate ordering not supported in ORDER BY".into(),
                ));
            }
            _ => {
                return Err(HtapError::Unsupported(
                    "expressions not supported in ORDER BY".into(),
                ));
            }
        };

        let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
            HtapError::InvalidArgument(format!("unknown column '{col_name}' in ORDER BY"))
        })?;

        if (!group_by.is_empty() || has_aggregates) && !group_by.contains(&col_idx) {
            return Err(HtapError::InvalidArgument(format!(
                "column '{col_name}' must appear in the GROUP BY clause"
            )));
        }

        let asc = ob_expr.options.asc.unwrap_or(true);
        // Explicit deterministic policy:
        // When nulls_first is specified, use it.
        // Otherwise: ASC -> NULLS FIRST, DESC -> NULLS LAST.
        let nulls_first = ob_expr.options.nulls_first.unwrap_or(asc);

        result.push(AnalyticOrderBy::new(col_idx, asc, nulls_first));
    }

    Ok(result)
}

fn bind_analytic_filter(
    selection: Option<&Expr>,
    table_desc: &TableDescriptor,
) -> Result<Option<AnalyticFilter>> {
    let expr = match selection {
        Some(e) => e,
        None => return Ok(None),
    };

    let mut leaves = Vec::new();
    collect_and_leaves(expr, &mut leaves)?;

    if leaves.is_empty() {
        return Err(HtapError::InvalidArgument("empty WHERE clause".into()));
    }

    let mut seen_eq_cols = std::collections::HashSet::new();
    let mut bound_leaves = Vec::with_capacity(leaves.len());

    for leaf in leaves {
        let mut unnested = leaf;
        while let Expr::Nested(inner) = unnested {
            unnested = inner;
        }

        match unnested {
            Expr::IsNull(inner) => {
                let mut col_expr = &**inner;
                while let Expr::Nested(i) = col_expr {
                    col_expr = i;
                }
                let col_name = match col_expr {
                    Expr::Identifier(ident) => &ident.value,
                    Expr::CompoundIdentifier(_) => {
                        return Err(HtapError::Unsupported(
                            "qualified column names not supported in WHERE clause".into(),
                        ));
                    }
                    _ => {
                        return Err(HtapError::InvalidArgument(
                            "IS NULL requires a column identifier".into(),
                        ));
                    }
                };
                let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "unknown column '{col_name}' in table '{}'",
                        table_desc.name
                    ))
                })?;
                bound_leaves.push(AnalyticFilter::IsNull { column: col_idx });
            }
            Expr::IsNotNull(inner) => {
                let mut col_expr = &**inner;
                while let Expr::Nested(i) = col_expr {
                    col_expr = i;
                }
                let col_name = match col_expr {
                    Expr::Identifier(ident) => &ident.value,
                    Expr::CompoundIdentifier(_) => {
                        return Err(HtapError::Unsupported(
                            "qualified column names not supported in WHERE clause".into(),
                        ));
                    }
                    _ => {
                        return Err(HtapError::InvalidArgument(
                            "IS NOT NULL requires a column identifier".into(),
                        ));
                    }
                };
                let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "unknown column '{col_name}' in table '{}'",
                        table_desc.name
                    ))
                })?;
                bound_leaves.push(AnalyticFilter::IsNotNull { column: col_idx });
            }
            Expr::BinaryOp { left, op, right } => {
                let comp_op = match op {
                    BinaryOperator::Eq => ComparisonOp::Eq,
                    BinaryOperator::NotEq => ComparisonOp::NotEq,
                    BinaryOperator::Lt => ComparisonOp::Lt,
                    BinaryOperator::LtEq => ComparisonOp::Lte,
                    BinaryOperator::Gt => ComparisonOp::Gt,
                    BinaryOperator::GtEq => ComparisonOp::Gte,
                    BinaryOperator::Or => {
                        return Err(HtapError::InvalidArgument(
                            "OR is not supported in WHERE clause".into(),
                        ));
                    }
                    other => {
                        return Err(HtapError::InvalidArgument(format!(
                            "unsupported operator in WHERE clause: '{other}'"
                        )));
                    }
                };

                let mut l = &**left;
                while let Expr::Nested(inner) = l {
                    l = inner;
                }
                let mut r = &**right;
                while let Expr::Nested(inner) = r {
                    r = inner;
                }

                if matches!(l, Expr::Value(_)) || matches!(r, Expr::Identifier(_)) {
                    if matches!(l, Expr::Identifier(_)) && matches!(r, Expr::Identifier(_)) {
                        return Err(HtapError::InvalidArgument(
                            "cross-column comparisons not supported in WHERE clause".into(),
                        ));
                    }
                    return Err(HtapError::InvalidArgument(
                        "reversed operands in comparison predicate: expected column <op> value"
                            .into(),
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
                            "left side of comparison must be a column identifier".into(),
                        ));
                    }
                };

                let col_idx = table_desc.schema.column_index(col_name).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "unknown column '{col_name}' in table '{}'",
                        table_desc.name
                    ))
                })?;

                if comp_op == ComparisonOp::Eq && !seen_eq_cols.insert(col_idx) {
                    let col_def = table_desc
                        .schema
                        .column(col_idx)
                        .expect("column must exist");
                    return Err(HtapError::InvalidArgument(format!(
                        "duplicate predicate on column '{}'",
                        col_def.name
                    )));
                }

                if matches!(r, Expr::Value(ref v) if matches!(v.value, sqlparser::ast::Value::Null))
                {
                    return Err(HtapError::InvalidArgument(format!(
                        "NULL literal not permitted in comparison predicate for column '{col_name}'; use IS NULL or IS NOT NULL"
                    )));
                }

                let col_def = table_desc
                    .schema
                    .column(col_idx)
                    .expect("column must exist");
                let parsed_value = parse_literal_value(r, col_def)?;
                bound_leaves.push(AnalyticFilter::Comparison {
                    column: col_idx,
                    op: comp_op,
                    value: parsed_value,
                });
            }
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                ..
            } => {
                return Err(HtapError::InvalidArgument(
                    "NOT is not supported in WHERE clause".into(),
                ));
            }
            other => {
                return Err(HtapError::InvalidArgument(format!(
                    "unsupported predicate in WHERE clause: {other}"
                )));
            }
        }
    }

    if bound_leaves.len() == 1 {
        Ok(Some(bound_leaves.remove(0)))
    } else {
        Ok(Some(AnalyticFilter::And(bound_leaves)))
    }
}
