//! Catalog data model: table, partition, tablet, and replica descriptors.

use std::collections::HashSet;

use htap_common::{HtapError, Result, Schema, Value, Version};
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
define_id!(AccountId, "Unique identifier for a catalog account.");

/// Bit set of privileges granted to an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct PrivilegeSet(pub u16);

impl PrivilegeSet {
    pub const SELECT: Self = Self(1 << 0);
    pub const INSERT: Self = Self(1 << 1);
    pub const UPDATE: Self = Self(1 << 2);
    pub const DELETE: Self = Self(1 << 3);
    pub const CREATE: Self = Self(1 << 4);
    pub const DROP: Self = Self(1 << 5);
    pub const ALTER: Self = Self(1 << 6);
    pub const ALL: Self = Self(
        Self::SELECT.0
            | Self::INSERT.0
            | Self::UPDATE.0
            | Self::DELETE.0
            | Self::CREATE.0
            | Self::DROP.0
            | Self::ALTER.0,
    );

    /// Returns an empty privilege set.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns true when this set includes every privilege in `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the union of this set and `other`.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns privileges in this set that are absent from `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Returns true when this set has no privileges.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for PrivilegeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let privileges = [
            (Self::SELECT, "SELECT"),
            (Self::INSERT, "INSERT"),
            (Self::UPDATE, "UPDATE"),
            (Self::DELETE, "DELETE"),
            (Self::CREATE, "CREATE"),
            (Self::DROP, "DROP"),
            (Self::ALTER, "ALTER"),
        ];

        let mut first = true;
        for (privilege, name) in privileges {
            if self.contains(privilege) {
                if !first {
                    f.write_str(",")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Catalog account metadata.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Unique account identifier.
    pub id: AccountId,
    /// Account login name.
    pub username: String,
    /// SHA-1 password hash. `None` represents an account without a password.
    pub password_hash: Option<[u8; 20]>,
    /// Whether login is disabled for this account.
    pub locked: bool,
    /// Whether this account bypasses ordinary privilege checks.
    pub is_superuser: bool,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .field("locked", &self.locked)
            .field("is_superuser", &self.is_superuser)
            .finish()
    }
}

/// Scope to which a privilege grant applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrivilegeScope {
    /// Applies to every table.
    Global,
    /// Applies to one table.
    Table(TableId),
}

/// Privileges granted to an account within one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Account receiving the privileges.
    pub account: AccountId,
    /// Scope covered by this grant.
    pub scope: PrivilegeScope,
    /// Granted privileges.
    pub privileges: PrivilegeSet,
}

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

/// Execution phase of an active storage format conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConversionPhase {
    /// Source snapshot version pinned; segments not yet fully written.
    SnapshotPinned,
    /// Columnar segments written to disk and verified.
    SegmentsWritten,
    /// Conversion catch-up completed and ready for catalog cutover.
    ReadyToPublish,
}

/// Descriptor tracking an in-flight storage format conversion for a partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionDescriptor {
    /// Generation identifier matching the storage descriptor converting generation.
    pub generation: u64,
    /// Source storage format.
    pub from: StorageFormat,
    /// Target storage format.
    pub to: StorageFormat,
    /// MVCC snapshot version pinned at the beginning of conversion.
    pub snapshot_version: Version,
    /// Current execution phase.
    pub phase: ConversionPhase,
}

impl ConversionDescriptor {
    /// Create a new conversion descriptor.
    pub fn new(
        generation: u64,
        from: StorageFormat,
        to: StorageFormat,
        snapshot_version: Version,
        phase: ConversionPhase,
    ) -> Self {
        Self {
            generation,
            from,
            to,
            snapshot_version,
            phase,
        }
    }
}

/// Reference to an on-disk columnar segment manifest for a tablet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnManifestRef {
    /// Catalog metadata generation when this manifest reference was established.
    pub generation: u64,
    /// Root-relative path to the manifest file (no absolute paths or '..' traversals).
    pub path: String,
    /// MVCC base snapshot version that this columnar manifest represents.
    pub base_version: Version,
    /// Total number of columnar segments referenced by the manifest.
    pub segment_count: u64,
    /// Total number of data rows across all columnar segments in the manifest.
    pub row_count: u64,
}

impl ColumnManifestRef {
    /// Create a new columnar manifest reference.
    pub fn new(
        generation: u64,
        path: impl Into<String>,
        base_version: Version,
        segment_count: u64,
        row_count: u64,
    ) -> Self {
        Self {
            generation,
            path: path.into(),
            base_version,
            segment_count,
            row_count,
        }
    }
}

/// Maximum reasonable number of segments per tablet manifest reference.
pub const MAX_MANIFEST_SEGMENTS: u64 = 10_000_000;
/// Maximum reasonable number of rows per tablet manifest reference.
pub const MAX_MANIFEST_ROWS: u64 = 1_000_000_000_000_000;

/// Partitioning strategy method for a partitioned relational table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PartitioningMethod {
    /// Range partitioning where partitions cover non-overlapping intervals `[lower, upper)`.
    Range,
    /// List partitioning where partitions cover disjoint sets of explicit values.
    List,
}

/// Partitioning descriptor defining key column and partitioning method.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartitioningDescriptor {
    /// Zero-based column index in table schema used as partition key.
    pub key_column: usize,
    /// Partitioning method (Range or List).
    pub method: PartitioningMethod,
}

impl PartitioningDescriptor {
    /// Create a new partitioning descriptor.
    pub fn new(key_column: usize, method: PartitioningMethod) -> Self {
        Self { key_column, method }
    }
}

/// Boundary for a range partition: lower-inclusive, upper-exclusive (`[lower, upper)`).
/// Endpoints can be unbounded (`lower: None` represents negative infinity, `upper: None` represents positive infinity/MAXVALUE).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RangeBound {
    /// Inclusive lower bound (`None` represents negative infinity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<Value>,
    /// Exclusive upper bound (`None` represents MAXVALUE / positive infinity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<Value>,
}

impl RangeBound {
    /// Create a new bounded range bound (`[lower, upper)`).
    pub fn new(lower: Value, upper: Value) -> Self {
        Self {
            lower: Some(lower),
            upper: Some(upper),
        }
    }

    /// Create a range bound with optional endpoints (`[lower, upper)`).
    pub fn new_opt(lower: Option<Value>, upper: Option<Value>) -> Self {
        Self { lower, upper }
    }

    /// Returns true if `value` is within this range bound `[lower, upper)`.
    pub fn contains(&self, value: &Value) -> bool {
        if let Some(l) = &self.lower {
            if value < l {
                return false;
            }
        }
        if let Some(u) = &self.upper {
            if value >= u {
                return false;
            }
        }
        true
    }
}

/// Definition of a single range partition with half-open bound `[lower, upper)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangePartitionDefinition {
    /// Partition name (unique within table).
    pub name: String,
    /// Inclusive lower bound value.
    pub lower: Value,
    /// Exclusive upper bound value (or MAXVALUE if None).
    pub upper: Value,
    /// Optional lower bound (for unbounded/MAXVALUE representation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower_opt: Option<Value>,
    /// Optional upper bound (for unbounded/MAXVALUE representation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_opt: Option<Value>,
}

impl RangePartitionDefinition {
    /// Creates a new range partition definition with bounded endpoints.
    pub fn new(name: impl Into<String>, lower: Value, upper: Value) -> Self {
        Self {
            name: name.into(),
            lower: lower.clone(),
            upper: upper.clone(),
            lower_opt: Some(lower),
            upper_opt: Some(upper),
        }
    }

    /// Creates a range partition definition with optional endpoints.
    pub fn new_opt(name: impl Into<String>, lower: Option<Value>, upper: Option<Value>) -> Self {
        let l = lower.clone().unwrap_or(Value::Null);
        let u = upper.clone().unwrap_or(Value::Null);
        Self {
            name: name.into(),
            lower: l,
            upper: u,
            lower_opt: lower,
            upper_opt: upper,
        }
    }

    /// Converts this range partition definition into a [`RangeBound`].
    pub fn range_bound(&self) -> RangeBound {
        if self.lower_opt.is_some() || self.upper_opt.is_some() {
            RangeBound::new_opt(self.lower_opt.clone(), self.upper_opt.clone())
        } else if !self.lower.is_null() || !self.upper.is_null() {
            RangeBound::new(self.lower.clone(), self.upper.clone())
        } else {
            RangeBound::new_opt(None, None)
        }
    }
}

/// Definition of a single list partition with explicit values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPartitionDefinition {
    /// Partition name (unique within table).
    pub name: String,
    /// Disjoint set of values belonging to this partition.
    pub values: Vec<Value>,
}

impl ListPartitionDefinition {
    /// Creates a new list partition definition.
    pub fn new(name: impl Into<String>, values: impl Into<Vec<Value>>) -> Self {
        Self {
            name: name.into(),
            values: values.into(),
        }
    }
}

/// Definition of a single partition (Range or List).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionDefinition {
    /// Range partition definition.
    Range(RangePartitionDefinition),
    /// List partition definition.
    List(ListPartitionDefinition),
}

impl PartitionDefinition {
    /// Returns the partition name.
    pub fn name(&self) -> &str {
        match self {
            Self::Range(r) => &r.name,
            Self::List(l) => &l.name,
        }
    }

    /// Returns a reference to the inner [`RangePartitionDefinition`], if Range.
    pub fn range(&self) -> Option<&RangePartitionDefinition> {
        match self {
            Self::Range(r) => Some(r),
            Self::List(_) => None,
        }
    }

    /// Returns a reference to the inner [`ListPartitionDefinition`], if List.
    pub fn list(&self) -> Option<&ListPartitionDefinition> {
        match self {
            Self::Range(_) => None,
            Self::List(l) => Some(l),
        }
    }
}

impl From<RangePartitionDefinition> for PartitionDefinition {
    fn from(r: RangePartitionDefinition) -> Self {
        Self::Range(r)
    }
}

impl From<ListPartitionDefinition> for PartitionDefinition {
    fn from(l: ListPartitionDefinition) -> Self {
        Self::List(l)
    }
}

/// Specification of an alteration to a table's partition topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionAlteration {
    /// Add one or more partitions to the table.
    Add {
        /// Definitions of new partitions to add.
        partitions: Vec<PartitionDefinition>,
    },
    /// Drop one or more partitions from the table.
    Drop {
        /// Names of partitions to drop.
        partitions: Vec<String>,
    },
    /// Reorganize one or more existing contiguous partitions into new partitions.
    Reorganize {
        /// Names of existing contiguous source partitions to replace.
        sources: Vec<String>,
        /// Definitions of target partitions replacing the sources.
        targets: Vec<PartitionDefinition>,
    },
}

impl PartitionAlteration {
    /// Constructs an `Add` partition alteration.
    pub fn add(partitions: impl IntoIterator<Item = impl Into<PartitionDefinition>>) -> Self {
        Self::Add {
            partitions: partitions.into_iter().map(Into::into).collect(),
        }
    }

    /// Constructs a `Drop` partition alteration.
    pub fn drop(partitions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Drop {
            partitions: partitions.into_iter().map(Into::into).collect(),
        }
    }

    /// Constructs a `Reorganize` partition alteration.
    pub fn reorganize(
        sources: impl IntoIterator<Item = impl Into<String>>,
        targets: impl IntoIterator<Item = impl Into<PartitionDefinition>>,
    ) -> Self {
        Self::Reorganize {
            sources: sources.into_iter().map(Into::into).collect(),
            targets: targets.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns the source partition names affected by this alteration (for Drop and Reorganize).
    pub fn source_partition_names(&self) -> &[String] {
        match self {
            Self::Add { .. } => &[],
            Self::Drop { partitions } => partitions,
            Self::Reorganize { sources, .. } => sources,
        }
    }
}

impl From<&PartitionAlteration> for PartitionAlteration {
    fn from(alteration: &PartitionAlteration) -> Self {
        alteration.clone()
    }
}

/// Statistics for a single table column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnStats {
    /// Number of null values in the column.
    pub null_count: u64,
    /// Estimated or exact number of distinct non-null values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct_count: Option<u64>,
    /// Minimum non-null value in the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<Value>,
    /// Maximum non-null value in the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Value>,
}

/// Statistics collected for a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStats {
    /// Catalog or data version at which these statistics were analyzed.
    pub analyzed_at_version: u64,
    /// Number of rows in the table.
    pub row_count: u64,
    /// Per-column statistics in schema column order.
    pub columns: Vec<ColumnStats>,
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
    /// Partitioning strategy descriptor, if table is partitioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partitioning: Option<PartitioningDescriptor>,
    /// Statistics collected for this table, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<TableStats>,
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
            partitioning: None,
            stats: None,
        }
    }

    /// Set partitioning descriptor metadata.
    pub fn with_partitioning(
        mut self,
        partitioning: impl Into<Option<PartitioningDescriptor>>,
    ) -> Self {
        self.partitioning = partitioning.into();
        self
    }

    /// Set table statistics metadata.
    pub fn with_stats(mut self, stats: impl Into<Option<TableStats>>) -> Self {
        self.stats = stats.into();
        self
    }

    /// Route a partition key value to the matching partition ID using `self.partitions` order.
    ///
    /// - For unpartitioned tables: routes to its sole partition (error if not exactly 1 partition).
    /// - For Range partitioning: routes to the partition where `lower <= value < upper`.
    /// - For List partitioning: routes to the partition containing `value`.
    /// - If unmatched: returns [`HtapError::InvalidArgument`].
    pub fn route_partition_value<S: PartitionSource + ?Sized>(
        &self,
        source: &S,
        value: &Value,
    ) -> Result<PartitionId> {
        match &self.partitioning {
            None => {
                if self.partitions.len() == 1 {
                    Ok(self.partitions[0])
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "unpartitioned table '{}' must have exactly one partition to route, found {}",
                        self.name,
                        self.partitions.len()
                    )))
                }
            }
            Some(partitioning) => {
                if partitioning.key_column >= self.schema.len() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key column index {} out of bounds for table '{}' (schema length {})",
                        partitioning.key_column,
                        self.name,
                        self.schema.len()
                    )));
                }
                let key_col = &self.schema.columns()[partitioning.key_column];
                if value.is_null() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key value cannot be null for table '{}'",
                        self.name
                    )));
                }
                if value.data_type() != Some(key_col.data_type) {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key value type mismatch for table '{}': expected {:?}, got {:?}",
                        self.name,
                        key_col.data_type,
                        value.data_type()
                    )));
                }

                for &part_id in &self.partitions {
                    let part = source.find_partition(part_id).ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "table '{}' references partition id {} which was not found",
                            self.name, part_id
                        ))
                    })?;

                    match partitioning.method {
                        PartitioningMethod::Range => {
                            if let Some(range) = &part.range {
                                if range.contains(value) {
                                    return Ok(part_id);
                                }
                            }
                        }
                        PartitioningMethod::List => {
                            if part.list_values.iter().any(|v| v == value) {
                                return Ok(part_id);
                            }
                        }
                    }
                }

                Err(HtapError::InvalidArgument(format!(
                    "partition key value {} does not match any partition in table '{}'",
                    value, self.name
                )))
            }
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
    /// In-flight conversion metadata, if converting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<ConversionDescriptor>,
    /// Range bounds if belonging to a range-partitioned table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<RangeBound>,
    /// List values if belonging to a list-partitioned table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub list_values: Vec<Value>,
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
            conversion: None,
            range: None,
            list_values: Vec::new(),
        }
    }

    /// Set conversion descriptor metadata.
    pub fn with_conversion(mut self, conversion: impl Into<Option<ConversionDescriptor>>) -> Self {
        self.conversion = conversion.into();
        self
    }

    /// Set range bound metadata.
    pub fn with_range(mut self, range: impl Into<Option<RangeBound>>) -> Self {
        self.range = range.into();
        self
    }

    /// Set list values metadata.
    pub fn with_list_values(mut self, list_values: impl Into<Vec<Value>>) -> Self {
        self.list_values = list_values.into();
        self
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
    /// Column manifest reference if in Column or Converting-to-Column format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_manifest: Option<ColumnManifestRef>,
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
            column_manifest: None,
        }
    }

    /// Set column manifest reference.
    pub fn with_column_manifest(
        mut self,
        column_manifest: impl Into<Option<ColumnManifestRef>>,
    ) -> Self {
        self.column_manifest = column_manifest.into();
        self
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
    /// All catalog accounts.
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// All account privilege grants.
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Whether bootstrap account initialization has completed.
    #[serde(default)]
    pub accounts_initialized: bool,
    /// All relational tables.
    pub tables: Vec<TableDescriptor>,
    /// All partitions across tables.
    pub partitions: Vec<PartitionDescriptor>,
    /// All tablets across partitions.
    pub tablets: Vec<TabletDescriptor>,
    /// All replicas across tablets.
    pub replicas: Vec<ReplicaDescriptor>,
    /// Highest identifiers ever allocated, persisted so that identifiers of dropped objects
    /// are never reissued (rowstore and columnar data are keyed by these ids and are not
    /// physically reclaimed on drop). Catalogs written before this field existed decode
    /// with zeros; [`CatalogSnapshot::id_high_water`] then falls back to the live maximum.
    #[serde(default)]
    pub id_high_water: IdHighWater,
}

/// Highest identifier ever allocated per identifier space.
///
/// Allocation must always start above `max(persisted counter, highest live id)`; see
/// [`CatalogSnapshot::id_high_water`] and [`IdHighWater::allocate`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdHighWater {
    /// Highest account id ever allocated.
    #[serde(default)]
    pub account: u64,
    /// Highest table id ever allocated.
    pub table: u64,
    /// Highest partition id ever allocated.
    pub partition: u64,
    /// Highest tablet id ever allocated.
    pub tablet: u64,
    /// Highest replica id ever allocated.
    pub replica: u64,
}

impl IdHighWater {
    /// Merges two high-water marks by taking the maximum of each counter.
    pub fn max(self, other: IdHighWater) -> IdHighWater {
        IdHighWater {
            account: self.account.max(other.account),
            table: self.table.max(other.table),
            partition: self.partition.max(other.partition),
            tablet: self.tablet.max(other.tablet),
            replica: self.replica.max(other.replica),
        }
    }

    fn bump(counter: &mut u64, name: &'static str) -> Result<u64> {
        *counter = counter
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow { counter: name })?;
        Ok(*counter)
    }

    /// Allocates the next account id.
    pub fn allocate_account(&mut self) -> Result<AccountId> {
        Self::bump(&mut self.account, "account_id").map(AccountId::new)
    }

    /// Allocates the next table id.
    pub fn allocate_table(&mut self) -> Result<TableId> {
        Self::bump(&mut self.table, "table_id").map(TableId::new)
    }

    /// Allocates the next partition id.
    pub fn allocate_partition(&mut self) -> Result<PartitionId> {
        Self::bump(&mut self.partition, "partition_id").map(PartitionId::new)
    }

    /// Allocates the next tablet id.
    pub fn allocate_tablet(&mut self) -> Result<TabletId> {
        Self::bump(&mut self.tablet, "tablet_id").map(TabletId::new)
    }

    /// Allocates the next replica id.
    pub fn allocate_replica(&mut self) -> Result<ReplicaId> {
        Self::bump(&mut self.replica, "replica_id").map(ReplicaId::new)
    }
}

/// Trait for partition lookup sources used during partition value routing.
pub trait PartitionSource {
    /// Look up a partition descriptor by its partition identifier.
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor>;
}

impl PartitionSource for CatalogSnapshot {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.partition(id)
    }
}

impl PartitionSource for [PartitionDescriptor] {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().find(|p| p.id == id)
    }
}

impl PartitionSource for Vec<PartitionDescriptor> {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().find(|p| p.id == id)
    }
}

impl PartitionSource for &[PartitionDescriptor] {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().find(|p| p.id == id)
    }
}

impl PartitionSource for [&PartitionDescriptor] {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().copied().find(|p| p.id == id)
    }
}

impl PartitionSource for Vec<&PartitionDescriptor> {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().copied().find(|p| p.id == id)
    }
}

impl PartitionSource for &[&PartitionDescriptor] {
    fn find_partition(&self, id: PartitionId) -> Option<&PartitionDescriptor> {
        self.iter().copied().find(|p| p.id == id)
    }
}

/// Trait for types that can resolve to a [`TableDescriptor`] within a catalog snapshot.
pub trait TableSelector<'a> {
    /// Resolve the table descriptor reference.
    fn resolve_table(self, snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor>;
}

impl<'a> TableSelector<'a> for TableId {
    fn resolve_table(self, snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor> {
        snapshot
            .table(self)
            .ok_or_else(|| HtapError::InvalidArgument(format!("table id {} not found", self)))
    }
}

impl<'a> TableSelector<'a> for &'a TableId {
    fn resolve_table(self, snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor> {
        snapshot
            .table(*self)
            .ok_or_else(|| HtapError::InvalidArgument(format!("table id {} not found", *self)))
    }
}

impl<'a> TableSelector<'a> for &'a TableDescriptor {
    fn resolve_table(self, _snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor> {
        Ok(self)
    }
}

impl<'a> TableSelector<'a> for &'a str {
    fn resolve_table(self, snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor> {
        snapshot
            .table_by_name(self)
            .ok_or_else(|| HtapError::NotFound(format!("table '{}' not found", self)))
    }
}

impl<'a> TableSelector<'a> for &'a String {
    fn resolve_table(self, snapshot: &'a CatalogSnapshot) -> Result<&'a TableDescriptor> {
        self.as_str().resolve_table(snapshot)
    }
}

fn validate_manifest_path(path_str: &str) -> Result<()> {
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        return Err(HtapError::InvalidArgument(
            "column manifest path cannot be empty".into(),
        ));
    }
    if path_str.starts_with('/') || path_str.starts_with('\\') {
        return Err(HtapError::InvalidArgument(format!(
            "column manifest path '{path_str}' must be relative, not absolute"
        )));
    }
    let p = std::path::Path::new(path_str);
    if p.is_absolute() {
        return Err(HtapError::InvalidArgument(format!(
            "column manifest path '{path_str}' must be relative, not absolute"
        )));
    }
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                return Err(HtapError::InvalidArgument(format!(
                    "column manifest path '{path_str}' cannot contain parent directory traversal ('..')"
                )));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(HtapError::InvalidArgument(format!(
                    "column manifest path '{path_str}' must be relative, not absolute"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

impl CatalogSnapshot {
    /// Create a new empty snapshot at generation 0.
    pub fn empty() -> Self {
        Self {
            generation: 0,
            accounts: Vec::new(),
            grants: Vec::new(),
            accounts_initialized: false,
            tables: Vec::new(),
            partitions: Vec::new(),
            tablets: Vec::new(),
            replicas: Vec::new(),
            id_high_water: IdHighWater::default(),
        }
    }

    /// Create a new catalog snapshot with the provided storage elements.
    ///
    /// This constructor initializes the account model as empty and uninitialized. It is intended
    /// for initial catalog creation; successors of an existing snapshot must preserve its account
    /// state, for example with [`CatalogSnapshot::with_account_state_from`].
    pub fn new(
        generation: u64,
        tables: Vec<TableDescriptor>,
        partitions: Vec<PartitionDescriptor>,
        tablets: Vec<TabletDescriptor>,
        replicas: Vec<ReplicaDescriptor>,
    ) -> Self {
        Self {
            generation,
            accounts: Vec::new(),
            grants: Vec::new(),
            accounts_initialized: false,
            tables,
            partitions,
            tablets,
            replicas,
            id_high_water: IdHighWater::default(),
        }
    }

    /// Sets the persisted identifier high-water mark (see [`CatalogSnapshot::id_high_water`]).
    pub fn with_id_high_water(mut self, high_water: IdHighWater) -> Self {
        self.id_high_water = high_water;
        self
    }

    /// Copies account metadata from an existing catalog snapshot when deriving a successor.
    pub fn with_account_state_from(mut self, source: &CatalogSnapshot) -> Self {
        self.accounts = source.accounts.clone();
        self.grants = source.grants.clone();
        self.accounts_initialized = source.accounts_initialized;
        self
    }

    /// Effective identifier high-water mark: the maximum of the persisted counters and the
    /// highest id of every live table, partition, tablet, and replica. New identifiers must be
    /// allocated above this mark so that ids of dropped objects are never reused.
    pub fn id_high_water(&self) -> IdHighWater {
        let live = IdHighWater {
            account: self
                .accounts
                .iter()
                .map(|a| a.id.as_u64())
                .max()
                .unwrap_or(0),
            table: self.tables.iter().map(|t| t.id.as_u64()).max().unwrap_or(0),
            partition: self
                .partitions
                .iter()
                .map(|p| p.id.as_u64())
                .max()
                .unwrap_or(0),
            tablet: self
                .tablets
                .iter()
                .map(|t| t.id.as_u64())
                .max()
                .unwrap_or(0),
            replica: self
                .replicas
                .iter()
                .map(|r| r.id.as_u64())
                .max()
                .unwrap_or(0),
        };
        self.id_high_water.max(live)
    }

    /// Find an account by account ID.
    pub fn account_by_id(&self, id: AccountId) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id == id)
    }

    /// Find an account by username.
    pub fn account_by_username(&self, username: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|account| account.username == username)
    }

    /// Get all grants belonging to an account.
    pub fn grants_for(&self, account: AccountId) -> Vec<&Grant> {
        self.grants
            .iter()
            .filter(|grant| grant.account == account)
            .collect()
    }

    /// Returns the effective global and table-specific privileges for an account.
    pub fn effective_privileges(&self, account: AccountId, table: TableId) -> PrivilegeSet {
        self.grants
            .iter()
            .filter(|grant| {
                grant.account == account
                    && (matches!(grant.scope, PrivilegeScope::Global)
                        || matches!(grant.scope, PrivilegeScope::Table(id) if id == table))
            })
            .fold(PrivilegeSet::empty(), |privileges, grant| {
                privileges.union(grant.privileges)
            })
    }

    /// Returns true when an account has any global or table-specific privilege.
    pub fn has_any_privilege_on(&self, account: AccountId, table: TableId) -> bool {
        !self.effective_privileges(account, table).is_empty()
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

    /// Route a partition key value to the matching partition ID.
    ///
    /// The table can be specified as a [`TableId`] or as a `&TableDescriptor`.
    ///
    /// - For unpartitioned tables: routes to its sole partition (error if not exactly 1 partition).
    /// - For Range partitioning: routes to the partition where `lower <= value < upper`.
    /// - For List partitioning: routes to the partition containing `value`.
    /// - If unmatched: returns [`HtapError::InvalidArgument`].
    pub fn route_partition_value<'a>(
        &'a self,
        table: impl TableSelector<'a>,
        value: &Value,
    ) -> Result<PartitionId> {
        let table_desc = table.resolve_table(self)?;
        table_desc.route_partition_value(self, value)
    }

    /// Identifies the existing source partition descriptors affected by an alteration.
    ///
    /// Returns an empty vector for [`PartitionAlteration::Add`].
    /// Validates that the table is partitioned, that all named source partitions exist,
    /// and that there are no duplicate names in the source list.
    pub fn source_partitions_for_alteration<'a>(
        &'a self,
        table: impl TableSelector<'a>,
        alteration: &PartitionAlteration,
    ) -> Result<Vec<&'a PartitionDescriptor>> {
        let table_desc = table.resolve_table(self)?;
        if table_desc.partitioning.is_none() {
            return Err(HtapError::InvalidArgument(format!(
                "table '{}' is not partitioned",
                table_desc.name
            )));
        }

        let names = alteration.source_partition_names();
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = HashSet::with_capacity(names.len());
        let mut result = Vec::with_capacity(names.len());

        for name in names {
            if !seen.insert(name.as_str()) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate source partition name '{name}' for table '{}'",
                    table_desc.name
                )));
            }

            let part = table_desc
                .partitions
                .iter()
                .filter_map(|&pid| self.partition(pid))
                .find(|p| p.name == *name)
                .ok_or_else(|| {
                    HtapError::NotFound(format!(
                        "partition '{name}' not found in table '{}'",
                        table_desc.name
                    ))
                })?;
            result.push(part);
        }

        Ok(result)
    }

    /// Pure candidate snapshot transformation applying a [`PartitionAlteration`] to a table.
    ///
    /// Validates:
    /// - Table exists and is partitioned.
    /// - New partition definitions match the table's partitioning method.
    /// - Partition names are non-empty and unique within table.
    /// - Overlaps and duplicate list values are rejected.
    /// - Source partition list for Reorganize is contiguous in the table's partition sequence.
    /// - Alteration does not drop all partitions from the table.
    /// - IDs and generation are checked against overflow.
    /// - Each new partition preserves single-tablet bucket-0 / healthy node 1 topology.
    /// - Full candidate snapshot validation via [`CatalogSnapshot::validate`].
    pub fn apply_partition_alteration<'a>(
        &'a self,
        table: impl TableSelector<'a>,
        alteration: &PartitionAlteration,
    ) -> Result<CatalogSnapshot> {
        let table_desc = table.resolve_table(self)?;
        let partitioning = table_desc.partitioning.as_ref().ok_or_else(|| {
            HtapError::InvalidArgument(format!("table '{}' is not partitioned", table_desc.name))
        })?;

        if partitioning.key_column >= table_desc.schema.len() {
            return Err(HtapError::InvalidArgument(format!(
                "partition key column index {} out of bounds for table '{}' (schema length {})",
                partitioning.key_column,
                table_desc.name,
                table_desc.schema.len()
            )));
        }
        let key_col = &table_desc.schema.columns()[partitioning.key_column];
        let expected_type = key_col.data_type;

        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow {
                counter: "catalog_generation",
            })?;

        let mut high_water = self.id_high_water();

        let validate_part_def = |def: &PartitionDefinition,
                                 table_name: &str|
         -> Result<(String, Option<RangeBound>, Vec<Value>)> {
            let name = def.name().trim();
            if name.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "partition name cannot be empty in table '{table_name}'"
                )));
            }

            match (partitioning.method, def) {
                    (PartitioningMethod::Range, PartitionDefinition::Range(r)) => {
                        let bound = r.range_bound();
                        if let Some(l) = &bound.lower {
                            if l.is_null() || l.data_type() != Some(expected_type) {
                                return Err(HtapError::InvalidArgument(format!(
                                    "range partition '{}' in table '{}' lower bound has invalid type: expected {:?}, got {:?}",
                                    name, table_name, expected_type, l.data_type()
                                )));
                            }
                        }
                        if let Some(u) = &bound.upper {
                            if u.is_null() || u.data_type() != Some(expected_type) {
                                return Err(HtapError::InvalidArgument(format!(
                                    "range partition '{}' in table '{}' upper bound has invalid type: expected {:?}, got {:?}",
                                    name, table_name, expected_type, u.data_type()
                                )));
                            }
                        }
                        if let (Some(l), Some(u)) = (&bound.lower, &bound.upper) {
                            if l >= u {
                                return Err(HtapError::InvalidArgument(format!(
                                    "range partition '{}' in table '{}' has invalid bounds: lower ({}) must be strictly less than upper ({})",
                                    name, table_name, l, u
                                )));
                            }
                        }
                        Ok((name.to_string(), Some(bound), Vec::new()))
                    }
                    (PartitioningMethod::List, PartitionDefinition::List(l)) => {
                        if l.values.is_empty() {
                            return Err(HtapError::InvalidArgument(format!(
                                "list partition '{}' in table '{}' has empty list values",
                                name, table_name
                            )));
                        }
                        let mut seen_vals = HashSet::with_capacity(l.values.len());
                        for val in &l.values {
                            if val.is_null() || val.data_type() != Some(expected_type) {
                                return Err(HtapError::InvalidArgument(format!(
                                    "list partition value '{}' in partition '{}' (table '{}') has invalid type: expected {:?}, got {:?}",
                                    val, name, table_name, expected_type, val.data_type()
                                )));
                            }
                            if !seen_vals.insert(val) {
                                return Err(HtapError::InvalidArgument(format!(
                                    "partition '{}' in table '{}' contains duplicate list value '{}'",
                                    name, table_name, val
                                )));
                            }
                        }
                        Ok((name.to_string(), None, l.values.clone()))
                    }
                    (PartitioningMethod::Range, PartitionDefinition::List(_)) => {
                        Err(HtapError::InvalidArgument(format!(
                            "cannot apply list partition definition to range-partitioned table '{table_name}'"
                        )))
                    }
                    (PartitioningMethod::List, PartitionDefinition::Range(_)) => {
                        Err(HtapError::InvalidArgument(format!(
                            "cannot apply range partition definition to list-partitioned table '{table_name}'"
                        )))
                    }
                }
        };

        match alteration {
            PartitionAlteration::Add { partitions } => {
                if partitions.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "add partition requires at least one partition definition for table '{}'",
                        table_desc.name
                    )));
                }

                let mut seen_names: HashSet<&str> = table_desc
                    .partitions
                    .iter()
                    .filter_map(|&pid| self.partition(pid))
                    .map(|p| p.name.as_str())
                    .collect();

                let mut validated_items = Vec::with_capacity(partitions.len());
                for def in partitions {
                    let (name, range_opt, list_vals) = validate_part_def(def, &table_desc.name)?;
                    if !seen_names.insert(def.name()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate partition name '{}' in table '{}'",
                            def.name(),
                            table_desc.name
                        )));
                    }
                    validated_items.push((name, range_opt, list_vals));
                }

                let mut new_partitions = Vec::with_capacity(validated_items.len());
                let mut new_tablets = Vec::with_capacity(validated_items.len());
                let mut new_replicas = Vec::with_capacity(validated_items.len());
                let mut new_partition_ids = Vec::with_capacity(validated_items.len());

                for (part_name, range_opt, list_values) in validated_items {
                    let partition_id = high_water.allocate_partition()?;
                    let tablet_id = high_water.allocate_tablet()?;
                    let replica_id = high_water.allocate_replica()?;

                    let replica_desc = ReplicaDescriptor::new(
                        replica_id,
                        tablet_id,
                        NodeId::new(1),
                        true,
                        true,
                        next_generation,
                    );
                    let tablet_desc = TabletDescriptor::new(
                        tablet_id,
                        partition_id,
                        0,
                        vec![replica_id],
                        next_generation,
                    );
                    let mut partition_desc = PartitionDescriptor::new(
                        partition_id,
                        table_desc.id,
                        part_name,
                        StorageDescriptor::Row,
                        vec![tablet_id],
                        next_generation,
                    );
                    if let Some(range) = range_opt {
                        partition_desc = partition_desc.with_range(range);
                    }
                    if !list_values.is_empty() {
                        partition_desc = partition_desc.with_list_values(list_values);
                    }

                    new_partition_ids.push(partition_id);
                    new_partitions.push(partition_desc);
                    new_tablets.push(tablet_desc);
                    new_replicas.push(replica_desc);
                }

                let mut updated_table = table_desc.clone();
                updated_table.partitions.extend(new_partition_ids);
                updated_table.generation = next_generation;

                let tables: Vec<TableDescriptor> = self
                    .tables
                    .iter()
                    .map(|t| {
                        if t.id == updated_table.id {
                            updated_table.clone()
                        } else {
                            t.clone()
                        }
                    })
                    .collect();

                let mut partitions = self.partitions.clone();
                partitions.extend(new_partitions);

                let mut tablets = self.tablets.clone();
                tablets.extend(new_tablets);

                let mut replicas = self.replicas.clone();
                replicas.extend(new_replicas);

                let candidate =
                    CatalogSnapshot::new(next_generation, tables, partitions, tablets, replicas)
                        .with_id_high_water(high_water)
                        .with_account_state_from(self);
                candidate.validate()?;
                Ok(candidate)
            }
            PartitionAlteration::Drop { partitions } => {
                if partitions.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "drop partition requires at least one partition name for table '{}'",
                        table_desc.name
                    )));
                }

                let mut seen_drop = HashSet::with_capacity(partitions.len());
                let mut drop_pids = Vec::with_capacity(partitions.len());

                for name in partitions {
                    if !seen_drop.insert(name.as_str()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate partition name '{name}' in drop list for table '{}'",
                            table_desc.name
                        )));
                    }
                    let pid = table_desc
                        .partitions
                        .iter()
                        .copied()
                        .find(|&pid| {
                            self.partition(pid)
                                .map(|p| p.name == *name)
                                .unwrap_or(false)
                        })
                        .ok_or_else(|| {
                            HtapError::NotFound(format!(
                                "partition '{name}' not found in table '{}'",
                                table_desc.name
                            ))
                        })?;
                    drop_pids.push(pid);
                }

                if drop_pids.len() >= table_desc.partitions.len() {
                    return Err(HtapError::InvalidArgument(format!(
                        "cannot drop all partitions of table '{}'",
                        table_desc.name
                    )));
                }

                let drop_pid_set: HashSet<PartitionId> = drop_pids.into_iter().collect();

                let mut dropped_tablet_ids = HashSet::new();
                let mut dropped_replica_ids = HashSet::new();
                for pid in &drop_pid_set {
                    if let Some(part) = self.partition(*pid) {
                        for tid in &part.tablets {
                            dropped_tablet_ids.insert(*tid);
                            if let Some(tab) = self.tablet(*tid) {
                                for rid in &tab.replicas {
                                    dropped_replica_ids.insert(*rid);
                                }
                            }
                        }
                    }
                }

                let mut updated_table = table_desc.clone();
                updated_table
                    .partitions
                    .retain(|pid| !drop_pid_set.contains(pid));
                updated_table.generation = next_generation;

                let tables: Vec<TableDescriptor> = self
                    .tables
                    .iter()
                    .map(|t| {
                        if t.id == updated_table.id {
                            updated_table.clone()
                        } else {
                            t.clone()
                        }
                    })
                    .collect();

                let remaining_partitions: Vec<PartitionDescriptor> = self
                    .partitions
                    .iter()
                    .filter(|p| !drop_pid_set.contains(&p.id))
                    .cloned()
                    .collect();

                let remaining_tablets: Vec<TabletDescriptor> = self
                    .tablets
                    .iter()
                    .filter(|t| !dropped_tablet_ids.contains(&t.id))
                    .cloned()
                    .collect();

                let remaining_replicas: Vec<ReplicaDescriptor> = self
                    .replicas
                    .iter()
                    .filter(|r| !dropped_replica_ids.contains(&r.id))
                    .cloned()
                    .collect();

                let candidate = CatalogSnapshot::new(
                    next_generation,
                    tables,
                    remaining_partitions,
                    remaining_tablets,
                    remaining_replicas,
                )
                .with_id_high_water(high_water)
                .with_account_state_from(self);
                candidate.validate()?;
                Ok(candidate)
            }
            PartitionAlteration::Reorganize { sources, targets } => {
                if sources.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "reorganize partition requires at least one source partition for table '{}'",
                        table_desc.name
                    )));
                }
                if targets.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "reorganize partition requires at least one target partition for table '{}'",
                        table_desc.name
                    )));
                }

                let mut seen_source_names = HashSet::with_capacity(sources.len());
                let mut source_indices = Vec::with_capacity(sources.len());
                let mut source_pids = HashSet::with_capacity(sources.len());

                for name in sources {
                    if !seen_source_names.insert(name.as_str()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate source partition name '{name}' in reorganize for table '{}'",
                            table_desc.name
                        )));
                    }
                    let pos = table_desc
                        .partitions
                        .iter()
                        .position(|&pid| {
                            self.partition(pid)
                                .map(|p| p.name == *name)
                                .unwrap_or(false)
                        })
                        .ok_or_else(|| {
                            HtapError::NotFound(format!(
                                "source partition '{name}' not found in table '{}'",
                                table_desc.name
                            ))
                        })?;
                    source_indices.push(pos);
                    source_pids.insert(table_desc.partitions[pos]);
                }

                source_indices.sort_unstable();
                let min_idx = source_indices[0];
                let max_idx = *source_indices.last().unwrap();
                if max_idx - min_idx + 1 != source_indices.len() {
                    return Err(HtapError::InvalidArgument(format!(
                        "reorganize partition sources must be contiguous in table '{}', got non-contiguous positions {:?}",
                        table_desc.name, source_indices
                    )));
                }

                // Names of remaining partitions (partitions not in sources)
                let remaining_names: HashSet<&str> = table_desc
                    .partitions
                    .iter()
                    .filter(|pid| !source_pids.contains(pid))
                    .filter_map(|&pid| self.partition(pid))
                    .map(|p| p.name.as_str())
                    .collect();

                let mut seen_target_names = HashSet::with_capacity(targets.len());
                let mut validated_targets = Vec::with_capacity(targets.len());

                for def in targets {
                    let (name, range_opt, list_vals) = validate_part_def(def, &table_desc.name)?;
                    if !seen_target_names.insert(def.name()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate target partition name '{}' in reorganize for table '{}'",
                            def.name(),
                            table_desc.name
                        )));
                    }
                    if remaining_names.contains(def.name()) {
                        return Err(HtapError::InvalidArgument(format!(
                            "target partition name '{}' already exists in table '{}'",
                            def.name(),
                            table_desc.name
                        )));
                    }
                    validated_targets.push((name, range_opt, list_vals));
                }

                let mut new_partitions = Vec::with_capacity(validated_targets.len());
                let mut new_tablets = Vec::with_capacity(validated_targets.len());
                let mut new_replicas = Vec::with_capacity(validated_targets.len());
                let mut target_partition_ids = Vec::with_capacity(validated_targets.len());

                for (part_name, range_opt, list_values) in validated_targets {
                    let partition_id = high_water.allocate_partition()?;
                    let tablet_id = high_water.allocate_tablet()?;
                    let replica_id = high_water.allocate_replica()?;

                    let replica_desc = ReplicaDescriptor::new(
                        replica_id,
                        tablet_id,
                        NodeId::new(1),
                        true,
                        true,
                        next_generation,
                    );
                    let tablet_desc = TabletDescriptor::new(
                        tablet_id,
                        partition_id,
                        0,
                        vec![replica_id],
                        next_generation,
                    );
                    let mut partition_desc = PartitionDescriptor::new(
                        partition_id,
                        table_desc.id,
                        part_name,
                        StorageDescriptor::Row,
                        vec![tablet_id],
                        next_generation,
                    );
                    if let Some(range) = range_opt {
                        partition_desc = partition_desc.with_range(range);
                    }
                    if !list_values.is_empty() {
                        partition_desc = partition_desc.with_list_values(list_values);
                    }

                    target_partition_ids.push(partition_id);
                    new_partitions.push(partition_desc);
                    new_tablets.push(tablet_desc);
                    new_replicas.push(replica_desc);
                }

                let mut updated_partitions = Vec::with_capacity(
                    table_desc.partitions.len() - source_indices.len() + target_partition_ids.len(),
                );
                updated_partitions.extend_from_slice(&table_desc.partitions[..min_idx]);
                updated_partitions.extend(target_partition_ids);
                updated_partitions.extend_from_slice(&table_desc.partitions[(max_idx + 1)..]);

                let mut updated_table = table_desc.clone();
                updated_table.partitions = updated_partitions;
                updated_table.generation = next_generation;

                let mut dropped_tablet_ids = HashSet::new();
                let mut dropped_replica_ids = HashSet::new();
                for pid in &source_pids {
                    if let Some(part) = self.partition(*pid) {
                        for tid in &part.tablets {
                            dropped_tablet_ids.insert(*tid);
                            if let Some(tab) = self.tablet(*tid) {
                                for rid in &tab.replicas {
                                    dropped_replica_ids.insert(*rid);
                                }
                            }
                        }
                    }
                }

                let tables: Vec<TableDescriptor> = self
                    .tables
                    .iter()
                    .map(|t| {
                        if t.id == updated_table.id {
                            updated_table.clone()
                        } else {
                            t.clone()
                        }
                    })
                    .collect();

                let mut partitions: Vec<PartitionDescriptor> = self
                    .partitions
                    .iter()
                    .filter(|p| !source_pids.contains(&p.id))
                    .cloned()
                    .collect();
                partitions.extend(new_partitions);

                let mut tablets: Vec<TabletDescriptor> = self
                    .tablets
                    .iter()
                    .filter(|t| !dropped_tablet_ids.contains(&t.id))
                    .cloned()
                    .collect();
                tablets.extend(new_tablets);

                let mut replicas: Vec<ReplicaDescriptor> = self
                    .replicas
                    .iter()
                    .filter(|r| !dropped_replica_ids.contains(&r.id))
                    .cloned()
                    .collect();
                replicas.extend(new_replicas);

                let candidate =
                    CatalogSnapshot::new(next_generation, tables, partitions, tablets, replicas)
                        .with_id_high_water(high_water)
                        .with_account_state_from(self);
                candidate.validate()?;
                Ok(candidate)
            }
        }
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
        let mut seen_account_ids = HashSet::with_capacity(self.accounts.len());
        let mut seen_usernames = HashSet::with_capacity(self.accounts.len());

        for account in &self.accounts {
            if !seen_account_ids.insert(account.id) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate account id: {}",
                    account.id
                )));
            }
            if !seen_usernames.insert(account.username.as_str()) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate account username: '{}'",
                    account.username
                )));
            }
            if account.id.as_u64() > self.id_high_water.account {
                return Err(HtapError::InvalidArgument(format!(
                    "account id {} exceeds account id high-water mark {}",
                    account.id, self.id_high_water.account
                )));
            }
        }

        let mut seen_grants = HashSet::with_capacity(self.grants.len());
        for grant in &self.grants {
            if !seen_account_ids.contains(&grant.account) {
                return Err(HtapError::InvalidArgument(format!(
                    "grant references nonexistent account {}",
                    grant.account
                )));
            }
            if let PrivilegeScope::Table(table_id) = grant.scope {
                if self.table(table_id).is_none() {
                    return Err(HtapError::InvalidArgument(format!(
                        "grant references nonexistent table {}",
                        table_id
                    )));
                }
            }
            if grant.privileges.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "grant for account {} has an empty privilege set",
                    grant.account
                )));
            }
            if !seen_grants.insert((grant.account, &grant.scope)) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate grant for account {} and scope {:?}",
                    grant.account, grant.scope
                )));
            }
        }

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

            if let Some(stats) = &table.stats {
                if stats.columns.len() != schema_len {
                    return Err(HtapError::InvalidArgument(format!(
                        "table '{}' statistics column count {} does not match schema column count {}",
                        table.name,
                        stats.columns.len(),
                        schema_len
                    )));
                }

                for (column_index, (column, column_stats)) in table
                    .schema
                    .columns()
                    .iter()
                    .zip(&stats.columns)
                    .enumerate()
                {
                    if column_stats.null_count > stats.row_count {
                        return Err(HtapError::InvalidArgument(format!(
                            "table '{}' statistics for column '{}' (index {}) has null count {} exceeding row count {}",
                            table.name,
                            column.name,
                            column_index,
                            column_stats.null_count,
                            stats.row_count
                        )));
                    }

                    let non_null_count = stats.row_count - column_stats.null_count;
                    if let Some(distinct_count) = column_stats.distinct_count {
                        if distinct_count > non_null_count {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has distinct count {} exceeding non-null row count {}",
                                table.name,
                                column.name,
                                column_index,
                                distinct_count,
                                non_null_count
                            )));
                        }
                    }

                    if let Some(min) = &column_stats.min {
                        if min.data_type() != Some(column.data_type) {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has min value with type {:?}, expected {:?}",
                                table.name,
                                column.name,
                                column_index,
                                min.data_type(),
                                column.data_type
                            )));
                        }
                        if matches!(min, Value::Float64(value) if !value.is_finite()) {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has non-finite min bound",
                                table.name, column.name, column_index
                            )));
                        }
                    }

                    if let Some(max) = &column_stats.max {
                        if max.data_type() != Some(column.data_type) {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has max value with type {:?}, expected {:?}",
                                table.name,
                                column.name,
                                column_index,
                                max.data_type(),
                                column.data_type
                            )));
                        }
                        if matches!(max, Value::Float64(value) if !value.is_finite()) {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has non-finite max bound",
                                table.name, column.name, column_index
                            )));
                        }
                    }

                    if let (Some(min), Some(max)) = (&column_stats.min, &column_stats.max) {
                        if min > max {
                            return Err(HtapError::InvalidArgument(format!(
                                "table '{}' statistics for column '{}' (index {}) has min value {} greater than max value {}",
                                table.name,
                                column.name,
                                column_index,
                                min,
                                max
                            )));
                        }
                    }
                }
            }

            if let Some(partitioning) = &table.partitioning {
                if partitioning.key_column >= schema_len {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key index {} out of bounds for table '{}' (schema length {})",
                        partitioning.key_column, table.name, schema_len
                    )));
                }

                let key_col = &table.schema.columns()[partitioning.key_column];
                if !table.primary_key.contains(&partitioning.key_column) {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key column '{}' (index {}) in table '{}' must be part of primary key",
                        key_col.name, partitioning.key_column, table.name
                    )));
                }

                if key_col.nullable {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition key column '{}' (index {}) in table '{}' cannot be nullable",
                        key_col.name, partitioning.key_column, table.name
                    )));
                }

                if table.partitions.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "partitioned table '{}' (id {}) must have at least one partition",
                        table.name, table.id
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

            if part.range.is_some() && !part.list_values.is_empty() {
                return Err(HtapError::InvalidArgument(format!(
                    "partition '{}' (id {}) defines both range bound and list values",
                    part.name, part.id
                )));
            }

            let parent_table = self.table(part.table_id).ok_or_else(|| {
                HtapError::InvalidArgument(format!(
                    "partition {} references nonexistent table {}",
                    part.id, part.table_id
                ))
            })?;

            match &parent_table.partitioning {
                None => {
                    if part.range.is_some() {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{}' (id {}) has range bound but parent table '{}' is unpartitioned",
                            part.name, part.id, parent_table.name
                        )));
                    }
                    if !part.list_values.is_empty() {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition '{}' (id {}) has list values but parent table '{}' is unpartitioned",
                            part.name, part.id, parent_table.name
                        )));
                    }
                }
                Some(partitioning) => {
                    let key_col = &parent_table.schema.columns()[partitioning.key_column];
                    let expected_type = key_col.data_type;

                    match partitioning.method {
                        PartitioningMethod::Range => {
                            if !part.list_values.is_empty() {
                                return Err(HtapError::InvalidArgument(format!(
                                    "partition '{}' (id {}) has list values but parent table '{}' uses range partitioning",
                                    part.name, part.id, parent_table.name
                                )));
                            }
                            let range = part.range.as_ref().ok_or_else(|| {
                                HtapError::InvalidArgument(format!(
                                    "partition '{}' (id {}) in range-partitioned table '{}' is missing range bound",
                                    part.name, part.id, parent_table.name
                                ))
                            })?;

                            if let Some(l) = &range.lower {
                                if l.is_null() || l.data_type() != Some(expected_type) {
                                    return Err(HtapError::InvalidArgument(format!(
                                        "range partition '{}' (id {}) lower bound has invalid type: expected {:?}, got {:?}",
                                        part.name,
                                        part.id,
                                        expected_type,
                                        l.data_type()
                                    )));
                                }
                            }
                            if let Some(u) = &range.upper {
                                if u.is_null() || u.data_type() != Some(expected_type) {
                                    return Err(HtapError::InvalidArgument(format!(
                                        "range partition '{}' (id {}) upper bound has invalid type: expected {:?}, got {:?}",
                                        part.name,
                                        part.id,
                                        expected_type,
                                        u.data_type()
                                    )));
                                }
                            }
                            if let (Some(l), Some(u)) = (&range.lower, &range.upper) {
                                if l >= u {
                                    return Err(HtapError::InvalidArgument(format!(
                                        "range partition '{}' (id {}) in table '{}' has invalid bounds: lower ({}) must be strictly less than upper ({})",
                                        part.name, part.id, parent_table.name, l, u
                                    )));
                                }
                            }
                        }
                        PartitioningMethod::List => {
                            if part.range.is_some() {
                                return Err(HtapError::InvalidArgument(format!(
                                    "partition '{}' (id {}) has range bound but parent table '{}' uses list partitioning",
                                    part.name, part.id, parent_table.name
                                )));
                            }
                            if part.list_values.is_empty() {
                                return Err(HtapError::InvalidArgument(format!(
                                    "partition '{}' (id {}) in list-partitioned table '{}' has empty list values",
                                    part.name, part.id, parent_table.name
                                )));
                            }

                            let mut seen_part_values =
                                HashSet::with_capacity(part.list_values.len());
                            for val in &part.list_values {
                                if val.is_null() || val.data_type() != Some(expected_type) {
                                    return Err(HtapError::InvalidArgument(format!(
                                        "list partition value '{}' in partition '{}' (id {}) has invalid type: expected {:?}, got {:?}",
                                        val,
                                        part.name,
                                        part.id,
                                        expected_type,
                                        val.data_type()
                                    )));
                                }
                                if !seen_part_values.insert(val) {
                                    return Err(HtapError::InvalidArgument(format!(
                                        "partition '{}' (id {}) contains duplicate list value '{}'",
                                        part.name, part.id, val
                                    )));
                                }
                            }
                        }
                    }
                }
            }

            // Storage and conversion descriptor validation
            if let Some(conv) = &part.conversion {
                if conv.generation == 0 {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition id {} conversion generation must be > 0",
                        part.id
                    )));
                }
                if conv.from == conv.to {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition id {} conversion 'from' and 'to' formats cannot be identical ({:?})",
                        part.id, conv.from
                    )));
                }
                if conv.snapshot_version.get() == 0 {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition id {} conversion snapshot version must be > 0",
                        part.id
                    )));
                }
                if conv.to == StorageFormat::Row
                    && matches!(conv.phase, ConversionPhase::SegmentsWritten)
                {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition id {} conversion phase SegmentsWritten is invalid when converting to Row",
                        part.id
                    )));
                }
            }

            match &part.storage {
                StorageDescriptor::Converting {
                    from,
                    to,
                    generation,
                } => {
                    if *generation == 0 {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition id {} Converting storage generation must be > 0",
                            part.id
                        )));
                    }
                    if from == to {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition id {} Converting storage from and to must differ",
                            part.id
                        )));
                    }
                    if let Some(conv) = &part.conversion {
                        if conv.generation != *generation {
                            return Err(HtapError::InvalidArgument(format!(
                                "partition id {} storage generation {} does not match conversion generation {}",
                                part.id, generation, conv.generation
                            )));
                        }
                        if conv.from != *from || conv.to != *to {
                            return Err(HtapError::InvalidArgument(format!(
                                "partition id {} storage direction ({:?} -> {:?}) does not match conversion direction ({:?} -> {:?})",
                                part.id, from, to, conv.from, conv.to
                            )));
                        }
                    }
                }
                StorageDescriptor::Row | StorageDescriptor::Column => {
                    if part.conversion.is_some() {
                        return Err(HtapError::InvalidArgument(format!(
                            "partition id {} has storage format {:?} but contains conversion metadata",
                            part.id, part.storage
                        )));
                    }
                }
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

            if let Some(partitioning) = &table.partitioning {
                match partitioning.method {
                    PartitioningMethod::Range => {
                        let mut ranges = Vec::with_capacity(table.partitions.len());
                        for &part_id in &table.partitions {
                            if let Some(part) = self.partition(part_id) {
                                if let Some(range) = &part.range {
                                    ranges.push((
                                        part.id,
                                        &part.name,
                                        range.lower.as_ref(),
                                        range.upper.as_ref(),
                                    ));
                                }
                            }
                        }
                        for i in 0..ranges.len() {
                            for j in (i + 1)..ranges.len() {
                                let (id1, name1, l1, u1) = ranges[i];
                                let (id2, name2, l2, u2) = ranges[j];
                                // Check overlap between [l1, u1) and [l2, u2)
                                // They overlap iff max(l1, l2) < min(u1, u2)
                                let overlaps = match (l1, l2, u1, u2) {
                                    // If l1 >= u2, they don't overlap
                                    (Some(l1_val), _, _, Some(u2_val)) if l1_val >= u2_val => false,
                                    // If l2 >= u1, they don't overlap
                                    (_, Some(l2_val), Some(u1_val), _) if l2_val >= u1_val => false,
                                    // Otherwise, they overlap!
                                    _ => true,
                                };
                                if overlaps {
                                    let l1_str =
                                        l1.map(|v| v.to_string()).unwrap_or_else(|| "-inf".into());
                                    let u1_str = u1
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "MAXVALUE".into());
                                    let l2_str =
                                        l2.map(|v| v.to_string()).unwrap_or_else(|| "-inf".into());
                                    let u2_str = u2
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "MAXVALUE".into());
                                    return Err(HtapError::InvalidArgument(format!(
                                        "table '{}' has overlapping range partitions: partition '{}' (id {}) [{}, {}) overlaps with partition '{}' (id {}) [{}, {})",
                                        table.name, name1, id1, l1_str, u1_str, name2, id2, l2_str, u2_str
                                    )));
                                }
                            }
                        }
                    }
                    PartitioningMethod::List => {
                        let mut seen_table_list_values = HashSet::new();
                        for &part_id in &table.partitions {
                            if let Some(part) = self.partition(part_id) {
                                for val in &part.list_values {
                                    if !seen_table_list_values.insert(val) {
                                        return Err(HtapError::InvalidArgument(format!(
                                            "table '{}' has duplicate list value '{}' across partitions (found in partition '{}' id {})",
                                            table.name, val, part.name, part.id
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }
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

        // 3. Validate tablets: unique IDs, FK to partition, column manifest validation.
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

            // Column manifest reference validation
            if let Some(manifest) = &tablet.column_manifest {
                if manifest.generation == 0 {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} column manifest generation must be > 0",
                        tablet.id
                    )));
                }
                if manifest.base_version.get() == 0 {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} column manifest base_version must be > 0",
                        tablet.id
                    )));
                }
                validate_manifest_path(&manifest.path)?;
                if manifest.segment_count == 0 && manifest.row_count > 0 {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} column manifest row_count {} > 0 but segment_count is 0",
                        tablet.id, manifest.row_count
                    )));
                }
                if manifest.segment_count > MAX_MANIFEST_SEGMENTS {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} column manifest segment_count {} exceeds maximum allowed {}",
                        tablet.id, manifest.segment_count, MAX_MANIFEST_SEGMENTS
                    )));
                }
                if manifest.row_count > MAX_MANIFEST_ROWS {
                    return Err(HtapError::InvalidArgument(format!(
                        "tablet id {} column manifest row_count {} exceeds maximum allowed {}",
                        tablet.id, manifest.row_count, MAX_MANIFEST_ROWS
                    )));
                }

                let parent_part = self.partition(tablet.partition_id).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "tablet {} references nonexistent partition {}",
                        tablet.id, tablet.partition_id
                    ))
                })?;

                match &parent_part.storage {
                    StorageDescriptor::Column => {
                        // Valid columnar state.
                    }
                    StorageDescriptor::Converting { to, generation, .. }
                        if *to == StorageFormat::Column =>
                    {
                        // Valid Converting-to-Column state.
                        if let Some(conv) = &parent_part.conversion {
                            if manifest.generation != conv.generation {
                                return Err(HtapError::InvalidArgument(format!(
                                    "tablet id {} column manifest generation {} does not match partition conversion generation {}",
                                    tablet.id, manifest.generation, conv.generation
                                )));
                            }
                            if matches!(conv.phase, ConversionPhase::SnapshotPinned) {
                                return Err(HtapError::InvalidArgument(format!(
                                    "tablet id {} column manifest cannot be present while partition is in SnapshotPinned phase",
                                    tablet.id
                                )));
                            }
                        } else if manifest.generation != *generation {
                            return Err(HtapError::InvalidArgument(format!(
                                "tablet id {} column manifest generation {} does not match partition converting generation {}",
                                tablet.id, manifest.generation, generation
                            )));
                        }
                    }
                    StorageDescriptor::Row => {
                        return Err(HtapError::InvalidArgument(format!(
                            "tablet id {} has column manifest but parent partition {} has Row storage",
                            tablet.id, parent_part.id
                        )));
                    }
                    StorageDescriptor::Converting { to, .. } => {
                        return Err(HtapError::InvalidArgument(format!(
                            "tablet id {} has column manifest but parent partition {} is converting to {:?}",
                            tablet.id, parent_part.id, to
                        )));
                    }
                }
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

    #[test]
    fn test_table_descriptor_stats_serde_round_trip() {
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

        let json_without_stats = serde_json::to_string(&table).unwrap();
        assert!(!json_without_stats.contains("\"stats\""));
        let decoded_without_stats: TableDescriptor =
            serde_json::from_str(&json_without_stats).unwrap();
        assert_eq!(decoded_without_stats, table);
        assert_eq!(decoded_without_stats.stats, None);

        let stats = TableStats {
            analyzed_at_version: 42,
            row_count: 100,
            columns: vec![ColumnStats {
                null_count: 3,
                distinct_count: Some(97),
                min: Some(Value::Int64(1)),
                max: Some(Value::Int64(100)),
            }],
        };
        let table_with_stats = table.with_stats(stats);

        let json_with_stats = serde_json::to_string(&table_with_stats).unwrap();
        assert!(json_with_stats.contains("\"stats\""));
        let decoded_with_stats: TableDescriptor = serde_json::from_str(&json_with_stats).unwrap();
        assert_eq!(decoded_with_stats, table_with_stats);
    }

    #[test]
    fn test_privilege_set_operations_and_display() {
        let read_write = PrivilegeSet::SELECT.union(PrivilegeSet::UPDATE);
        assert!(read_write.contains(PrivilegeSet::SELECT));
        assert!(read_write.contains(PrivilegeSet::UPDATE));
        assert!(!read_write.contains(PrivilegeSet::DELETE));
        assert_eq!(
            read_write.difference(PrivilegeSet::SELECT),
            PrivilegeSet::UPDATE
        );
        assert!(PrivilegeSet::empty().is_empty());
        assert_eq!(read_write.to_string(), "SELECT,UPDATE");
        assert!(PrivilegeSet::ALL.contains(read_write));
    }

    #[test]
    fn test_allocate_account_and_account_debug_redaction() {
        let mut high_water = IdHighWater::default();
        assert_eq!(high_water.allocate_account().unwrap(), AccountId::new(1));

        let account = Account {
            id: AccountId::new(1),
            username: "admin".into(),
            password_hash: Some([42; 20]),
            locked: false,
            is_superuser: true,
        };
        let debug = format!("{account:?}");
        assert!(debug.contains("password_hash: \"<redacted>\""));
        assert!(!debug.contains("42"));
    }

    #[test]
    fn test_account_and_grant_validation_failures() {
        let valid = || {
            CatalogSnapshot::empty().with_id_high_water(IdHighWater {
                account: 1,
                ..IdHighWater::default()
            })
        };

        let mut snapshot = valid();
        snapshot.accounts = vec![
            Account {
                id: AccountId::new(1),
                username: "one".into(),
                password_hash: None,
                locked: false,
                is_superuser: false,
            },
            Account {
                id: AccountId::new(1),
                username: "two".into(),
                password_hash: None,
                locked: false,
                is_superuser: false,
            },
        ];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate account id"));

        let mut snapshot = valid();
        snapshot.accounts = vec![
            Account {
                id: AccountId::new(1),
                username: "same".into(),
                password_hash: None,
                locked: false,
                is_superuser: false,
            },
            Account {
                id: AccountId::new(0),
                username: "same".into(),
                password_hash: None,
                locked: false,
                is_superuser: false,
            },
        ];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate account username"));

        let mut snapshot = CatalogSnapshot::empty();
        snapshot.accounts = vec![Account {
            id: AccountId::new(1),
            username: "one".into(),
            password_hash: None,
            locked: false,
            is_superuser: false,
        }];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exceeds account id high-water"));

        let mut snapshot = valid();
        snapshot.grants = vec![Grant {
            account: AccountId::new(1),
            scope: PrivilegeScope::Global,
            privileges: PrivilegeSet::SELECT,
        }];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("nonexistent account"));

        let mut snapshot = valid();
        snapshot.accounts = vec![Account {
            id: AccountId::new(1),
            username: "one".into(),
            password_hash: None,
            locked: false,
            is_superuser: false,
        }];
        snapshot.grants = vec![Grant {
            account: AccountId::new(1),
            scope: PrivilegeScope::Table(TableId::new(1)),
            privileges: PrivilegeSet::SELECT,
        }];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("nonexistent table"));

        snapshot.grants = vec![Grant {
            account: AccountId::new(1),
            scope: PrivilegeScope::Global,
            privileges: PrivilegeSet::empty(),
        }];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("empty privilege set"));

        snapshot.grants = vec![
            Grant {
                account: AccountId::new(1),
                scope: PrivilegeScope::Global,
                privileges: PrivilegeSet::SELECT,
            },
            Grant {
                account: AccountId::new(1),
                scope: PrivilegeScope::Global,
                privileges: PrivilegeSet::INSERT,
            },
        ];
        assert!(snapshot
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate grant"));
    }

    #[test]
    fn test_conversion_and_manifest_descriptors() {
        let manifest =
            ColumnManifestRef::new(1, "segments/manifest_0.json", Version::new(10), 4, 100_000);
        let conv = ConversionDescriptor::new(
            1,
            StorageFormat::Row,
            StorageFormat::Column,
            Version::new(10),
            ConversionPhase::SegmentsWritten,
        );

        let json_m = serde_json::to_string(&manifest).unwrap();
        let dec_m: ColumnManifestRef = serde_json::from_str(&json_m).unwrap();
        assert_eq!(dec_m, manifest);

        let json_c = serde_json::to_string(&conv).unwrap();
        let dec_c: ConversionDescriptor = serde_json::from_str(&json_c).unwrap();
        assert_eq!(dec_c, conv);
    }

    #[test]
    fn test_partitioning_descriptors() {
        let p_desc = PartitioningDescriptor::new(1, PartitioningMethod::Range);
        let json_p = serde_json::to_string(&p_desc).unwrap();
        let dec_p: PartitioningDescriptor = serde_json::from_str(&json_p).unwrap();
        assert_eq!(dec_p, p_desc);

        let bound = RangeBound::new(Value::Int64(10), Value::Int64(100));
        let json_b = serde_json::to_string(&bound).unwrap();
        let dec_b: RangeBound = serde_json::from_str(&json_b).unwrap();
        assert_eq!(dec_b, bound);

        // Test backward-compatible JSON decode where lower and upper were direct values
        let legacy_json = r#"{"lower":{"Int64":10},"upper":{"Int64":100}}"#;
        let dec_legacy: RangeBound = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(dec_legacy, bound);

        // Test unbounded upper (MAXVALUE)
        let maxvalue_bound = RangeBound::new_opt(Some(Value::Int64(100)), None);
        let json_mv = serde_json::to_string(&maxvalue_bound).unwrap();
        let dec_mv: RangeBound = serde_json::from_str(&json_mv).unwrap();
        assert_eq!(maxvalue_bound, dec_mv);

        let list_method = PartitioningMethod::List;
        let json_lm = serde_json::to_string(&list_method).unwrap();
        let dec_lm: PartitioningMethod = serde_json::from_str(&json_lm).unwrap();
        assert_eq!(dec_lm, list_method);
    }
}
