//! In-memory write buffer (memtable) with multi-version concurrency control (MVCC).
//!
//! The memtable is the in-memory write buffer of the LSM row store. Mutations
//! (both inserts/updates and deletion tombstones) are appended here after being
//! made durable in the write-ahead log ([`crate::wal::Wal`]).
//!
//! # Storage and Concurrency
//!
//! Storage is backed by [`BTreeMap`] rather than a concurrent skip-list. Phase 1
//! prioritizes deterministic sorted iteration and standard-library safety;
//! engine-level locking provides the concurrency boundary between readers and writers.
//!
//! # MVCC Key Ordering
//!
//! Entries are indexed by [`InternalKey`], which orders by:
//! 1. `partition_id` ascending
//! 2. `user_key` ascending (bytewise)
//! 3. `version` **descending** (newest first)
//!
//! Ordering versions descending makes all versions of one key contiguous with the
//! newest first, so a snapshot lookup is a single [`BTreeMap::range`] seek rather
//! than a scan.
//!
//! # Visibility and Tombstones
//!
//! Lookups at a given snapshot [`Version`] seek to the newest version `<= snapshot`.
//! Deletions are stored as tombstones ([`ValueKind::Delete`]). When a tombstone is
//! the visible version for a key, [`Memtable::get`] returns `Some(MemtableEntry { value: ValueKind::Delete, .. })`,
//! **never** `None`. Callers must distinguish "deleted here" from "not present here"
//! so that older SST layers do not resurrect deleted records.

use std::cmp::Ordering;
use std::collections::btree_map::Values;
use std::collections::BTreeMap;
use std::mem::size_of;

use htap_common::{HtapError, Result, Row, Value, Version};

/// The kind of operation represented by a memtable value.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    /// An inserted or updated row.
    Put(Row),
    /// A tombstone representing deletion of the key.
    Delete,
}

/// Internal MVCC key combining a partition ID, user key bytes, and commit version.
///
/// # Ordering
///
/// Keys are ordered by:
/// 1. `partition_id` ascending
/// 2. `user_key` ascending bytewise
/// 3. `version` descending (newest first)
///
/// This ordering guarantees that all versions of a given key are stored contiguously,
/// with the most recent version first. Consequently, point lookups for an MVCC snapshot
/// can seek directly to the latest visible version with a single range seek.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InternalKey {
    /// Partition identifier.
    pub partition_id: u64,
    /// Raw byte representation of the user key.
    pub user_key: Vec<u8>,
    /// MVCC commit version.
    pub version: Version,
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partition_id
            .cmp(&other.partition_id)
            .then_with(|| self.user_key.cmp(&other.user_key))
            .then_with(|| other.version.cmp(&self.version))
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An entry stored in the memtable, pairing an MVCC internal key with its value.
#[derive(Debug, Clone, PartialEq)]
pub struct MemtableEntry {
    /// The internal MVCC key.
    pub key: InternalKey,
    /// The row value or deletion tombstone.
    pub value: ValueKind,
}

/// In-memory write buffer and MVCC read structure for an LSM row store.
///
/// Backed by a [`BTreeMap`] for deterministic iteration and standard-library safety.
/// Concurrency is managed externally at the engine layer.
#[derive(Debug)]
pub struct Memtable {
    /// In-memory sorted storage mapping internal keys to memtable entries.
    ///
    /// Storing [`MemtableEntry`] duplicates the [`InternalKey`] inside the value,
    /// but allows [`Memtable::iter`] to yield borrowed `&MemtableEntry` references
    /// without allocations or synthesizing references on the fly.
    entries: BTreeMap<InternalKey, MemtableEntry>,
    /// Running estimate of memory usage in bytes.
    approximate_size_bytes: usize,
    /// Highest commit version applied to this memtable.
    max_version: Option<Version>,
}

impl Default for Memtable {
    fn default() -> Self {
        Self::new()
    }
}

impl Memtable {
    /// Create a new, empty memtable.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            approximate_size_bytes: 0,
            max_version: None,
        }
    }

    /// Apply an MVCC record to the memtable.
    ///
    /// # Idempotency and Conflicts
    ///
    /// Applying the exact same `(partition_id, user_key, version)` with an identical
    /// value is idempotent: it succeeds without changing anything or incrementing the
    /// approximate size (this occurs during WAL replay of records already flushed or seen).
    ///
    /// Applying the same `(partition_id, user_key, version)` with a different value
    /// returns [`HtapError::Corruption`].
    pub fn apply(
        &mut self,
        partition_id: u64,
        user_key: Vec<u8>,
        version: Version,
        value: ValueKind,
    ) -> Result<()> {
        let key = InternalKey {
            partition_id,
            user_key,
            version,
        };

        if let Some(existing) = self.entries.get(&key) {
            if existing.value == value {
                return Ok(());
            }
            return Err(HtapError::Corruption(format!(
                "conflicting value for partition {}, user_key {:?}, version {}",
                key.partition_id, key.user_key, key.version
            )));
        }

        let entry_size = estimate_entry_size(key.user_key.len(), &value)?;
        self.approximate_size_bytes += entry_size;
        self.max_version = Some(self.max_version.map_or(version, |mv| mv.max(version)));
        self.entries
            .insert(key.clone(), MemtableEntry { key, value });

        Ok(())
    }

    /// Retrieve the visible entry for `(partition_id, user_key)` at `snapshot`.
    ///
    /// # Visibility Contract (Load-bearing)
    ///
    /// Returns the entry with the highest `version <= snapshot`.
    ///
    /// **Tombstones are returned**: if the latest version visible at `snapshot` is a
    /// deletion tombstone ([`ValueKind::Delete`]), this method returns
    /// `Some(MemtableEntry { value: ValueKind::Delete, .. })`, NOT `None`.
    /// The caller must distinguish "deleted at this version" from "not present in this memtable",
    /// otherwise queries over layered SSTs would resurrect previously deleted rows.
    ///
    /// Returns `None` only if no version of `(partition_id, user_key)` exists with `version <= snapshot`.
    pub fn get(
        &self,
        partition_id: u64,
        user_key: &[u8],
        snapshot: Version,
    ) -> Option<MemtableEntry> {
        let seek_key = InternalKey {
            partition_id,
            user_key: user_key.to_vec(),
            version: Version::new(u64::MAX),
        };

        for (key, entry) in self.entries.range(seek_key..) {
            if key.partition_id != partition_id || key.user_key.as_slice() != user_key {
                break;
            }
            if key.version <= snapshot {
                return Some(entry.clone());
            }
        }

        None
    }

    /// Iterator over all memtable entries in sorted order.
    ///
    /// Entries are yielded in ascending order of `partition_id`, ascending bytewise
    /// order of `user_key`, and descending order of `version`, including tombstones.
    pub fn iter(&self) -> impl Iterator<Item = &MemtableEntry> {
        self.entries.values()
    }

    /// Approximate memory consumption in bytes.
    ///
    /// This is maintained as a running counter updated during [`Memtable::apply`]
    /// using a structural estimate rather than exact heap accounting or serialization.
    /// It serves as an approximate flush threshold indicator.
    pub fn approximate_size_bytes(&self) -> usize {
        self.approximate_size_bytes
    }

    /// Return `true` if the memtable contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The highest MVCC version applied to this memtable, or `None` if empty.
    pub fn max_version(&self) -> Option<Version> {
        self.max_version
    }
}

impl<'a> IntoIterator for &'a Memtable {
    type Item = &'a MemtableEntry;
    type IntoIter = Values<'a, InternalKey, MemtableEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.values()
    }
}

/// Compute a cheap structural size estimate for a memtable entry without serialization.
fn estimate_entry_size(user_key_len: usize, value: &ValueKind) -> Result<usize> {
    let mut size = size_of::<InternalKey>() + user_key_len + size_of::<ValueKind>();
    if let ValueKind::Put(row) = value {
        for v in row.values() {
            match v {
                Value::String(s) => size += s.len(),
                Value::Bytes(b) => size += b.len(),
                Value::Null
                | Value::Bool(_)
                | Value::Int32(_)
                | Value::Int64(_)
                | Value::Float64(_)
                | Value::Timestamp(_)
                | Value::Decimal { .. } => {}
            }
        }
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_row(name: &str, id: i64) -> Row {
        Row::new(vec![Value::Int64(id), Value::String(name.to_string())])
    }

    #[test]
    fn test_internal_key_ordering() {
        let k1 = InternalKey {
            partition_id: 0,
            user_key: b"a".to_vec(),
            version: Version::new(2),
        };
        let k2 = InternalKey {
            partition_id: 0,
            user_key: b"a".to_vec(),
            version: Version::new(5),
        };
        let k3 = InternalKey {
            partition_id: 0,
            user_key: b"b".to_vec(),
            version: Version::new(1),
        };
        let k4 = InternalKey {
            partition_id: 1,
            user_key: b"a".to_vec(),
            version: Version::new(1),
        };

        // Same partition and user_key: higher version comes FIRST (version DESC).
        assert_eq!(k2.cmp(&k1), Ordering::Less);
        assert!(k2 < k1);

        // Same partition, different user_key: bytewise ASC.
        assert_eq!(k1.cmp(&k3), Ordering::Less);
        assert!(k1 < k3);

        // Different partition: partition_id ASC.
        assert_eq!(k3.cmp(&k4), Ordering::Less);
        assert!(k3 < k4);

        // Sort a list of keys and check order.
        let mut keys = vec![k1.clone(), k2.clone(), k3.clone(), k4.clone()];
        keys.sort();
        assert_eq!(keys, vec![k2, k1, k3, k4]);
    }

    #[test]
    fn test_canonical_visibility_and_tombstones() {
        let mut memtable = Memtable::new();
        let p = 0;
        let key = b"user1".to_vec();

        let row_a = test_row("alice", 1);
        let row_b = test_row("bob", 2);

        // v2: Put(A)
        memtable
            .apply(
                p,
                key.clone(),
                Version::new(2),
                ValueKind::Put(row_a.clone()),
            )
            .unwrap();
        // v4: Delete
        memtable
            .apply(p, key.clone(), Version::new(4), ValueKind::Delete)
            .unwrap();
        // v5: Put(B)
        memtable
            .apply(
                p,
                key.clone(),
                Version::new(5),
                ValueKind::Put(row_b.clone()),
            )
            .unwrap();

        // snapshot v1 -> None
        assert_eq!(memtable.get(p, &key, Version::new(1)), None);

        // snapshot v2 -> Put(A)
        let entry_v2 = memtable.get(p, &key, Version::new(2)).unwrap();
        assert_eq!(entry_v2.value, ValueKind::Put(row_a.clone()));
        assert_eq!(entry_v2.key.version, Version::new(2));

        // snapshot v3 -> Put(A)
        let entry_v3 = memtable.get(p, &key, Version::new(3)).unwrap();
        assert_eq!(entry_v3.value, ValueKind::Put(row_a));
        assert_eq!(entry_v3.key.version, Version::new(2));

        // snapshot v4 -> Delete (tombstone returned, never None!)
        let entry_v4 = memtable.get(p, &key, Version::new(4)).unwrap();
        assert_eq!(entry_v4.value, ValueKind::Delete);
        assert_eq!(entry_v4.key.version, Version::new(4));

        // snapshot v5 -> Put(B)
        let entry_v5 = memtable.get(p, &key, Version::new(5)).unwrap();
        assert_eq!(entry_v5.value, ValueKind::Put(row_b.clone()));
        assert_eq!(entry_v5.key.version, Version::new(5));

        // snapshot v10 -> Put(B)
        let entry_v10 = memtable.get(p, &key, Version::new(10)).unwrap();
        assert_eq!(entry_v10.value, ValueKind::Put(row_b));
        assert_eq!(entry_v10.key.version, Version::new(5));
    }

    #[test]
    fn test_exact_commit_version_visible() {
        let mut memtable = Memtable::new();
        let row = test_row("val", 100);
        memtable
            .apply(
                0,
                b"k".to_vec(),
                Version::new(5),
                ValueKind::Put(row.clone()),
            )
            .unwrap();

        // Exactly version 5 is visible
        let entry = memtable.get(0, b"k", Version::new(5)).unwrap();
        assert_eq!(entry.value, ValueKind::Put(row));
    }

    #[test]
    fn test_idempotent_duplicate_apply() {
        let mut memtable = Memtable::new();
        let row = test_row("val", 42);

        memtable
            .apply(
                0,
                b"k1".to_vec(),
                Version::new(2),
                ValueKind::Put(row.clone()),
            )
            .unwrap();
        let size_after_first = memtable.approximate_size_bytes();

        // Re-applying exact same key, version, and value must succeed and not change size
        memtable
            .apply(0, b"k1".to_vec(), Version::new(2), ValueKind::Put(row))
            .unwrap();
        assert_eq!(memtable.approximate_size_bytes(), size_after_first);
        assert_eq!(memtable.iter().count(), 1);
    }

    #[test]
    fn test_conflicting_duplicate_apply_errors() {
        let mut memtable = Memtable::new();
        let row1 = test_row("val1", 1);
        let row2 = test_row("val2", 2);

        memtable
            .apply(0, b"k".to_vec(), Version::new(2), ValueKind::Put(row1))
            .unwrap();

        // Same (partition, key, version) but different value
        let err = memtable
            .apply(0, b"k".to_vec(), Version::new(2), ValueKind::Put(row2))
            .unwrap_err();
        assert!(matches!(err, HtapError::Corruption(_)));

        // Same (partition, key, version) with Delete vs Put
        let err_del = memtable
            .apply(0, b"k".to_vec(), Version::new(2), ValueKind::Delete)
            .unwrap_err();
        assert!(matches!(err_del, HtapError::Corruption(_)));
    }

    #[test]
    fn test_prefix_isolation() {
        let mut memtable = Memtable::new();
        let row_a = test_row("a", 1);
        let row_ab = test_row("ab", 2);

        memtable
            .apply(
                0,
                b"a".to_vec(),
                Version::new(2),
                ValueKind::Put(row_a.clone()),
            )
            .unwrap();
        memtable
            .apply(
                0,
                b"ab".to_vec(),
                Version::new(2),
                ValueKind::Put(row_ab.clone()),
            )
            .unwrap();

        // b"a" should find row_a and not row_ab
        let res_a = memtable.get(0, b"a", Version::new(2)).unwrap();
        assert_eq!(res_a.value, ValueKind::Put(row_a));

        // b"ab" should find row_ab
        let res_ab = memtable.get(0, b"ab", Version::new(2)).unwrap();
        assert_eq!(res_ab.value, ValueKind::Put(row_ab));

        // b"b" does not exist
        assert_eq!(memtable.get(0, b"b", Version::new(2)), None);

        // b"aa" does not exist
        assert_eq!(memtable.get(0, b"aa", Version::new(2)), None);
    }

    #[test]
    fn test_metrics_monotonic_and_max_version() {
        let mut memtable = Memtable::new();
        assert!(memtable.is_empty());
        assert_eq!(memtable.max_version(), None);
        assert_eq!(memtable.approximate_size_bytes(), 0);

        memtable
            .apply(0, b"k1".to_vec(), Version::new(2), ValueKind::Delete)
            .unwrap();
        assert!(!memtable.is_empty());
        assert_eq!(memtable.max_version(), Some(Version::new(2)));
        let size1 = memtable.approximate_size_bytes();
        assert!(size1 > 0);

        // Insert version 10
        let row = test_row("string_data", 100);
        memtable
            .apply(0, b"k2".to_vec(), Version::new(10), ValueKind::Put(row))
            .unwrap();
        assert_eq!(memtable.max_version(), Some(Version::new(10)));
        let size2 = memtable.approximate_size_bytes();
        assert!(size2 > size1);

        // Insert version 5 (out-of-order version applied, max_version should stay 10)
        memtable
            .apply(0, b"k3".to_vec(), Version::new(5), ValueKind::Delete)
            .unwrap();
        assert_eq!(memtable.max_version(), Some(Version::new(10)));
        let size3 = memtable.approximate_size_bytes();
        assert!(size3 > size2);
    }

    #[test]
    fn test_iter_order() {
        let mut memtable = Memtable::new();
        memtable
            .apply(1, b"k1".to_vec(), Version::new(1), ValueKind::Delete)
            .unwrap();
        memtable
            .apply(0, b"k2".to_vec(), Version::new(2), ValueKind::Delete)
            .unwrap();
        memtable
            .apply(0, b"k1".to_vec(), Version::new(1), ValueKind::Delete)
            .unwrap();
        memtable
            .apply(0, b"k1".to_vec(), Version::new(3), ValueKind::Delete)
            .unwrap();

        let entries: Vec<&MemtableEntry> = memtable.iter().collect();
        assert_eq!(entries.len(), 4);

        // Expect:
        // (partition 0, k1, v3)
        // (partition 0, k1, v1)
        // (partition 0, k2, v2)
        // (partition 1, k1, v1)
        assert_eq!(entries[0].key.partition_id, 0);
        assert_eq!(entries[0].key.user_key, b"k1");
        assert_eq!(entries[0].key.version, Version::new(3));

        assert_eq!(entries[1].key.partition_id, 0);
        assert_eq!(entries[1].key.user_key, b"k1");
        assert_eq!(entries[1].key.version, Version::new(1));

        assert_eq!(entries[2].key.partition_id, 0);
        assert_eq!(entries[2].key.user_key, b"k2");
        assert_eq!(entries[2].key.version, Version::new(2));

        assert_eq!(entries[3].key.partition_id, 1);
        assert_eq!(entries[3].key.user_key, b"k1");
        assert_eq!(entries[3].key.version, Version::new(1));
    }

    #[test]
    fn test_decimal_value_is_persistable() {
        let mut memtable = Memtable::new();
        let decimal = Value::Decimal {
            value: 123,
            precision: 3,
            scale: 2,
        };
        let row = Row::new(vec![decimal.clone()]);

        memtable
            .apply(0, b"decimal".to_vec(), Version::new(1), ValueKind::Put(row))
            .unwrap();

        let entry = memtable
            .get(0, b"decimal", Version::new(1))
            .expect("decimal row should be visible");
        let ValueKind::Put(row) = entry.value else {
            panic!("expected decimal row");
        };
        let Value::Decimal {
            value,
            precision,
            scale,
        } = row.get(0).expect("decimal column should exist")
        else {
            panic!("expected decimal value");
        };

        assert_eq!(*value, 123);
        assert_eq!(*precision, 3);
        assert_eq!(*scale, 2);
        assert_eq!(
            memtable.approximate_size_bytes(),
            size_of::<InternalKey>() + b"decimal".len() + size_of::<ValueKind>()
        );
        assert_eq!(memtable.max_version(), Some(Version::new(1)));
    }
}
