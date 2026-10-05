//! Docker Compose smoke tests driven by `ci/docker-smoke.sh`.
//!
//! Each invocation must select exactly one phase through
//! `HTAP_DOCKER_SMOKE_PHASE`. These tests are ignored by default so normal
//! workspace tests remain Docker-independent.

use std::env;
use std::path::PathBuf;

use htap_client::{ClientOptions, RemoteClient, StatementResult};
use htap_common::types::{Row, Value};
use htap_common::HtapError;
use htap_wire::TlsMode;

const TABLE: &str = "docker_smoke";

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("required environment variable {name} is not set"))
}

fn require_phase(expected: &str) {
    let actual = required_env("HTAP_DOCKER_SMOKE_PHASE");
    assert_eq!(
        actual, expected,
        "HTAP_DOCKER_SMOKE_PHASE must be '{expected}' for this test, got '{actual}'"
    );
}

fn address() -> String {
    required_env("HTAP_DOCKER_SMOKE_ADDR")
}

fn password() -> String {
    required_env("HTAP_DOCKER_SMOKE_PASSWORD")
}

fn plain_client(password: &str) -> RemoteClient {
    RemoteClient::connect(address(), Some(password))
        .unwrap_or_else(|err| panic!("failed to connect to Docker htapd: {err}"))
}

fn tls_client(server_name: String) -> Result<RemoteClient, HtapError> {
    RemoteClient::connect_with(
        address(),
        ClientOptions {
            password: Some(password()),
            tls: TlsMode::Required {
                ca_cert: PathBuf::from(required_env("HTAP_DOCKER_SMOKE_CA")),
                server_name: Some(server_name),
            },
            ..ClientOptions::default()
        },
    )
}

fn query_rows(client: &mut RemoteClient, sql: &str) -> Vec<Row> {
    match client
        .execute(sql)
        .unwrap_or_else(|err| panic!("query failed for {sql:?}: {err}"))
    {
        StatementResult::Query(result) => result.rows,
        other => panic!("expected query result for {sql:?}, got {other:?}"),
    }
}

fn scalar_i64(client: &mut RemoteClient, sql: &str) -> i64 {
    let rows = query_rows(client, sql);
    assert_eq!(rows.len(), 1, "expected one row for {sql:?}: {rows:?}");
    assert_eq!(
        rows[0].len(),
        1,
        "expected one column for {sql:?}: {rows:?}"
    );

    match rows[0].get(0) {
        Some(Value::Int32(value)) => i64::from(*value),
        Some(Value::Int64(value)) => *value,
        Some(value) => panic!("expected integer scalar for {sql:?}, got {value:?}"),
        None => unreachable!("row length was asserted to be one"),
    }
}

fn assert_point_row(client: &mut RemoteClient, id: i64, amount: i64) {
    let sql = format!("SELECT id, amount FROM {TABLE} WHERE id = {id}");
    let rows = query_rows(client, &sql);
    assert_eq!(
        rows,
        vec![Row::new(vec![Value::Int64(id), Value::Int64(amount)])],
        "unexpected point-select result for id {id}"
    );
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_write() {
    require_phase("write");

    let password = password();
    let mut client = plain_client(&password);

    let existing_rows = match client.execute(&format!("SELECT COUNT(*) FROM {TABLE}")) {
        Ok(StatementResult::Query(result)) => {
            assert_eq!(
                result.rows.len(),
                1,
                "COUNT query returned unexpected rows: {:?}",
                result.rows
            );
            match result.rows[0].values() {
                [Value::Int32(value)] => i64::from(*value),
                [Value::Int64(value)] => *value,
                values => panic!("COUNT query returned unexpected values: {values:?}"),
            }
        }
        Ok(other) => panic!("COUNT query returned a non-query result: {other:?}"),
        Err(HtapError::NotFound(_)) => {
            client
                .execute(&format!(
                    "CREATE TABLE {TABLE} \
                     (id BIGINT PRIMARY KEY, label VARCHAR(32), amount BIGINT)"
                ))
                .unwrap_or_else(|err| panic!("failed to create smoke table: {err}"));
            0
        }
        Err(err) => panic!("failed to inspect smoke table: {err}"),
    };

    match existing_rows {
        0 => {
            client
                .execute(&format!(
                    "INSERT INTO {TABLE} (id, label, amount) VALUES \
                     (1, 'first', 10), (2, 'second', 20)"
                ))
                .unwrap_or_else(|err| panic!("failed to insert initial smoke rows: {err}"));

            client
                .execute("BEGIN")
                .unwrap_or_else(|err| panic!("failed to begin smoke transaction: {err}"));
            client
                .execute(&format!(
                    "INSERT INTO {TABLE} (id, label, amount) VALUES (3, 'committed', 30)"
                ))
                .unwrap_or_else(|err| panic!("failed to insert transactional smoke row: {err}"));
            client
                .execute("COMMIT")
                .unwrap_or_else(|err| panic!("failed to commit smoke transaction: {err}"));
        }
        3 => {
            client
                .execute(&format!(
                    "INSERT INTO {TABLE} (id, label, amount) VALUES \
                     (4, 'post-kill', 40), (5, 'post-kill-transaction', 50)"
                ))
                .unwrap_or_else(|err| panic!("failed to insert post-kill row: {err}"));
        }
        5 => {}
        count => panic!("unexpected pre-existing smoke row count: {count}"),
    }

    assert_point_row(&mut client, 1, 10);

    let count = scalar_i64(&mut client, &format!("SELECT COUNT(*) FROM {TABLE}"));
    let sum = scalar_i64(&mut client, &format!("SELECT SUM(amount) FROM {TABLE}"));
    match count {
        3 => assert_eq!(sum, 60, "unexpected initial aggregate sum"),
        5 => assert_eq!(sum, 150, "unexpected post-kill aggregate sum"),
        other => panic!("unexpected smoke row count after write: {other}"),
    }
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_verify() {
    require_phase("verify");

    let password = password();
    let mut client = plain_client(&password);

    assert_point_row(&mut client, 1, 10);
    assert_point_row(&mut client, 2, 20);
    assert_point_row(&mut client, 3, 30);

    let count = scalar_i64(&mut client, &format!("SELECT COUNT(*) FROM {TABLE}"));
    let sum = scalar_i64(&mut client, &format!("SELECT SUM(amount) FROM {TABLE}"));

    match count {
        3 => assert_eq!(sum, 60, "unexpected aggregate before post-kill write"),
        5 => {
            assert_point_row(&mut client, 4, 40);
            assert_point_row(&mut client, 5, 50);
            assert_eq!(sum, 150, "unexpected aggregate after post-kill write");
        }
        other => panic!("unexpected persisted smoke row count: {other}"),
    }
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_wrong_password_rejected() {
    require_phase("wrong_password");

    let correct_password = password();
    let wrong_password = format!("{correct_password}-wrong");

    match RemoteClient::connect(address(), Some(&wrong_password)) {
        Err(HtapError::PermissionDenied(message)) => {
            assert!(
                message.contains("Access denied"),
                "wrong-password error did not contain the expected authentication detail: {message}"
            );
        }
        Err(other) => panic!(
            "wrong password must return HtapError::PermissionDenied (MySQL 1045), got {other:?}"
        ),
        Ok(_) => panic!("wrong password unexpectedly authenticated"),
    }
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_tls_ok() {
    require_phase("tls_ok");

    let server_name = required_env("HTAP_DOCKER_SMOKE_SERVER_NAME");
    let mut client =
        tls_client(server_name).unwrap_or_else(|err| panic!("TLS connection failed: {err}"));
    client
        .ping()
        .unwrap_or_else(|err| panic!("TLS COM_PING failed: {err}"));
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_plaintext_rejected_when_secure_transport_required() {
    require_phase("plaintext_rejected");

    let password = password();
    match RemoteClient::connect(address(), Some(&password)) {
        Err(HtapError::PermissionDenied(message)) => {
            assert!(
                message.contains("Secure transport required"),
                "secure-transport rejection lacked the expected detail: {message}"
            );
        }
        Err(other) => panic!(
            "plaintext against require-secure-transport must return HtapError::PermissionDenied, \
             got {other:?}"
        ),
        Ok(_) => panic!("plaintext connection unexpectedly passed require-secure-transport"),
    }
}

#[test]
#[ignore = "run via ci/docker-smoke.sh"]
fn smoke_tls_bad_server_name_rejected() {
    require_phase("tls_bad_name");

    required_env("HTAP_DOCKER_SMOKE_SERVER_NAME");
    let bad_name = "docker-smoke-invalid.example";

    match tls_client(bad_name.to_owned()) {
        Err(HtapError::Io(error)) => {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::InvalidData,
                "rustls certificate-name failure had an unexpected I/O error kind: {error}"
            );
            let message = error.to_string().to_ascii_lowercase();
            assert!(
                message.contains("certificate")
                    || message.contains("not valid")
                    || message.contains("name"),
                "rustls certificate-name failure lacked certificate detail: {message}"
            );
        }
        Err(other) => panic!(
            "bad TLS server name must be wrapped as HtapError::Io by the wire client, got {other:?}"
        ),
        Ok(_) => panic!("TLS connection unexpectedly accepted an invalid server name"),
    }
}
