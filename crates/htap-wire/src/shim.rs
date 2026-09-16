//! Compatibility shim for statements MySQL clients issue on connection start-up.
//!
//! Drivers and CLIs probe system variables (`SELECT @@version_comment`), set session
//! variables (`SET NAMES utf8mb4`), or select a database (`USE htap`) before running user
//! SQL. None of these exist in the engine, so the wire layer answers a small, explicit set of
//! them itself. Everything else is passed to [`htap_server::LocalServer::execute`] unchanged.
//!
//! `SET` statements are accepted as no-ops: every statement auto-commits regardless of any
//! `autocommit` or isolation-level setting a client claims to apply.

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

/// Known system variables and their constant values.
fn system_variable(name: &str) -> Option<Value> {
    let v = match name {
        "version_comment" => Value::String("fluidb".into()),
        "version" => Value::String(SERVER_VERSION.into()),
        "max_allowed_packet" => Value::Int64(16 * 1024 * 1024),
        "wait_timeout" | "interactive_timeout" => Value::Int64(28_800),
        "net_write_timeout" | "net_read_timeout" => Value::Int64(60),
        "autocommit" => Value::Int64(1),
        "auto_increment_increment" => Value::Int64(1),
        "lower_case_table_names" => Value::Int64(0),
        "socket" => Value::Null,
        "tx_isolation" | "transaction_isolation" => Value::String("REPEATABLE-READ".into()),
        "tx_read_only" | "transaction_read_only" => Value::Int64(0),
        "sql_mode" => Value::String(String::new()),
        "time_zone" | "system_time_zone" => Value::String("UTC".into()),
        "character_set_client"
        | "character_set_connection"
        | "character_set_results"
        | "character_set_server"
        | "character_set_database" => Value::String("utf8mb4".into()),
        "collation_connection" | "collation_server" | "collation_database" => {
            Value::String("utf8mb4_general_ci".into())
        }
        "init_connect" | "license" => Value::String(String::new()),
        "performance_schema" | "query_cache_size" | "query_cache_type" => Value::Int64(0),
        _ => return None,
    };
    Some(v)
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Parses one `@@[session.|global.]name [[AS] alias]` projection item.
fn parse_sysvar_item(item: &str) -> Option<(String, Value)> {
    let item = item.trim();
    let rest = item.strip_prefix("@@")?;
    let lower = rest.to_ascii_lowercase();
    let lower = lower
        .strip_prefix("session.")
        .or_else(|| lower.strip_prefix("global."))
        .unwrap_or(&lower);
    let name_len = lower.chars().take_while(|c| is_ident_char(*c)).count();
    if name_len == 0 {
        return None;
    }
    let name = &lower[..name_len];
    let value = system_variable(name)?;
    let tail = lower[name_len..].trim();
    let column = if tail.is_empty() {
        item.to_string()
    } else {
        let alias = tail.strip_prefix("as ").map(str::trim).unwrap_or(tail);
        if alias.is_empty() || !alias.chars().all(is_ident_char) {
            return None;
        }
        alias.to_string()
    };
    Some((column, value))
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
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

/// Intercepts client start-up statements. Returns `None` for ordinary SQL.
pub fn try_shim(sql: &str) -> Option<ShimOutcome> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    if lower == "set" || lower.starts_with("set ") {
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
    let original_body = strip_limit(trimmed[7..].trim());

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

    if !body.starts_with("@@") {
        return None;
    }
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for item in split_top_level_commas(original_body) {
        let (name, value) = parse_sysvar_item(item)?;
        let col = match &value {
            Value::Int64(_) => int_col(&name),
            Value::Null => string_col(&name, true),
            _ => string_col(&name, false),
        };
        columns.push(col);
        values.push(value);
    }
    Some(ShimOutcome::Rows {
        columns,
        rows: vec![Row::new(values)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_set_returns_ok() {
        assert_eq!(try_shim("SET NAMES utf8mb4"), Some(ShimOutcome::Ok));
        assert_eq!(try_shim("set autocommit=1;"), Some(ShimOutcome::Ok));
        assert_eq!(
            try_shim("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED"),
            Some(ShimOutcome::Ok)
        );
        assert_eq!(try_shim("settle"), None);
    }

    #[test]
    fn shim_version_comment_single_row() {
        match try_shim("select @@version_comment limit 1").unwrap() {
            ShimOutcome::Rows { columns, rows } => {
                assert_eq!(columns[0].name, "@@version_comment");
                assert_eq!(rows[0].values(), &[Value::String("fluidb".into())]);
            }
            other => panic!("unexpected {other:?}"),
        }
        match try_shim("SELECT VERSION()").unwrap() {
            ShimOutcome::Rows { rows, .. } => {
                assert_eq!(rows[0].values(), &[Value::String(SERVER_VERSION.into())]);
            }
            other => panic!("unexpected {other:?}"),
        }
        match try_shim("SELECT 1").unwrap() {
            ShimOutcome::Rows { rows, .. } => assert_eq!(rows[0].values(), &[Value::Int64(1)]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn shim_multi_sysvar_with_aliases() {
        let sql = "SELECT @@session.auto_increment_increment AS auto_increment_increment, \
                   @@character_set_client AS character_set_client, @@max_allowed_packet, @@socket";
        match try_shim(sql).unwrap() {
            ShimOutcome::Rows { columns, rows } => {
                let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(
                    names,
                    [
                        "auto_increment_increment",
                        "character_set_client",
                        "@@max_allowed_packet",
                        "@@socket"
                    ]
                );
                assert_eq!(
                    rows[0].values(),
                    &[
                        Value::Int64(1),
                        Value::String("utf8mb4".into()),
                        Value::Int64(16 * 1024 * 1024),
                        Value::Null
                    ]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(try_shim("SELECT @@no_such_variable"), None);
        assert_eq!(try_shim("SELECT @@version, id FROM t"), None);
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
