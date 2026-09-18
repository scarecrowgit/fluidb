use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_server::LocalServer;
use tempfile::TempDir;

#[test]
fn bootstrap_adopts_existing_root_account() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE USER root IDENTIFIED BY 'existing-password'")
        .unwrap();

    let report = server
        .bootstrap_root_account(Some("existing-password"))
        .unwrap();
    assert!(!report.created_root);
    assert_eq!(report.config_password_matches_root, Some(true));

    let store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = store.load().unwrap().unwrap();
    assert!(snapshot.accounts_initialized);
    assert!(snapshot.account_by_username("root").unwrap().is_superuser);

    let second = server
        .bootstrap_root_account(Some("existing-password"))
        .unwrap();
    assert!(!second.created_root);
    assert_eq!(second.config_password_matches_root, Some(true));
}
