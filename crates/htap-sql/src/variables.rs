//! MySQL-compatible system variable registry.
//!
//! Backs three call sites: [`crate::expr::Expr::Variable`] evaluation (`SELECT @@name`),
//! `SET <target> = <value>` classification, and the session-layer implementation of
//! [`crate::expr::VariableLookup`] (`htap-server`, Phase 10).
//!
//! Every name here was previously faked ad hoc by `htap_wire::shim::system_variable` for
//! client start-up probing; this registry is the single source of truth going forward; the
//! wire-layer shim is retired in favor of it (see Phase 10 plan, task 9). Two documented MySQL
//! aliases exist: `tx_isolation`/`transaction_isolation` and `tx_read_only`/`transaction_read_only`.
//! Names not listed here (`version_comment`, `version`, `max_allowed_packet`, ...) are constant
//! for the life of the process; three names are *dynamic*: `autocommit`, `transaction_isolation`
//! (alias `tx_isolation`), and `transaction_read_only` (alias `tx_read_only`), whose values
//! reflect live session state through [`SessionVarsView`].

use htap_common::error::{HtapError, Result};
use htap_common::types::Value;

/// Reported `@@version`. `htap_wire::proto::SERVER_VERSION` references this constant directly
/// (that crate depends on this one) so the two never drift apart; see
/// `htap-wire/src/proto.rs`'s `version_constant_matches_variable_registry` test.
pub const REPORTED_VERSION: &str = "8.0.0-fluidb-0.1.0";

/// One registry entry: a canonical name plus any MySQL aliases for it.
struct VariableEntry {
    canonical: &'static str,
    aliases: &'static [&'static str],
    /// Whether the value is read from [`SessionVarsView`] (`true`) or is a process-wide
    /// constant computed by [`constant_value`] (`false`).
    dynamic: bool,
}

/// All known system variables. Constant values mirror the previous
/// `htap_wire::shim::system_variable` fakes; dynamic ones are resolved through
/// [`SessionVarsView`].
const VARIABLES: &[VariableEntry] = &[
    VariableEntry {
        canonical: "version_comment",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "version",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "max_allowed_packet",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "wait_timeout",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "interactive_timeout",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "net_write_timeout",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "net_read_timeout",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "autocommit",
        aliases: &[],
        dynamic: true,
    },
    VariableEntry {
        canonical: "auto_increment_increment",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "lower_case_table_names",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "socket",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "transaction_isolation",
        aliases: &["tx_isolation"],
        dynamic: true,
    },
    VariableEntry {
        canonical: "transaction_read_only",
        aliases: &["tx_read_only"],
        dynamic: true,
    },
    VariableEntry {
        canonical: "sql_mode",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "time_zone",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "system_time_zone",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "character_set_client",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "character_set_connection",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "character_set_results",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "character_set_server",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "character_set_database",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "collation_connection",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "collation_server",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "collation_database",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "init_connect",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "license",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "performance_schema",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "query_cache_size",
        aliases: &[],
        dynamic: false,
    },
    VariableEntry {
        canonical: "query_cache_type",
        aliases: &[],
        dynamic: false,
    },
];

fn find_entry(name: &str) -> Option<&'static VariableEntry> {
    VARIABLES.iter().find(|e| {
        e.canonical.eq_ignore_ascii_case(name)
            || e.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    })
}

fn unknown_variable(name: &str) -> HtapError {
    HtapError::Unsupported(format!("unknown system variable '{name}'"))
}

/// Value of a variable whose value never changes for the life of the process.
fn constant_value(canonical: &str) -> Value {
    match canonical {
        "version_comment" => Value::String("fluidb".into()),
        "version" => Value::String(REPORTED_VERSION.into()),
        "max_allowed_packet" => Value::Int64(16 * 1024 * 1024),
        "wait_timeout" | "interactive_timeout" => Value::Int64(28_800),
        "net_write_timeout" | "net_read_timeout" => Value::Int64(60),
        "auto_increment_increment" => Value::Int64(1),
        "lower_case_table_names" => Value::Int64(0),
        "socket" => Value::Null,
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
        other => unreachable!("constant_value called for non-constant variable '{other}'"),
    }
}

/// Live per-session state needed to answer dynamic system variables.
///
/// Implemented by `htap_server::Session` (Phase 10, task 7).
pub trait SessionVarsView {
    /// Current `autocommit` setting.
    fn autocommit(&self) -> bool;
    /// Isolation level reported for `transaction_isolation` / `tx_isolation`. Always
    /// `"REPEATABLE-READ"` today: this engine offers snapshot isolation and rejects any other
    /// requested level (see [`validate_isolation_level`]) rather than silently downgrading, so
    /// there is never another value to report. Exposed through the trait, rather than
    /// hard-coded in [`system_variable_value`], so a future isolation level does not need a
    /// registry change.
    fn transaction_isolation(&self) -> &'static str {
        "REPEATABLE-READ"
    }
    /// Whether the current/next transaction is read-only (`transaction_read_only` /
    /// `tx_read_only`).
    fn transaction_read_only(&self) -> bool;
}

/// Value of a variable resolved from live session state.
fn dynamic_value(canonical: &str, session: &dyn SessionVarsView) -> Value {
    match canonical {
        "autocommit" => Value::Int64(session.autocommit() as i64),
        "transaction_isolation" => Value::String(session.transaction_isolation().to_string()),
        "transaction_read_only" => Value::Int64(session.transaction_read_only() as i64),
        other => unreachable!("dynamic_value called for non-dynamic variable '{other}'"),
    }
}

/// Resolves the current value of a system variable (`@@name`, case-insensitive, aliases
/// resolved to the same entry).
///
/// Returns `Err(HtapError::Unsupported(..))` if `name` is not a recognized system variable.
pub fn system_variable_value(name: &str, session: &dyn SessionVarsView) -> Result<Value> {
    let entry = find_entry(name).ok_or_else(|| unknown_variable(name))?;
    Ok(if entry.dynamic {
        dynamic_value(entry.canonical, session)
    } else {
        constant_value(entry.canonical)
    })
}

/// Scope of a `SET` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetScope {
    /// `SET [SESSION] name = ...` / `SET @@name = ...` / `SET @@session.name = ...`. The only
    /// scope this single-node embedded engine offers.
    Session,
    /// `SET GLOBAL name = ...` / `SET @@global.name = ...`.
    Global,
}

/// How the session layer should handle a `SET <target> = <value>` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetClass {
    /// A dynamic variable (`autocommit`, `transaction_isolation`/`tx_isolation`,
    /// `transaction_read_only`/`tx_read_only`) whose value the session layer must apply.
    Dynamic,
    /// A known variable this engine treats as read-only: accepted as a no-op (amendment A2 —
    /// MySQL connectors set these unconditionally on connect, e.g. `character_set_client`,
    /// `sql_mode`, `time_zone`).
    ReadOnlyNoOp,
}

/// Classifies a `SET <target> = <value>` statement for the session layer.
///
/// `GLOBAL` scope is always rejected with [`HtapError::Unsupported`]: this is a single-node
/// embedded engine with no server-wide variable distinct from the session's. An unrecognized
/// name is also rejected with [`HtapError::Unsupported`]. This function only classifies
/// `name = value` targets; the `SET NAMES ...` / `SET CHARACTER SET ...` statement forms are
/// not variable assignments and are recognized separately by the caller.
pub fn classify_set_target(scope: SetScope, name: &str) -> Result<SetClass> {
    if scope == SetScope::Global {
        return Err(HtapError::Unsupported(format!(
            "SET GLOBAL is not supported (variable '{name}'); only SESSION scope is available"
        )));
    }
    let entry = find_entry(name).ok_or_else(|| unknown_variable(name))?;
    Ok(if entry.dynamic {
        SetClass::Dynamic
    } else {
        SetClass::ReadOnlyNoOp
    })
}

/// Validates a `[SESSION] TRANSACTION ISOLATION LEVEL <level>` request (`BEGIN`/`SET`).
///
/// Only `REPEATABLE READ` is accepted (case-insensitive, `-` or ` ` as the separator word):
/// this engine offers snapshot isolation and reports it as `REPEATABLE READ`; weaker or
/// stronger levels are rejected rather than silently downgraded or upgraded.
pub fn validate_isolation_level(level: &str) -> Result<()> {
    let normalized = level.trim().to_ascii_uppercase().replace('-', " ");
    if normalized == "REPEATABLE READ" {
        Ok(())
    } else {
        Err(HtapError::Unsupported(format!(
            "isolation level '{}' is not supported; this engine offers snapshot isolation, reported as REPEATABLE READ",
            level.trim()
        )))
    }
}

fn invalid_autocommit(value: &Value) -> HtapError {
    HtapError::InvalidArgument(format!(
        "invalid value for 'autocommit': expected 0, 1, ON, OFF, TRUE, or FALSE, found {value}"
    ))
}

/// Parses a `SET autocommit = <value>` right-hand side.
///
/// Accepts `0`/`1` (the form MySQL documents) and, case-insensitively, `ON`/`OFF` and
/// `TRUE`/`FALSE`: MySQL accepts both spellings for boolean-typed session variables generally,
/// and drivers occasionally send the word form, so both are treated as MySQL-compatible here.
pub fn parse_autocommit_value(value: &Value) -> Result<bool> {
    match value {
        Value::Int32(0) | Value::Int64(0) => Ok(false),
        Value::Int32(1) | Value::Int64(1) => Ok(true),
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_uppercase().as_str() {
            "0" | "OFF" | "FALSE" => Ok(false),
            "1" | "ON" | "TRUE" => Ok(true),
            _ => Err(invalid_autocommit(value)),
        },
        other => Err(invalid_autocommit(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSession {
        autocommit: bool,
        read_only: bool,
    }

    impl SessionVarsView for FakeSession {
        fn autocommit(&self) -> bool {
            self.autocommit
        }
        fn transaction_read_only(&self) -> bool {
            self.read_only
        }
    }

    const SESSION: FakeSession = FakeSession {
        autocommit: true,
        read_only: false,
    };

    #[test]
    fn constant_variables_and_aliases_resolve() {
        assert_eq!(
            system_variable_value("version_comment", &SESSION).unwrap(),
            Value::String("fluidb".into())
        );
        assert_eq!(
            system_variable_value("VERSION_COMMENT", &SESSION).unwrap(),
            Value::String("fluidb".into())
        );
        assert_eq!(
            system_variable_value("max_allowed_packet", &SESSION).unwrap(),
            Value::Int64(16 * 1024 * 1024)
        );
        assert_eq!(
            system_variable_value("socket", &SESSION).unwrap(),
            Value::Null
        );
        assert_eq!(
            system_variable_value("character_set_client", &SESSION).unwrap(),
            Value::String("utf8mb4".into())
        );
        assert!(system_variable_value("no_such_variable", &SESSION).is_err());
    }

    #[test]
    fn dynamic_variables_read_session_state() {
        let on = FakeSession {
            autocommit: true,
            read_only: false,
        };
        let off = FakeSession {
            autocommit: false,
            read_only: true,
        };
        assert_eq!(
            system_variable_value("autocommit", &on).unwrap(),
            Value::Int64(1)
        );
        assert_eq!(
            system_variable_value("autocommit", &off).unwrap(),
            Value::Int64(0)
        );
        assert_eq!(
            system_variable_value("tx_isolation", &on).unwrap(),
            Value::String("REPEATABLE-READ".into())
        );
        assert_eq!(
            system_variable_value("transaction_isolation", &on).unwrap(),
            Value::String("REPEATABLE-READ".into())
        );
        assert_eq!(
            system_variable_value("tx_read_only", &off).unwrap(),
            Value::Int64(1)
        );
        assert_eq!(
            system_variable_value("transaction_read_only", &on).unwrap(),
            Value::Int64(0)
        );
    }

    #[test]
    fn classify_set_target_dynamic_readonly_unknown_and_global() {
        assert_eq!(
            classify_set_target(SetScope::Session, "autocommit").unwrap(),
            SetClass::Dynamic
        );
        assert_eq!(
            classify_set_target(SetScope::Session, "tx_isolation").unwrap(),
            SetClass::Dynamic
        );
        assert_eq!(
            classify_set_target(SetScope::Session, "sql_mode").unwrap(),
            SetClass::ReadOnlyNoOp
        );
        assert_eq!(
            classify_set_target(SetScope::Session, "character_set_client").unwrap(),
            SetClass::ReadOnlyNoOp
        );
        assert!(matches!(
            classify_set_target(SetScope::Session, "no_such_variable"),
            Err(HtapError::Unsupported(_))
        ));
        assert!(matches!(
            classify_set_target(SetScope::Global, "autocommit"),
            Err(HtapError::Unsupported(_))
        ));
        assert!(matches!(
            classify_set_target(SetScope::Global, "sql_mode"),
            Err(HtapError::Unsupported(_))
        ));
    }

    #[test]
    fn isolation_level_validation() {
        assert!(validate_isolation_level("REPEATABLE READ").is_ok());
        assert!(validate_isolation_level("repeatable-read").is_ok());
        assert!(validate_isolation_level("  Repeatable Read  ").is_ok());
        let err = validate_isolation_level("READ COMMITTED").unwrap_err();
        assert!(matches!(err, HtapError::Unsupported(_)));
        assert!(err.to_string().contains("snapshot isolation"));
        assert!(validate_isolation_level("SERIALIZABLE").is_err());
        assert!(validate_isolation_level("READ UNCOMMITTED").is_err());
    }

    #[test]
    fn autocommit_value_parsing() {
        assert!(!parse_autocommit_value(&Value::Int64(0)).unwrap());
        assert!(parse_autocommit_value(&Value::Int64(1)).unwrap());
        assert!(!parse_autocommit_value(&Value::Int32(0)).unwrap());
        assert!(parse_autocommit_value(&Value::Int32(1)).unwrap());
        assert!(parse_autocommit_value(&Value::Bool(true)).unwrap());
        assert!(!parse_autocommit_value(&Value::Bool(false)).unwrap());
        assert!(parse_autocommit_value(&Value::String("on".into())).unwrap());
        assert!(!parse_autocommit_value(&Value::String("OFF".into())).unwrap());
        assert!(parse_autocommit_value(&Value::String("true".into())).unwrap());
        assert!(!parse_autocommit_value(&Value::String("False".into())).unwrap());
        assert!(parse_autocommit_value(&Value::Int64(2)).is_err());
        assert!(parse_autocommit_value(&Value::String("maybe".into())).is_err());
        assert!(parse_autocommit_value(&Value::Null).is_err());
    }
}
