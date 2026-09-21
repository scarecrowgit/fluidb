//! Session privilege checks for bound SQL statements.

use htap_catalog::{PrivilegeScope, PrivilegeSet};
use htap_common::error::{HtapError, Result};
use htap_sql::{
    ast::{BoundStatement, DeleteTarget, InsertSource, ShowStatement, UpdateTarget},
    referenced_table_names, table_not_found,
};
use sqlparser::ast::{ObjectType, Statement};

use crate::{CatalogSnapshot, Principal};

fn denied(username: &str, privilege: PrivilegeSet, table: &str) -> HtapError {
    HtapError::PermissionDenied(format!(
        "{privilege} command denied to user '{username}' for table '{table}'"
    ))
}

fn check_table_privilege(
    username: &str,
    account: htap_catalog::AccountId,
    catalog: &CatalogSnapshot,
    table: &str,
    required: PrivilegeSet,
) -> Result<()> {
    let descriptor = catalog
        .table_by_name(table)
        .ok_or_else(|| table_not_found(table))?;
    let privileges = catalog.effective_privileges(account, descriptor.id);

    if privileges.is_empty() {
        return Err(table_not_found(table));
    }
    if !privileges.contains(required) {
        return Err(denied(username, required, table));
    }
    Ok(())
}

fn check_global_privilege(
    username: &str,
    account: htap_catalog::AccountId,
    catalog: &CatalogSnapshot,
    required: PrivilegeSet,
) -> Result<()> {
    let privileges = catalog
        .grants_for(account)
        .into_iter()
        .filter(|grant| matches!(grant.scope, PrivilegeScope::Global))
        .fold(PrivilegeSet::empty(), |privileges, grant| {
            privileges.union(grant.privileges)
        });
    if !privileges.contains(required) {
        return Err(HtapError::PermissionDenied(format!(
            "{required} command denied to user '{username}'"
        )));
    }
    Ok(())
}

fn check_query_privileges(
    username: &str,
    account: htap_catalog::AccountId,
    query: &htap_sql::BoundQuery,
    catalog: &CatalogSnapshot,
) -> Result<()> {
    fn walk(
        username: &str,
        account: htap_catalog::AccountId,
        query: &htap_sql::BoundQuery,
        catalog: &CatalogSnapshot,
    ) -> Result<()> {
        for subquery in &query.subqueries {
            walk(username, account, subquery, catalog)?;
        }
        match &query.body {
            htap_sql::QueryBody::Select(select) => {
                for slot in &select.slots {
                    match slot {
                        htap_sql::TableSlot::Base { table, .. } => check_table_privilege(
                            username,
                            account,
                            catalog,
                            table,
                            PrivilegeSet::SELECT,
                        )?,
                        htap_sql::TableSlot::Derived { query, .. } => {
                            walk(username, account, query.as_ref(), catalog)?
                        }
                        htap_sql::TableSlot::WorkingTableSlot { .. } => {}
                    }
                }
            }
            htap_sql::QueryBody::SetOp { left, right, .. } => {
                walk(username, account, left.as_ref(), catalog)?;
                walk(username, account, right.as_ref(), catalog)?;
            }
            htap_sql::QueryBody::RecursiveQueryBody {
                anchor,
                recursive_term,
                ..
            } => {
                walk(username, account, anchor.as_ref(), catalog)?;
                walk(username, account, recursive_term.as_ref(), catalog)?;
            }
        }
        Ok(())
    }

    walk(username, account, query, catalog)
}

/// Checks that an unbound prepared statement does not expose a table to an account with no
/// privileges on it. Full statement privileges are checked later at execution time.
pub(crate) fn check_statement_visible(
    principal: &Principal,
    statement: &sqlparser::ast::Statement,
    catalog: &CatalogSnapshot,
) -> Result<()> {
    let Principal::Account { id, username } = principal else {
        return Ok(());
    };

    let account = catalog
        .account_by_id(*id)
        .filter(|account| !account.locked)
        .ok_or_else(|| {
            HtapError::PermissionDenied(format!("access denied for user '{username}'"))
        })?;
    if account.is_superuser {
        return Ok(());
    }

    if matches!(
        statement,
        Statement::CreateUser { .. }
            | Statement::AlterUser { .. }
            | Statement::Grant { .. }
            | Statement::Revoke { .. }
            | Statement::Drop {
                object_type: ObjectType::User,
                ..
            }
    ) {
        return Err(HtapError::PermissionDenied(format!(
            "access denied for user '{username}'"
        )));
    }

    let referenced_tables = referenced_table_names(statement);
    let truncate_if_exists =
        matches!(statement, Statement::Truncate(truncate) if truncate.if_exists);

    for table in referenced_tables {
        let table_by_name = catalog.table_by_name(&table);

        let Some(descriptor) = table_by_name else {
            if truncate_if_exists {
                continue;
            }
            return Err(table_not_found(&table));
        };
        let has_any_privilege = catalog.has_any_privilege_on(account.id, descriptor.id);

        if !has_any_privilege {
            if truncate_if_exists {
                continue;
            }
            return Err(table_not_found(&table));
        }
    }
    Ok(())
}

/// Checks whether `principal` can execute a bound statement against `catalog`.
pub(crate) fn check_privileges(
    principal: &Principal,
    bound: &BoundStatement,
    catalog: &CatalogSnapshot,
) -> Result<()> {
    let Principal::Account { id, username } = principal else {
        return Ok(());
    };

    let account = catalog
        .account_by_id(*id)
        .filter(|account| !account.locked)
        .ok_or_else(|| {
            HtapError::PermissionDenied(format!("access denied for user '{username}'"))
        })?;

    if account.is_superuser {
        return Ok(());
    }

    match bound {
        BoundStatement::CreateTable(_) => {
            check_global_privilege(username, account.id, catalog, PrivilegeSet::CREATE)
        }
        BoundStatement::Insert(insert) => {
            check_table_privilege(
                username,
                account.id,
                catalog,
                &insert.table,
                PrivilegeSet::INSERT,
            )?;
            if let InsertSource::Query { query, .. } = &insert.source {
                check_query_privileges(username, account.id, query, catalog)?;
            }
            Ok(())
        }
        BoundStatement::Delete(delete) => {
            let descriptor = catalog.table_by_name(&delete.table);
            if delete.if_exists && descriptor.is_none() {
                return Ok(());
            }
            if delete.if_exists
                && descriptor.is_some_and(|descriptor| {
                    !catalog.has_any_privilege_on(account.id, descriptor.id)
                })
            {
                return Ok(());
            }
            check_table_privilege(
                username,
                account.id,
                catalog,
                &delete.table,
                PrivilegeSet::DELETE,
            )?;
            if matches!(delete.target, DeleteTarget::Filter(Some(_))) {
                check_table_privilege(
                    username,
                    account.id,
                    catalog,
                    &delete.table,
                    PrivilegeSet::SELECT,
                )?;
            }
            Ok(())
        }
        BoundStatement::Select(select) => check_table_privilege(
            username,
            account.id,
            catalog,
            &select.table,
            PrivilegeSet::SELECT,
        ),
        BoundStatement::AnalyticSelect(select) => check_table_privilege(
            username,
            account.id,
            catalog,
            &select.table,
            PrivilegeSet::SELECT,
        ),
        BoundStatement::AnalyzeTable(table_name) => check_table_privilege(
            username,
            account.id,
            catalog,
            table_name,
            PrivilegeSet::SELECT,
        ),
        BoundStatement::Query(query) => {
            check_query_privileges(username, account.id, query, catalog)
        }
        BoundStatement::Explain { inner, .. } => {
            check_privileges(principal, inner.as_ref(), catalog)
        }
        BoundStatement::Update(update) => {
            check_table_privilege(
                username,
                account.id,
                catalog,
                &update.table,
                PrivilegeSet::UPDATE,
            )?;
            if matches!(update.target, UpdateTarget::Filter(Some(_))) {
                check_table_privilege(
                    username,
                    account.id,
                    catalog,
                    &update.table,
                    PrivilegeSet::SELECT,
                )?;
            }
            Ok(())
        }
        BoundStatement::AlterPartitions(alter) => check_table_privilege(
            username,
            account.id,
            catalog,
            &alter.table,
            PrivilegeSet::ALTER,
        ),
        BoundStatement::DropTable(drop) => check_table_privilege(
            username,
            account.id,
            catalog,
            &drop.table,
            PrivilegeSet::DROP,
        ),
        BoundStatement::Show(ShowStatement::Columns { table })
        | BoundStatement::Show(ShowStatement::Describe { table }) => {
            let descriptor = catalog
                .table_by_name(table)
                .ok_or_else(|| table_not_found(table))?;
            if !catalog.has_any_privilege_on(account.id, descriptor.id) {
                return Err(table_not_found(table));
            }
            Ok(())
        }
        BoundStatement::Show(ShowStatement::Tables { .. })
        | BoundStatement::Show(ShowStatement::Databases) => Ok(()),
        BoundStatement::CreateUser(_)
        | BoundStatement::AlterUser(_)
        | BoundStatement::DropUser(_)
        | BoundStatement::GrantPrivileges(_)
        | BoundStatement::RevokePrivileges(_) => Err(HtapError::PermissionDenied(format!(
            "access denied for user '{username}'"
        ))),
        BoundStatement::ShowGrants(show) => {
            if show.for_username.is_none() || show.for_username.as_deref() == Some(username) {
                Ok(())
            } else {
                Err(HtapError::PermissionDenied(format!(
                    "access denied for user '{username}'"
                )))
            }
        }
    }
}
