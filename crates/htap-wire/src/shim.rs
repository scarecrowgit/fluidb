//! Compatibility shim for a small set of statements MySQL clients issue on connection
//! start-up that the engine itself cannot answer.
//!
//! Since Phase 10, every connection owns a real [`htap_server::Session`] (see
//! [`crate::server`]): `SET`, `SELECT @@sysvar`, and `SELECT @uservar` all flow through it and
//! are answered by the real system/user-variable registry rather than faked here. Only three
//! things remain shimmed:
//! - `USE <db>` / `COM_INIT_DB`: the server has one flat namespace, so this just validates the
//!   name against [`ACCEPTED_SCHEMAS`].
//! - `SELECT 1` / `SELECT VERSION()` / `SELECT DATABASE()` / `SELECT SCHEMA()`: trivial
//!   constant probes some clients send that the general query executor does not implement
//!   (no scalar functions, no zero-table `SELECT 1`).
//! - `SET CHARACTER SET <x>` / `SET CHARSET <x>`: MySQL-specific positional `SET` forms (as
//!   opposed to `SET NAMES <x>`, which the vendored parser handles as `Set::SetNames`) that
//!   `vendor/sqlparser` has no AST node for at all, so `htap_sql::parse_one` fails on them
//!   before a session ever gets a chance to run them. Verified directly against the vendored
//!   parser: `SET CHARACTER SET utf8mb4` and `SET CHARSET utf8mb4` both fail to parse, while
//!   `SET NAMES utf8mb4` parses fine.
//!
//! Everything else is passed to the connection's [`htap_server::Session::execute`] unchanged.

use htap_common::types::{ColumnDef, DataType, Row, Value};

use crate::proto::{ACCEPTED_SCHEMAS, DEFAULT_SCHEMA, SERVER_VERSION};

/// Result of intercepting a statement before it reaches the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum ShimOutcome {
    /// Answer with an OK packet.
    Ok,
    /// Answer with a constant result set.
    Rows {
        /// Output columns.
        columns: Vec<ColumnDef>,
        /// Output rows.
        rows: Vec<Row>,
    },
    /// The client selected a database; the server validates the name.
    UseDb(String),
}

/// Returns `true` if `name` is an accepted logical database name.
pub fn is_accepted_schema(name: &str) -> bool {
    ACCEPTED_SCHEMAS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

fn string_col(name: &str, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::String,
        nullable,
        primary_key: false,
    }
}

fn int_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: false,
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Strips a trailing `LIMIT <n>` clause, if any.
fn strip_limit(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    if let Some(idx) = lower.rfind(" limit ") {
        let tail = lower[idx + 7..].trim();
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return s[..idx].trim_end();
        }
    }
    s
}

/// Returns `true` for the two MySQL positional `SET` forms `SET CHARACTER SET <x>` / `SET
/// CHARSET <x>` that `vendor/sqlparser` cannot parse at all (see module docs). `lower` is
/// already trimmed and lowercased with any trailing `;` removed.
fn is_unparseable_charset_set(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("set ") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("character set ") || rest.starts_with("charset ")
}

/// Intercepts client start-up statements the engine cannot answer itself. Returns `None` for
/// ordinary SQL, which the caller runs through the connection's `htap_server::Session`.
pub fn try_shim(sql: &str) -> Option<ShimOutcome> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    if is_unparseable_charset_set(&lower) {
        return Some(ShimOutcome::Ok);
    }

    if let Some(rest) = lower.strip_prefix("use ") {
        let name = rest.trim().trim_matches('`');
        if !name.is_empty() && name.chars().all(is_ident_char) {
            return Some(ShimOutcome::UseDb(name.to_string()));
        }
        return None;
    }

    let body = lower.strip_prefix("select ")?;
    let body = strip_limit(body.trim());

    if body == "1" {
        return Some(ShimOutcome::Rows {
            columns: vec![int_col("1")],
            rows: vec![Row::new(vec![Value::Int64(1)])],
        });
    }
    if body == "version()" {
        return Some(ShimOutcome::Rows {
            columns: vec![string_col("version()", false)],
            rows: vec![Row::new(vec![Value::String(SERVER_VERSION.into())])],
        });
    }
    if body == "database()" || body == "schema()" {
        return Some(ShimOutcome::Rows {
            columns: vec![string_col("database()", true)],
            rows: vec![Row::new(vec![Value::String(DEFAULT_SCHEMA.into())])],
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_charset_set_forms_return_ok() {
        assert_eq!(try_shim("SET CHARACTER SET utf8mb4"), Some(ShimOutcome::Ok));
        assert_eq!(try_shim("set charset utf8mb4;"), Some(ShimOutcome::Ok));
        assert_eq!(try_shim("SET CHARSET DEFAULT"), Some(ShimOutcome::Ok));
    }

    #[test]
    fn shim_no_longer_answers_plain_set_or_sysvar_reads() {
        // Phase 10 task 9: these now flow to the connection's `htap_server::Session`, not the
        // shim, so every one of these returns `None` here.
        assert_eq!(try_shim("SET NAMES utf8mb4"), None);
        assert_eq!(try_shim("set autocommit=1;"), None);
        assert_eq!(
            try_shim("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED"),
            None
        );
        assert_eq!(try_shim("SELECT @@version_comment"), None);
        assert_eq!(try_shim("SELECT @@autocommit"), None);
        assert_eq!(try_shim("SELECT @x"), None);
        assert_eq!(try_shim("settle"), None);
    }

    #[test]
    fn shim_version_comment_single_row() {
        match try_shim("SELECT VERSION()").unwrap() {
            ShimOutcome::Rows { rows, .. } => {
                assert_eq!(rows[0].values(), &[Value::String(SERVER_VERSION.into())]);
            }
            other => panic!("unexpected {other:?}"),
        }
        match try_shim("SELECT 1 limit 1").unwrap() {
            ShimOutcome::Rows { rows, .. } => assert_eq!(rows[0].values(), &[Value::Int64(1)]),
            other => panic!("unexpected {other:?}"),
        }
        match try_shim("SELECT database()").unwrap() {
            ShimOutcome::Rows { rows, .. } => {
                assert_eq!(rows[0].values(), &[Value::String("htap".into())]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn shim_use_db() {
        assert_eq!(
            try_shim("USE htap"),
            Some(ShimOutcome::UseDb("htap".into()))
        );
        assert_eq!(
            try_shim("use `fluidb`;"),
            Some(ShimOutcome::UseDb("fluidb".into()))
        );
        assert!(is_accepted_schema("HTAP"));
        assert!(!is_accepted_schema("other"));
    }

    #[test]
    fn shim_passthrough_for_normal_sql() {
        assert_eq!(try_shim("SELECT id FROM t WHERE id = 1"), None);
        assert_eq!(try_shim("INSERT INTO t VALUES (1)"), None);
        assert_eq!(try_shim("CREATE TABLE t (id BIGINT PRIMARY KEY)"), None);
        assert_eq!(try_shim(""), None);
    }
}
