//! Coordinator trait, membership, leadership management, and local durable implementation.
//!
//! Provides cluster node membership tracking, scoped leadership election with monotonically
//! increasing fencing tokens, fence validation, and atomic catalog compare-and-set updates
//! under coordinator state serialization.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use htap_catalog::store::CatalogStore;
use htap_catalog::{CatalogSnapshot, NodeId};
use htap_common::envelope::{decode_envelope, encode_envelope, EnvelopeError, SizeCheckMode};
use htap_common::fs::{atomic_publish as publish_file, sync_dir as sync_directory};
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
            let expected = last_tok.checked_add(1).ok_or(HtapError::CounterOverflow {
                counter: "fencing_token",
            })?;
            Err(HtapError::Fenced {
                expected,
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
        let next_token = new_state
            .next_token
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow {
                counter: "fencing_token",
            })?;
        new_state.next_token = next_token;
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
        let next_token = new_state
            .next_token
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow {
                counter: "fencing_token",
            })?;
        new_state.next_token = next_token;
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

    Ok(encode_envelope(HEADER_MAGIC, FORMAT_VERSION, &payload))
}

/// Decode and validate coordinator state from a binary envelope.
pub fn decode_state(bytes: &[u8]) -> Result<CoordinatorState> {
    let (_, payload) = decode_envelope(
        bytes,
        HEADER_MAGIC,
        FORMAT_VERSION..=FORMAT_VERSION,
        MAX_COORDINATOR_PAYLOAD_BYTES,
        SizeCheckMode::TruncatedThenTrailing,
    )
    .map_err(|err| {
        let message = match err {
            EnvelopeError::TooSmall { found, min } => {
                format!("coordinator file too small: {found} bytes, minimum header size is {min}")
            }
            EnvelopeError::BadMagic => "invalid coordinator header magic".into(),
            EnvelopeError::UnsupportedVersion(version) => {
                format!("unsupported coordinator format version: {version}")
            }
            EnvelopeError::PayloadTooLarge { len, max } => {
                format!("coordinator payload length {len} exceeds maximum limit {max}")
            }
            EnvelopeError::Truncated { expected, found } => {
                format!("truncated coordinator file: expected {expected} bytes, found {found}")
            }
            EnvelopeError::TrailingBytes { extra } => {
                format!("coordinator file has {extra} trailing leftover bytes")
            }
            EnvelopeError::SizeMismatch { expected, found } => {
                format!("truncated coordinator file: expected {expected} bytes, found {found}")
            }
            EnvelopeError::ChecksumMismatch { expected, actual } => {
                format!(
                    "coordinator checksum mismatch: expected {expected:#010x}, got {actual:#010x}"
                )
            }
        };
        HtapError::Corruption(message)
    })?;

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
        state.next_token = max_token.checked_add(1).ok_or(HtapError::CounterOverflow {
            counter: "fencing_token",
        })?;
    }
    if state.next_token == 0 {
        state.next_token = 1;
    }

    Ok(state)
}

/// Atomically publish coordinator state: write tmp -> fsync -> rename -> fsync directory.
fn atomic_publish(dir: &Path, state: &CoordinatorState) -> Result<()> {
    let encoded = encode_state(state)?;
    publish_file(
        dir,
        COORDINATOR_TMP_FILE_NAME,
        COORDINATOR_FILE_NAME,
        &encoded,
        None,
        true,
    )
}

/// Fsync a directory to ensure metadata operations like rename are durable.
pub fn sync_dir(path: &Path) -> Result<()> {
    sync_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_state_golden_bytes() {
        let state = CoordinatorState::default();
        let bytes = encode_state(&state).unwrap();
        assert_eq!(
            bytes,
            &[
                72, 84, 65, 80, 67, 82, 68, 49, 1, 0, 60, 0, 0, 0, 92, 233, 97, 148, 123, 34, 109,
                101, 109, 98, 101, 114, 115, 34, 58, 91, 93, 44, 34, 108, 101, 97, 100, 101, 114,
                115, 34, 58, 123, 125, 44, 34, 115, 99, 111, 112, 101, 95, 116, 111, 107, 101, 110,
                115, 34, 58, 123, 125, 44, 34, 110, 101, 120, 116, 95, 116, 111, 107, 101, 110, 34,
                58, 49, 125
            ]
        );
    }

    #[test]
    fn test_coordinator_state_bad_magic_and_oversized() {
        let mut bad_magic = encode_state(&CoordinatorState::default()).unwrap();
        bad_magic[0] = b'X';
        let bad_magic_err = decode_state(&bad_magic).unwrap_err();

        let mut oversized = Vec::new();
        oversized.extend_from_slice(HEADER_MAGIC);
        oversized.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        oversized.extend_from_slice(&(MAX_COORDINATOR_PAYLOAD_BYTES + 1).to_le_bytes());
        oversized.extend_from_slice(&0_u32.to_le_bytes());
        let oversized_err = decode_state(&oversized).unwrap_err();

        assert_eq!(
            (bad_magic_err.to_string(), oversized_err.to_string()),
            (
                "Corruption error: invalid coordinator header magic".to_string(),
                "Corruption error: coordinator payload length 67108865 exceeds maximum limit 67108864"
                    .to_string(),
            )
        );
    }

    #[test]
    fn test_coordinator_state_bad_version_and_crc() {
        let mut bad_version = encode_state(&CoordinatorState::default()).unwrap();
        bad_version[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let bad_version_err = decode_state(&bad_version).unwrap_err();

        let mut bad_crc = encode_state(&CoordinatorState::default()).unwrap();
        bad_crc[14..18].copy_from_slice(&0_u32.to_le_bytes());
        let bad_crc_err = decode_state(&bad_crc).unwrap_err();

        assert_eq!(
            (bad_version_err.to_string(), bad_crc_err.to_string()),
            (
                "Corruption error: unsupported coordinator format version: 2".to_string(),
                "Corruption error: coordinator checksum mismatch: expected 0x00000000, got 0x9461e95c"
                    .to_string(),
            )
        );
    }

    #[test]
    fn test_coordinator_state_size_check_truncated() {
        let mut bytes = encode_state(&CoordinatorState::default()).unwrap();
        bytes.pop();
        let err = decode_state(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: truncated coordinator file: expected 78 bytes, found 77"
        );
    }

    #[test]
    fn test_coordinator_state_size_check_trailing() {
        let mut bytes = encode_state(&CoordinatorState::default()).unwrap();
        bytes.push(0);
        let err = decode_state(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: coordinator file has 1 trailing leftover bytes"
        );
    }
}
