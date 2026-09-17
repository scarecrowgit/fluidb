//! Per-connection prepared-statement registry: `COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`/
//! `SEND_LONG_DATA` state (Phase 11 plan task 4).
//!
//! Pure data structure, no I/O: packet framing, decoding, and dispatch live in [`crate::server`].
//! One registry is owned by each connection (never shared across connections; unlike
//! [`htap_server::Session`], which already lives one-per-connection in [`crate::server`], this
//! state has no counterpart inside `htap-server` at all, since prepared statements are a wire
//! protocol concept with no meaning to the embedded engine).

use std::collections::HashMap;

use htap_common::types::ColumnDef;
use sqlparser::ast::Statement;

use crate::binary_codec::ParamType;
use crate::proto::ER_NET_PACKET_TOO_LARGE;

/// Maximum number of simultaneously prepared statements per connection.
pub const MAX_PREPARED_STATEMENTS: usize = 4096;

/// Default cap, in bytes, on the total `COM_STMT_SEND_LONG_DATA` payload buffered at once for a
/// connection, summed across every parameter of every one of its prepared statements.
///
/// Used only by [`PreparedStatementRegistry::default`] (standalone construction, e.g. tests);
/// `htap_wire::server` instead constructs each connection's registry with the wire server's own
/// configured `WireServerConfig::max_allowed_packet` (Phase 11 plan task 8), so this constant
/// exists purely as a MySQL-compatible fallback default, matching
/// [`htap_sql::DEFAULT_MAX_ALLOWED_PACKET`].
pub const DEFAULT_MAX_LONG_DATA_BYTES: usize = htap_sql::DEFAULT_MAX_ALLOWED_PACKET as usize;

/// One prepared statement: its parsed template, parameter count, best-effort output schema, and
/// mutable per-statement state (`COM_STMT_SEND_LONG_DATA` buffers, the amendment-A2 bound-
/// parameter-type cache, and poisoning).
#[derive(Debug, Clone)]
pub struct PreparedStmt {
    /// The parsed statement template. Never mutated in place; each `EXECUTE` substitutes
    /// placeholders into a fresh clone (see `crate::server::respond_stmt_execute`), so the same
    /// prepared statement can be executed repeatedly with different parameters.
    pub statement: Statement,
    /// Number of `?` placeholders (`checked_placeholder_count` at `PREPARE` time).
    pub num_params: u16,
    /// Best-effort result-set column schema (`htap_sql::resolve_prepare_output_schema`); `None`
    /// when it could not be statically inferred (see that function's doc for exactly when).
    pub output_schema: Option<Vec<ColumnDef>>,
    /// Cached `(type, unsigned)` list from the last `COM_STMT_EXECUTE` that set
    /// `new_params_bound_flag = 1`, used to decode a later `EXECUTE` that sets the flag to 0
    /// (amendment A2). `None` until the first `EXECUTE`.
    pub cached_types: Option<Vec<ParamType>>,
    /// Bytes accumulated so far via `COM_STMT_SEND_LONG_DATA`, keyed by parameter index. A
    /// parameter present here (even with an empty `Vec`) means the next `EXECUTE`'s payload
    /// carries no value bytes for it (see `binary_codec::decode_execute`'s `long_data_pending`).
    pub long_data: HashMap<u16, Vec<u8>>,
    /// Set once a `COM_STMT_SEND_LONG_DATA` targeting this statement fails (unknown parameter
    /// index or the connection's long-data byte cap exceeded): the stored `(code, sqlstate,
    /// message)` is returned verbatim by the next `EXECUTE` instead of attempting it (decision 5:
    /// `SEND_LONG_DATA` has no response of its own, so its errors must surface later).
    pub poisoned: Option<(u16, &'static str, String)>,
}

impl PreparedStmt {
    /// Creates a freshly prepared statement with no long data, no cached types, and no poison.
    pub fn new(
        statement: Statement,
        num_params: u16,
        output_schema: Option<Vec<ColumnDef>>,
    ) -> Self {
        Self {
            statement,
            num_params,
            output_schema,
            cached_types: None,
            long_data: HashMap::new(),
            poisoned: None,
        }
    }

    /// Returns, for each parameter index in order, whether `COM_STMT_SEND_LONG_DATA` has already
    /// buffered bytes for it (the `long_data_pending` argument `binary_codec::decode_execute`
    /// needs).
    pub fn long_data_pending(&self) -> Vec<bool> {
        (0..self.num_params)
            .map(|i| self.long_data.contains_key(&i))
            .collect()
    }

    /// Total bytes currently buffered across every parameter of this statement.
    fn long_data_len(&self) -> usize {
        self.long_data.values().map(Vec::len).sum()
    }
}

/// Per-connection registry of prepared statements, keyed by server-assigned statement id.
#[derive(Debug)]
pub struct PreparedStatementRegistry {
    max_statements: usize,
    max_long_data_bytes: usize,
    next_id: u32,
    statements: HashMap<u32, PreparedStmt>,
    /// Running total of every statement's [`PreparedStmt::long_data_len`], maintained
    /// incrementally so [`Self::append_long_data`] never needs to re-sum the whole registry to
    /// check the cap.
    total_long_data_bytes: usize,
}

impl Default for PreparedStatementRegistry {
    fn default() -> Self {
        Self::new(MAX_PREPARED_STATEMENTS, DEFAULT_MAX_LONG_DATA_BYTES)
    }
}

impl PreparedStatementRegistry {
    /// Creates an empty registry with the given per-connection caps.
    pub fn new(max_statements: usize, max_long_data_bytes: usize) -> Self {
        Self {
            max_statements,
            max_long_data_bytes,
            next_id: 1,
            statements: HashMap::new(),
            total_long_data_bytes: 0,
        }
    }

    /// Number of currently prepared statements.
    pub fn len(&self) -> usize {
        self.statements.len()
    }

    /// Returns `true` if no statement is currently prepared.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// Registers a freshly prepared statement and returns its assigned id.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message (not an [`htap_common::HtapError`]: this is a wire-level
    /// resource limit, not an engine error) if the connection has already reached
    /// [`Self::new`]'s `max_statements` cap.
    pub fn insert(&mut self, stmt: PreparedStmt) -> Result<u32, String> {
        if self.statements.len() >= self.max_statements {
            return Err(format!(
                "maximum number of prepared statements ({}) reached for this connection",
                self.max_statements
            ));
        }
        // `next_id` wraps (u32) long before a real connection could ever prepare that many
        // statements, but once it does, it can land on an id still in use by a long-lived
        // statement that was never closed: since `self.statements.len() < self.max_statements`
        // is already guaranteed above, at least one id is free, so skip past every id still
        // occupied rather than silently overwriting (and thereby leaking) that statement.
        let mut id = if self.next_id == 0 { 1 } else { self.next_id };
        while self.statements.contains_key(&id) {
            id = id.wrapping_add(1);
            if id == 0 {
                id = 1;
            }
        }
        self.next_id = id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.statements.insert(id, stmt);
        Ok(id)
    }

    /// Looks up a prepared statement by id.
    pub fn get(&self, id: u32) -> Option<&PreparedStmt> {
        self.statements.get(&id)
    }

    /// Looks up a prepared statement by id, mutably.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut PreparedStmt> {
        self.statements.get_mut(&id)
    }

    /// Closes (discards) a prepared statement; a no-op if `id` is unknown (`COM_STMT_CLOSE` has
    /// no response either way, so there is nothing to report).
    pub fn close(&mut self, id: u32) {
        if let Some(stmt) = self.statements.remove(&id) {
            self.total_long_data_bytes -= stmt.long_data_len();
        }
    }

    /// Discards every prepared statement (backs `COM_RESET_CONNECTION` and a successful
    /// `COM_CHANGE_USER`).
    pub fn clear(&mut self) {
        self.statements.clear();
        self.total_long_data_bytes = 0;
    }

    /// Appends bytes to a parameter's long-data buffer (`COM_STMT_SEND_LONG_DATA`).
    ///
    /// Silently ignored if `id` is unknown (nothing to poison; the protocol has no response for
    /// this command regardless). If `param_index` is out of range for the statement, or if
    /// appending `data` would push the connection's total buffered long-data bytes over the
    /// configured cap, the statement is poisoned instead of appending (decision 5) and `data` is
    /// dropped.
    pub fn append_long_data(&mut self, id: u32, param_index: u16, data: &[u8]) {
        let max = self.max_long_data_bytes;
        let prospective_total = self.total_long_data_bytes + data.len();
        let Some(stmt) = self.statements.get_mut(&id) else {
            return;
        };
        if param_index >= stmt.num_params {
            stmt.poisoned = Some((
                1064,
                "42000",
                format!(
                    "COM_STMT_SEND_LONG_DATA: parameter index {param_index} out of range for a \
                     statement with {} parameter(s)",
                    stmt.num_params
                ),
            ));
            return;
        }
        if prospective_total > max {
            stmt.poisoned = Some(net_packet_too_large_error(max));
            return;
        }
        self.total_long_data_bytes = prospective_total;
        stmt.long_data
            .entry(param_index)
            .or_default()
            .extend_from_slice(data);
    }

    /// Clears a statement's buffered long data (backs `COM_STMT_EXECUTE`'s "clear long data
    /// after the execute" step); a no-op if `id` is unknown.
    pub fn clear_long_data(&mut self, id: u32) {
        if let Some(stmt) = self.statements.get_mut(&id) {
            self.total_long_data_bytes -= stmt.long_data_len();
            stmt.long_data.clear();
        }
    }

    /// `COM_STMT_RESET`: clears a statement's buffered long data, poison, and cached parameter
    /// types; a no-op if `id` is unknown.
    pub fn reset_statement(&mut self, id: u32) {
        if let Some(stmt) = self.statements.get_mut(&id) {
            self.total_long_data_bytes -= stmt.long_data_len();
            stmt.long_data.clear();
            stmt.poisoned = None;
            stmt.cached_types = None;
        }
    }
}

/// Builds the `(code, sqlstate, message)` a long-data cap overflow poisons a statement with,
/// reusing [`ER_NET_PACKET_TOO_LARGE`] (semantically exact: the client sent more data than this
/// connection accepts).
fn net_packet_too_large_error(max: usize) -> (u16, &'static str, String) {
    let (code, state) = ER_NET_PACKET_TOO_LARGE;
    (
        code,
        state,
        format!("COM_STMT_SEND_LONG_DATA exceeds the maximum long-data size of {max} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_statement() -> Statement {
        htap_sql::parse_one("SELECT 1").unwrap()
    }

    fn dummy_stmt(num_params: u16) -> PreparedStmt {
        PreparedStmt::new(dummy_statement(), num_params, Some(Vec::new()))
    }

    #[test]
    fn insert_get_close_round_trip() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        let id1 = reg.insert(dummy_stmt(0)).unwrap();
        let id2 = reg.insert(dummy_stmt(2)).unwrap();
        assert_ne!(id1, id2);
        assert_eq!(reg.get(id1).unwrap().num_params, 0);
        assert_eq!(reg.get(id2).unwrap().num_params, 2);
        assert_eq!(reg.len(), 2);
        reg.close(id1);
        assert!(reg.get(id1).is_none());
        assert_eq!(reg.len(), 1);
        // Closing an unknown id is a harmless no-op.
        reg.close(id1);
    }

    #[test]
    fn insert_rejects_over_the_statement_cap() {
        let mut reg = PreparedStatementRegistry::new(2, 1024);
        reg.insert(dummy_stmt(0)).unwrap();
        reg.insert(dummy_stmt(0)).unwrap();
        assert!(reg.insert(dummy_stmt(0)).is_err());
    }

    #[test]
    fn clear_drops_every_statement_and_long_data_accounting() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        let id = reg.insert(dummy_stmt(1)).unwrap();
        reg.append_long_data(id, 0, b"hello");
        assert_eq!(reg.total_long_data_bytes, 5);
        reg.clear();
        assert!(reg.is_empty());
        assert_eq!(reg.total_long_data_bytes, 0);
    }

    #[test]
    fn append_long_data_accumulates_and_clears() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        let id = reg.insert(dummy_stmt(1)).unwrap();
        reg.append_long_data(id, 0, b"foo");
        reg.append_long_data(id, 0, b"bar");
        assert_eq!(reg.get(id).unwrap().long_data.get(&0).unwrap(), b"foobar");
        assert_eq!(reg.get(id).unwrap().long_data_pending(), vec![true]);
        reg.clear_long_data(id);
        assert!(reg.get(id).unwrap().long_data.is_empty());
        assert_eq!(reg.get(id).unwrap().long_data_pending(), vec![false]);
        assert_eq!(reg.total_long_data_bytes, 0);
    }

    #[test]
    fn append_long_data_unknown_statement_is_a_no_op() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        reg.append_long_data(999, 0, b"data");
        assert_eq!(reg.total_long_data_bytes, 0);
    }

    #[test]
    fn append_long_data_poisons_on_out_of_range_param_index() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        let id = reg.insert(dummy_stmt(1)).unwrap();
        reg.append_long_data(id, 5, b"data");
        let stmt = reg.get(id).unwrap();
        assert!(stmt.long_data.is_empty());
        let (code, _, msg) = stmt.poisoned.clone().unwrap();
        assert_eq!(code, 1064);
        assert!(msg.contains("out of range"));
    }

    #[test]
    fn append_long_data_poisons_over_the_byte_cap() {
        let mut reg = PreparedStatementRegistry::new(10, 4);
        let id = reg.insert(dummy_stmt(1)).unwrap();
        reg.append_long_data(id, 0, b"hello"); // 5 bytes > cap of 4
        let stmt = reg.get(id).unwrap();
        assert!(stmt.long_data.is_empty());
        let (code, _, _) = stmt.poisoned.clone().unwrap();
        assert_eq!(code, ER_NET_PACKET_TOO_LARGE.0);
        assert_eq!(reg.total_long_data_bytes, 0);
    }

    #[test]
    fn insert_skips_ids_still_in_use_when_next_id_wraps() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        // Force `next_id` to be about to wrap, with the id it would land on (1, after wrapping
        // past 0) already occupied by a still-open statement.
        let occupied = reg.insert(dummy_stmt(0)).unwrap();
        reg.next_id = u32::MAX;
        // The next insert lands on `u32::MAX`, then wraps to `0` -> remapped to `1`, which is
        // still occupied by `occupied`: it must skip past it rather than overwrite it.
        let first = reg.insert(dummy_stmt(0)).unwrap();
        assert_eq!(first, u32::MAX);
        let second = reg.insert(dummy_stmt(0)).unwrap();
        assert_ne!(
            second, occupied,
            "wraparound must never reassign an id still in use"
        );
        assert!(reg.get(occupied).is_some(), "occupied id must survive");
        assert!(reg.get(first).is_some());
        assert!(reg.get(second).is_some());
    }

    #[test]
    fn reset_statement_clears_long_data_poison_and_cache() {
        let mut reg = PreparedStatementRegistry::new(10, 1024);
        let id = reg.insert(dummy_stmt(1)).unwrap();
        reg.append_long_data(id, 5, b"oops"); // poisons (out of range)
        reg.get_mut(id).unwrap().cached_types = Some(vec![ParamType {
            mysql_type: 0x03,
            unsigned: false,
        }]);
        reg.reset_statement(id);
        let stmt = reg.get(id).unwrap();
        assert!(stmt.poisoned.is_none());
        assert!(stmt.cached_types.is_none());
        assert!(stmt.long_data.is_empty());
    }
}
