//! Server-side sessions: explicit transactions with snapshot isolation and session-buffered
//! uncommitted writes (Phase 10).
//!
//! # Model
//!
//! A [`Session`] is opened against an `Arc<LocalServer>` and owns at most one open transaction
//! at a time ([`OpenTxn`]). Uncommitted writes never touch the rowstore or the transaction
//! journal: they live only in the session's [`WriteSet`], keyed by `(partition_id, encoded
//! primary key)` so a later statement's write to the same row simply replaces the earlier one
//! (last-writer-within-txn-wins). `COMMIT` builds one [`TransactionRequest`] from the
//! accumulated write set and runs it through the existing 2PC path exactly once, against the
//! transaction's own pinned snapshot (never `TransactionManager::commit_request`; see
//! [`Session::commit`]).
//!
//! # Control statements and autocommit (Phase 10 task 7)
//!
//! [`Session::execute`] intercepts `BEGIN`/`START TRANSACTION`, `COMMIT`, `ROLLBACK`, and `SET`
//! before binding. With `autocommit` off (the MySQL-compatible default is on), the first
//! statement after `Idle`/`COMMIT`/`ROLLBACK` implicitly begins a transaction; `BEGIN` while one
//! is already open implicitly commits it first; `SET autocommit = 1` while one is open commits
//! it. DDL is rejected inside any open transaction (explicit or implicit) without poisoning it.
//! `@name` user variables and `@@name` system variables are resolved through
//! [`SessionVariables`], which also backs `SET @x = expr` (evaluated with a table-less
//! [`EvalContext`], so `SET @b = @a + 1` reads `@a` back through the same session).
//!
//! # Poisoning and commit-time revalidation (Phase 10 task 6)
//!
//! A read inside an explicit transaction can hit a [`HtapError::Conflict`] that has nothing to
//! do with an ordinary statement error (NOT NULL, type mismatch, ...): most notably, the
//! stale-snapshot-vs-columnar-base conflict in [`crate::scan_partition_compact`]. Any such
//! `Conflict` from a statement inside an open transaction poisons it: every further ordinary
//! statement fails with the same stored message. A `COMMIT` on a poisoned transaction (storage-
//! reviewer finding F7 corrected this doc, which previously read as though `COMMIT` left a
//! poisoned transaction open pending a separate `ROLLBACK`) returns that same stored message
//! *and* discards the transaction in the same call, exactly as `ROLLBACK` would — there is no
//! separate "poisoned but still open" state to roll back afterward; a `ROLLBACK` issued after a
//! poisoned `COMMIT` is simply a no-op against an already-`Idle` session. Ordinary statement
//! errors do not poison (MySQL does not abort a transaction for a failed `UPDATE`, so neither do
//! we). At `COMMIT`, buffered partitions are revalidated against a freshly loaded catalog (a
//! concurrent `DROP TABLE`/`ALTER` since the writes were buffered is a conflict, not a silent
//! write to the wrong table) — like a poisoned transaction, this outcome both returns `Conflict`
//! and discards the transaction; by contrast, an *infrastructure* failure in that same
//! pre-decision work (a catalog load I/O error, say) leaves the transaction open exactly as it
//! was, so `COMMIT` can simply be retried (F7). A `DurablePending` outcome from the underlying
//! 2PC commit itself (the one step that is genuinely irreversible once attempted) moves the
//! session to [`SessionState::CommitOutcomePending`], which rejects every further statement,
//! `ROLLBACK` included, with the original `DurablePending` error (never `Conflict`; see
//! [`outcome_pending_error`] — storage-reviewer finding F1).

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use htap_catalog::store::CatalogStore;
use htap_common::encode_key;
use htap_common::error::{HtapError, Result};
use htap_common::password::{
    constant_time_eq_20, hash_native_password, verify_empty_password_response,
    verify_native_password_hash,
};
use htap_common::types::{Mutation, Row, Value};
use htap_common::Version;
use htap_rowstore::Snapshot;
use htap_sql::ast::BoundStatement;
use htap_sql::expr::VariableLookup;
use htap_sql::result::StatementResult;
use htap_sql::{
    classify_set_target, parse_autocommit_value, system_variable_value, validate_isolation_level,
    QueryBody, SessionVarsView, SetClass, SetScope, DEFAULT_MAX_ALLOWED_PACKET,
};
use htap_txn::{
    intent_frame_size_bound, ParticipantId, ParticipantWork, RowstoreParticipant, Transaction,
    TransactionRequest, MAX_PAYLOAD_SIZE,
};
use sqlparser::ast::{
    ContextModifier, Expr as SqlExpr, ObjectName, ObjectNamePart, Set, Statement as SqlStatement,
    TransactionAccessMode, TransactionMode,
};

use crate::{
    privilege::{check_privileges, check_statement_visible},
    CatalogSnapshot, ExecMode, LocalServer, PartitionId, TableId,
};
use htap_catalog::AccountId;

/// Authenticated identity associated with a server session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// The embedded/default session identity, which retains existing unrestricted behavior.
    Superuser,
    /// An authenticated catalog account.
    Account {
        /// Stable catalog account identifier.
        id: AccountId,
        /// Account login name.
        username: String,
    },
}

/// Unique, process-lifetime-monotonic identifier for a [`Session`].
///
/// Allocated from [`LocalServer`]'s internal counter; never reused within a process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// Returns the inner identifier value.
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("principal", &self.principal)
            .finish()
    }
}

/// One buffered mutation in a session's uncommitted write set.
#[derive(Debug, Clone)]
pub(crate) struct BufferedMutation {
    /// Table the mutation belongs to; used by [`WriteSet::touched_partitions`] to revalidate at
    /// commit time that a buffered partition's table still exists, unchanged.
    pub table_id: TableId,
    /// The buffered mutation; its own `partition_id`/`key` fields are the write set's map key.
    pub mutation: Mutation,
    /// This mutation's own JSON-encoded length (`serde_json::to_vec(&self.mutation).len()`),
    /// precomputed once on insert so [`WriteSet`] can track its aggregate encoded size
    /// incrementally instead of re-encoding the whole write set on every statement
    /// (storage-reviewer finding F9). Exact: `RowstoreParticipant::encode_payload`'s JSON array
    /// encoding of `Vec<Mutation>` is `[` + each element's own standalone JSON bytes joined by
    /// `,` + `]`, and a value's JSON encoding never depends on its position in an array, so this
    /// is precisely that element's contribution, not merely an upper bound.
    encoded_len: usize,
}

/// Computes `mutation`'s own standalone JSON-encoded length, as it would contribute to
/// [`RowstoreParticipant::encode_payload`]'s `Vec<Mutation>` array encoding.
fn mutation_encoded_len(mutation: &Mutation) -> Result<usize> {
    serde_json::to_vec(mutation)
        .map(|bytes| bytes.len())
        .map_err(|e| HtapError::InvalidArgument(format!("failed to serialize mutation: {e}")))
}

/// Exact total JSON-encoded size of a `Vec<Mutation>` array containing `count` elements whose own
/// standalone encoded lengths sum to `sum_encoded_len`: `[` + elements joined by `,` + `]`.
fn array_encoded_size(count: usize, sum_encoded_len: usize) -> usize {
    if count == 0 {
        2 // "[]"
    } else {
        2 + sum_encoded_len + (count - 1) // brackets + elements + (count - 1) commas
    }
}

/// Uncommitted mutations buffered by an open [`Session`] transaction.
///
/// Keyed by `(partition_id, encoded primary key)`: a later write to the same row replaces the
/// earlier one within the same transaction (last-writer-within-txn-wins), and lookups by exact
/// key are used to overlay reads ("read your own writes") below relational operators.
#[derive(Debug, Clone, Default)]
pub(crate) struct WriteSet {
    entries: BTreeMap<(u64, Vec<u8>), BufferedMutation>,
    /// Running sum of every entry's own `encoded_len` (storage-reviewer finding F9), maintained
    /// incrementally by [`Self::insert`] and [`Self::try_merge`] so the aggregate encoded payload
    /// size (`array_encoded_size`) never needs a full re-encode to check the cap.
    sum_encoded_len: usize,
}

impl WriteSet {
    /// Creates an empty write set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if no mutations are buffered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of buffered mutations.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn key_of(mutation: &Mutation) -> (u64, Vec<u8>) {
        match mutation {
            Mutation::Put {
                partition_id, key, ..
            } => (*partition_id, key.clone()),
            Mutation::Delete { partition_id, key } => (*partition_id, key.clone()),
        }
    }

    /// Buffers one mutation, replacing any earlier mutation to the same `(partition_id, key)`,
    /// and maintains `sum_encoded_len` incrementally (F9).
    pub fn insert(&mut self, table_id: TableId, mutation: Mutation) -> Result<()> {
        let key = Self::key_of(&mutation);
        let encoded_len = mutation_encoded_len(&mutation)?;
        let replaced = self.entries.insert(
            key,
            BufferedMutation {
                table_id,
                mutation,
                encoded_len,
            },
        );
        if let Some(old) = replaced {
            self.sum_encoded_len -= old.encoded_len;
        }
        self.sum_encoded_len += encoded_len;
        Ok(())
    }

    /// Looks up a buffered mutation for an exact `(partition_id, key)`.
    pub fn get(&self, partition_id: u64, key: &[u8]) -> Option<&BufferedMutation> {
        self.entries.get(&(partition_id, key.to_vec()))
    }

    /// Iterates buffered mutations belonging to one partition, in ascending encoded-key order.
    pub fn entries_for_partition(
        &self,
        partition_id: u64,
    ) -> impl Iterator<Item = &BufferedMutation> {
        self.entries
            .range((partition_id, Vec::new())..(partition_id + 1, Vec::new()))
            .map(|(_, v)| v)
    }

    /// Returns every buffered mutation, in ascending `(partition_id, key)` order.
    pub fn all_mutations(&self) -> Vec<Mutation> {
        self.entries.values().map(|b| b.mutation.clone()).collect()
    }

    /// Returns the distinct `(partition_id, table_id)` pairs touched by this write set.
    ///
    /// Used for commit-time catalog revalidation (Phase 10 task 6b): every partition a buffered
    /// write touched must still belong to the same table when the transaction commits, or a
    /// concurrent `DROP TABLE`/partition `ALTER` between the buffered writes and `COMMIT` would
    /// otherwise be silently applied against a partition it no longer owns.
    pub fn touched_partitions(&self) -> Vec<(u64, TableId)> {
        let mut seen: BTreeMap<u64, TableId> = BTreeMap::new();
        for ((partition_id, _key), buffered) in &self.entries {
            seen.entry(*partition_id).or_insert(buffered.table_id);
        }
        seen.into_iter().collect()
    }

    /// Atomically merges `delta` into `self`.
    ///
    /// Storage-reviewer finding F9: rather than re-encoding the whole merged write set as JSON on
    /// every statement (quadratic over a transaction with many statements), this computes what
    /// the merged set's exact encoded size (`array_encoded_size`) would be from `self` and
    /// `delta`'s already-known per-entry `encoded_len`s in `O(delta.len())`, and rejects before
    /// mutating `self` at all if that would exceed
    /// [`RowstoreParticipant::encode_payload`]'s cap (`htap_txn::MAX_PAYLOAD_SIZE`, 16 MiB) — so a
    /// statement that would overflow it never partially pollutes the write set. This incremental
    /// size is exact (see [`BufferedMutation::encoded_len`]'s doc), not merely a conservative
    /// bound, so it can never let the real cap be exceeded; [`Session::commit_locked`] still runs
    /// the real `RowstoreParticipant::encode_payload` once more at commit as the authoritative
    /// check.
    ///
    /// Fix-pass item 3a: a payload that fits under `MAX_PAYLOAD_SIZE` can still produce a durable
    /// `Intent` journal frame bigger than the journal's own `max_frame_size` once `serde_json`
    /// re-encodes it as a JSON number array (see [`intent_frame_size_bound`]'s doc); this rejects
    /// that case here too, using the same conservative bound
    /// `htap_txn::TransactionManager::commit` itself uses, so it never partially pollutes the
    /// write set with a payload that would only be discovered oversize at `COMMIT`. Fix-pass
    /// round 3, item 3(d): `max_frame_size` is the caller's own `TransactionManager`'s actual
    /// configured limit (`TransactionManager::max_frame_size`), not the `DEFAULT_MAX_FRAME_SIZE`
    /// constant, so this stays correct even if a manager is ever built with non-default
    /// `JournalOptions`.
    pub fn try_merge(&mut self, delta: WriteSet, max_frame_size: usize) -> Result<()> {
        let mut new_count = self.entries.len();
        let mut new_sum = self.sum_encoded_len;
        for (key, buffered) in &delta.entries {
            match self.entries.get(key) {
                Some(existing) => {
                    new_sum = new_sum - existing.encoded_len + buffered.encoded_len;
                }
                None => {
                    new_count += 1;
                    new_sum += buffered.encoded_len;
                }
            }
        }

        let estimated_size = array_encoded_size(new_count, new_sum);
        if estimated_size > MAX_PAYLOAD_SIZE {
            return Err(HtapError::InvalidArgument(format!(
                "mutation payload size {estimated_size} exceeds maximum 16 MiB"
            )));
        }
        // This session always commits its whole write set as exactly one `RowstoreParticipant`
        // work unit (`ParticipantId::new(1)`; see `Session::commit_locked`), so the bound's
        // participant count is always 1.
        let intent_bound = intent_frame_size_bound(estimated_size, 1);
        if intent_bound > max_frame_size {
            return Err(HtapError::InvalidArgument(format!(
                "mutation payload size {estimated_size} would produce an estimated durable \
                 Intent frame of {intent_bound} bytes, exceeding the journal's maximum frame \
                 size of {max_frame_size} bytes"
            )));
        }

        for (key, buffered) in delta.entries {
            self.entries.insert(key, buffered);
        }
        self.sum_encoded_len = new_sum;
        Ok(())
    }
}

/// Overlays a session's buffered writes for one partition onto already-scanned base rows.
///
/// `base_rows` must already be projected to `layout`, an ascending list of schema column
/// indices that is a superset of `primary_key` (callers request the extra primary-key columns
/// from storage when an overlay is active; see `scan_partition_compact`). Buffered `Put`s
/// replace or append rows and buffered `Delete`s remove them. The result is re-projected to
/// `output_columns` (a subset of `layout`) and returned in ascending encoded-primary-key order.
pub(crate) fn overlay_rows(
    base_rows: Vec<Row>,
    layout: &[usize],
    output_columns: &[usize],
    primary_key: &[usize],
    partition_id: u64,
    write_set: &WriteSet,
) -> Result<Vec<Row>> {
    let pk_positions: Vec<usize> = primary_key
        .iter()
        .map(|&pk_idx| {
            layout.iter().position(|&c| c == pk_idx).ok_or_else(|| {
                HtapError::Internal(format!(
                    "primary key column {pk_idx} missing from overlay row layout"
                ))
            })
        })
        .collect::<Result<_>>()?;

    let project = |row: &Row, row_layout: &[usize]| -> Result<Row> {
        let mut values = Vec::with_capacity(output_columns.len());
        for &col in output_columns {
            let pos = row_layout.iter().position(|&c| c == col).ok_or_else(|| {
                HtapError::Internal(format!("column {col} missing from overlay row layout"))
            })?;
            values.push(row.get(pos).cloned().ok_or_else(|| {
                HtapError::Internal(format!("row missing column at position {pos}"))
            })?);
        }
        Ok(Row::new(values))
    };

    let mut keyed: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
    for row in &base_rows {
        let mut pk_values = Vec::with_capacity(pk_positions.len());
        for &pos in &pk_positions {
            pk_values.push(row.get(pos).cloned().ok_or_else(|| {
                HtapError::Internal(format!("row missing primary key column at position {pos}"))
            })?);
        }
        let key = encode_key(&pk_values)?;
        if write_set.get(partition_id, &key).is_some() {
            // Overwritten or deleted by a buffered mutation; the buffered mutation wins below.
            continue;
        }
        keyed.insert(key, project(row, layout)?);
    }

    for buffered in write_set.entries_for_partition(partition_id) {
        if let Mutation::Put { key, row, .. } = &buffered.mutation {
            let identity_layout: Vec<usize> = (0..row.values().len()).collect();
            keyed.insert(key.clone(), project(row, &identity_layout)?);
        }
        // Delete: nothing to insert; a matching base row, if any, was already excluded above.
    }

    Ok(keyed.into_values().collect())
}

/// State of an open explicit transaction.
pub(crate) struct OpenTxn {
    /// MVCC snapshot pinned at `BEGIN`; every read inside the transaction observes this version
    /// overlaid with `write_set`.
    pub snapshot: Snapshot,
    /// Whether the transaction was opened `READ ONLY`: a write statement inside it is rejected
    /// before it can be buffered (see [`Session::execute`]).
    pub read_only: bool,
    /// Mutations buffered by statements executed so far in this transaction.
    pub write_set: WriteSet,
    /// Set once a statement inside this transaction hits a [`HtapError::Conflict`] (write-write
    /// conflict surfaced only at `COMMIT`, or the stale-snapshot-vs-columnar-base conflict
    /// surfaced by a read); once poisoned every further statement except `ROLLBACK` fails with
    /// this stored message. An ordinary statement error (NOT NULL, type mismatch, ...) does not
    /// poison: MySQL does not abort a transaction for a failed statement, so neither do we.
    pub poisoned: Option<String>,
}

/// Lifecycle state of a [`Session`].
pub(crate) enum SessionState {
    /// No open transaction; statements run autocommit.
    Idle,
    /// A transaction is open and accumulating a write set.
    InTxn(OpenTxn),
    /// A prior `COMMIT` returned `DurablePending` (fsync ambiguity after the commit record was
    /// durable): the outcome cannot be trusted either way from this process, so every further
    /// statement, including `ROLLBACK`, is rejected until server recovery resolves it. The
    /// original `DurablePending` details are stored so every rejection can re-raise the exact
    /// same non-retryable error rather than an `HtapError::Conflict`, which the wire layer maps
    /// to MySQL 1213/`40001` ("rolled back, retry") and would invite a client to double-apply the
    /// write (storage-reviewer finding F1).
    CommitOutcomePending {
        txn_id: u64,
        version: Version,
        reason: String,
    },
}

/// Error returned once a session enters [`SessionState::CommitOutcomePending`]: the original
/// `DurablePending` error, reconstructed from the stored details, never a `Conflict`.
///
/// # Panics
///
/// Panics if `state` is not [`SessionState::CommitOutcomePending`]; every call site first checks
/// that with `matches!`.
fn outcome_pending_error(state: &SessionState) -> HtapError {
    match state {
        SessionState::CommitOutcomePending {
            txn_id,
            version,
            reason,
        } => HtapError::DurablePending {
            txn_id: *txn_id,
            version: *version,
            reason: reason.clone(),
        },
        _ => unreachable!("outcome_pending_error called on a non-CommitOutcomePending state"),
    }
}

/// Whether `bound` is DDL: rejected inside any open transaction, explicit or implicit (Phase 10
/// task 7).
fn is_ddl(bound: &BoundStatement) -> bool {
    matches!(
        bound,
        BoundStatement::CreateTable(_)
            | BoundStatement::DropTable(_)
            | BoundStatement::AlterPartitions(_)
            | BoundStatement::AnalyzeTable(_)
            | BoundStatement::CreateUser(_)
            | BoundStatement::AlterUser(_)
            | BoundStatement::DropUser(_)
            | BoundStatement::GrantPrivileges(_)
            | BoundStatement::RevokePrivileges(_)
            | BoundStatement::ShowGrants(_)
    )
}

/// Whether `bound` is a data-modifying statement: rejected inside a `READ ONLY` transaction
/// before it can be buffered.
fn is_write(bound: &BoundStatement) -> bool {
    matches!(
        bound,
        BoundStatement::Insert(_) | BoundStatement::Delete(_) | BoundStatement::Update(_)
    )
}

/// Returns the statement actually executed by an `EXPLAIN ANALYZE`; plain `EXPLAIN` only plans
/// its inner statement and therefore remains permitted inside an open transaction.
fn execution_target(bound: &BoundStatement) -> &BoundStatement {
    match bound {
        BoundStatement::Explain {
            inner,
            analyze: true,
        } => execution_target(inner),
        _ => bound,
    }
}

fn access_denied(username: &str) -> HtapError {
    HtapError::PermissionDenied(format!("access denied for user '{username}'"))
}

/// Authenticates a catalog account and returns its session principal.
pub(crate) fn authenticate_principal(
    server: &LocalServer,
    username: &str,
    scramble: &[u8],
    auth_response: &[u8],
) -> Result<Principal> {
    let catalog = server
        .catalog
        .load()?
        .unwrap_or_else(CatalogSnapshot::empty);
    // Keep unknown, locked, and malformed-password failures on the same SHA-1 verification
    // path as an ordinary failed password attempt.
    let dummy_hash = hash_native_password("htap authentication dummy verifier");
    let Some(account) = catalog.account_by_username(username) else {
        let _ = verify_native_password_hash(scramble, &dummy_hash, auth_response);
        return Err(access_denied(username));
    };

    if account.locked {
        let _ = verify_native_password_hash(scramble, &dummy_hash, auth_response);
        return Err(access_denied(username));
    }

    let verified = match &account.password_hash {
        None if auth_response.is_empty() => true,
        None => {
            let _ = verify_native_password_hash(scramble, &dummy_hash, auth_response);
            false
        }
        Some(hash) if auth_response.is_empty() => {
            verify_empty_password_response(auth_response)
                && constant_time_eq_20(hash, &hash_native_password(""))
        }
        Some(hash) => verify_native_password_hash(scramble, hash, auth_response),
    };
    if !verified {
        return Err(access_denied(username));
    }

    Ok(Principal::Account {
        id: account.id,
        username: account.username.clone(),
    })
}

/// A server-side session: sequential SQL execution against a [`LocalServer`], with at most one
/// open explicit transaction at a time.
///
/// Opened via [`LocalServer::open_session`]. Dropping a session with an open transaction rolls
/// it back on a best-effort basis (uncommitted writes only ever live in the session's own
/// [`WriteSet`], so "rollback" is simply discarding session state; nothing external needs to be
/// undone).
pub struct Session {
    id: SessionId,
    server: Arc<LocalServer>,
    principal: Principal,
    state: SessionState,
    /// `@name` user variables set by `SET @name = expr`, scoped to this session for its
    /// lifetime. An unset user variable reads as `NULL` (see [`SessionVariables::lookup`]).
    user_vars: BTreeMap<String, Value>,
    /// Whether autocommit is enabled. MySQL-compatible default: on.
    autocommit: bool,
    /// `READ ONLY`/`READ WRITE` access mode requested by a `SET [SESSION] TRANSACTION ...`
    /// statement (without an explicit access mode on the next `BEGIN`/`START TRANSACTION`
    /// itself) for the *next* transaction only; consumed and cleared by the next explicit or
    /// implicit `BEGIN`.
    next_txn_read_only: Option<bool>,
    /// Configured `@@max_allowed_packet` (Phase 11 plan task 8), reported dynamically via
    /// [`SessionVarsView::max_allowed_packet`]. Set once by `htap_wire::server` right after
    /// [`LocalServer::open_session`] to the wire server's configured value; embedded sessions
    /// (`LocalServer::execute`, or a `Session` never given a wire connection) keep the
    /// MySQL-compatible default.
    max_allowed_packet: u64,
}

impl Session {
    /// Returns this session's unique identifier.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the identity authenticated for this session.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Replaces this session's identity after successful authentication.
    pub(crate) fn set_principal(&mut self, principal: Principal) {
        self.principal = principal;
    }

    /// Authenticates and switches this session to `username`.
    ///
    /// A successful switch resets session state before replacing the principal. If the session is
    /// waiting for an ambiguous commit outcome, [`Session::reset`] rejects the request and the
    /// existing principal remains unchanged.
    pub fn change_user(
        &mut self,
        username: &str,
        scramble: &[u8],
        auth_response: &[u8],
    ) -> Result<()> {
        let principal = authenticate_principal(&self.server, username, scramble, auth_response)?;
        self.reset()?;
        self.principal = principal;
        Ok(())
    }

    /// Returns `true` if a transaction is currently open.
    pub fn in_transaction(&self) -> bool {
        matches!(self.state, SessionState::InTxn(_))
    }

    /// Returns `true` if autocommit is currently enabled (MySQL-compatible default: on). Used by
    /// `htap-wire` to report `SERVER_STATUS_AUTOCOMMIT` accurately instead of hardcoding it.
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    /// Returns a fresh catalog snapshot.
    ///
    /// Minimal accessor for callers outside this crate that need to resolve schema information
    /// without dispatching a statement (Phase 11 plan task 4: `htap-wire`'s `COM_STMT_PREPARE`
    /// handler resolves a prepared statement's output schema against a snapshot obtained this
    /// way). Never reads this session's own in-flight transaction state; equivalent to what
    /// [`Session::execute_statement`] loads for an ordinary statement.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on a catalog load I/O failure.
    pub fn catalog_snapshot(&self) -> Result<CatalogSnapshot> {
        Ok(self
            .server
            .catalog
            .load()?
            .unwrap_or_else(CatalogSnapshot::empty))
    }

    /// Checks a parsed statement's visibility and returns the catalog snapshot checked.
    ///
    /// Prepared-statement callers must use the returned snapshot for all metadata resolution so
    /// visibility and schema information cannot be resolved against different catalog versions.
    /// This deliberately does not bind: a prepared statement still contains `?` placeholders.
    /// Execution performs the authoritative bound-statement privilege check after substitution.
    pub fn check_statement_visible(&self, statement: &SqlStatement) -> Result<CatalogSnapshot> {
        let catalog = self
            .server
            .catalog
            .load()?
            .unwrap_or_else(CatalogSnapshot::empty);
        check_statement_visible(&self.principal, statement, &catalog)?;
        Ok(catalog)
    }

    /// Sets this session's reported `@@max_allowed_packet` (Phase 11 plan task 8).
    ///
    /// Called once by `htap_wire::server` right after [`LocalServer::open_session`], with the
    /// wire server's own configured `WireServerConfig::max_allowed_packet`; never changed again
    /// for the life of the session (`SET max_allowed_packet = ...` stays a read-only no-op, per
    /// amendment A2 Phase 10 semantics — see `htap_sql::classify_set_target`). Never fails and
    /// never affects any in-flight or future statement's own execution limits (e.g. the rowstore
    /// commit payload cap): it is purely the value this session reports back to `SELECT
    /// @@max_allowed_packet`.
    pub fn set_max_allowed_packet(&mut self, max_allowed_packet: u64) {
        self.max_allowed_packet = max_allowed_packet;
    }

    /// Opens a new [`OpenTxn`] pinning the current visible MVCC version as the read snapshot.
    fn open_new_txn(&mut self, read_only: bool) {
        let snapshot = Snapshot::new(self.server.txn_manager.visible_version());
        self.server
            .register_pinned_snapshot(self.id, snapshot.version);
        self.state = SessionState::InTxn(OpenTxn {
            snapshot,
            read_only,
            write_set: WriteSet::new(),
            poisoned: None,
        });
    }

    /// Starts an explicit transaction, pinning the current visible MVCC version as the read
    /// snapshot (always `READ WRITE`; ignores any pending `SET TRANSACTION READ ONLY`).
    ///
    /// Goes through the same guarded path as SQL `BEGIN`/`START TRANSACTION`
    /// ([`Session::handle_start_transaction`]; storage-reviewer finding F2): rejected outright if
    /// the session is [`SessionState::CommitOutcomePending`] (previously this overwrote that
    /// state unconditionally, so a subsequent `rollback()` would report `Ok` for a session whose
    /// last commit outcome was still ambiguous); an already-open transaction is implicitly
    /// committed first, exactly like `BEGIN` while one is active (previously calling this twice
    /// silently dropped the first transaction's buffered write set instead); and the new
    /// snapshot is pinned under `LocalServer.execution_lock`, exactly like every other snapshot
    /// read in this server. Full `BEGIN`/`START TRANSACTION` SQL semantics (`READ ONLY`/`READ
    /// WRITE` modes, isolation-level validation) live in [`Session::execute`]'s control-statement
    /// handling; this method exists for callers that manage transactions directly rather than
    /// through SQL text, and only ever opens a `READ WRITE` transaction.
    ///
    /// # Errors
    ///
    /// Returns the stored `DurablePending` error if the session is `CommitOutcomePending`, or
    /// whatever [`Session::commit`] returns if an already-open transaction's implicit commit
    /// fails (in which case no new transaction is started).
    pub fn begin(&mut self) -> Result<()> {
        self.begin_guarded_prelude()?;
        self.pin_new_txn_locked(false);
        Ok(())
    }

    /// Rejects outright if the session is `CommitOutcomePending`, then implicitly commits an
    /// already-open transaction, if any. Shared by [`Session::begin`] and
    /// [`Session::handle_start_transaction`] (F2): every path that can open a new transaction
    /// goes through this same guard before doing so.
    fn begin_guarded_prelude(&mut self) -> Result<()> {
        if matches!(self.state, SessionState::CommitOutcomePending { .. }) {
            return Err(outcome_pending_error(&self.state));
        }
        if self.in_transaction() {
            self.commit()?;
        }
        Ok(())
    }

    /// Pins a new transaction's snapshot under `LocalServer.execution_lock`, exactly like every
    /// other snapshot read in this server, via a cloned `Arc<LocalServer>` handle so the guard
    /// does not borrow `self`.
    fn pin_new_txn_locked(&mut self, read_only: bool) {
        let server = Arc::clone(&self.server);
        let _guard = server.execution_lock.lock();
        self.open_new_txn(read_only);
    }

    /// Opens an implicit transaction (autocommit off) consuming any pending `next_txn_read_only`
    /// default.
    fn begin_implicit(&mut self) {
        let read_only = self.next_txn_read_only.take().unwrap_or(false);
        self.open_new_txn(read_only);
    }

    /// Parses `sql` into a single statement and executes it via [`Session::execute_statement`].
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on parse failure, or whatever [`Session::execute_statement`] returns
    /// for the parsed statement.
    pub fn execute(&mut self, sql: &str) -> Result<StatementResult> {
        let statement = htap_sql::parse_one(sql)?;
        self.execute_statement(statement)
    }

    /// Executes one already-parsed statement.
    ///
    /// Control statements (`BEGIN`/`START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SET`) are
    /// intercepted before binding; see the module docs for the full state machine. Every other
    /// statement is bound and dispatched through the open transaction's buffered write set when
    /// one is open (explicit or implicit), or autocommit otherwise, under the same
    /// `LocalServer::execution_lock` serialization as [`LocalServer::execute`] (Phase 10 plan
    /// amendment A1).
    ///
    /// The [`SessionState::CommitOutcomePending`] gate is checked first, before any other work
    /// (Phase 11 plan task 2): every caller that dispatches a parsed [`sqlparser::ast::Statement`]
    /// against this session, not just [`Session::execute`]'s own text path (e.g. a prepared
    /// statement's `EXECUTE`), goes through this same check.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on bind/execution failure; [`HtapError::Conflict`] if the
    /// session's last commit left it in the `CommitOutcomePending` state, if the transaction is
    /// already poisoned, or if this statement's own read hits a new conflict (which poisons the
    /// transaction); [`HtapError::Unsupported`] for DDL inside an open transaction or an
    /// unsupported `SET`/`BEGIN` form; [`HtapError::InvalidArgument`] for a write statement
    /// inside a `READ ONLY` transaction.
    ///
    /// Control statements (`BEGIN`/`START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SET`) are handled
    /// before any lock is taken here: each one that touches shared server state locks
    /// independently for just that part ([`Session::commit`]; the `BEGIN`/`START TRANSACTION`
    /// handler for its snapshot pin), per Phase 10 plan amendment A1's "Session::execute (each
    /// non-control statement)" wording. `execution_lock` is held for the whole duration of an
    /// ordinary statement's catalog load, bind, and dispatch below, exactly like
    /// [`LocalServer::execute`] — never re-locked while already held, which would deadlock
    /// against `parking_lot::Mutex`'s non-reentrant lock.
    pub fn execute_statement(&mut self, statement: SqlStatement) -> Result<StatementResult> {
        if matches!(self.state, SessionState::CommitOutcomePending { .. }) {
            return Err(outcome_pending_error(&self.state));
        }

        match &statement {
            SqlStatement::StartTransaction {
                modes,
                modifier,
                statements,
                exception,
                has_end_keyword,
                ..
            } => {
                if modifier.is_some()
                    || !statements.is_empty()
                    || exception.is_some()
                    || *has_end_keyword
                {
                    return Err(HtapError::Unsupported(
                        "BEGIN/START TRANSACTION with a nested block, modifier, or exception \
                         clause is not supported"
                            .into(),
                    ));
                }
                return self.handle_start_transaction(modes);
            }
            SqlStatement::Commit {
                chain,
                end,
                modifier,
            } => {
                if *chain || *end || modifier.is_some() {
                    return Err(HtapError::Unsupported(
                        "COMMIT AND CHAIN / END TRY|CATCH / modifiers are not supported".into(),
                    ));
                }
                self.commit()?;
                return Ok(StatementResult::ddl(0));
            }
            SqlStatement::Rollback { chain, savepoint } => {
                if *chain || savepoint.is_some() {
                    return Err(HtapError::Unsupported(
                        "ROLLBACK AND CHAIN / TO SAVEPOINT is not supported".into(),
                    ));
                }
                self.rollback()?;
                return Ok(StatementResult::ddl(0));
            }
            SqlStatement::Set(set) => return self.handle_set(set),
            _ => {}
        }

        // Ordinary statement: catalog load, bind, and dispatch all happen under one
        // `execution_lock` acquisition, exactly like `LocalServer::execute`. Locking through a
        // cloned `Arc<LocalServer>` handle (rather than `self.server` directly) means the guard
        // does not borrow `self`, so the `&mut self` calls below (`begin_implicit`,
        // `dispatch_bound` via `&mut self.state`) are not blocked by it.
        let server = Arc::clone(&self.server);
        let _guard = server.execution_lock.lock();

        let catalog = self
            .server
            .catalog
            .load()?
            .unwrap_or_else(CatalogSnapshot::empty);
        check_statement_visible(&self.principal, &statement, &catalog)?;
        let bound = htap_sql::bind(&statement, &catalog)?;
        check_privileges(&self.principal, &bound, &catalog)?;

        // DDL is rejected inside any open transaction, explicit or implicit (autocommit off);
        // the transaction, if any, survives untouched (not poisoned).
        let execution_target = execution_target(&bound);
        if is_ddl(execution_target) && (self.in_transaction() || !self.autocommit) {
            return Err(HtapError::Unsupported(
                "DDL is not supported inside an explicit transaction; COMMIT or ROLLBACK first"
                    .into(),
            ));
        }

        if matches!(self.state, SessionState::Idle) && !self.autocommit {
            self.begin_implicit();
        }

        let read_only_ctx = matches!(&self.state, SessionState::InTxn(t) if t.read_only);
        let vars = SessionVariables {
            user_vars: &self.user_vars,
            autocommit: self.autocommit,
            read_only: read_only_ctx,
            max_allowed_packet: self.max_allowed_packet,
        };

        let outcome = match &mut self.state {
            SessionState::Idle => self.server.dispatch_bound(
                bound,
                &catalog,
                ExecMode::Autocommit,
                &vars,
                &self.principal,
            ),
            SessionState::InTxn(open_txn) => {
                if let Some(reason) = &open_txn.poisoned {
                    return Err(HtapError::Conflict(reason.clone()));
                }
                if open_txn.read_only && is_write(execution_target) {
                    return Err(HtapError::InvalidArgument(
                        "cannot execute a data-modifying statement inside a READ ONLY \
                         transaction"
                            .into(),
                    ));
                }
                let mode = ExecMode::Txn {
                    snapshot: open_txn.snapshot,
                    write_set: &mut open_txn.write_set,
                };
                self.server
                    .dispatch_bound(bound, &catalog, mode, &vars, &self.principal)
            }
            SessionState::CommitOutcomePending { .. } => {
                unreachable!("checked and rejected at the top of `execute` before parsing")
            }
        };

        if let Err(HtapError::Conflict(msg)) = &outcome {
            if let SessionState::InTxn(open_txn) = &mut self.state {
                open_txn.poisoned = Some(msg.clone());
            }
        }
        // Storage-reviewer finding F8: an autocommit write's own 2PC commit
        // (`ExecMode::Autocommit`, dispatched only from `SessionState::Idle` above) can return
        // `DurablePending` exactly like a session's own `COMMIT` can (`Session::commit_locked`);
        // it must quarantine the session the same way, not leave it `Idle` where a later
        // statement could run against a rowstore whose recovery is still outstanding.
        if let Err(HtapError::DurablePending {
            txn_id,
            version,
            reason,
        }) = &outcome
        {
            self.state = SessionState::CommitOutcomePending {
                txn_id: *txn_id,
                version: *version,
                reason: reason.clone(),
            };
        }
        outcome
    }

    /// `BEGIN`/`START TRANSACTION`: implicitly commits an already-open transaction first (on
    /// failure, does not start a new one), then validates any requested isolation level and
    /// resolves the access mode (explicit `READ ONLY`/`READ WRITE` modes on this statement win
    /// over a pending `SET TRANSACTION READ ONLY`/`READ WRITE` default; the default is consumed
    /// either way), and opens the new transaction.
    fn handle_start_transaction(&mut self, modes: &[TransactionMode]) -> Result<StatementResult> {
        self.begin_guarded_prelude()?;

        let mut explicit_read_only: Option<bool> = None;
        for mode in modes {
            match mode {
                TransactionMode::IsolationLevel(level) => {
                    validate_isolation_level(&level.to_string())?;
                }
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => {
                    explicit_read_only = Some(true);
                }
                TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => {
                    explicit_read_only = Some(false);
                }
            }
        }
        let read_only = match explicit_read_only {
            Some(read_only) => {
                self.next_txn_read_only = None;
                read_only
            }
            None => self.next_txn_read_only.take().unwrap_or(false),
        };

        // Pin the new snapshot under `execution_lock` (see `Session::pin_new_txn_locked`).
        // `self.begin_guarded_prelude()` above has already released its own lock by this point
        // (it calls `self.commit()`, which takes and releases `execution_lock` itself), so this
        // does not nest.
        self.pin_new_txn_locked(read_only);

        Ok(StatementResult::ddl(0))
    }

    /// `SET`: `autocommit` / `@@autocommit` / `@@session.autocommit`; `SET [SESSION] TRANSACTION
    /// ISOLATION LEVEL ...` / `READ ONLY` / `READ WRITE`; `@x = expr` (including multiple
    /// comma-separated assignments); `SET NAMES ...` / `SET NAMES DEFAULT` as no-ops; known
    /// read-only variables as no-ops (amendment A2); `GLOBAL` scope and unknown targets rejected
    /// with [`HtapError::Unsupported`].
    fn handle_set(&mut self, set: &Set) -> Result<StatementResult> {
        match set {
            Set::SingleAssignment {
                scope,
                variable,
                values,
                ..
            } => {
                let [value] = values.as_slice() else {
                    return Err(HtapError::Unsupported(format!(
                        "SET statement with {} values is not supported for '{variable}'",
                        values.len()
                    )));
                };
                self.apply_assignment(*scope, variable, value)?;
                Ok(StatementResult::ddl(0))
            }
            Set::MultipleAssignments { assignments } => {
                for assignment in assignments {
                    self.apply_assignment(assignment.scope, &assignment.name, &assignment.value)?;
                }
                Ok(StatementResult::ddl(0))
            }
            Set::SetNames { .. } | Set::SetNamesDefault {} => Ok(StatementResult::ddl(0)),
            Set::SetTransaction {
                modes, snapshot, ..
            } => {
                if snapshot.is_some() {
                    return Err(HtapError::Unsupported(
                        "SET TRANSACTION SNAPSHOT is not supported".into(),
                    ));
                }
                self.apply_transaction_modes(modes)?;
                Ok(StatementResult::ddl(0))
            }
            other => Err(HtapError::Unsupported(format!(
                "SET statement form '{other}' is not supported"
            ))),
        }
    }

    /// Applies the modes of a `SET [SESSION] TRANSACTION ...` statement: validates every
    /// isolation level first (so a request with an invalid level changes nothing), then records
    /// any access mode as the pending default for the *next* `BEGIN`/`START TRANSACTION`.
    fn apply_transaction_modes(&mut self, modes: &[TransactionMode]) -> Result<()> {
        for mode in modes {
            if let TransactionMode::IsolationLevel(level) = mode {
                validate_isolation_level(&level.to_string())?;
            }
        }
        for mode in modes {
            match mode {
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => {
                    self.next_txn_read_only = Some(true);
                }
                TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => {
                    self.next_txn_read_only = Some(false);
                }
                TransactionMode::IsolationLevel(_) => {}
            }
        }
        Ok(())
    }

    /// Classifies and applies one `SET <target> = <value>` assignment.
    fn apply_assignment(
        &mut self,
        scope: Option<ContextModifier>,
        name: &ObjectName,
        value: &SqlExpr,
    ) -> Result<()> {
        match classify_object_name(scope, name)? {
            SetTarget::UserVar(var_name) => {
                let value = self.eval_scalar_expr(value)?;
                self.user_vars.insert(var_name, value);
                Ok(())
            }
            SetTarget::SystemVar { scope, name } => match classify_set_target(scope, &name)? {
                // Amendment A2: MySQL connectors send these unconditionally on connect; accepted
                // as a no-op without evaluating or validating the right-hand side.
                SetClass::ReadOnlyNoOp => Ok(()),
                SetClass::Dynamic => self.apply_dynamic_variable(&name, value),
            },
        }
    }

    /// Applies an assignment to one of the three dynamic system variables (`autocommit`,
    /// `transaction_isolation`/`tx_isolation`, `transaction_read_only`/`tx_read_only`).
    fn apply_dynamic_variable(&mut self, name: &str, value_expr: &SqlExpr) -> Result<()> {
        match name.to_ascii_lowercase().as_str() {
            "autocommit" => {
                let value = self.eval_scalar_expr(value_expr)?;
                let on = parse_autocommit_value(&value)?;
                self.set_autocommit(on)
            }
            "transaction_isolation" | "tx_isolation" => {
                let value = self.eval_scalar_expr(value_expr)?;
                // Only one isolation level is ever valid; `transaction_isolation` always
                // reports it (see `SessionVarsView::transaction_isolation`), so a validated
                // assignment has nothing further to apply.
                validate_isolation_level(&value.to_string())?;
                Ok(())
            }
            "transaction_read_only" | "tx_read_only" => {
                let value = self.eval_scalar_expr(value_expr)?;
                let read_only = parse_bool_flag("transaction_read_only", &value)?;
                self.next_txn_read_only = Some(read_only);
                Ok(())
            }
            other => unreachable!(
                "classify_set_target only returns SetClass::Dynamic for a known dynamic \
                 variable, got '{other}'"
            ),
        }
    }

    /// Sets `autocommit`. Turning it on while an explicit or implicit transaction is open
    /// commits that transaction first (MySQL semantics); turning it on redundantly (already on)
    /// leaves an open transaction untouched, matching MySQL's behavior of only committing on the
    /// off-to-on transition.
    fn set_autocommit(&mut self, on: bool) -> Result<()> {
        let was_off = !self.autocommit;
        self.autocommit = on;
        if on && was_off && self.in_transaction() {
            self.commit()?;
        }
        Ok(())
    }

    /// Evaluates a scalar SQL expression (a `SET @x = expr` or `SET <dynamic var> = expr`
    /// right-hand side) against a table-less context: wraps it as `SELECT <expr>`, binds it
    /// through the same zero-table general-query path `SELECT @x`/`SELECT @@autocommit` use, and
    /// evaluates the single projected value with this session as the [`VariableLookup`], so `SET
    /// @b = @a + 1` reads `@a` back from this same session.
    fn eval_scalar_expr(&self, expr: &SqlExpr) -> Result<Value> {
        let server = Arc::clone(&self.server);
        let _guard = server.execution_lock.lock();
        let sql = format!("SELECT {expr}");
        let statement = htap_sql::parse_one(&sql)?;
        let catalog = self
            .server
            .catalog
            .load()?
            .unwrap_or_else(CatalogSnapshot::empty);
        check_statement_visible(&self.principal, &statement, &catalog)?;
        let bound = htap_sql::bind(&statement, &catalog)?;
        let query = match bound {
            BoundStatement::Query(query) => query,
            _ => {
                return Err(HtapError::Internal(
                    "expected a scalar SET expression to bind as a zero-table query".into(),
                ))
            }
        };
        // SET scalar expressions are evaluated directly from the outer table-less SELECT.
        // Any scalar subquery, including one containing WITH RECURSIVE, is rejected here before
        // the query executor is invoked.
        if !query.subqueries.is_empty() {
            return Err(HtapError::Unsupported(
                "SET does not support subquery expressions".into(),
            ));
        }
        let sel = match &query.body {
            QueryBody::Select(sel) => sel,
            QueryBody::SetOp { .. } => {
                return Err(HtapError::Internal(
                    "expected a single SELECT for a scalar SET expression".into(),
                ))
            }
            QueryBody::RecursiveQueryBody { .. } => {
                return Err(HtapError::Unsupported(
                    "recursive CTE execution not yet implemented".into(),
                ));
            }
        };
        if sel.projection.len() != 1 {
            return Err(HtapError::Internal(
                "expected exactly one projected value for a scalar SET expression".into(),
            ));
        }
        let read_only = matches!(&self.state, SessionState::InTxn(t) if t.read_only);
        let vars = SessionVariables {
            user_vars: &self.user_vars,
            autocommit: self.autocommit,
            read_only,
            max_allowed_packet: self.max_allowed_packet,
        };
        let eval_ctx = htap_sql::eval_context! {
            row: &[],
            current_outer_row: None,
            aggregates: &[],
            output: None,
            subqueries: &[],
            variables: Some(&vars),
            subquery_runner: None,
            subquery_budget: None,
        };
        sel.projection[0].expr.eval(&eval_ctx)
    }

    /// Commits the open transaction, if any.
    ///
    /// - No open transaction: no-op.
    /// - Poisoned: clears the transaction and returns the stored conflict.
    /// - Read-only, or nothing buffered: clears the transaction without running 2PC.
    /// - Otherwise: reloads the catalog and checks that every buffered partition still belongs
    ///   to the same table (a concurrent `DROP TABLE`/`ALTER` is a [`HtapError::Conflict`], not a
    ///   silent write to a partition this transaction no longer recognizes), then commits one
    ///   [`TransactionRequest`] built from the whole write set directly via
    ///   `TransactionManager::commit` against this transaction's own pinned snapshot — never
    ///   `commit_request`, whose `begin()` would instead pin `read_version` at the *current*
    ///   visible version and defeat the prepare-time first-writer-wins check's stale-snapshot
    ///   detection (Phase 10 plan amendment A3). A [`HtapError::Conflict`] from that commit
    ///   clears the transaction (aborted) and propagates; a `DurablePending` outcome instead
    ///   moves the session to [`SessionState::CommitOutcomePending`].
    ///
    /// Holds `LocalServer.execution_lock` for its whole duration (Phase 10 plan amendment A1),
    /// independently of [`Session::execute`] — callers may call this directly (as
    /// `Session::execute`'s `COMMIT`/`BEGIN`-implicit-commit handling and tests do), and
    /// `Session::execute` never calls this while already holding the lock itself (see its doc
    /// comment), so this never nests.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] if the transaction was poisoned, if commit-time catalog
    /// revalidation fails, or if the underlying 2PC commit fails.
    pub fn commit(&mut self) -> Result<()> {
        let server = Arc::clone(&self.server);
        let _guard = server.execution_lock.lock();
        self.commit_locked()
    }

    /// The actual body of [`Session::commit`], run under `execution_lock`.
    ///
    /// Unlike an earlier version of this method, the open transaction is *not* removed from
    /// `self.state` until every fallible pre-decision step below (catalog load, payload encode,
    /// transaction id allocation) has succeeded: an infrastructure failure there (e.g. catalog
    /// I/O error) leaves the transaction exactly as open as it was before this `COMMIT` was
    /// attempted, so the caller can simply retry `COMMIT`, rather than silently aborting it
    /// (storage-reviewer finding F7). It is taken out of `self.state` only at the actual decision
    /// point, immediately before the call into [`htap_txn::TransactionManager::commit`] — the one
    /// step that cannot be retried (a second `commit` would allocate a second transaction id and
    /// commit version for the same logical write). A concurrent `DROP TABLE`/`ALTER` caught by
    /// catalog revalidation is a genuine, intentional abort (task 6b), not an infrastructure
    /// failure, so it does take the transaction out of session state.
    fn commit_locked(&mut self) -> Result<()> {
        if matches!(self.state, SessionState::CommitOutcomePending { .. }) {
            return Err(outcome_pending_error(&self.state));
        }

        let open_txn = match &mut self.state {
            SessionState::InTxn(open_txn) => open_txn,
            SessionState::Idle => return Ok(()),
            SessionState::CommitOutcomePending { .. } => {
                unreachable!("checked and rejected above")
            }
        };

        if let Some(reason) = open_txn.poisoned.clone() {
            self.state = SessionState::Idle;
            self.server.unregister_pinned_snapshot(self.id);
            return Err(HtapError::Conflict(reason));
        }

        if open_txn.read_only || open_txn.write_set.is_empty() {
            self.state = SessionState::Idle;
            self.server.unregister_pinned_snapshot(self.id);
            return Ok(());
        }

        // Commit-time catalog revalidation (task 6b): every partition a buffered write touched
        // must still belong to the same table. A `Catalog::load` I/O failure here is
        // infrastructure, not a decision: propagate it with the transaction left open (F7).
        let catalog = self
            .server
            .catalog
            .load()?
            .unwrap_or_else(CatalogSnapshot::empty);
        for (partition_id, table_id) in open_txn.write_set.touched_partitions() {
            let still_valid = catalog
                .partition(PartitionId::new(partition_id))
                .is_some_and(|p| p.table_id == table_id);
            if !still_valid {
                // A genuine conflict, not an infrastructure failure: the transaction is aborted.
                self.state = SessionState::Idle;
                self.server.unregister_pinned_snapshot(self.id);
                return Err(HtapError::Conflict(format!(
                    "table dropped or partition altered during transaction (partition {partition_id})"
                )));
            }
        }

        let mutations = open_txn.write_set.all_mutations();
        // `encode_payload`/`next_txn_id` failures are infrastructure (payload size was already
        // enforced incrementally by `WriteSet::try_merge`; `next_txn_id` only fails on `u64`
        // exhaustion): both propagate with the transaction still open (F7).
        let payload = RowstoreParticipant::encode_payload(&mutations)?;

        // Fix-pass item 3b: `WriteSet::try_merge`'s incremental check already rejects this on
        // almost every path, but reconfirm the real, final payload length here as the
        // authoritative check (mirroring `RowstoreParticipant::encode_payload`'s own re-check
        // above) — BEFORE `mem::replace` below removes the transaction from session state, so an
        // oversize commit leaves the transaction open (F7) instead of silently discarding its
        // write set. Fix-pass round 3, item 3(d): use this manager's own configured
        // `max_frame_size` rather than assuming `DEFAULT_MAX_FRAME_SIZE`.
        let max_frame_size = self.server.txn_manager.max_frame_size();
        let intent_bound = intent_frame_size_bound(payload.len(), 1);
        if intent_bound > max_frame_size {
            return Err(HtapError::InvalidArgument(format!(
                "mutation payload size {} would produce an estimated durable Intent frame of \
                 {intent_bound} bytes, exceeding the journal's maximum frame size of \
                 {max_frame_size} bytes",
                payload.len()
            )));
        }

        let work = ParticipantWork::new(ParticipantId::new(1), payload);
        let request = TransactionRequest::new(vec![work])?;
        let txn_id = self.server.txn_manager.next_txn_id()?;

        // Decision point: every pre-decision step above succeeded, so the open transaction is
        // now committed to this one attempt and removed from session state.
        let open_txn = match std::mem::replace(&mut self.state, SessionState::Idle) {
            SessionState::InTxn(open_txn) => open_txn,
            _ => unreachable!(
                "self.state was InTxn just above and execution_lock excludes concurrent access"
            ),
        };

        let mut txn = Transaction::new(txn_id, open_txn.snapshot.version);
        txn.set_request(request);
        match self.server.txn_manager.commit(&mut txn) {
            Ok(_) => {
                self.server.unregister_pinned_snapshot(self.id);
                Ok(())
            }
            Err(HtapError::DurablePending {
                txn_id,
                version,
                reason,
            }) => {
                self.server.unregister_pinned_snapshot(self.id);
                self.state = SessionState::CommitOutcomePending {
                    txn_id,
                    version,
                    reason: reason.clone(),
                };
                Err(HtapError::DurablePending {
                    txn_id,
                    version,
                    reason,
                })
            }
            // Fix-pass item 5: the manager rejected this commit before doing any work because an
            // earlier, unrelated transaction's outcome is still ambiguous (see
            // `htap_txn::TransactionManager`'s recovery latch); this transaction itself
            // definitely did not commit, so it stays open exactly as it was, and the session is
            // not quarantined into `CommitOutcomePending` for someone else's still-pending
            // recovery (storage-reviewer fix-pass finding: a latched manager previously returned
            // the latched transaction's own `DurablePending` here, which this session would then
            // wrongly adopt as its own outcome, losing its write set).
            Err(HtapError::RecoveryRequired {
                blocking_txn,
                reason,
            }) => {
                self.state = SessionState::InTxn(open_txn);
                Err(HtapError::RecoveryRequired {
                    blocking_txn,
                    reason,
                })
            }
            // `self.state` is already `Idle` (aborted) from the `mem::replace` above.
            Err(err) => {
                self.server.unregister_pinned_snapshot(self.id);
                Err(err)
            }
        }
    }

    /// Rolls back the open transaction, if any, discarding its buffered write set.
    ///
    /// Best-effort in the sense the plan describes: since buffered writes never touch storage
    /// before `COMMIT`, there is nothing external to undo; this simply clears session state. A
    /// poisoned transaction rolls back cleanly too (poisoning only blocks further statements and
    /// `COMMIT`, never `ROLLBACK`).
    ///
    /// # Errors
    ///
    /// Returns the original stored `DurablePending` error if the session's last commit left it in
    /// the `CommitOutcomePending` state (never `HtapError::Conflict`; see [`outcome_pending_error`]).
    pub fn rollback(&mut self) -> Result<()> {
        if matches!(self.state, SessionState::CommitOutcomePending { .. }) {
            return Err(outcome_pending_error(&self.state));
        }
        if matches!(self.state, SessionState::InTxn(_)) {
            self.server.unregister_pinned_snapshot(self.id);
        }
        self.state = SessionState::Idle;
        Ok(())
    }

    /// Resets this session to a freshly opened state (Phase 11 plan task 2; backs
    /// `COM_RESET_CONNECTION`).
    ///
    /// If the session is [`SessionState::CommitOutcomePending`], returns the stored
    /// `DurablePending` error and changes nothing at all — the session stays quarantined until
    /// server recovery resolves the ambiguity, exactly like every other statement rejected in
    /// that state ([`outcome_pending_error`]). Otherwise: rolls back any open transaction
    /// (discarding its buffered write set, same as [`Session::rollback`]), clears every `@name`
    /// user variable, sets `autocommit` back to its MySQL-compatible default of on, and clears
    /// any pending `SET TRANSACTION READ ONLY`/`READ WRITE` default for the next transaction.
    ///
    /// # Errors
    ///
    /// Returns the stored `DurablePending` error if the session is `CommitOutcomePending`;
    /// otherwise never fails.
    pub fn reset(&mut self) -> Result<()> {
        if matches!(self.state, SessionState::CommitOutcomePending { .. }) {
            return Err(outcome_pending_error(&self.state));
        }
        self.rollback()?;
        self.user_vars.clear();
        self.autocommit = true;
        self.next_txn_read_only = None;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if matches!(self.state, SessionState::InTxn(_)) {
            let _ = self.rollback();
        }
    }
}

/// Live view over one [`Session`]'s user variables and dynamic system-variable state, used as
/// the [`VariableLookup`]/[`SessionVarsView`] for every statement dispatched through it
/// (autocommit or inside a transaction) and for [`Session::eval_scalar_expr`].
struct SessionVariables<'a> {
    user_vars: &'a BTreeMap<String, Value>,
    autocommit: bool,
    read_only: bool,
    max_allowed_packet: u64,
}

impl VariableLookup for SessionVariables<'_> {
    fn lookup(&self, name: &str, is_system: bool) -> Result<Value> {
        if !is_system {
            return Ok(self.user_vars.get(name).cloned().unwrap_or(Value::Null));
        }
        system_variable_value(name, self)
    }
}

impl SessionVarsView for SessionVariables<'_> {
    fn autocommit(&self) -> bool {
        self.autocommit
    }

    fn transaction_read_only(&self) -> bool {
        self.read_only
    }

    fn max_allowed_packet(&self) -> u64 {
        self.max_allowed_packet
    }
}

/// [`VariableLookup`] used by [`LocalServer::execute`], which has no session: a user variable is
/// always `NULL` (there is nowhere to store one), and a system variable resolves against
/// process-default session state (`autocommit = 1`, not read-only), so `SELECT @@autocommit`
/// still works the same way a freshly opened session would report it.
pub(crate) struct DefaultVariables;

impl VariableLookup for DefaultVariables {
    fn lookup(&self, name: &str, is_system: bool) -> Result<Value> {
        if !is_system {
            return Ok(Value::Null);
        }
        system_variable_value(name, &DefaultVariables)
    }
}

impl SessionVarsView for DefaultVariables {
    fn autocommit(&self) -> bool {
        true
    }

    fn transaction_read_only(&self) -> bool {
        false
    }
}

/// One `SET <target> = <value>` target, classified from its raw parsed name.
enum SetTarget {
    /// `@name` (user variable).
    UserVar(String),
    /// A system variable: `name` (bare, scope from `SET [SESSION|GLOBAL]`), `@@name`
    /// (session-scoped), or `@@session.name`/`@@global.name`.
    SystemVar { scope: SetScope, name: String },
}

/// Classifies a `SET` assignment target from its raw scope modifier (`SET SESSION x = ...`) and
/// parsed `ObjectName` (`x`, `@x`, `@@x`, or `@@session.x`/`@@global.x`).
fn classify_object_name(scope: Option<ContextModifier>, name: &ObjectName) -> Result<SetTarget> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [one] if one.starts_with("@@") => {
            let rest = &one[2..];
            if rest.is_empty() {
                return Err(HtapError::InvalidArgument(
                    "empty system variable name".into(),
                ));
            }
            Ok(SetTarget::SystemVar {
                scope: scope_from_context_modifier(scope)?,
                name: rest.to_string(),
            })
        }
        [one] if one.starts_with('@') => {
            let rest = &one[1..];
            if rest.is_empty() {
                return Err(HtapError::InvalidArgument(
                    "empty user variable name".into(),
                ));
            }
            Ok(SetTarget::UserVar(rest.to_string()))
        }
        [scope_part, name_part] if scope_part.starts_with("@@") => {
            let scope_word = &scope_part[2..];
            let resolved_scope = match scope_word.to_ascii_lowercase().as_str() {
                "session" => SetScope::Session,
                "global" => SetScope::Global,
                other => {
                    return Err(HtapError::Unsupported(format!(
                        "unsupported system variable scope '{other}'"
                    )))
                }
            };
            Ok(SetTarget::SystemVar {
                scope: resolved_scope,
                name: name_part.clone(),
            })
        }
        [one] => Ok(SetTarget::SystemVar {
            scope: scope_from_context_modifier(scope)?,
            name: one.clone(),
        }),
        _ => Err(HtapError::Unsupported(format!(
            "unsupported SET target '{name}'"
        ))),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<String>> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
            ObjectNamePart::Function(_) => Err(HtapError::Unsupported(
                "function-valued SET target names are not supported".into(),
            )),
        })
        .collect()
}

fn scope_from_context_modifier(modifier: Option<ContextModifier>) -> Result<SetScope> {
    match modifier {
        None | Some(ContextModifier::Session) | Some(ContextModifier::Local) => {
            Ok(SetScope::Session)
        }
        Some(ContextModifier::Global) => Ok(SetScope::Global),
    }
}

fn invalid_bool_flag(field: &str, value: &Value) -> HtapError {
    HtapError::InvalidArgument(format!(
        "invalid value for '{field}': expected 0, 1, ON, OFF, TRUE, or FALSE, found {value}"
    ))
}

/// Parses a MySQL-style boolean session variable value (`0`/`1`, `ON`/`OFF`, `TRUE`/`FALSE`,
/// case-insensitive). Mirrors `htap_sql::variables::parse_autocommit_value` for variables other
/// than `autocommit` (which has its own dedicated, differently-worded parser in that crate).
fn parse_bool_flag(field: &str, value: &Value) -> Result<bool> {
    match value {
        Value::Int32(0) | Value::Int64(0) => Ok(false),
        Value::Int32(1) | Value::Int64(1) => Ok(true),
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_uppercase().as_str() {
            "0" | "OFF" | "FALSE" => Ok(false),
            "1" | "ON" | "TRUE" => Ok(true),
            _ => Err(invalid_bool_flag(field, value)),
        },
        other => Err(invalid_bool_flag(field, other)),
    }
}

impl LocalServer {
    /// Opens a new session bound to this server instance.
    ///
    /// Each session has a unique, monotonically increasing [`SessionId`] for the lifetime of the
    /// process. Autocommit defaults on, matching MySQL.
    pub fn open_session(self: &Arc<Self>) -> Session {
        let id = SessionId(self.next_session_id.fetch_add(1, Ordering::SeqCst));
        Session {
            id,
            server: Arc::clone(self),
            principal: Principal::Superuser,
            state: SessionState::Idle,
            user_vars: BTreeMap::new(),
            autocommit: true,
            next_txn_read_only: None,
            max_allowed_packet: DEFAULT_MAX_ALLOWED_PACKET,
        }
    }
}
