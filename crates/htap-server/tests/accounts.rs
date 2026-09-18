use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{Account, Grant, PrivilegeScope, PrivilegeSet};
use htap_common::hash_native_password;
use htap_common::password::scramble_native_password;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

#[test]
fn test_drop_table_removes_table_grants_and_catalog_still_valid_after_reopen() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server.bootstrap_root_account(Some("root")).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();

    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snapshot = catalog_store.load().unwrap().unwrap();
    let expected_generation = snapshot.generation;
    let table_id = snapshot.table_by_name("t").unwrap().id;
    let root_id = snapshot.account_by_username("root").unwrap().id;
    snapshot.generation += 1;
    snapshot.grants.push(Grant {
        account: root_id,
        scope: PrivilegeScope::Table(table_id),
        privileges: PrivilegeSet::SELECT,
    });
    catalog_store
        .compare_and_set(expected_generation, snapshot)
        .unwrap();

    server.execute("DROP TABLE t").unwrap();
    let snapshot = catalog_store.load().unwrap().unwrap();
    assert!(snapshot.grants.is_empty());
    snapshot.validate().unwrap();

    drop(server);
    let reopened = LocalServer::open(dir.path()).unwrap();
    drop(reopened);
}

#[test]
fn test_bootstrap_is_noop_after_root_dropped() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server.bootstrap_root_account(Some("root")).unwrap();

    // Direct catalog CAS creates the second superuser needed to make dropping root valid.
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snapshot = catalog_store.load().unwrap().unwrap();
    let expected_generation = snapshot.generation;
    let mut high_water = snapshot.id_high_water();
    let admin_id = high_water.allocate_account().unwrap();
    snapshot.generation += 1;
    snapshot.id_high_water = high_water;
    snapshot.accounts.push(Account {
        id: admin_id,
        username: "admin".to_string(),
        password_hash: Some(hash_native_password("admin")),
        locked: false,
        is_superuser: true,
    });
    catalog_store
        .compare_and_set(expected_generation, snapshot)
        .unwrap();

    server.execute("DROP USER root").unwrap();
    let report = server.bootstrap_root_account(Some("replacement")).unwrap();

    assert!(!report.created_root);
    assert_eq!(report.config_password_matches_root, Some(false));
    let snapshot = catalog_store.load().unwrap().unwrap();
    assert!(snapshot.account_by_username("root").is_none());
}

#[test]
fn test_bootstrap_reports_config_password_mismatch() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    assert!(
        server
            .bootstrap_root_account(Some("correct"))
            .unwrap()
            .created_root
    );
    let report = server.bootstrap_root_account(Some("wrong")).unwrap();

    assert!(!report.created_root);
    assert_eq!(report.config_password_matches_root, Some(false));
}

#[test]
fn test_account_debug_output_redacts_password_hash() {
    let hash = hash_native_password("secret");
    let account = Account {
        id: htap_catalog::AccountId::new(1),
        username: "alice".to_string(),
        password_hash: Some(hash),
        locked: false,
        is_superuser: false,
    };

    let debug = format!("{account:?}");
    assert!(!debug.contains(&format!("{hash:?}")));
    assert!(!debug.contains("password_hash: Some"));
    assert!(debug.contains("<redacted>"));
}

fn grant_strings(result: StatementResult) -> Vec<String> {
    match result {
        StatementResult::Query(query) => query
            .rows
            .into_iter()
            .map(|row| match row.get(0) {
                Some(htap_common::types::Value::String(value)) => value.clone(),
                other => panic!("expected string grant row, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SHOW GRANTS query result, got {other:?}"),
    }
}

#[test]
fn test_bootstrap_creates_root_when_uninitialized() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let report = server
        .bootstrap_root_account(Some("root-password"))
        .unwrap();

    assert!(report.created_root);
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let root = snapshot.account_by_username("root").unwrap();
    assert!(root.is_superuser);
    assert!(snapshot.accounts_initialized);
}

#[test]
fn test_bootstrap_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    assert!(
        server
            .bootstrap_root_account(Some("root"))
            .unwrap()
            .created_root
    );
    let report = server.bootstrap_root_account(Some("root")).unwrap();

    assert!(!report.created_root);
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .filter(|account| account.username == "root")
            .count(),
        1
    );
}

#[test]
fn test_create_user_sql_creates_an_account() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE USER alice IDENTIFIED BY 'secret'")
        .unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let alice = snapshot.account_by_username("alice").unwrap();
    assert_eq!(alice.password_hash, Some(hash_native_password("secret")));
    assert!(!alice.is_superuser);
}

#[test]
fn test_create_user_duplicate_conflict_and_if_not_exists() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE USER alice IDENTIFIED BY 'secret'")
        .unwrap();
    let error = server
        .execute("CREATE USER alice IDENTIFIED BY 'other'")
        .unwrap_err();
    assert!(matches!(error, HtapError::Conflict(_)));

    let result = server
        .execute("CREATE USER IF NOT EXISTS alice IDENTIFIED BY 'other'")
        .unwrap();
    assert_eq!(result, StatementResult::ddl(0));

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert_eq!(
        snapshot.account_by_username("alice").unwrap().password_hash,
        Some(hash_native_password("secret"))
    );
}

#[test]
fn test_empty_password_accounts() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server.execute("CREATE USER u IDENTIFIED BY ''").unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert_eq!(
        snapshot.account_by_username("u").unwrap().password_hash,
        Some(hash_native_password(""))
    );

    let error = server.execute("CREATE USER u2").unwrap_err();
    assert!(matches!(error, HtapError::InvalidArgument(_)));
}

#[test]
fn test_create_alter_drop_user_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE USER alice IDENTIFIED BY 'old-password'")
        .unwrap();
    server
        .execute("ALTER USER alice IDENTIFIED BY 'new-password'")
        .unwrap();
    drop(server);

    let server = LocalServer::open(dir.path()).unwrap();
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert_eq!(
        snapshot.account_by_username("alice").unwrap().password_hash,
        Some(hash_native_password("new-password"))
    );

    server.execute("DROP USER alice").unwrap();
    drop(server);

    let reopened = LocalServer::open(dir.path()).unwrap();
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert!(snapshot.account_by_username("alice").is_none());
    drop(reopened);
}

#[test]
fn test_table_scoped_grant_after_create_user() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    server
        .execute("CREATE USER alice IDENTIFIED BY 'secret'")
        .unwrap();
    server.execute("GRANT SELECT ON t TO alice").unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let account = snapshot.account_by_username("alice").unwrap();
    let table = snapshot.table_by_name("t").unwrap();
    assert_eq!(
        snapshot.effective_privileges(account.id, table.id),
        PrivilegeSet::SELECT
    );
}

#[test]
fn test_grant_revoke_merge_and_remove_rows_and_show_grants_format() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server.bootstrap_root_account(Some("root")).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    server
        .execute("CREATE USER u IDENTIFIED BY 'secret'")
        .unwrap();

    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR u").unwrap()),
        vec!["GRANT USAGE ON *.* TO 'u'@'%'"]
    );
    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR root").unwrap()),
        vec!["GRANT ALL PRIVILEGES ON *.* TO 'root'@'%'"]
    );

    server.execute("GRANT SELECT ON *.* TO u").unwrap();
    server.execute("GRANT INSERT ON *.* TO u").unwrap();
    server.execute("GRANT SELECT ON t TO u").unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let account = snapshot.account_by_username("u").unwrap();
    assert_eq!(
        snapshot
            .grants
            .iter()
            .filter(|grant| grant.account == account.id)
            .count(),
        2
    );
    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR u").unwrap()),
        vec![
            "GRANT SELECT,INSERT ON *.* TO 'u'@'%'",
            "GRANT SELECT ON htap.t TO 'u'@'%'",
        ]
    );

    server.execute("REVOKE SELECT ON *.* FROM u").unwrap();
    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR u").unwrap()),
        vec![
            "GRANT INSERT ON *.* TO 'u'@'%'",
            "GRANT SELECT ON htap.t TO 'u'@'%'",
        ]
    );

    server.execute("REVOKE INSERT ON *.* FROM u").unwrap();
    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR u").unwrap()),
        vec!["GRANT SELECT ON htap.t TO 'u'@'%'"]
    );

    server.execute("REVOKE SELECT ON t FROM u").unwrap();
    assert_eq!(
        grant_strings(server.execute("SHOW GRANTS FOR u").unwrap()),
        vec!["GRANT USAGE ON *.* TO 'u'@'%'"]
    );
}

#[test]
fn test_drop_user_removes_its_grants() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    server
        .execute("CREATE USER u IDENTIFIED BY 'secret'")
        .unwrap();
    server.execute("GRANT SELECT ON t TO u").unwrap();
    server.execute("DROP USER u").unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert!(snapshot.account_by_username("u").is_none());
    assert!(snapshot.grants.is_empty());
    snapshot.validate().unwrap();
}

#[test]
fn test_cannot_drop_last_superuser() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server.bootstrap_root_account(Some("root")).unwrap();

    let error = server.execute("DROP USER root").unwrap_err();

    assert!(matches!(error, HtapError::InvalidArgument(_)));
}

#[test]
fn test_revoke_never_granted_is_noop() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE USER u IDENTIFIED BY 'secret'")
        .unwrap();

    let result = server.execute("REVOKE SELECT ON *.* FROM u").unwrap();

    assert_eq!(result, StatementResult::ddl(1));
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    assert!(snapshot.grants.is_empty());
}

#[test]
fn test_partition_alterations_preserve_accounts_and_grants() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE t (id BIGINT PRIMARY KEY) PARTITION BY RANGE (id) \
             (PARTITION p0 VALUES LESS THAN (MAXVALUE))",
        )
        .unwrap();
    server.bootstrap_root_account(Some("root")).unwrap();
    server
        .execute("CREATE USER alice IDENTIFIED BY 'secret'")
        .unwrap();
    server.execute("GRANT SELECT ON *.* TO alice").unwrap();

    server
        .execute(
            "ALTER TABLE t REORGANIZE PARTITION p0 INTO \
             (PARTITION p0 VALUES LESS THAN (100), PARTITION p1 VALUES LESS THAN (MAXVALUE))",
        )
        .unwrap();

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let alice = snapshot.account_by_username("alice").unwrap();
    assert!(snapshot.grants.iter().any(|grant| {
        grant.account == alice.id
            && grant.scope == PrivilegeScope::Global
            && grant.privileges == PrivilegeSet::SELECT
    }));

    drop(server);
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());

    let report = server.bootstrap_root_account(Some("root")).unwrap();
    assert!(!report.created_root);

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();
    let alice = snapshot.account_by_username("alice").unwrap();
    assert!(snapshot.grants.iter().any(|grant| {
        grant.account == alice.id
            && grant.scope == PrivilegeScope::Global
            && grant.privileges == PrivilegeSet::SELECT
    }));

    let scramble = b"01234567890123456789";
    let response = scramble_native_password(scramble, "secret");
    server
        .authenticate_session("alice", scramble, &response)
        .unwrap();
}
