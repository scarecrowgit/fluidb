#![forbid(unsafe_code)]

//! In-memory validation state for serializable snapshot transactions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use htap_common::{HtapError, Version};
use parking_lot::Mutex;

/// An encoded primary key written by a transaction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WriteKey {
    /// Partition containing the key.
    pub partition_id: u64,
    /// Encoded primary key.
    pub key: Vec<u8>,
}

impl WriteKey {
    /// Creates a write key in `partition_id`.
    pub fn new(partition_id: u64, key: impl Into<Vec<u8>>) -> Self {
        Self {
            partition_id,
            key: key.into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PartitionFootprint {
    points: BTreeSet<Vec<u8>>,
    ranges: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    partition: bool,
}

/// Reads performed by a transaction, grouped by partition.
#[derive(Clone, Debug)]
pub struct ReadFootprint {
    partitions: BTreeMap<u64, PartitionFootprint>,
    per_partition_cap: usize,
}

impl Default for ReadFootprint {
    fn default() -> Self {
        Self {
            partitions: BTreeMap::new(),
            per_partition_cap: 100,
        }
    }
}

impl ReadFootprint {
    /// Returns a footprint that promotes partitions after `cap` point and range reads.
    pub fn with_per_partition_cap(cap: usize) -> Self {
        Self {
            partitions: BTreeMap::new(),
            per_partition_cap: cap,
        }
    }

    /// Records a point read, including a read of an absent key.
    pub fn record_point(&mut self, partition_id: u64, key: Vec<u8>) {
        let entry = self.partitions.entry(partition_id).or_default();
        if entry.partition {
            return;
        }
        entry.points.insert(key);
        self.promote_if_needed(partition_id);
    }

    /// Records a half-open range read `[start, end)`, with `None` as an unbounded end.
    pub fn record_range(&mut self, partition_id: u64, start: Vec<u8>, end: Option<Vec<u8>>) {
        let entry = self.partitions.entry(partition_id).or_default();
        if entry.partition {
            return;
        }
        entry.ranges.push((start, end));
        self.promote_if_needed(partition_id);
    }

    /// Records a whole-partition read.
    pub fn record_partition(&mut self, partition_id: u64) {
        let entry = self.partitions.entry(partition_id).or_default();
        entry.partition = true;
        entry.points.clear();
        entry.ranges.clear();
    }

    /// Merges all reads from `other` into this footprint.
    pub fn merge(&mut self, other: &ReadFootprint) {
        self.per_partition_cap = self.per_partition_cap.min(other.per_partition_cap);
        for (&partition_id, other_entry) in &other.partitions {
            if other_entry.partition {
                self.record_partition(partition_id);
                continue;
            }

            for key in &other_entry.points {
                self.record_point(partition_id, key.clone());
            }
            for (start, end) in &other_entry.ranges {
                self.record_range(partition_id, start.clone(), end.clone());
            }
            self.promote_if_needed(partition_id);
        }
    }

    /// Returns whether no reads have been recorded.
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// Summarizes point-key counts and whole-partition reads by partition.
    pub fn summarize(&self) -> (BTreeMap<u64, usize>, BTreeSet<u64>) {
        let mut point_counts = BTreeMap::new();
        let mut whole_partitions = BTreeSet::new();

        for (&partition_id, footprint) in &self.partitions {
            if footprint.partition {
                whole_partitions.insert(partition_id);
            } else {
                point_counts.insert(partition_id, footprint.points.len());
            }
        }

        (point_counts, whole_partitions)
    }

    fn promote_if_needed(&mut self, partition_id: u64) {
        let entry = self
            .partitions
            .get_mut(&partition_id)
            .expect("partition was inserted before promotion");
        if entry.points.len() + entry.ranges.len() >= self.per_partition_cap {
            entry.partition = true;
            entry.points.clear();
            entry.ranges.clear();
        }
    }
}

/// Classification of a serializable validation failure.
#[derive(Debug)]
pub(crate) enum ValidationFailure {
    /// Retained write history is incomplete for this snapshot.
    Floor(HtapError),
    /// A newer committed write overlaps the transaction's read footprint.
    Dependency(HtapError),
}

/// An in-memory index of recently committed point writes.
#[derive(Clone, Debug)]
pub(crate) struct RecentWrites {
    point_writes: BTreeMap<(u64, Vec<u8>), Version>,
    partition_max_versions: BTreeMap<u64, Version>,
    partition_floors: BTreeMap<u64, Version>,
    global_floor: Version,
    version_log: BTreeMap<Version, Vec<(u64, Vec<u8>)>>,
    version_log_entries: usize,
    max_entries: usize,
}

impl RecentWrites {
    /// Creates an empty write index with a 10,000-entry retention bound.
    pub fn new() -> Self {
        Self {
            point_writes: BTreeMap::new(),
            partition_max_versions: BTreeMap::new(),
            partition_floors: BTreeMap::new(),
            global_floor: Version::INITIAL,
            version_log: BTreeMap::new(),
            version_log_entries: 0,
            max_entries: 10_000,
        }
    }

    /// Records writes committed together at `at_version`.
    pub fn record_batch(&mut self, writes: &[WriteKey], at_version: Version) {
        for write in writes {
            let partition_id = write.partition_id;
            let map_key = (partition_id, write.key.clone());
            let prior = self.point_writes.get(&map_key).copied();

            if prior.is_none_or(|version| at_version > version) {
                self.point_writes.insert(map_key.clone(), at_version);
                self.version_log
                    .entry(at_version)
                    .or_default()
                    .push(map_key);
                self.version_log_entries += 1;
                self.partition_max_versions
                    .entry(partition_id)
                    .and_modify(|version| *version = (*version).max(at_version))
                    .or_insert(at_version);
            }
        }

        // Retain the oldest versions when point-key capacity is exceeded. The floor covers
        // every evicted key in that partition for snapshots older than the eviction.
        while self.point_writes.len() > self.max_entries {
            let Some((map_key, version)) = self
                .point_writes
                .iter()
                .min_by_key(|(key, version)| (**version, *key))
                .map(|(key, version)| (key.clone(), *version))
            else {
                break;
            };

            self.partition_floors
                .entry(map_key.0)
                .and_modify(|floor| *floor = (*floor).max(version))
                .or_insert(version);
            self.point_writes.remove(&map_key);
            self.refresh_partition_max(map_key.0);
        }

        // The version log is historical and can grow even when repeated writes replace one
        // retained point key. Once its budget is exhausted, retain no partial history.
        if self.version_log_entries > self.max_entries {
            self.global_floor = self.global_floor.max(at_version);
            self.point_writes.clear();
            self.partition_max_versions.clear();
            self.partition_floors.clear();
            self.version_log.clear();
            self.version_log_entries = 0;
        }
    }

    /// Returns the latest committed write version for a point key.
    pub fn point_lookup(&self, partition_id: u64, key: &[u8]) -> Option<Version> {
        self.point_writes
            .get(&(partition_id, key.to_vec()))
            .copied()
    }

    /// Returns writes in the half-open range `[start, end)`, ordered by key.
    pub fn range_lookup(
        &self,
        partition_id: u64,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Vec<(Vec<u8>, Version)> {
        self.point_writes
            .range((partition_id, start.to_vec())..)
            .take_while(|((partition, key), _)| {
                *partition == partition_id && end.is_none_or(|end| key.as_slice() < end)
            })
            .map(|((_, key), version)| (key.clone(), *version))
            .collect()
    }

    /// Removes retained writes at or before `horizon`.
    pub fn prune(&mut self, horizon: Version) {
        let versions: Vec<Version> = self
            .version_log
            .range(..=horizon)
            .map(|(version, _)| *version)
            .collect();
        let mut partitions_needing_refresh = BTreeSet::new();

        for version in versions {
            if let Some(entries) = self.version_log.remove(&version) {
                self.version_log_entries -= entries.len();
                for (partition_id, key) in entries {
                    if self.point_writes.get(&(partition_id, key.clone())).copied() == Some(version)
                    {
                        self.point_writes.remove(&(partition_id, key));
                        partitions_needing_refresh.insert(partition_id);
                    }
                }
            }
        }

        for partition_id in partitions_needing_refresh {
            self.refresh_partition_max(partition_id);
        }
        self.partition_floors.retain(|_, floor| *floor > horizon);
    }

    /// Validates reads and identifies whether a failure is due to a floor or dependency.
    pub(crate) fn validate_detailed(
        &self,
        snapshot: Version,
        footprint: &ReadFootprint,
    ) -> std::result::Result<(), ValidationFailure> {
        if self.global_floor > snapshot {
            return Err(ValidationFailure::Floor(HtapError::Conflict(format!(
                "serialization failure (validation window exceeded): global retained-write floor {} exceeds snapshot {}",
                self.global_floor, snapshot
            ))));
        }

        for (&partition_id, reads) in &footprint.partitions {
            if self.get_partition_floor(partition_id) > snapshot {
                return Err(ValidationFailure::Floor(HtapError::Conflict(format!(
                    "serialization failure (validation window exceeded): partition {} retained-write floor {} exceeds snapshot {}",
                    partition_id,
                    self.get_partition_floor(partition_id),
                    snapshot
                ))));
            }

            if self
                .partition_max_versions
                .get(&partition_id)
                .is_none_or(|version| *version <= snapshot)
            {
                continue;
            }

            let conflict = if reads.partition {
                self.range_lookup(partition_id, &[], None)
                    .into_iter()
                    .find(|(_, version)| *version > snapshot)
            } else {
                let point_conflict = reads.points.iter().find_map(|key| {
                    self.point_lookup(partition_id, key)
                        .filter(|version| *version > snapshot)
                        .map(|version| (key.clone(), version))
                });
                point_conflict.or_else(|| {
                    reads.ranges.iter().find_map(|(start, end)| {
                        self.range_lookup(partition_id, start, end.as_deref())
                            .into_iter()
                            .find(|(_, version)| *version > snapshot)
                    })
                })
            };

            if let Some((key, version)) = conflict {
                return Err(ValidationFailure::Dependency(HtapError::Conflict(format!(
                    "serialization failure (read-write dependency): partition {} key {:?} was written at {} after snapshot {}",
                    partition_id, key, version, snapshot
                ))));
            }
        }

        Ok(())
    }

    /// Raises the global validation floor to `version` if it is newer.
    ///
    /// This fail-closed path is used when committed write keys cannot be reported.
    pub fn set_global_floor(&mut self, version: Version) {
        self.global_floor = self.global_floor.max(version);
    }

    /// Returns the global validation floor.
    #[cfg(test)]
    pub fn get_global_floor(&self) -> Version {
        self.global_floor
    }

    /// Returns the validation floor for `partition_id`.
    pub fn get_partition_floor(&self, partition_id: u64) -> Version {
        self.partition_floors
            .get(&partition_id)
            .copied()
            .unwrap_or(Version::INITIAL)
    }

    /// Discards all retained validation state when no serializable snapshots remain pinned.
    pub fn clear(&mut self) {
        self.point_writes.clear();
        self.partition_max_versions.clear();
        self.partition_floors.clear();
        self.global_floor = Version::INITIAL;
        self.version_log.clear();
        self.version_log_entries = 0;
    }

    fn refresh_partition_max(&mut self, partition_id: u64) {
        let max_version = self
            .point_writes
            .range((partition_id, Vec::new())..)
            .take_while(|((partition, _), _)| *partition == partition_id)
            .map(|(_, version)| *version)
            .max();

        match max_version {
            Some(version) => {
                self.partition_max_versions.insert(partition_id, version);
            }
            None => {
                self.partition_max_versions.remove(&partition_id);
            }
        }
    }
}

impl Default for RecentWrites {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub(crate) struct PinnedSnapshotState {
    snapshots: BTreeMap<Version, usize>,
}

/// RAII pin for a serializable transaction snapshot.
///
/// Tickets are issued while holding the transaction manager's `decision_lock`,
/// then this registry mutex. Task 2 uses the ticket during
/// `TransactionManager::commit`; dropping it only acquires this registry mutex.
#[derive(Debug)]
pub struct SerializableTicket {
    snapshot: Version,
    manager_id: u64,
    registry: Weak<Mutex<PinnedSnapshotState>>,
}

impl SerializableTicket {
    /// Returns the pinned transaction snapshot.
    pub fn snapshot(&self) -> Version {
        self.snapshot
    }

    /// Returns the unique identity of the manager that issued this ticket.
    pub fn manager_id(&self) -> u64 {
        self.manager_id
    }

    /// Returns whether this ticket was issued by the specified registry allocation.
    pub(crate) fn belongs_to(&self, state: &Arc<Mutex<PinnedSnapshotState>>) -> bool {
        self.registry.ptr_eq(&Arc::downgrade(state))
    }
}

impl Drop for SerializableTicket {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut state = registry.lock();
            if let Some(count) = state.snapshots.get_mut(&self.snapshot) {
                *count -= 1;
                if *count == 0 {
                    state.snapshots.remove(&self.snapshot);
                }
            }
        }
    }
}

/// Registry of snapshots pinned by live serializable transactions.
///
/// Callers acquire locks in the order `decision_lock -> registry mutex`. Task 2
/// uses this registry from `TransactionManager::commit` while holding `decision_lock`.
#[derive(Debug)]
pub(crate) struct PinnedSnapshotRegistry {
    manager_id: u64,
    state: Arc<Mutex<PinnedSnapshotState>>,
}

impl PinnedSnapshotRegistry {
    /// Creates a registry owned by `manager_id`.
    ///
    /// Callers acquire locks in the order `decision_lock -> registry mutex`.
    /// Task 2 pins snapshots from `TransactionManager::commit` under `decision_lock`.
    pub(crate) fn new(manager_id: u64) -> Self {
        Self {
            manager_id,
            state: Arc::new(Mutex::new(PinnedSnapshotState::default())),
        }
    }

    /// Pins `snapshot` until the returned ticket is dropped.
    ///
    /// Callers acquire locks in the order `decision_lock -> registry mutex`.
    /// Task 2 calls this from `TransactionManager::commit` under `decision_lock`.
    pub(crate) fn pin(&mut self, snapshot: Version) -> SerializableTicket {
        let mut state = self.state.lock();
        *state.snapshots.entry(snapshot).or_default() += 1;

        SerializableTicket {
            snapshot,
            manager_id: self.manager_id,
            registry: Arc::downgrade(&self.state),
        }
    }

    /// Returns the oldest currently pinned snapshot.
    ///
    /// Callers acquire locks in the order `decision_lock -> registry mutex`.
    /// Task 2 queries this from `TransactionManager::commit` under `decision_lock`.
    pub(crate) fn oldest_pinned(&self) -> Option<Version> {
        self.state.lock().snapshots.keys().next().copied()
    }

    /// Returns the number of currently pinned tickets.
    ///
    /// Callers acquire locks in the order `decision_lock -> registry mutex`.
    /// Task 2 uses this registry from `TransactionManager::commit` under `decision_lock`.
    pub(crate) fn pinned_count(&self) -> usize {
        self.state.lock().snapshots.values().sum()
    }

    pub(crate) fn owns(&self, ticket: &SerializableTicket) -> bool {
        ticket.belongs_to(&self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_promotes_to_partition_at_cap() {
        let mut footprint = ReadFootprint::with_per_partition_cap(2);
        footprint.record_point(7, b"a".to_vec());
        footprint.record_range(7, b"b".to_vec(), Some(b"c".to_vec()));

        let entry = footprint.partitions.get(&7).unwrap();
        assert!(entry.partition);
        assert!(entry.points.is_empty());
        assert!(entry.ranges.is_empty());
    }

    #[test]
    fn recent_writes_point_range_partition_hits() {
        let mut writes = RecentWrites::new();
        writes.record_batch(
            &[
                WriteKey::new(1, b"a"),
                WriteKey::new(1, b"b"),
                WriteKey::new(1, b"c"),
            ],
            Version::new(3),
        );
        writes.record_batch(&[WriteKey::new(2, b"x")], Version::new(4));

        assert_eq!(writes.point_lookup(1, b"b"), Some(Version::new(3)));
        assert_eq!(
            writes.range_lookup(1, b"b", Some(b"c")),
            vec![(b"b".to_vec(), Version::new(3))]
        );

        let mut footprint = ReadFootprint::default();
        footprint.record_partition(1);
        assert!(matches!(
            writes.validate_detailed(Version::new(2), &footprint),
            Err(ValidationFailure::Dependency(HtapError::Conflict(_)))
        ));
    }

    #[test]
    fn prune_respects_oldest_ticket() {
        let registry = PinnedSnapshotRegistry::new(1);
        let mut registry = registry;
        let _ticket = registry.pin(Version::new(3));

        let mut writes = RecentWrites::new();
        writes.record_batch(&[WriteKey::new(1, b"old")], Version::new(2));
        writes.record_batch(&[WriteKey::new(1, b"at-pin")], Version::new(3));
        writes.record_batch(&[WriteKey::new(1, b"new")], Version::new(4));
        writes.prune(registry.oldest_pinned().unwrap());

        assert_eq!(writes.point_lookup(1, b"old"), None);
        assert_eq!(writes.point_lookup(1, b"at-pin"), None);
        assert_eq!(writes.point_lookup(1, b"new"), Some(Version::new(4)));
    }

    #[test]
    fn overflow_raises_partition_floor_and_aborts_old_snapshots() {
        let mut writes = RecentWrites::new();
        writes.max_entries = 2;
        writes.record_batch(&[WriteKey::new(1, b"a")], Version::new(2));
        writes.record_batch(&[WriteKey::new(1, b"b")], Version::new(3));

        // Isolate point-write eviction from the independently bounded historical log.
        writes.version_log.clear();
        writes.version_log_entries = 0;
        writes.max_entries = 1;
        writes.record_batch(&[], Version::new(3));

        assert_eq!(writes.get_partition_floor(1), Version::new(2));
        assert_eq!(writes.get_global_floor(), Version::INITIAL);

        let mut footprint = ReadFootprint::default();
        footprint.record_point(1, b"a".to_vec());
        assert!(matches!(
            writes.validate_detailed(Version::new(1), &footprint),
            Err(ValidationFailure::Floor(HtapError::Conflict(message)))
                if message.starts_with("serialization failure (validation window exceeded): ")
        ));
        assert!(writes
            .validate_detailed(Version::new(2), &footprint)
            .is_ok());

        let mut other_partition = ReadFootprint::default();
        other_partition.record_point(2, b"a".to_vec());
        assert!(writes
            .validate_detailed(Version::new(2), &other_partition)
            .is_ok());
    }

    #[test]
    fn version_log_retention_is_bounded_and_fails_closed() {
        fn assert_log_count(writes: &RecentWrites) {
            assert_eq!(
                writes.version_log_entries,
                writes.version_log.values().map(Vec::len).sum::<usize>()
            );
        }

        let mut writes = RecentWrites::new();
        writes.max_entries = 3;

        // Keep an old snapshot logically pinned by retaining all history during overflow.
        for version in 2..=4 {
            writes.record_batch(&[WriteKey::new(1, b"same")], Version::new(version));
            assert!(writes.version_log_entries <= writes.max_entries);
            assert!(writes.point_writes.len() <= writes.max_entries);
            assert_log_count(&writes);
        }

        writes.record_batch(
            &[
                WriteKey::new(1, b"a"),
                WriteKey::new(1, b"b"),
                WriteKey::new(1, b"c"),
                WriteKey::new(1, b"d"),
            ],
            Version::new(5),
        );
        assert!(writes.version_log_entries <= writes.max_entries);
        assert!(writes.point_writes.len() <= writes.max_entries);
        assert_log_count(&writes);

        let mut footprint = ReadFootprint::default();
        footprint.record_point(1, b"same".to_vec());
        assert!(matches!(
            writes.validate_detailed(Version::new(1), &footprint),
            Err(ValidationFailure::Floor(HtapError::Conflict(message)))
                if message.contains("validation window exceeded")
        ));
        writes
            .validate_detailed(Version::new(5), &footprint)
            .unwrap();

        writes.record_batch(&[WriteKey::new(1, b"new")], Version::new(6));
        writes.prune(Version::new(6));
        assert_log_count(&writes);

        writes.clear();
        assert_log_count(&writes);
    }

    #[test]
    fn equal_version_is_not_a_conflict() {
        let mut writes = RecentWrites::new();
        writes.record_batch(&[WriteKey::new(1, b"a")], Version::new(2));

        let mut footprint = ReadFootprint::default();
        footprint.record_point(1, b"a".to_vec());
        writes
            .validate_detailed(Version::new(2), &footprint)
            .unwrap();
    }

    #[test]
    fn global_floor_aborts_every_older_snapshot() {
        let mut registry = PinnedSnapshotRegistry::new(1);
        let ticket = registry.pin(Version::new(3));

        let mut writes = RecentWrites::new();
        writes.set_global_floor(Version::new(4));

        let mut footprint = ReadFootprint::default();
        footprint.record_point(99, b"unrelated".to_vec());
        assert_eq!(ticket.snapshot(), Version::new(3));
        assert!(matches!(
            writes.validate_detailed(ticket.snapshot(), &footprint),
            Err(ValidationFailure::Floor(HtapError::Conflict(message)))
                if message.starts_with("serialization failure (validation window exceeded): ")
        ));
    }

    #[test]
    fn range_edges_merge_and_ticket_drop() {
        let mut writes = RecentWrites::new();
        writes.record_batch(
            &[
                WriteKey::new(1, b"a"),
                WriteKey::new(1, b"b"),
                WriteKey::new(1, b"c"),
            ],
            Version::new(2),
        );
        assert_eq!(writes.range_lookup(1, b"b", Some(b"c")).len(), 1);
        assert_eq!(writes.range_lookup(1, b"b", None).len(), 2);

        let mut left = ReadFootprint::default();
        let mut right = ReadFootprint::default();
        left.record_point(1, b"a".to_vec());
        right.record_partition(2);
        left.merge(&right);
        assert!(left.partitions.get(&2).unwrap().partition);

        let mut registry = PinnedSnapshotRegistry::new(9);
        let first = registry.pin(Version::new(2));
        let second = registry.pin(Version::new(2));
        assert_eq!(first.manager_id(), 9);
        assert_eq!(first.snapshot(), Version::new(2));
        assert_eq!(registry.pinned_count(), 2);
        drop(first);
        assert_eq!(registry.oldest_pinned(), Some(Version::new(2)));
        drop(second);
        assert_eq!(registry.oldest_pinned(), None);
    }
}
