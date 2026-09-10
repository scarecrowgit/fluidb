//! Coordinator trait, membership, leadership management, and local durable implementation.
//!
//! Provides cluster node membership tracking, scoped leadership election with monotonically
//! increasing fencing tokens, fence validation, and atomic catalog compare-and-set updates
//! under coordinator state serialization.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use htap_catalog::store::CatalogStore;
use htap_catalog::{CatalogSnapshot, NodeId};
use htap_common::lock::ProcessLock;
use htap_common::{read_file_exact_bounded, FencingToken, HtapError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub mod placement;

pub use placement::{
    activate_placement_addition, activate_placement_plan, plan_placement, stage_placement_addition,
    PlacementAddition, PlacementPlan, TabletPlacement,
};

/// Coordinator persistent state file name.
pub const COORDINATOR_FILE_NAME: &str = "COORDINATOR";

/// Temporary file name used for atomic two-phase coordinator state publish.
pub const COORDINATOR_TMP_FILE_NAME: &str = "COORDINATOR.tmp";

/// Header magic bytes identifying coordinator state files ("HTAPCRD1").
pub const HEADER_MAGIC: &[u8; 8] = b"HTAPCRD1";

/// Supported coordinator binary envelope format version.
pub const FORMAT_VERSION: u16 = 1;

/// Fixed header length (8 magic + 2 version + 4 payload_len + 4 crc32c = 18 bytes).
pub const HEADER_LEN: usize = 18;

/// Maximum allowed coordinator state payload size (64 MiB) to guard against unbounded allocations.
pub const MAX_COORDINATOR_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Record representing leadership of a given scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leadership {
    /// Identifier of the leadership scope (e.g. tablet, partition, or subsystem name).
    pub scope: String,
    /// Node holding the leadership lease.
    pub holder: NodeId,
    /// Monotonically increasing fencing token issued when leadership was granted.
    pub token: FencingToken,
}

impl Leadership {
    /// Create a new [`Leadership`] instance.
    pub fn new(scope: impl Into<String>, holder: NodeId, token: FencingToken) -> Self {
        Self {
            scope: scope.into(),
            holder,
            token,
        }
    }

    /// Return the scope string of this leadership.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Return the node holding leadership.
    pub fn holder(&self) -> NodeId {
        self.holder
    }

    /// Return the fencing token.
    pub fn token(&self) -> FencingToken {
        self.token
    }
}

/// Synchronous, object-safe trait for cluster coordination, membership, and fencing.
pub trait Coordinator: Send + Sync {
    /// Register a cluster node into the membership set.
    ///
    /// Registration is idempotent; registering an already-present node succeeds.
    fn register_node(&self, node_id: NodeId) -> Result<()>;

    /// Remove a cluster node from the membership set.
    ///
    /// Removal is idempotent; removing a non-existent node succeeds.
    fn remove_node(&self, node_id: NodeId) -> Result<()>;

    /// List all registered cluster nodes, sorted in deterministic ascending order of [`NodeId`].
    fn list_nodes(&self) -> Result<Vec<NodeId>>;

    /// Acquire leadership for a scope.
    ///
    /// Returns [`HtapError::Conflict`] if the scope already has an active leader.
    fn acquire_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership>;

    /// Get current leadership for a scope, if held.
    fn current_leadership(&self, scope: &str) -> Result<Option<Leadership>>;

    /// Release leadership for a scope.
    ///
    /// If the scope currently has no active leader, this is a no-op and succeeds.
    fn release_leadership(&self, scope: &str) -> Result<()>;

    /// Replace leadership for a scope with a new holder, issuing a strictly increasing fencing token.
    ///
    /// If the scope was not previously held, this assigns leadership to `holder`.
    fn replace_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership>;

    /// Validate that `token` matches the current active fencing token for `scope`.
    ///
    /// Returns [`HtapError::Fenced`] if the token is stale or does not match the active leader.
    /// Returns [`HtapError::NotFound`] if the scope has never been acquired.
    fn validate_fence(&self, scope: &str, token: FencingToken) -> Result<()>;

    /// Atomically validate fencing token and execute catalog snapshot compare-and-set.
    ///
    /// While holding the coordinator state lock:
    /// 1. Validates that `token` matches the current active leader token for `scope`.
    /// 2. If valid, invokes `catalog.compare_and_set(expected_generation, next)`.
    /// 3. If stale, returns [`HtapError::Fenced`] and leaves `catalog` unchanged.
    fn fenced_catalog_compare_and_set(
        &self,
        scope: &str,
        token: FencingToken,
        catalog: &dyn CatalogStore,
        expected_generation: u64,
        next: CatalogSnapshot,
    ) -> Result<()>;

    /// Alias for [`Coordinator::register_node`].
    fn register_member(&self, node_id: NodeId) -> Result<()> {
        self.register_node(node_id)
    }

    /// Alias for [`Coordinator::remove_node`].
    fn remove_member(&self, node_id: NodeId) -> Result<()> {
        self.remove_node(node_id)
    }

    /// Alias for [`Coordinator::list_nodes`].
    fn list_members(&self) -> Result<Vec<NodeId>> {
        self.list_nodes()
    }

    /// Alias for [`Coordinator::acquire_leadership`].
    fn acquire(&self, scope: &str, holder: NodeId) -> Result<Leadership> {
        self.acquire_leadership(scope, holder)
    }

    /// Alias for [`Coordinator::current_leadership`].
    fn current(&self, scope: &str) -> Result<Option<Leadership>> {
        self.current_leadership(scope)
    }

    /// Alias for [`Coordinator::release_leadership`].
    fn release(&self, scope: &str) -> Result<()> {
        self.release_leadership(scope)
    }

    /// Alias for [`Coordinator::replace_leadership`].
    fn replace(&self, scope: &str, holder: NodeId) -> Result<Leadership> {
        self.replace_leadership(scope, holder)
    }
}

/// Durable coordinator state serialized to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorState {
    /// Set of registered cluster node IDs.
    pub members: BTreeSet<NodeId>,
    /// Currently active leadership by scope.
    pub leaders: BTreeMap<String, Leadership>,
    /// Highest fencing token ever issued for each scope.
    pub scope_tokens: BTreeMap<String, u64>,
    /// Monotonically increasing counter for the next fencing token.
    pub next_token: u64,
}

impl Default for CoordinatorState {
    fn default() -> Self {
        Self {
            members: BTreeSet::new(),
            leaders: BTreeMap::new(),
            scope_tokens: BTreeMap::new(),
            next_token: 1,
        }
    }
}

/// Local directory-backed coordinator implementing [`Coordinator`].
///
/// Persists membership, current leadership by scope, and monotonic fencing token allocator
/// state at `<root>/COORDINATOR` inside a versioned CRC32C binary envelope with atomic
/// write-fsync-rename-dirsync semantics.
///
/// # Concurrency & Cross-Process Limitations
/// All coordinator operations within a single process are serialized using an internal mutex.
/// Cross-process root directory access is strictly exclusive: `LocalCoordinator::open`
/// acquires an OS-level advisory lock at `<root>/LOCK`. Concurrent access by multiple processes
/// against the same root directory is rejected with [`HtapError::Conflict`].
#[derive(Debug)]
pub struct LocalCoordinator {
    root: PathBuf,
    _lock: ProcessLock,
    lock: Mutex<CoordinatorState>,
}

impl LocalCoordinator {
    /// Open or initialize a local coordinator at the specified directory path.
    ///
    /// Canonicalizes/creates `root` and acquires an exclusive non-blocking advisory
    /// lock at `<root>/LOCK` before opening or publishing coordinator state files.
    ///
    /// If the directory does not exist, it will be created.
    /// If a published `COORDINATOR` file exists, its integrity is verified on startup.
    /// If no `COORDINATOR` file exists, an initial state envelope is written.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Conflict`] if the root directory is already locked by another
    /// process. Returns other [`HtapError`] variants if state reading or publishing fails.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let canonical_root = root.canonicalize()?;
        let lock_guard = ProcessLock::acquire(&canonical_root)?;

        let coord_path = canonical_root.join(COORDINATOR_FILE_NAME);
        let max_bytes = HEADER_LEN + MAX_COORDINATOR_PAYLOAD_BYTES as usize;
        let state = match read_file_exact_bounded(&coord_path, max_bytes) {
            Ok(bytes) => decode_state(&bytes)?,
            Err(HtapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let initial = CoordinatorState::default();
                atomic_publish(&canonical_root, &initial)?;
                initial
            }
            Err(e) => return Err(e),
        };

        Ok(Self {
            root: canonical_root,
            _lock: lock_guard,
            lock: Mutex::new(state),
        })
    }

    /// Return the root directory path of this coordinator.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn validate_fence_locked(
        state: &CoordinatorState,
        scope: &str,
        token: FencingToken,
    ) -> Result<()> {
        if let Some(leadership) = state.leaders.get(scope) {
            if leadership.token == token {
                Ok(())
            } else {
                Err(HtapError::Fenced {
                    expected: leadership.token.get(),
                    got: token.get(),
                })
            }
        } else if let Some(&last_tok) = state.scope_tokens.get(scope) {
            Err(HtapError::Fenced {
                expected: last_tok + 1,
                got: token.get(),
            })
        } else {
            Err(HtapError::NotFound(format!(
                "leadership scope '{scope}' not found"
            )))
        }
    }
}

impl Coordinator for LocalCoordinator {
    fn register_node(&self, node_id: NodeId) -> Result<()> {
        let mut state = self.lock.lock();
        if state.members.contains(&node_id) {
            return Ok(());
        }
        let mut new_state = state.clone();
        new_state.members.insert(node_id);
        atomic_publish(&self.root, &new_state)?;
        *state = new_state;
        Ok(())
    }

    fn remove_node(&self, node_id: NodeId) -> Result<()> {
        let mut state = self.lock.lock();
        if !state.members.contains(&node_id) {
            return Ok(());
        }
        let mut new_state = state.clone();
        new_state.members.remove(&node_id);
        atomic_publish(&self.root, &new_state)?;
        *state = new_state;
        Ok(())
    }

    fn list_nodes(&self) -> Result<Vec<NodeId>> {
        let state = self.lock.lock();
        Ok(state.members.iter().copied().collect())
    }

    fn acquire_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership> {
        let mut state = self.lock.lock();
        if let Some(current) = state.leaders.get(scope) {
            return Err(HtapError::Conflict(format!(
                "scope '{scope}' is already held by node {}",
                current.holder
            )));
        }

        let mut new_state = state.clone();
        let token = FencingToken::new(new_state.next_token);
        new_state.next_token += 1;
        let leadership = Leadership::new(scope, holder, token);
        new_state
            .leaders
            .insert(scope.to_string(), leadership.clone());
        new_state
            .scope_tokens
            .insert(scope.to_string(), token.get());
        atomic_publish(&self.root, &new_state)?;
        *state = new_state;
        Ok(leadership)
    }

    fn current_leadership(&self, scope: &str) -> Result<Option<Leadership>> {
        let state = self.lock.lock();
        Ok(state.leaders.get(scope).cloned())
    }

    fn release_leadership(&self, scope: &str) -> Result<()> {
        let mut state = self.lock.lock();
        if !state.leaders.contains_key(scope) {
            return Ok(());
        }
        let mut new_state = state.clone();
        new_state.leaders.remove(scope);
        atomic_publish(&self.root, &new_state)?;
        *state = new_state;
        Ok(())
    }

    fn replace_leadership(&self, scope: &str, holder: NodeId) -> Result<Leadership> {
        let mut state = self.lock.lock();
        let mut new_state = state.clone();
        let token = FencingToken::new(new_state.next_token);
        new_state.next_token += 1;
        let leadership = Leadership::new(scope, holder, token);
        new_state
            .leaders
            .insert(scope.to_string(), leadership.clone());
        new_state
            .scope_tokens
            .insert(scope.to_string(), token.get());
        atomic_publish(&self.root, &new_state)?;
        *state = new_state;
        Ok(leadership)
    }

    fn validate_fence(&self, scope: &str, token: FencingToken) -> Result<()> {
        let state = self.lock.lock();
        Self::validate_fence_locked(&state, scope, token)
    }

    fn fenced_catalog_compare_and_set(
        &self,
        scope: &str,
        token: FencingToken,
        catalog: &dyn CatalogStore,
        expected_generation: u64,
        next: CatalogSnapshot,
    ) -> Result<()> {
        let _guard = self.lock.lock();
        Self::validate_fence_locked(&_guard, scope, token)?;
        catalog.compare_and_set(expected_generation, next)
    }
}

/// Encode coordinator state into a versioned binary envelope.
pub fn encode_state(state: &CoordinatorState) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(state)
        .map_err(|e| HtapError::Internal(format!("failed to serialize coordinator state: {e}")))?;

    if payload.len() > MAX_COORDINATOR_PAYLOAD_BYTES as usize {
        return Err(HtapError::InvalidArgument(format!(
            "coordinator payload size {} exceeds maximum allowed {}",
            payload.len(),
            MAX_COORDINATOR_PAYLOAD_BYTES
        )));
    }

    let payload_len = payload.len() as u32;
    let checksum = crc32c::crc32c(&payload);

    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(HEADER_MAGIC);
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&payload);

    Ok(buf)
}

/// Decode and validate coordinator state from a binary envelope.
pub fn decode_state(bytes: &[u8]) -> Result<CoordinatorState> {
    if bytes.len() < HEADER_LEN {
        return Err(HtapError::Corruption(format!(
            "coordinator file too small: {} bytes, minimum header size is {}",
            bytes.len(),
            HEADER_LEN
        )));
    }

    if &bytes[0..8] != HEADER_MAGIC {
        return Err(HtapError::Corruption(
            "invalid coordinator header magic".into(),
        ));
    }

    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(HtapError::Corruption(format!(
            "unsupported coordinator format version: {version}"
        )));
    }

    let payload_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
    let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

    if payload_len > MAX_COORDINATOR_PAYLOAD_BYTES {
        return Err(HtapError::Corruption(format!(
            "coordinator payload length {payload_len} exceeds maximum limit {MAX_COORDINATOR_PAYLOAD_BYTES}"
        )));
    }

    let expected_total = HEADER_LEN + payload_len as usize;
    if bytes.len() < expected_total {
        return Err(HtapError::Corruption(format!(
            "truncated coordinator file: expected {} bytes, found {}",
            expected_total,
            bytes.len()
        )));
    }

    if bytes.len() > expected_total {
        return Err(HtapError::Corruption(format!(
            "coordinator file has {} trailing leftover bytes",
            bytes.len() - expected_total
        )));
    }

    let payload = &bytes[HEADER_LEN..expected_total];
    let computed_crc = crc32c::crc32c(payload);
    if computed_crc != expected_crc {
        return Err(HtapError::Corruption(format!(
            "coordinator checksum mismatch: expected {expected_crc:#010x}, got {computed_crc:#010x}"
        )));
    }

    let mut state: CoordinatorState = serde_json::from_slice(payload).map_err(|e| {
        HtapError::Corruption(format!("failed to parse coordinator state JSON: {e}"))
    })?;

    let max_token = state
        .leaders
        .values()
        .map(|l| l.token.get())
        .chain(state.scope_tokens.values().copied())
        .max()
        .unwrap_or(0);

    if state.next_token <= max_token {
        state.next_token = max_token + 1;
    }
    if state.next_token == 0 {
        state.next_token = 1;
    }

    Ok(state)
}

/// Atomically publish coordinator state: write tmp -> fsync -> rename -> fsync directory.
fn atomic_publish(dir: &Path, state: &CoordinatorState) -> Result<()> {
    let tmp_path = dir.join(COORDINATOR_TMP_FILE_NAME);
    let final_path = dir.join(COORDINATOR_FILE_NAME);

    let encoded = encode_state(state)?;

    let write_res = (|| -> Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(HtapError::Io(e));
    }

    sync_dir(dir)?;

    Ok(())
}

/// Fsync a directory to ensure metadata operations like rename are durable.
pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = File::open(path)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
