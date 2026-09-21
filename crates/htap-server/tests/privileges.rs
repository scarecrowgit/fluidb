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
fn test_explain_masks_unprivileged_table_as_missing() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    let existing = session.execute("EXPLAIN SELECT * FROM a").unwrap_err();
    let missing = session.execute("EXPLAIN SELECT * FROM zzz").unwrap_err();

    assert!(matches!(&existing, HtapError::NotFound(_)));
    assert!(matches!(&missing, HtapError::NotFound(_)));
    assert_masked("a", existing, missing);
}

#[test]
fn test_analyze_table_masks_unprivileged_table_as_missing() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    let existing = session.execute("ANALYZE TABLE a").unwrap_err();
    let missing = session.execute("ANALYZE TABLE zzz").unwrap_err();

    assert!(matches!(&existing, HtapError::NotFound(_)));
    assert!(matches!(&missing, HtapError::NotFound(_)));
    assert_masked("a", existing, missing);
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
fn test_join_using_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "b",
        session
            .execute("SELECT * FROM a JOIN b USING(id)")
            .unwrap_err(),
        session
            .execute("SELECT * FROM a JOIN zzz USING(id)")
            .unwrap_err(),
    );
}

#[test]
fn test_natural_join_with_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "b",
        session
            .execute("SELECT * FROM a NATURAL JOIN b")
            .unwrap_err(),
        session
            .execute("SELECT * FROM a NATURAL JOIN zzz")
            .unwrap_err(),
    );
}

#[test]
fn test_nested_join_tree_with_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE hidden (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    server.execute("GRANT SELECT ON b TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "hidden",
        session
            .execute(
                "SELECT a.id FROM a \
                 JOIN (b JOIN hidden ON b.id = hidden.id) ON a.id = b.id",
            )
            .unwrap_err(),
        session
            .execute(
                "SELECT a.id FROM a \
                 JOIN (b JOIN zzz ON b.id = zzz.id) ON a.id = b.id",
            )
            .unwrap_err(),
    );
}

#[test]
fn test_full_outer_join_with_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "b",
        session
            .execute("SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id")
            .unwrap_err(),
        session
            .execute("SELECT * FROM a FULL OUTER JOIN zzz ON a.id = zzz.id")
            .unwrap_err(),
    );
}

#[test]
fn test_visible_but_unselectable_join_tables_are_denied_post_bind() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE hidden (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    server.execute("GRANT UPDATE ON b TO u").unwrap();
    server.execute("GRANT UPDATE ON hidden TO u").unwrap();
    let mut session = user_session(&server);

    for sql in [
        "SELECT * FROM a NATURAL JOIN b",
        "SELECT a.id FROM a \
         JOIN (b JOIN hidden ON b.id = hidden.id) ON a.id = b.id",
        "SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id",
    ] {
        assert!(matches!(
            session.execute(sql).unwrap_err(),
            HtapError::PermissionDenied(_)
        ));
    }
}

#[test]
fn test_filtered_delete_subquery_with_ungranted_table_is_masked() {
    let (_dir, server) = setup();
    server.execute("GRANT DELETE ON a TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "b",
        session
            .execute("DELETE FROM a WHERE id IN (SELECT id FROM b)")
            .unwrap_err(),
        session
            .execute("DELETE FROM a WHERE id IN (SELECT id FROM zzz)")
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
fn test_delete_without_grants_masks_filtered_target_table() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    assert_masked(
        "a",
        session.execute("DELETE FROM a WHERE v > 1").unwrap_err(),
        session.execute("DELETE FROM zzz WHERE v > 1").unwrap_err(),
    );
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
fn test_delete_requires_only_delete_for_point_targets_but_select_for_filters() {
    let (_dir, server) = setup();
    server.execute("GRANT UPDATE ON a TO u").unwrap();
    server.execute("GRANT DELETE ON a TO u").unwrap();
    let mut session = user_session(&server);

    session.execute("DELETE FROM a WHERE id = 1").unwrap();
    assert!(matches!(
        session.execute("DELETE FROM a WHERE v > 1").unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
    server.execute("GRANT SELECT ON a TO u").unwrap();
    session.execute("DELETE FROM a WHERE v > 1").unwrap();
    session.execute("DELETE FROM a").unwrap();
}

#[test]
fn test_truncate_requires_delete_only_and_masks_hidden_tables() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    assert_masked(
        "a",
        session.execute("TRUNCATE TABLE a").unwrap_err(),
        session.execute("TRUNCATE TABLE zzz").unwrap_err(),
    );

    server.execute("GRANT DELETE ON a TO u").unwrap();
    session.execute("TRUNCATE TABLE a").unwrap();
}

#[test]
fn test_truncate_if_exists_missing_table_is_a_noop() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    session.execute("TRUNCATE TABLE IF EXISTS zzz").unwrap();
}

#[test]
fn test_truncate_if_exists_invisible_table_masks_as_missing() {
    let (_dir, server) = setup();
    let mut session = user_session(&server);

    let hidden = session.execute("TRUNCATE TABLE IF EXISTS a").unwrap();
    let missing = session.execute("TRUNCATE TABLE IF EXISTS zzz").unwrap();
    assert_eq!(hidden, missing);

    assert_eq!(
        rows(server.execute("SELECT id FROM a ORDER BY id").unwrap()),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
        ]
    );
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

#[test]
fn test_insert_select_requires_select_on_source_and_leaves_target_unchanged_when_denied() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE src (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server
        .execute("INSERT INTO src (id, v) VALUES (1, 10), (2, 20)")
        .unwrap();
    server
        .execute("CREATE TABLE dst (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT INSERT ON dst TO u").unwrap();
    server.execute("GRANT UPDATE ON src TO u").unwrap();
    let mut session = user_session(&server);

    assert!(matches!(
        session
            .execute("INSERT INTO dst (id, v) SELECT id, v FROM src")
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
    assert!(rows(server.execute("SELECT * FROM dst").unwrap()).is_empty());

    server.execute("GRANT SELECT ON src TO u").unwrap();
    session
        .execute("INSERT INTO dst (id, v) SELECT id, v FROM src")
        .unwrap();
    assert_eq!(
        rows(server.execute("SELECT id, v FROM dst ORDER BY id").unwrap()),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ]
    );
}

#[test]
fn test_insert_select_masks_invisible_source_as_missing_table() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE src (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server
        .execute("CREATE TABLE dst (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT INSERT ON dst TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "src",
        session
            .execute("INSERT INTO dst (id, v) SELECT id, v FROM src")
            .unwrap_err(),
        session
            .execute("INSERT INTO dst (id, v) SELECT id, v FROM zzz")
            .unwrap_err(),
    );
}

#[test]
fn test_insert_select_masks_hidden_sources_in_subquery_and_cte() {
    let (_dir, server) = setup();
    for table in ["src", "hidden", "visible"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, v INT)"
            ))
            .unwrap();
    }
    server
        .execute("INSERT INTO src (id, v) VALUES (1, 10)")
        .unwrap();
    server
        .execute("INSERT INTO hidden (id, v) VALUES (1, 10)")
        .unwrap();
    server.execute("GRANT INSERT ON visible TO u").unwrap();
    server.execute("GRANT SELECT ON src TO u").unwrap();
    let mut session = user_session(&server);

    assert_masked(
        "hidden",
        session
            .execute(
                "INSERT INTO visible (id, v) SELECT id, v FROM src \
                 WHERE id IN (SELECT id FROM hidden)",
            )
            .unwrap_err(),
        session
            .execute(
                "INSERT INTO visible (id, v) SELECT id, v FROM src \
                 WHERE id IN (SELECT id FROM zzz)",
            )
            .unwrap_err(),
    );
    assert_masked(
        "hidden",
        session
            .execute(
                "WITH c AS (SELECT id, v FROM hidden) \
                 INSERT INTO visible (id, v) SELECT * FROM c",
            )
            .unwrap_err(),
        session
            .execute(
                "WITH c AS (SELECT id, v FROM zzz) \
                 INSERT INTO visible (id, v) SELECT * FROM c",
            )
            .unwrap_err(),
    );
}

#[test]
fn test_insert_select_requires_insert_on_target() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE src (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server
        .execute("INSERT INTO src (id, v) VALUES (1, 10)")
        .unwrap();
    server
        .execute("CREATE TABLE dst (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT SELECT ON src TO u").unwrap();
    server.execute("GRANT UPDATE ON dst TO u").unwrap();
    let mut session = user_session(&server);

    assert!(matches!(
        session
            .execute("INSERT INTO dst (id, v) SELECT id, v FROM src")
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
}

#[test]
fn test_recursive_cte_privilege_checks_masking_and_self_reference() {
    let (_dir, server) = setup();
    server
        .execute("CREATE TABLE hidden (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server
        .execute("CREATE TABLE visible (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    server.execute("GRANT UPDATE ON visible TO u").unwrap();
    server.execute("GRANT SELECT ON a TO u").unwrap();
    let mut session = user_session(&server);

    // A recursive CTE can read a visible base table and reference itself.
    assert_eq!(
        rows(
            session
                .execute(
                    "WITH RECURSIVE x AS (\
                         SELECT id FROM a WHERE id = 1 \
                         UNION ALL \
                         SELECT id + 1 FROM x WHERE id < 2\
                     ) \
                     SELECT id FROM x ORDER BY id",
                )
                .unwrap()
        ),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
        ]
    );

    let hidden_error = session
        .execute(
            "WITH RECURSIVE x AS (\
             SELECT id FROM hidden \
             UNION ALL \
             SELECT id FROM x) \
             SELECT * FROM x",
        )
        .unwrap_err();
    let missing_error = session
        .execute(
            "WITH RECURSIVE x AS (\
             SELECT id FROM zzz \
             UNION ALL \
             SELECT id FROM x) \
             SELECT * FROM x",
        )
        .unwrap_err();
    assert!(hidden_error.to_string().contains("'hidden'"));
    assert!(missing_error.to_string().contains("'zzz'"));
    assert_masked("hidden", hidden_error, missing_error);

    let hidden_error = session
        .execute(
            "WITH RECURSIVE x AS (\
             SELECT id FROM a \
             UNION ALL \
             SELECT hidden.id FROM x JOIN hidden ON x.id = hidden.id) \
             SELECT * FROM x",
        )
        .unwrap_err();
    let missing_error = session
        .execute(
            "WITH RECURSIVE x AS (\
             SELECT id FROM a \
             UNION ALL \
             SELECT zzz.id FROM x JOIN zzz ON x.id = zzz.id) \
             SELECT * FROM x",
        )
        .unwrap_err();
    assert!(hidden_error.to_string().contains("'hidden'"));
    assert!(missing_error.to_string().contains("'zzz'"));
    assert_masked("hidden", hidden_error, missing_error);

    assert!(matches!(
        session
            .execute(
                "WITH RECURSIVE x AS (\
                     SELECT id FROM visible \
                     UNION ALL \
                     SELECT id FROM x) \
                     SELECT * FROM x",
            )
            .unwrap_err(),
        HtapError::PermissionDenied(_)
    ));
}
