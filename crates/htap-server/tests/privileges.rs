use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::password::scramble_native_password;
use htap_common::types::{Row, Value};
use htap_common::HtapError;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

const SCRAMBLE: &[u8; 20] = b"01234567890123456789";

fn setup() -> (TempDir, Arc<LocalServer>) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server.bootstrap_root_account(Some("root")).unwrap();
    server
        .execute("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 10), (2, 20)")
        .unwrap();
    server
        .execute("CREATE TABLE b (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("CREATE USER u IDENTIFIED BY 'pw'").unwrap();
    (dir, server)
}

fn user_session(server: &Arc<LocalServer>) -> Session {
    let response = scramble_native_password(SCRAMBLE, "pw");
    server
        .authenticate_session("u", SCRAMBLE, &response)
        .unwrap()
}

fn rows(result: StatementResult) -> Vec<Row> {
    match result {
        StatementResult::Query(query) => query.rows,
        other => panic!("expected query result, got {other:?}"),
    }
}

fn assert_masked(hidden_name: &str, existing: HtapError, missing: HtapError) {
    assert!(matches!(existing, HtapError::NotFound(_)));
    assert!(matches!(missing, HtapError::NotFound(_)));
    assert_eq!(
        existing
            .to_string()
            .replace(&format!("'{hidden_name}'"), "'zzz'"),
        missing.to_string()
    );
}

#[test]
fn test_superuser_session_unrestricted() {
    let (_dir, server) = setup();
    let mut session = server.open_session();

    session.execute("SELECT v FROM a WHERE id = 1").unwrap();
    session
        .execute("INSERT INTO b (id, v) VALUES (1, 1)")
        .unwrap();
    session.execute("UPDATE b SET v = 2 WHERE id = 1").unwrap();
    session.execute("DELETE FROM b WHERE id = 1").unwrap();
    session.execute("DROP TABLE b").unwrap();
}

#[test]
fn test_account_without_grants_gets_identical_error_to_missing_table() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    for (existing, missing) in [
        ("SELECT * FROM a", "SELECT * FROM zzz"),
        (
            "INSERT INTO a (id, v) VALUES (3, 30)",
            "INSERT INTO zzz (id, v) VALUES (3, 30)",
        ),
        (
            "UPDATE a SET v = 30 WHERE id = 1",
            "UPDATE zzz SET v = 30 WHERE id = 1",
        ),
        ("DELETE FROM a WHERE id = 1", "DELETE FROM zzz WHERE id = 1"),
        ("DROP TABLE a", "DROP TABLE zzz"),
        ("DESCRIBE a", "DESCRIBE zzz"),
    ] {
        assert_masked(
            "a",
            session.execute(existing).unwrap_err(),
            session.execute(missing).unwrap_err(),
        );
    }
}

#[test]
fn test_select_grant_allows_point_analytic_and_general_query_but_not_insert() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_eq!(
        rows(session.execute("SELECT v FROM a WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    assert_eq!(
        rows(
            session
                .execute("SELECT id FROM a WHERE v > 10 ORDER BY id")
                .unwrap()
        ),
        vec![Row::new(vec![Value::Int64(2)])]
    );
    assert_eq!(
        rows(
            session
                .execute("SELECT COUNT(*) FROM a ORDER BY COUNT(*)")
                .unwrap()
        ),
        vec![Row::new(vec![Value::Int64(2)])]
    );
    assert!(matches!(
        session
            .execute("INSERT INTO a (id, v) VALUES (3, 30)")
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
}

#[test]
fn test_join_with_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "b",
        session
            .execute("SELECT a.id FROM a JOIN b ON a.id = b.id")
            .unwrap_err(),
        session
            .execute("SELECT a.id FROM a JOIN zzz ON a.id = zzz.id")
            .unwrap_err(),
    );
}

#[test]
fn test_subquery_and_cte_referencing_ungranted_table_masked() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    assert_masked(
        "a",
        session.execute("SELECT (SELECT v FROM a)").unwrap_err(),
        session.execute("SELECT (SELECT v FROM zzz)").unwrap_err(),
    );
    assert_masked(
        "a",
        session
            .execute("WITH x AS (SELECT * FROM a) SELECT * FROM x")
            .unwrap_err(),
        session
            .execute("WITH x AS (SELECT * FROM zzz) SELECT * FROM x")
            .unwrap_err(),
    );
}

#[test]
fn test_global_grant_applies_to_all_tables() {
    let (_dir, server) = setup();
    server
        .execute("GRANT SELECT, INSERT, UPDATE, DELETE ON *.* TO u")
        .unwrap();
    let mut session = user_session(&server);

    session.execute("SELECT * FROM a").unwrap();
    session
        .execute("INSERT INTO b (id, v) VALUES (1, 1)")
        .unwrap();
    session.execute("UPDATE b SET v = 2 WHERE id = 1").unwrap();
    session.execute("DELETE FROM b WHERE id = 1").unwrap();
}

#[test]
fn test_revoke_takes_effect_on_next_statement_in_same_session() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    session.execute("SELECT * FROM a").unwrap();
    server.execute("REVOKE SELECT ON a FROM u").unwrap();
    assert!(matches!(
        session.execute("SELECT * FROM a").unwrap_err(),
        HtapError::NotFound(_)
    ));
}

#[test]
fn test_create_table_requires_global_create_and_creator_gets_no_implicit_privileges() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    assert!(matches!(
        session
            .execute("CREATE TABLE c (id BIGINT PRIMARY KEY)")
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
    server.execute("GRANT CREATE ON *.* TO u").unwrap();
    session
        .execute("CREATE TABLE c (id BIGINT PRIMARY KEY)")
        .unwrap();
    // CREATE is global, so c is visible even though u has no SELECT privilege on it.
    assert!(matches!(
        session.execute("SELECT * FROM c").unwrap_err(),
        HtapError::PermissionDenied(_)
    ));

    server.execute("CREATE USER u2 IDENTIFIED BY 'pw'").unwrap();
    server.execute("GRANT SELECT ON a TO u2").unwrap();
    let mut u2_session = server
        .authenticate_session("u2", SCRAMBLE, &scramble_native_password(SCRAMBLE, "pw"))
        .unwrap();
    assert!(matches!(
        u2_session.execute("SELECT * FROM c").unwrap_err(),
        HtapError::NotFound(_)
    ));
}

#[test]
fn test_account_ddl_requires_superuser() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    for sql in [
        "CREATE USER x IDENTIFIED BY 'pw'",
        "ALTER USER u IDENTIFIED BY 'other'",
        "DROP USER u",
        "GRANT SELECT ON a TO u",
        "REVOKE SELECT ON a FROM u",
    ] {
        assert!(matches!(
            session.execute(sql).unwrap_err(),
            HtapError::PermissionDenied(_)
        ));
    }

    let existing = session.execute("GRANT SELECT ON a TO u").unwrap_err();
    let missing = session.execute("GRANT SELECT ON zzz TO u").unwrap_err();
    assert!(matches!(existing, HtapError::PermissionDenied(_)));
    assert!(matches!(missing, HtapError::PermissionDenied(_)));
    assert_eq!(existing.to_string(), missing.to_string());
}

#[test]
fn test_show_grants_self_allowed_other_denied() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    session.execute("SHOW GRANTS").unwrap();
    session.execute("SHOW GRANTS FOR u").unwrap();
    assert!(matches!(
        session.execute("SHOW GRANTS FOR root").unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
}

#[test]
fn test_show_tables_filtered_for_account() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_eq!(
        rows(session.execute("SHOW TABLES").unwrap()),
        vec![Row::new(vec![Value::String("a".into())])]
    );
}

#[test]
fn test_locked_or_dropped_account_mid_session_fails_closed() {
    let (dir, server) = setup();
    let mut session = user_session(&server);

    let store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snapshot = store.load().unwrap().unwrap();
    let generation = snapshot.generation;
    snapshot.generation += 1;
    snapshot
        .accounts
        .iter_mut()
        .find(|account| account.username == "u")
        .unwrap()
        .locked = true;
    store.compare_and_set(generation, snapshot).unwrap();
    assert!(matches!(
        session.execute("SELECT 1").unwrap_err(),
        HtapError::PermissionDenied(_)
    ));

    let (_dir, server) = setup();
    let mut session = user_session(&server);
    server.execute("DROP USER u").unwrap();
    assert!(matches!(
        session.execute("SELECT 1").unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
}

#[test]
fn test_update_requires_update_and_select_when_where_present() {
    let (_dir, server) = setup();
    server.execute("GRANT UPDATE ON a TO u").unwrap();
    let mut session = user_session(&server);

    session.execute("UPDATE a SET v = 11 WHERE id = 1").unwrap();
    server.execute("REVOKE UPDATE ON a FROM u").unwrap();
    server.execute("GRANT UPDATE ON a TO u").unwrap();
    assert!(matches!(
        session
            .execute("UPDATE a SET v = 12 WHERE v = 11")
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
    server.execute("GRANT SELECT ON a TO u").unwrap();
    session.execute("UPDATE a SET v = 12 WHERE v = 11").unwrap();
}

#[test]
fn test_delete_requires_delete() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    assert!(matches!(
        session.execute("DELETE FROM a WHERE id = 1").unwrap_err(),
        HtapError::NotFound(_)
    ));
    server.execute("GRANT DELETE ON a TO u").unwrap();
    session.execute("DELETE FROM a WHERE id = 1").unwrap();
}

#[test]
fn test_check_statement_visible_masks_invisible_table() {
    let (_dir, server) = setup();
    let session = user_session(&server);

    let hidden = htap_sql::parse_one("SELECT * FROM a").unwrap();
    let missing = htap_sql::parse_one("SELECT * FROM zzz").unwrap();
    assert_masked(
        "a",
        session.check_statement_visible(&hidden).unwrap_err(),
        session.check_statement_visible(&missing).unwrap_err(),
    );
}

#[test]
fn test_check_statement_visible_allows_placeholders() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let session = user_session(&server);

    let visible = htap_sql::parse_one("SELECT v FROM a WHERE id = ?").unwrap();
    session.check_statement_visible(&visible).unwrap();

    server.execute("REVOKE SELECT ON a FROM u").unwrap();
    let hidden = htap_sql::parse_one("SELECT v FROM a WHERE id = ?").unwrap();
    let missing = htap_sql::parse_one("SELECT v FROM zzz WHERE id = ?").unwrap();
    assert_masked(
        "a",
        session.check_statement_visible(&hidden).unwrap_err(),
        session.check_statement_visible(&missing).unwrap_err(),
    );
}

#[test]
fn test_prepared_style_execute_statement_rechecks_privileges() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);
    let statement = htap_sql::parse_one("SELECT * FROM a").unwrap();

    session.execute_statement(statement.clone()).unwrap();
    server.execute("REVOKE SELECT ON a FROM u").unwrap();
    assert!(matches!(
        session.execute_statement(statement).unwrap_err(),
        HtapError::NotFound(_)
    ));
}

#[test]
fn test_prebind_visibility_masks_alter_table_targets() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    for (existing, missing) in [
        (
            "ALTER TABLE a ADD COLUMN x INT",
            "ALTER TABLE zzz ADD COLUMN x INT",
        ),
        (
            "ALTER TABLE a DROP PARTITION nope",
            "ALTER TABLE zzz DROP PARTITION nope",
        ),
    ] {
        assert_masked(
            "a",
            session.execute(existing).unwrap_err(),
            session.execute(missing).unwrap_err(),
        );
    }
}

#[test]
fn test_prebind_visibility_masks_schema_errors_and_cte_self_references() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    for (existing, missing) in [
        ("SELECT missing FROM a", "SELECT missing FROM zzz"),
        (
            "SELECT id FROM a ORDER BY missing",
            "SELECT id FROM zzz ORDER BY missing",
        ),
        (
            "SELECT id FROM a GROUP BY missing",
            "SELECT id FROM zzz GROUP BY missing",
        ),
        (
            "INSERT INTO a (id, v) VALUES (3, 30, 40)",
            "INSERT INTO zzz (id, v) VALUES (3, 30, 40)",
        ),
        ("DESCRIBE a", "DESCRIBE zzz"),
        ("SET @x = (SELECT v FROM a)", "SET @x = (SELECT v FROM zzz)"),
    ] {
        assert_masked(
            "a",
            session.execute(existing).unwrap_err(),
            session.execute(missing).unwrap_err(),
        );
    }

    assert_masked(
        "secret",
        session
            .execute("WITH secret AS (SELECT * FROM secret) SELECT * FROM secret")
            .unwrap_err(),
        session
            .execute("WITH secret AS (SELECT * FROM zzz) SELECT * FROM secret")
            .unwrap_err(),
    );
}

#[test]
fn test_show_grants_without_for_uses_calling_account() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_eq!(
        rows(session.execute("SHOW GRANTS").unwrap()),
        vec![Row::new(vec![Value::String(
            "GRANT SELECT ON htap.a TO 'u'@'%'".into()
        )])]
    );
}
