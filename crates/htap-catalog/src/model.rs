//! Catalog data model: table, partition, tablet, and replica descriptors.

use std::collections::HashSet;

use htap_common::{HtapError, Result, Schema};
use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            #[inline]
            pub const fn new(id: u64) -> Self {
                Self(id)
            }

            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }

            #[inline]
            pub const fn as_u64(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            #[inline]
            fn from(id: u64) -> Self {
                Self::new(id)
            }
        }

        impl From<$name> for u64 {
            #[inline]
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(TableId, "Unique identifier for a catalog table.");
define_id!(PartitionId, "Unique identifier for a table partition.");
define_id!(TabletId, "Unique identifier for a partition tablet.");
define_id!(ReplicaId, "Unique identifier for a tablet replica.");
define_id!(NodeId, "Unique identifier for a cluster node.");

/// Storage format of data inside a partition or tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StorageFormat {
    /// Row-oriented storage (LSM row store).
    Row,
    /// Columnar-oriented storage (column segments).
    Column,
}

/// Storage layout descriptor of a partition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StorageDescriptor {
    /// Row-oriented storage format.
    Row,
    /// Columnar storage format.
    Column,
    /// Actively converting from one storage format to another.
    Converting {
        from: StorageFormat,
        to: StorageFormat,
        generation: u64,
    },
}

/// Metadata descriptor for a relational table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDescriptor {
    /// Unique table identifier.
    pub id: TableId,
    /// Logical name of the table.
    pub name: String,
    /// Column definitions and table schema.
    pub schema: Schema,
    /// Indices into `schema` identifying primary key columns in order.
    pub primary_key: Vec<usize>,
    /// Partitions belonging to this table.
    pub partitions: Vec<PartitionId>,
    /// Schema or metadata generation version for this table.
    pub generation: u64,
}

impl TableDescriptor {
    /// Create a new table descriptor.
    pub fn new(
        id: TableId,
        name: impl Into<String>,
        schema: Schema,
        primary_key: Vec<usize>,
        partitions: Vec<PartitionId>,
        generation: u64,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            schema,
            primary_key,
            partitions,
            generation,
        }
    }
}

/// Metadata descriptor for a table partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionDescriptor {
    /// Unique partition identifier.
    pub id: PartitionId,
    /// Parent table identifier.
    pub table_id: TableId,
    /// Partition name (unique within the parent table).
    pub name: String,
    /// Physical storage format descriptor.
    pub storage: StorageDescriptor,
    /// Tablets belonging to this partition.
    pub tablets: Vec<TabletId>,
    /// Generation version for this partition metadata.
    pub generation: u64,
}

impl PartitionDescriptor {
    /// Create a new partition descriptor.
    pub fn new(
        id: PartitionId,
        table_id: TableId,
        name: impl Into<String>,
        storage: StorageDescriptor,
        tablets: Vec<TabletId>,
        generation: u64,
    ) -> Self {
        Self {
            id,
            table_id,
            name: name.into(),
            storage,
            tablets,
            generation,
        }
    }
}

/// Metadata descriptor for a tablet (horizontal partition bucket).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletDescriptor {
    /// Unique tablet identifier.
    pub id: TabletId,
    /// Parent partition identifier.
    pub partition_id: PartitionId,
    /// Bucket ordinal or hash ring slot.
    pub bucket: u32,
    /// Replicas of this tablet.
    pub replicas: Vec<ReplicaId>,
    /// Generation version for this tablet metadata.
    pub generation: u64,
}

impl TabletDescriptor {
    /// Create a new tablet descriptor.
    pub fn new(
        id: TabletId,
        partition_id: PartitionId,
        bucket: u32,
        replicas: Vec<ReplicaId>,
        generation: u64,
    ) -> Self {
        Self {
            id,
            partition_id,
            bucket,
            replicas,
            generation,
        }
    }
}

/// Metadata descriptor for a single replica of a tablet hosted on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaDescriptor {
    /// Unique replica identifier.
    pub id: ReplicaId,
    /// Parent tablet identifier.
    pub tablet_id: TabletId,
    /// Host cluster node identifier.
    pub node_id: NodeId,
    /// Whether this replica is currently the group leader.
    pub is_leader: bool,
    /// Whether this replica is healthy and participating in consensus.
    pub healthy: bool,
    /// Generation version for this replica metadata.
    pub generation: u64,
}

impl ReplicaDescriptor {
    /// Create a new replica descriptor.
    pub fn new(
        id: ReplicaId,
        tablet_id: TabletId,
        node_id: NodeId,
        is_leader: bool,
        healthy: bool,
        generation: u64,
    ) -> Self {
        Self {
            id,
            tablet_id,
            node_id,
            is_leader,
            healthy,
            generation,
        }
    }
}

/// An immutable point-in-time snapshot of the complete cluster catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CatalogSnapshot {
    /// Monotonically increasing snapshot generation counter.
    pub generation: u64,
    /// All relational tables.
    pub tables: Vec<TableDescriptor>,
    /// All partitions across tables.
    pub partitions: Vec<PartitionDescriptor>,
    /// All tablets across partitions.
    pub tablets: Vec<TabletDescriptor>,
    /// All replicas across tablets.
    pub replicas: Vec<ReplicaDescriptor>,
}

impl CatalogSnapshot {
    /// Create a new empty snapshot at generation 0.
    pub fn empty() -> Self {
        Self {
            generation: 0,
            tables: Vec::new(),
            partitions: Vec::new(),
            tablets: Vec::new(),
            replicas: Vec::new(),
        }
    }

    /// Create a new catalog snapshot with the provided elements.
    pub fn new(
        generation: u64,
        tables: Vec<TableDescriptor>,
        partitions: Vec<PartitionDescriptor>,
        tablets: Vec<TabletDescriptor>,
        replicas: Vec<ReplicaDescriptor>,
    ) -> Self {
        Self {
            generation,
            tables,
            partitions,
            tablets,
            replicas,
        }
    }

    /// Find a table descriptor by table ID.
    pub fn table(&self, id: TableId) -> Option<&TableDescriptor> {
        self.tables.iter().find(|t| t.id == id)
    }

    /// Find a table descriptor by table name.
    pub fn table_by_name(&self, name: &str) -> Option<&TableDescriptor> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Find a partition descriptor by partition ID.
    pub fn partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.partitions.iter().find(|p| p.id == id)
    }

    /// Find a tablet descriptor by tablet ID.
    pub fn tablet(&self, id: TabletId) -> Option<&TabletDescriptor> {
        self.tablets.iter().find(|t| t.id == id)
    }

    /// Find a replica descriptor by replica ID.
    pub fn replica(&self, id: ReplicaId) -> Option<&ReplicaDescriptor> {
        self.replicas.iter().find(|r| r.id == id)
    }

    /// Get all partitions belonging to a table.
    pub fn table_partitions(&self, table_id: TableId) -> Vec<&PartitionDescriptor> {
        self.partitions
            .iter()
            .filter(|p| p.table_id == table_id)
            .collect()
    }

    /// Get all tablets belonging to a partition.
    pub fn partition_tablets(&self, partition_id: PartitionId) -> Vec<&TabletDescriptor> {
        self.tablets
            .iter()
            .filter(|t| t.partition_id == partition_id)
            .collect()
    }

    /// Get all replicas belonging to a tablet.
    pub fn tablet_replicas(&self, tablet_id: TabletId) -> Vec<&ReplicaDescriptor> {
        self.replicas
            .iter()
            .filter(|r| r.tablet_id == tablet_id)
            .collect()
    }

    /// Validate the catalog snapshot for semantic correctness.
    ///
    /// Checks:
    /// - Unique IDs for tables, partitions, tablets, and replicas.
    /// - Non-empty names for tables and partitions.
    /// - Unique table names across the catalog.
    /// - Unique partition names within each table.
    /// - Table schema validity and primary key indices within schema bounds without duplicates.
    /// - Foreign key integrity from partition to table, tablet to partition, replica to tablet.
    /// - Complete and bidirectional ownership references:
    ///   - Each table references exactly the partitions that claim it.
    ///   - Each partition references exactly the tablets that claim it.
    ///   - Each tablet references exactly the replicas that claim it.
    ///   - No orphaned or duplicate entity references.
    pub fn validate(&self) -> Result<()> {
        // 1. Validate tables: unique IDs, unique non-empty names, valid schemas and primary keys.
        let mut seen_table_ids = HashSet::with_capacity(self.tables.len());
        let mut seen_table_names = HashSet::with_capacity(self.tables.len());

        for table in &self.tables {
            if table.name.trim().is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "table id {} has an empty name",
                    table.id
                )));
            }

            if !seen_table_ids.insert(table.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate table id: {}",
                    table.id
                )));
            }

            if !seen_table_names.insert(table.name.as_str()) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate table name: '{}'",
                    table.name
                )));
            }

            if table.schema.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "table '{}' has an empty schema",
                    table.name
                )));
            }

            // Schema column validation: non-empty unique column names
            let mut seen_cols = HashSet::with_capacity(table.schema.len());
            for col in table.schema.columns() {
                if col.name.trim().is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' has a column with an empty name",
                        table.name
                    )));
                }
                if !seen_cols.insert(col.name.as_str()) {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' has duplicate column name '{}'",
                        table.name, col.name
                    )));
                }
            }

            // Primary key validation: non-empty, indices within bounds and unique,
            // PK indices reference columns marked primary_key, and all columns marked primary_key are in PK.
            if table.primary_key.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "table '{}' has an empty primary key",
                    table.name
                )));
            }

            let schema_len = table.schema.len();
            let mut seen_pk_indices = HashSet::with_capacity(table.primary_key.len());
            for &idx in &table.primary_key {
                if idx >= schema_len {
                    return Err(HtapError::InvalidArgument(format!(
                        "primary key index {} out of bounds for table '{}' (schema length {})",
                        idx, table.name, schema_len
                    )));
                }
                if !seen_pk_indices.insert(idx) {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' has duplicate primary key index {}",
                        table.name, idx
                    )));
                }
                if !table.schema.columns()[idx].primary_key {
                    return Err(HtapError::InvalidArgument(format!(
                        "primary key index {} in table '{}' refers to column '{}' which is not marked as primary key",
                        idx, table.name, table.schema.columns()[idx].name
                    )));
                }
            }

            for (col_idx, col) in table.schema.columns().iter().enumerate() {
                if col.primary_key && !seen_pk_indices.contains(&col_idx) {
                    return Err(HtapError::InvalidArgument(format!(
                        "column '{}' (index {}) in table '{}' is marked as primary key but absent from primary key list",
                        col.name, col_idx, table.name
                    )));
                }
            }
        }

        // 2. Validate partitions: unique IDs, non-empty names, unique names per table, FK to table.
        let mut seen_partition_ids = HashSet::with_capacity(self.partitions.len());
        let mut seen_partition_names = HashSet::with_capacity(self.partitions.len());

        for part in &self.partitions {
            if part.name.trim().is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "partition id {} has an empty name",
                    part.id
                )));
            }

            if !seen_partition_ids.insert(part.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate partition id: {}",
                    part.id
                )));
            }

            if !seen_partition_names.insert((part.table_id, part.name.as_str())) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate partition name '{}' in table id {}",
                    part.name, part.table_id
                )));
            }

            // Foreign key: table must exist
            if !seen_table_ids.contains(&part.table_id) {
                return Err(HtapError::InvalidArgument(format!(
                    "partition {} references nonexistent table {}",
                    part.id, part.table_id
                )));
            }
        }

        // Check bidirectional ownership between Tables and Partitions
        let mut claimed_partitions = HashSet::with_capacity(self.partitions.len());
        for table in &self.tables {
            let mut table_seen_parts = HashSet::with_capacity(table.partitions.len());
            for &part_id in &table.partitions {
                if !table_seen_parts.insert(part_id) {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' (id {}) lists duplicate partition id {}",
                        table.name, table.id, part_id
                    )));
                }
                let part = self.partition(part_id).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "table '{}' (id {}) references nonexistent partition id {}",
                        table.name, table.id, part_id
                    ))
                })?;
                if part.table_id != table.id {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' (id {}) references partition {} belonging to table id {}",
                        table.name, table.id, part_id, part.table_id
                    )));
                }
                claimed_partitions.insert(part_id);
            }
        }

        for part in &self.partitions {
            if !claimed_partitions.contains(&part.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "partition {} is not referenced by parent table {}",
                    part.id, part.table_id
                )));
            }
        }

        // 3. Validate tablets: unique IDs, FK to partition.
        let mut seen_tablet_ids = HashSet::with_capacity(self.tablets.len());
        for tablet in &self.tablets {
            if !seen_tablet_ids.insert(tablet.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate tablet id: {}",
                    tablet.id
                )));
            }

            // Foreign key: partition must exist
            if !seen_partition_ids.contains(&tablet.partition_id) {
                return Err(HtapError::InvalidArgument(format!(
                    "tablet {} references nonexistent partition {}",
                    tablet.id, tablet.partition_id
                )));
            }
        }

        // Check bidirectional ownership between Partitions and Tablets
        let mut claimed_tablets = HashSet::with_capacity(self.tablets.len());
        for part in &self.partitions {
            let mut part_seen_tablets = HashSet::with_capacity(part.tablets.len());
            for &tablet_id in &part.tablets {
                if !part_seen_tablets.insert(tablet_id) {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{}' (id {}) lists duplicate tablet id {}",
                        part.name, part.id, tablet_id
                    )));
                }
                let tab = self.tablet(tablet_id).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "partition '{}' (id {}) references nonexistent tablet id {}",
                        part.name, part.id, tablet_id
                    ))
                })?;
                if tab.partition_id != part.id {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition '{}' (id {}) references tablet {} belonging to partition id {}",
                        part.name, part.id, tablet_id, tab.partition_id
                    )));
                }
                claimed_tablets.insert(tablet_id);
            }
        }

        for tab in &self.tablets {
            if !claimed_tablets.contains(&tab.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "tablet {} is not referenced by parent partition {}",
                    tab.id, tab.partition_id
                )));
            }
        }

        // 4. Validate replicas: unique IDs, FK to tablet.
        let mut seen_replica_ids = HashSet::with_capacity(self.replicas.len());
        for replica in &self.replicas {
            if !seen_replica_ids.insert(replica.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate replica id: {}",
                    replica.id
                )));
            }

            // Foreign key: tablet must exist
            if !seen_tablet_ids.contains(&replica.tablet_id) {
                return Err(HtapError::InvalidArgument(format!(
                    "replica {} references nonexistent tablet {}",
                    replica.id, replica.tablet_id
                )));
            }
        }

        // Check bidirectional ownership between Tablets and Replicas
        let mut claimed_replicas = HashSet::with_capacity(self.replicas.len());
        for tab in &self.tablets {
            let mut tab_seen_replicas = HashSet::with_capacity(tab.replicas.len());
            for &replica_id in &tab.replicas {
                if !tab_seen_replicas.insert(replica_id) {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} lists duplicate replica id {}",
                        tab.id, replica_id
                    )));
                }
                let rep = self.replica(replica_id).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "tablet id {} references nonexistent replica id {}",
                        tab.id, replica_id
                    ))
                })?;
                if rep.tablet_id != tab.id {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} references replica {} belonging to tablet id {}",
                        tab.id, replica_id, rep.tablet_id
                    )));
                }
                claimed_replicas.insert(replica_id);
            }
        }

        for rep in &self.replicas {
            if !claimed_replicas.contains(&rep.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "replica {} is not referenced by parent tablet {}",
                    rep.id, rep.tablet_id
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::{ColumnDef, DataType};
    use std::collections::HashSet;

    #[test]
    fn test_id_types_and_traits() {
        let tid = TableId::new(10);
        assert_eq!(tid.get(), 10);
        assert_eq!(tid.as_u64(), 10);
        assert_eq!(TableId::from(10), tid);
        assert_eq!(u64::from(tid), 10);
        assert_eq!(tid.to_string(), "10");

        let pid = PartitionId::new(20);
        assert_eq!(pid.get(), 20);
        let tab_id = TabletId::new(30);
        assert_eq!(tab_id.get(), 30);
        let rid = ReplicaId::new(40);
        assert_eq!(rid.get(), 40);
        let nid = NodeId::new(50);
        assert_eq!(nid.get(), 50);

        let mut set = HashSet::new();
        set.insert(tid);
        assert!(set.contains(&TableId::new(10)));
        assert!(!set.contains(&TableId::new(11)));

        let json = serde_json::to_string(&tid).unwrap();
        assert_eq!(json, "10");
        let parsed: TableId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tid);
    }

    #[test]
    fn test_storage_descriptors() {
        let r = StorageDescriptor::Row;
        let c = StorageDescriptor::Column;
        let conv = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: 1,
        };

        let json = serde_json::to_string(&conv).unwrap();
        let decoded: StorageDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, conv);
        assert_ne!(r, c);
    }

    #[test]
    fn test_snapshot_helpers() {
        let schema = Schema::new(vec![ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        }])
        .unwrap();

        let table = TableDescriptor::new(
            TableId::new(1),
            "users",
            schema,
            vec![0],
            vec![PartitionId::new(10)],
            1,
        );
        let partition = PartitionDescriptor::new(
            PartitionId::new(10),
            TableId::new(1),
            "p0",
            StorageDescriptor::Row,
            vec![TabletId::new(100)],
            1,
        );
        let tablet = TabletDescriptor::new(
            TabletId::new(100),
            PartitionId::new(10),
            0,
            vec![ReplicaId::new(1000)],
            1,
        );
        let replica = ReplicaDescriptor::new(
            ReplicaId::new(1000),
            TabletId::new(100),
            NodeId::new(42),
            true,
            true,
            1,
        );

        let snapshot = CatalogSnapshot::new(
            1,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tablet.clone()],
            vec![replica.clone()],
        );

        assert_eq!(snapshot.table(TableId::new(1)), Some(&table));
        assert_eq!(snapshot.table(TableId::new(999)), None);
        assert_eq!(snapshot.table_by_name("users"), Some(&table));
        assert_eq!(snapshot.table_by_name("unknown"), None);
        assert_eq!(snapshot.partition(PartitionId::new(10)), Some(&partition));
        assert_eq!(snapshot.tablet(TabletId::new(100)), Some(&tablet));
        assert_eq!(snapshot.replica(ReplicaId::new(1000)), Some(&replica));

        assert_eq!(snapshot.table_partitions(TableId::new(1)), vec![&partition]);
        assert!(snapshot.table_partitions(TableId::new(999)).is_empty());
        assert_eq!(
            snapshot.partition_tablets(PartitionId::new(10)),
            vec![&tablet]
        );
        assert_eq!(snapshot.tablet_replicas(TabletId::new(100)), vec![&replica]);

        assert!(snapshot.validate().is_ok());

        let empty = CatalogSnapshot::empty();
        assert_eq!(empty.generation, 0);
        assert!(empty.validate().is_ok());
    }
}
