use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

const A_STRING_ALPHABET: &str = r##"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!"#$%&'()*+,-./:;<=>?@[\]^_`{|}~ "##;

fn setup() -> Result<(TempDir, Arc<LocalServer>), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    server.execute("CREATE TABLE string_escaping (id INT PRIMARY KEY, value VARCHAR(500))")?;
    Ok((directory, server))
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn query_string(session: &mut Session) -> Result<String, Box<dyn Error>> {
    let StatementResult::Query(result) =
        session.execute("SELECT value FROM string_escaping WHERE id = 1")?
    else {
        panic!("expected query result");
    };
    match result.rows()[0].get(0).expect("result value") {
        Value::String(value) => Ok(value.clone()),
        other => panic!("expected string value, got {other:?}"),
    }
}

#[test]
fn unescaped_trailing_backslash_fails_to_insert() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    assert!(session
        .execute("INSERT INTO string_escaping (id, value) VALUES (1, 'ends with backslash\\')")
        .is_err());
    Ok(())
}

#[test]
fn unescaped_backslash_quote_is_reinterpreted_by_parser() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    let value = r#"backslash\'quote"#;

    // The parser accepts backslash-quote and interprets it as an embedded quote.
    session.execute(&format!(
        "INSERT INTO string_escaping (id, value) VALUES (1, '{value}')"
    ))?;

    assert_eq!(query_string(&mut session)?, "backslash'quote");
    Ok(())
}

#[test]
fn escaped_literals_round_trip_exactly() -> Result<(), Box<dyn Error>> {
    let values = [
        "ends with backslash\\",
        r#"backslash\'quote"#,
        "two consecutive quotes: ''",
        A_STRING_ALPHABET,
    ];

    for value in values {
        let (_directory, server) = setup()?;
        let mut session = server.open_session()?;
        session.execute(&format!(
            "INSERT INTO string_escaping (id, value) VALUES (1, {})",
            sql_literal(value)
        ))?;
        assert_eq!(query_string(&mut session)?, value);
    }
    Ok(())
}
