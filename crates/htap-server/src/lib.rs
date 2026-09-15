//! Synchronous local single-node HTAP database server.
//!
//! Provides [`LocalServer`], an embedded synchronous server coordinating SQL
//! parsing, semantic binding, catalog topology updates, transaction management,
//! LSM rowstore storage, and data movement operations via [`LocalServerDataMover`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod olap;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
pub use htap_catalog::{
    CatalogSnapshot, ColumnManifestRef, ConversionDescriptor, ConversionPhase,
    ListPartitionDefinition, NodeId, PartitionAlteration, PartitionDefinition, PartitionDescriptor,
    PartitionId, PartitioningDescriptor, PartitioningMethod, RangeBound, RangePartitionDefinition,
    ReplicaDescriptor, ReplicaId, StorageDescriptor, StorageFormat, TableDescriptor, TableId,
    TabletDescriptor, TabletId,
};
use htap_common::encode_key;
use htap_common::error::{HtapError, Result};
use htap_common::lock::ProcessLock;
use htap_common::types::{ColumnDef, Mutation, Row, Schema, Value};
pub use htap_convert::{
    ConversionAction, ConversionErrorCategory, ConversionPolicy, ConversionTarget,
    ConversionTickReport, PartitionConversionReport, Predicate, SegmentOptions,
    TableConversionReport,
};
use htap_movement::{
    CopyOptions, CopyReport, LocalDataMover, MovementJob, TabletCloneOptions, TabletPackageManifest,
};
use htap_rowstore::{Engine, EngineOptions, Snapshot};
use htap_sql::ast::{
    AnalyticSelect, BoundPartitioning, BoundStatement, CreateTable, DeleteByPrimaryKey, Insert,
    PointSelect,
};
use htap_sql::result::StatementResult;
use htap_sql::route::{classify_route, Route};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};
use parking_lot::Mutex;

/// Definition of a partitioned table to be created via [`LocalServer::create_partitioned_table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionedTableDefinition {
    /// Logical name of the partitioned table.
    pub name: String,
    /// Column definitions and schema of the table.
    pub schema: Schema,
    /// Schema column indices constituting the primary key.
    pub primary_key: Vec<usize>,
    /// Partition topology specification (Range or List).
    pub topology: PartitionTopology,
}

impl PartitionedTableDefinition {
    /// Creates a new partitioned table definition.
    pub fn new(
        name: impl Into<String>,
        schema: Schema,
        primary_key: Vec<usize>,
        topology: PartitionTopology,
    ) -> Self {
        Self {
            name: name.into(),
            schema,
            primary_key,
            topology,
        }
    }
}

/// Partitioning topology defining strategy and individual partitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionTopology {
    /// Range partitioning where partitions cover non-overlapping intervals `[lower, upper)`.
    Range {
        /// Zero-based column index in table schema used as partition key.
        key_column: usize,
        /// Ordered partition boundaries.
        partitions: Vec<RangePartitionDefinition>,
    },
    /// List partitioning where partitions cover disjoint sets of explicit values.
    List {
        /// Zero-based column index in table schema used as partition key.
        key_column: usize,
        /// Explicit list value sets for each partition.
        partitions: Vec<ListPartitionDefinition>,
    },
}

/// Default bounded worker count for concurrent analytical partition scans.
pub const DEFAULT_SCAN_WORKERS: usize = 4;

/// Synchronous local database server.
///
/// Encapsulates catalog metadata management, transactional write logging,
/// LSM-based rowstore persistence, and data movement within a single local directory.
pub struct LocalServer {
    _lock: ProcessLock,
    catalog: Arc<LocalCatalogStore>,
    engine: Arc<Engine>,
    txn_manager: TransactionManager,
    data_mover: LocalDataMover,
    execution_lock: Mutex<()>,
    colstore_dir: PathBuf,
    scan_workers: usize,
}

impl std::fmt::Debug for LocalServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServer")
            .field("colstore_dir", &self.colstore_dir)
            .field("scan_workers", &self.scan_workers)
            .finish()
    }
}

impl LocalServer {
    /// Opens or recovers a local server instance rooted at `root`.
    ///
    /// Canonicalizes/creates `root` and acquires an exclusive non-blocking advisory
    /// lock at `<root>/LOCK` before opening catalog, engine, txn journal, or movement directories.
    ///
    /// The root directory layout contains:
    /// - `root/LOCK` - Advisory lock file ensuring single-process exclusive root ownership.
    /// - `root/catalog` - Directory for durable catalog snapshots.
    /// - `root/rowstore` - Directory for rowstore LSM data (WAL, SSTs, manifest).
    /// - `root/txn.journal` - Journal file for 2PC transaction coordination.
    /// - `root/movement` - Directory for data movement jobs and tablet packages.
    /// - `root/colstore` - Directory for columnar storage segments and tablet manifests.
    ///
    /// Recovery registers a stable [`RowstoreParticipant`] with ID 1 and
    /// replays committed transactions from the transaction journal.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Conflict`] if the root directory is already locked by another
    /// process. Returns other [`HtapError`] variants if initialization, catalog loading,
    /// or transaction recovery fails.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let canonical_root = root.canonicalize()?;
        let lock_guard = ProcessLock::acquire(&canonical_root)?;

        let catalog_dir = canonical_root.join("catalog");
        let catalog = Arc::new(LocalCatalogStore::open(catalog_dir)?);

        let rowstore_dir = canonical_root.join("rowstore");
        let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir))?);

        let txn_journal_path = canonical_root.join("txn.journal");
        let txn_manager = TransactionManager::open(txn_journal_path)?;

        let participant = Arc::new(RowstoreParticipant::new(
            ParticipantId::new(1),
            Arc::clone(&engine),
        ));
        txn_manager.register_participant(participant);

        txn_manager.recover()?;

        let movement_dir = canonical_root.join("movement");
        let data_mover = LocalDataMover::new(movement_dir)?;

        let colstore_dir = canonical_root.join("colstore");
        std::fs::create_dir_all(&colstore_dir)?;

        Self::validate_storage_state_on_open(&catalog, &colstore_dir)?;

        Ok(Self {
            _lock: lock_guard,
            catalog,
            engine,
            txn_manager,
            data_mover,
            execution_lock: Mutex::new(()),
            colstore_dir,
            scan_workers: DEFAULT_SCAN_WORKERS,
        })
    }

    /// Configures the maximum number of worker threads used for concurrent partition scans.
    ///
    /// Must be at least 1; values less than 1 are clamped to 1.
    pub fn with_scan_workers(mut self, scan_workers: usize) -> Self {
        self.scan_workers = scan_workers.max(1);
        self
    }

    /// Sets the maximum number of worker threads used for concurrent partition scans.
    ///
    /// Must be at least 1; values less than 1 are clamped to 1.
    pub fn set_scan_workers(&mut self, scan_workers: usize) {
        self.scan_workers = scan_workers.max(1);
    }

    /// Returns the configured scan worker thread count.
    pub fn scan_workers(&self) -> usize {
        self.scan_workers
    }

    /// Returns a borrowing façade for server-integrated data movement operations.
    pub fn data_mover(&self) -> LocalServerDataMover<'_> {
        LocalServerDataMover {
            mover: &self.data_mover,
            catalog: &self.catalog,
            engine: &self.engine,
            txn_manager: &self.txn_manager,
        }
    }

    /// Synchronously executes a single SQL statement against the local database.
    ///
    /// All operations are serialized via internal locking to guarantee dense,
    /// monotonically contiguous transaction versions.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on SQL syntax error, catalog conflict or missing
    /// table, schema violation, unsupported storage layout, or storage failure.
    pub fn execute(&self, sql: &str) -> Result<StatementResult> {
        let _guard = self.execution_lock.lock();

        let statement = htap_sql::parse_one(sql)?;
        let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);
        let bound = htap_sql::bind(&statement, &catalog)?;

        match bound {
            BoundStatement::CreateTable(create) => {
                let _route = classify_route(
                    &BoundStatement::CreateTable(create.clone()),
                    &StorageDescriptor::Row,
                )?;
                self.execute_create_table(create, &catalog)
            }
            BoundStatement::Insert(insert) => {
                let table_desc = self.resolve_table(&insert.table, &catalog)?;
                self.execute_insert(insert, table_desc, &catalog)
            }
            BoundStatement::Delete(delete) => {
                let table_desc = self.resolve_table(&delete.table, &catalog)?;
                let partition_id = self.route_pk_to_partition(table_desc, &delete.key, &catalog)?;
                let partition = self.validate_partition(table_desc, partition_id, &catalog)?;
                let _route =
                    classify_route(&BoundStatement::Delete(delete.clone()), &partition.storage)?;
                self.execute_delete(delete, partition.id)
            }
            BoundStatement::Select(select) => {
                let table_desc = self.resolve_table(&select.table, &catalog)?;
                let partition_id = self.route_pk_to_partition(table_desc, &select.key, &catalog)?;
                let partition = self.validate_partition(table_desc, partition_id, &catalog)?;
                let route =
                    classify_route(&BoundStatement::Select(select.clone()), &partition.storage)?;
                let key = match route {
                    Route::RowstorePointRead { key } => key,
                    _ => unreachable!(),
                };
                self.execute_select(select, table_desc, partition.id, key)
            }
            BoundStatement::AnalyticSelect(select) => {
                let (table_desc, partitions) =
                    self.resolve_table_and_all_partitions(&select.table, &catalog)?;
                for partition in &partitions {
                    let _route = classify_route(
                        &BoundStatement::AnalyticSelect(select.clone()),
                        &partition.storage,
                    )?;
                }
                self.execute_analytic_select(select, table_desc, &partitions, &catalog)
            }
        }
    }

    /// Internal lock-safe helper for table creation (unpartitioned and partitioned).
    fn create_table_internal(
        &self,
        catalog: &CatalogSnapshot,
        name: String,
        schema: Schema,
        primary_key: Vec<usize>,
        partitioning_desc: Option<PartitioningDescriptor>,
        partition_items: Vec<(String, Option<RangeBound>, Vec<Value>)>,
    ) -> Result<StatementResult> {
        if catalog.table_by_name(&name).is_some() {
            return Err(HtapError::Conflict(format!(
                "table '{name}' already exists"
            )));
        }

        let max_table_id = catalog
            .tables
            .iter()
            .map(|t| t.id.as_u64())
            .max()
            .unwrap_or(0);
        let next_table_id = max_table_id
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow {
                counter: "table_id",
            })?;
        let table_id = TableId::new(next_table_id);

        let mut cur_partition_id = catalog
            .partitions
            .iter()
            .map(|p| p.id.as_u64())
            .max()
            .unwrap_or(0);
        let mut cur_tablet_id = catalog
            .tablets
            .iter()
            .map(|t| t.id.as_u64())
            .max()
            .unwrap_or(0);
        let mut cur_replica_id = catalog
            .replicas
            .iter()
            .map(|r| r.id.as_u64())
            .max()
            .unwrap_or(0);

        let next_generation =
            catalog
                .generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;

        let mut table_partition_ids = Vec::with_capacity(partition_items.len());
        let mut new_partitions = Vec::with_capacity(partition_items.len());
        let mut new_tablets = Vec::with_capacity(partition_items.len());
        let mut new_replicas = Vec::with_capacity(partition_items.len());

        for (part_name, range_opt, list_values) in partition_items {
            cur_partition_id =
                cur_partition_id
                    .checked_add(1)
                    .ok_or(HtapError::CounterOverflow {
                        counter: "partition_id",
                    })?;
            let partition_id = PartitionId::new(cur_partition_id);

            cur_tablet_id = cur_tablet_id
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "tablet_id",
                })?;
            let tablet_id = TabletId::new(cur_tablet_id);

            cur_replica_id = cur_replica_id
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "replica_id",
                })?;
            let replica_id = ReplicaId::new(cur_replica_id);

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
                table_id,
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

            table_partition_ids.push(partition_id);
            new_partitions.push(partition_desc);
            new_tablets.push(tablet_desc);
            new_replicas.push(replica_desc);
        }

        let mut table_desc = TableDescriptor::new(
            table_id,
            name,
            schema,
            primary_key,
            table_partition_ids,
            next_generation,
        );
        if let Some(p_desc) = partitioning_desc {
            table_desc = table_desc.with_partitioning(p_desc);
        }

        let mut tables = catalog.tables.clone();
        tables.push(table_desc);

        let mut partitions = catalog.partitions.clone();
        partitions.extend(new_partitions);

        let mut tablets = catalog.tablets.clone();
        tablets.extend(new_tablets);

        let mut replicas = catalog.replicas.clone();
        replicas.extend(new_replicas);

        let next_snapshot =
            CatalogSnapshot::new(next_generation, tables, partitions, tablets, replicas);

        self.catalog
            .compare_and_set(catalog.generation, next_snapshot)?;

        Ok(StatementResult::ddl(1))
    }

    /// Creates a new partitioned table according to the provided [`PartitionedTableDefinition`].
    ///
    /// The table and its partitions are persisted to the catalog in a single atomic CAS operation.
    /// Each defined partition is initialized with Row storage, a single tablet (bucket 0), and a healthy
    /// leader replica on node 1.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Conflict`] if a table with the same name already exists.
    /// Returns [`HtapError::InvalidArgument`] if partition definitions are invalid, overlapping, or
    /// schema/PK constraints are violated.
    /// Returns [`HtapError::CounterOverflow`] if IDs or catalog generation overflow.
    pub fn create_partitioned_table(
        &self,
        definition: PartitionedTableDefinition,
    ) -> Result<StatementResult> {
        let is_empty_topology = match &definition.topology {
            PartitionTopology::Range { partitions, .. } => partitions.is_empty(),
            PartitionTopology::List { partitions, .. } => partitions.is_empty(),
        };
        if is_empty_topology {
            return Err(HtapError::InvalidArgument(format!(
                "partitioned table '{}' must define at least one partition",
                definition.name
            )));
        }

        let _guard = self.execution_lock.lock();
        let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);

        let (partitioning_desc, partition_items) = match definition.topology {
            PartitionTopology::Range {
                key_column,
                partitions,
            } => (
                PartitioningDescriptor::new(key_column, PartitioningMethod::Range),
                partitions
                    .into_iter()
                    .map(|p| {
                        let bound = if p.lower_opt.is_some() || p.upper_opt.is_some() {
                            RangeBound::new_opt(p.lower_opt, p.upper_opt)
                        } else {
                            RangeBound::new(p.lower, p.upper)
                        };
                        (p.name, Some(bound), Vec::new())
                    })
                    .collect::<Vec<_>>(),
            ),
            PartitionTopology::List {
                key_column,
                partitions,
            } => (
                PartitioningDescriptor::new(key_column, PartitioningMethod::List),
                partitions
                    .into_iter()
                    .map(|p| (p.name, None, p.values))
                    .collect::<Vec<_>>(),
            ),
        };

        self.create_table_internal(
            &catalog,
            definition.name,
            definition.schema,
            definition.primary_key,
            Some(partitioning_desc),
            partition_items,
        )
    }

    /// Alters partition topology of an existing partitioned table.
    ///
    /// Supports typed [`PartitionAlteration`]:
    /// - [`PartitionAlteration::Add`]: Adds one or more partitions.
    /// - [`PartitionAlteration::Drop`]: Drops one or more empty partitions.
    /// - [`PartitionAlteration::Reorganize`]: Reorganizes contiguous empty source partitions into new target partitions.
    ///
    /// Executes under `execution_lock`. Source partitions for `Drop` and `Reorganize` are scanned
    /// at the current visible snapshot; rowstore entries are collapsed to determine logical occupancy.
    /// If any source partition contains active rows, the alteration is rejected with
    /// [`HtapError::InvalidArgument`] without mutating the catalog or burning identifier sequences.
    /// On success, candidate catalog snapshot is committed via a single atomic CAS.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::NotFound`] if the table or a named source partition does not exist.
    /// Returns [`HtapError::InvalidArgument`] if the table is unpartitioned, all partitions would be dropped,
    /// definitions overlap, sources are non-contiguous, or source partitions are populated.
    /// Returns [`HtapError::Conflict`] if catalog CAS fails due to concurrent modification.
    pub fn alter_partitions(
        &self,
        table_name: &str,
        alteration: impl Into<PartitionAlteration>,
    ) -> Result<StatementResult> {
        let alteration = alteration.into();
        let _guard = self.execution_lock.lock();
        let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);

        let source_partitions =
            catalog.source_partitions_for_alteration(table_name, &alteration)?;

        if !source_partitions.is_empty() {
            let snapshot = Snapshot::new(self.txn_manager.visible_version());
            for part in &source_partitions {
                let entries = self.engine.scan_partition(part.id.as_u64(), snapshot)?;
                let rows = htap_convert::collapse_entries_to_rows(&entries);
                if !rows.is_empty() {
                    return Err(HtapError::InvalidArgument(format!(
                        "cannot alter partition '{}' of table '{table_name}': partition is populated with {} row(s)",
                        part.name,
                        rows.len()
                    )));
                }
            }
        }

        let candidate = catalog.apply_partition_alteration(table_name, &alteration)?;

        self.catalog
            .compare_and_set(catalog.generation, candidate)?;

        Ok(StatementResult::ddl(1))
    }

    fn resolve_table<'a>(
        &self,
        table_name: &str,
        catalog: &'a CatalogSnapshot,
    ) -> Result<&'a TableDescriptor> {
        catalog
            .table_by_name(table_name)
            .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))
    }

    fn validate_partition<'a>(
        &self,
        table_desc: &TableDescriptor,
        partition_id: PartitionId,
        catalog: &'a CatalogSnapshot,
    ) -> Result<&'a PartitionDescriptor> {
        let partition = catalog.partition(partition_id).ok_or_else(|| {
            HtapError::Internal(format!(
                "partition {partition_id} referenced by table '{}' not found in catalog",
                table_desc.name
            ))
        })?;

        if partition.table_id != table_desc.id {
            return Err(HtapError::Internal(format!(
                "partition {partition_id} table_id mismatch: expected {}, got {}",
                table_desc.id, partition.table_id
            )));
        }

        if partition.tablets.len() != 1 {
            return Err(HtapError::Unsupported(format!(
                "partition {partition_id} must have exactly one tablet, found {}",
                partition.tablets.len()
            )));
        }

        let tablet_id = partition.tablets[0];
        let tablet = catalog.tablet(tablet_id).ok_or_else(|| {
            HtapError::Internal(format!(
                "tablet {tablet_id} referenced by partition {partition_id} not found in catalog"
            ))
        })?;

        if tablet.partition_id != partition.id {
            return Err(HtapError::Internal(format!(
                "tablet {tablet_id} partition_id mismatch: expected {}, got {}",
                partition.id, tablet.partition_id
            )));
        }

        if tablet.replicas.len() != 1 {
            return Err(HtapError::Unsupported(format!(
                "tablet {tablet_id} must have exactly one replica, found {}",
                tablet.replicas.len()
            )));
        }

        let replica_id = tablet.replicas[0];
        let replica = catalog.replica(replica_id).ok_or_else(|| {
            HtapError::Internal(format!(
                "replica {replica_id} referenced by tablet {tablet_id} not found in catalog"
            ))
        })?;

        if replica.tablet_id != tablet.id {
            return Err(HtapError::Internal(format!(
                "replica {replica_id} tablet_id mismatch: expected {}, got {}",
                tablet.id, replica.tablet_id
            )));
        }

        if !replica.healthy || !replica.is_leader {
            return Err(HtapError::Internal(format!(
                "replica {replica_id} is not a healthy leader"
            )));
        }

        Ok(partition)
    }

    fn resolve_table_and_all_partitions<'a>(
        &self,
        table_name: &str,
        catalog: &'a CatalogSnapshot,
    ) -> Result<(&'a TableDescriptor, Vec<&'a PartitionDescriptor>)> {
        let table_desc = self.resolve_table(table_name, catalog)?;
        let partitions = table_desc
            .partitions
            .iter()
            .map(|&pid| self.validate_partition(table_desc, pid, catalog))
            .collect::<Result<Vec<_>>>()?;
        Ok((table_desc, partitions))
    }

    fn resolve_single_partition_table<'a>(
        &self,
        table_name: &str,
        catalog: &'a CatalogSnapshot,
    ) -> Result<(&'a TableDescriptor, &'a PartitionDescriptor)> {
        let table_desc = self.resolve_table(table_name, catalog)?;

        if table_desc.partitions.len() != 1 {
            return Err(HtapError::Unsupported(format!(
                "table '{table_name}' must have exactly one partition, found {}",
                table_desc.partitions.len()
            )));
        }

        let partition = self.validate_partition(table_desc, table_desc.partitions[0], catalog)?;
        Ok((table_desc, partition))
    }

    fn route_pk_to_partition(
        &self,
        table_desc: &TableDescriptor,
        pk_key: &[Value],
        catalog: &CatalogSnapshot,
    ) -> Result<PartitionId> {
        match &table_desc.partitioning {
            Some(partitioning) => {
                let pk_pos = table_desc
                    .primary_key
                    .iter()
                    .position(|&col_idx| col_idx == partitioning.key_column)
                    .ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "partition key column index {} not found in primary key for table '{}'",
                            partitioning.key_column, table_desc.name
                        ))
                    })?;
                let part_val = pk_key.get(pk_pos).ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "primary key value missing index {pk_pos} for table '{}'",
                        table_desc.name
                    ))
                })?;
                table_desc.route_partition_value(catalog, part_val)
            }
            None => table_desc.route_partition_value(catalog, &Value::Null),
        }
    }

    fn execute_create_table(
        &self,
        create: CreateTable,
        catalog: &CatalogSnapshot,
    ) -> Result<StatementResult> {
        let (partitioning_desc, partition_items) = match create.partitioning {
            Some(BoundPartitioning::Range {
                key_column,
                partitions,
            }) => (
                Some(PartitioningDescriptor::new(
                    key_column,
                    PartitioningMethod::Range,
                )),
                partitions
                    .into_iter()
                    .map(|p| {
                        (
                            p.name,
                            Some(RangeBound::new_opt(p.lower, p.upper)),
                            Vec::new(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Some(BoundPartitioning::List {
                key_column,
                partitions,
            }) => (
                Some(PartitioningDescriptor::new(
                    key_column,
                    PartitioningMethod::List,
                )),
                partitions
                    .into_iter()
                    .map(|p| (p.name, None, p.values))
                    .collect::<Vec<_>>(),
            ),
            None => (None, vec![("p0".to_string(), None, Vec::new())]),
        };

        self.create_table_internal(
            catalog,
            create.name,
            create.schema,
            create.primary_key,
            partitioning_desc,
            partition_items,
        )
    }

    fn execute_insert(
        &self,
        insert: Insert,
        table_desc: &TableDescriptor,
        catalog: &CatalogSnapshot,
    ) -> Result<StatementResult> {
        let mut mutations = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
            let partition_id = match &table_desc.partitioning {
                Some(partitioning) => {
                    let val = row.get(partitioning.key_column).ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "row missing partition key column index {}",
                            partitioning.key_column
                        ))
                    })?;
                    table_desc.route_partition_value(catalog, val)?
                }
                None => table_desc.route_partition_value(catalog, &Value::Null)?,
            };

            let partition = self.validate_partition(table_desc, partition_id, catalog)?;
            let _route = classify_route(
                &BoundStatement::Insert(Insert::new(&table_desc.name, vec![row.clone()])),
                &partition.storage,
            )?;

            let pk_values: Vec<Value> = table_desc
                .primary_key
                .iter()
                .map(|&idx| {
                    row.get(idx).cloned().ok_or_else(|| {
                        HtapError::InvalidArgument(format!(
                            "row missing primary key column index {idx}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let key = encode_key(&pk_values)?;
            mutations.push(Mutation::Put {
                partition_id: partition_id.as_u64(),
                key,
                row: row.clone(),
            });
        }

        let payload = RowstoreParticipant::encode_payload(&mutations)?;
        let work = ParticipantWork::new(ParticipantId::new(1), payload);
        let req = TransactionRequest::new(vec![work])?;
        let committed = self.txn_manager.commit_request(req)?;

        Ok(StatementResult::dml(
            mutations.len() as u64,
            Some(committed.version),
        ))
    }

    fn execute_delete(
        &self,
        delete: DeleteByPrimaryKey,
        partition_id: PartitionId,
    ) -> Result<StatementResult> {
        let key = encode_key(&delete.key)?;
        let mutation = Mutation::Delete {
            partition_id: partition_id.as_u64(),
            key,
        };

        let payload = RowstoreParticipant::encode_payload(&[mutation])?;
        let work = ParticipantWork::new(ParticipantId::new(1), payload);
        let req = TransactionRequest::new(vec![work])?;
        let committed = self.txn_manager.commit_request(req)?;

        Ok(StatementResult::dml(1, Some(committed.version)))
    }

    fn execute_select(
        &self,
        select: PointSelect,
        table_desc: &TableDescriptor,
        partition_id: PartitionId,
        key: Vec<u8>,
    ) -> Result<StatementResult> {
        let projected_columns: Vec<ColumnDef> = select
            .projection
            .iter()
            .map(|&idx| {
                table_desc
                    .schema
                    .columns()
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| HtapError::Internal(format!("column index {idx} out of bounds")))
            })
            .collect::<Result<Vec<_>>>()?;

        let snapshot = Snapshot::new(self.txn_manager.visible_version());
        let row_opt = self.engine.get(partition_id.as_u64(), &key, snapshot)?;

        let rows = match row_opt {
            Some(row) => {
                let projected_values: Vec<Value> = select
                    .projection
                    .iter()
                    .map(|&idx| {
                        row.get(idx).cloned().ok_or_else(|| {
                            HtapError::Internal(format!("row missing column index {idx}"))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                vec![Row::new(projected_values)]
            }
            None => Vec::new(),
        };

        Ok(StatementResult::query(projected_columns, rows))
    }

    fn execute_analytic_select(
        &self,
        select: AnalyticSelect,
        table_desc: &TableDescriptor,
        partitions: &[&PartitionDescriptor],
        catalog: &CatalogSnapshot,
    ) -> Result<StatementResult> {
        // 1. Freeze catalog, snapshot, and plan
        let selected_partitions =
            olap::prune_partitions(table_desc, partitions, select.filter.as_ref());
        let snapshot = Snapshot::new(self.txn_manager.visible_version());
        let (source_columns, mapping) = olap::plan_source_columns(&select);
        let pushdown_predicate = olap::select_pushdown_predicate(select.filter.as_ref());

        let n_parts = selected_partitions.len();
        let num_workers = self.scan_workers.max(1).min(n_parts.max(1));

        // 2. Concurrently execute partition scans with bounded workers
        let partition_results: Vec<Result<Vec<Row>>> = if n_parts <= 1 || num_workers <= 1 {
            selected_partitions
                .iter()
                .map(|p| {
                    scan_partition_compact(
                        &self.engine,
                        &self.colstore_dir,
                        catalog,
                        p,
                        snapshot,
                        &source_columns,
                        &table_desc.primary_key,
                        pushdown_predicate.as_ref(),
                    )
                })
                .collect()
        } else {
            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(num_workers);
                for worker_id in 0..num_workers {
                    let worker_parts: Vec<(usize, &PartitionDescriptor)> = selected_partitions
                        .iter()
                        .copied()
                        .enumerate()
                        .filter(|(idx, _)| idx % num_workers == worker_id)
                        .collect();

                    let engine = &self.engine;
                    let colstore_dir = &self.colstore_dir;
                    let source_cols = &source_columns;
                    let pk = &table_desc.primary_key;
                    let pred = pushdown_predicate.as_ref();

                    handles.push(s.spawn(move || {
                        let mut res = Vec::with_capacity(worker_parts.len());
                        for (idx, part) in worker_parts {
                            let part_res = scan_partition_compact(
                                engine,
                                colstore_dir,
                                catalog,
                                part,
                                snapshot,
                                source_cols,
                                pk,
                                pred,
                            );
                            res.push((idx, part_res));
                        }
                        res
                    }));
                }

                let mut indexed_results = Vec::with_capacity(n_parts);
                for handle in handles {
                    let worker_res = handle.join().expect("scan worker thread panicked");
                    indexed_results.extend(worker_res);
                }
                indexed_results.sort_by_key(|(idx, _)| *idx);
                indexed_results.into_iter().map(|(_, res)| res).collect()
            })
        };

        // 3. Deterministic partition-order merge
        let mut all_compact_rows = Vec::new();
        for res in partition_results {
            all_compact_rows.extend(res?);
        }

        // 4. Global filter, aggregate, group, and order
        let result = olap::execute_analytic_select_compact(&select, all_compact_rows, &mapping)?;
        Ok(StatementResult::Query(result))
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_partition_compact(
    engine: &Engine,
    colstore_dir: &Path,
    catalog: &CatalogSnapshot,
    partition: &PartitionDescriptor,
    snapshot: Snapshot,
    source_columns: &[usize],
    primary_key: &[usize],
    pushdown_predicate: Option<&Predicate>,
) -> Result<Vec<Row>> {
    let tablet_id = partition.tablets[0];
    let tablet = catalog
        .tablet(tablet_id)
        .ok_or_else(|| HtapError::Internal(format!("tablet {tablet_id} not found")))?;

    match &partition.storage {
        StorageDescriptor::Row => {
            let entries = engine.scan_partition(partition.id.as_u64(), snapshot)?;
            let rows = htap_convert::collapse_entries_to_rows(&entries);
            let mut compact_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values = Vec::with_capacity(source_columns.len());
                for &idx in source_columns {
                    let val = row.get(idx).cloned().ok_or_else(|| {
                        HtapError::Internal(format!("row missing column index {idx}"))
                    })?;
                    values.push(val);
                }
                compact_rows.push(Row::new(values));
            }
            Ok(compact_rows)
        }
        StorageDescriptor::Column => {
            let cat_manifest = tablet.column_manifest.as_ref().ok_or_else(|| {
                HtapError::InvalidArgument(format!(
                    "partition {} has Column storage but tablet {tablet_id} has no column manifest in catalog",
                    partition.id
                ))
            })?;
            let disk_manifest = htap_convert::open(colstore_dir, tablet_id)?;
            if disk_manifest.generation != cat_manifest.generation {
                return Err(HtapError::InvalidArgument(format!(
                    "column manifest generation mismatch for tablet {tablet_id}: disk {} != catalog {}",
                    disk_manifest.generation, cat_manifest.generation
                )));
            }
            let compact_res = htap_convert::read_column_partition_compact_core(
                catalog,
                engine,
                colstore_dir,
                partition.id,
                snapshot,
                source_columns,
                primary_key,
                pushdown_predicate.cloned(),
            )?;
            Ok(compact_res.rows)
        }
        StorageDescriptor::Converting { .. } => match &tablet.column_manifest {
            Some(cat_manifest) => {
                let disk_manifest = htap_convert::open(colstore_dir, tablet_id)?;
                if disk_manifest.generation != cat_manifest.generation {
                    return Err(HtapError::InvalidArgument(format!(
                        "column manifest generation mismatch for converting tablet {tablet_id}: disk {} != catalog {}",
                        disk_manifest.generation, cat_manifest.generation
                    )));
                }
                let compact_res = htap_convert::read_column_partition_compact_core(
                    catalog,
                    engine,
                    colstore_dir,
                    partition.id,
                    snapshot,
                    source_columns,
                    primary_key,
                    pushdown_predicate.cloned(),
                )?;
                Ok(compact_res.rows)
            }
            None => {
                let is_snapshot_pinned = partition
                    .conversion
                    .as_ref()
                    .map(|c| c.phase == htap_catalog::ConversionPhase::SnapshotPinned)
                    .unwrap_or(false);
                if is_snapshot_pinned {
                    let entries = engine.scan_partition(partition.id.as_u64(), snapshot)?;
                    let rows = htap_convert::collapse_entries_to_rows(&entries);
                    let mut compact_rows = Vec::with_capacity(rows.len());
                    for row in rows {
                        let mut values = Vec::with_capacity(source_columns.len());
                        for &idx in source_columns {
                            let val = row.get(idx).cloned().ok_or_else(|| {
                                HtapError::Internal(format!("row missing column index {idx}"))
                            })?;
                            values.push(val);
                        }
                        compact_rows.push(Row::new(values));
                    }
                    Ok(compact_rows)
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "partition {} is converting without manifest but phase is not SnapshotPinned",
                        partition.id
                    )))
                }
            }
        },
    }
}

impl LocalServer {
    /// Returns the columnar storage root directory for this local server.
    pub fn colstore_dir(&self) -> &Path {
        &self.colstore_dir
    }

    /// Converts a single-partition table from row to column format using the server's colstore directory.
    pub fn convert_table(&self, table_name: &str) -> Result<htap_convert::TabletColumnManifest> {
        let _guard = self.execution_lock.lock();
        let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);
        let (_, partition) = self.resolve_single_partition_table(table_name, &catalog)?;
        let partition_id = partition.id;
        let converter = htap_convert::LocalConverter::new(
            Arc::clone(&self.catalog) as Arc<dyn CatalogStore>,
            Arc::clone(&self.engine),
            &self.colstore_dir,
            htap_convert::SegmentOptions::default(),
        );
        converter.convert_partition(partition_id)
    }

    /// Converts all partitions of a table to columnar format, returning a [`TableConversionReport`].
    pub fn convert_table_to_column(&self, table_name: &str) -> Result<TableConversionReport> {
        let _guard = self.execution_lock.lock();
        let converter = htap_convert::LocalConverter::new(
            Arc::clone(&self.catalog) as Arc<dyn CatalogStore>,
            Arc::clone(&self.engine),
            &self.colstore_dir,
            SegmentOptions::default(),
        );
        converter.convert_table_to_column(table_name)
    }

    /// Demotes all partitions of a table from columnar format back to row storage, returning a [`TableConversionReport`].
    pub fn convert_table_to_row(&self, table_name: &str) -> Result<TableConversionReport> {
        let _guard = self.execution_lock.lock();
        let converter = htap_convert::LocalConverter::new(
            Arc::clone(&self.catalog) as Arc<dyn CatalogStore>,
            Arc::clone(&self.engine),
            &self.colstore_dir,
            SegmentOptions::default(),
        );
        converter.convert_table_to_row(table_name)
    }

    /// Synchronously executes a conversion tick according to the given policy.
    pub fn conversion_tick(&self, policy: ConversionPolicy) -> Result<ConversionTickReport> {
        let _guard = self.execution_lock.lock();
        let converter = htap_convert::LocalConverter::new(
            Arc::clone(&self.catalog) as Arc<dyn CatalogStore>,
            Arc::clone(&self.engine),
            &self.colstore_dir,
            SegmentOptions::default(),
        );
        converter.conversion_tick(&policy)
    }

    /// Synchronously executes a conversion tick using default manual policy (resuming in-flight converting jobs only).
    pub fn tick(&self) -> Result<ConversionTickReport> {
        self.conversion_tick(ConversionPolicy::manual())
    }

    fn validate_storage_state_on_open(
        catalog: &LocalCatalogStore,
        colstore_dir: &Path,
    ) -> Result<()> {
        let cat_snap = match catalog.load()? {
            Some(snap) => snap,
            None => return Ok(()),
        };

        for partition in &cat_snap.partitions {
            match &partition.storage {
                StorageDescriptor::Column => {
                    if partition.tablets.is_empty() {
                        return Err(HtapError::Corruption(format!(
                            "partition {} in Column storage has no tablets",
                            partition.id
                        )));
                    }
                    if partition.tablets.len() != 1 {
                        return Err(HtapError::Corruption(format!(
                            "partition {} in Column storage has {} tablets; exactly 1 tablet is required",
                            partition.id,
                            partition.tablets.len()
                        )));
                    }
                    if partition.conversion.is_some() {
                        return Err(HtapError::Corruption(format!(
                            "partition {} in Column storage has active conversion descriptor",
                            partition.id
                        )));
                    }

                    let tablet_id = partition.tablets[0];
                    let tablet = cat_snap.tablet(tablet_id).ok_or_else(|| {
                        HtapError::Corruption(format!(
                            "tablet {tablet_id} referenced by partition {} not found in catalog",
                            partition.id
                        ))
                    })?;

                    let cat_manifest = tablet.column_manifest.as_ref().ok_or_else(|| {
                        HtapError::Corruption(format!(
                            "partition {} in Column storage missing tablet column_manifest in catalog",
                            partition.id
                        ))
                    })?;

                    let disk_manifest = htap_convert::open(colstore_dir, tablet_id)?;
                    if disk_manifest.generation != cat_manifest.generation
                        || disk_manifest.base_version != cat_manifest.base_version
                        || disk_manifest.segments.len() as u64 != cat_manifest.segment_count
                        || disk_manifest.total_rows() != cat_manifest.row_count
                    {
                        return Err(HtapError::Corruption(format!(
                            "column manifest on disk does not match catalog reference for tablet {tablet_id}"
                        )));
                    }
                }
                StorageDescriptor::Converting {
                    from,
                    to,
                    generation,
                } => {
                    if partition.tablets.is_empty() {
                        return Err(HtapError::Corruption(format!(
                            "partition {} in Converting storage has no tablets",
                            partition.id
                        )));
                    }
                    if partition.tablets.len() != 1 {
                        return Err(HtapError::Corruption(format!(
                            "partition {} in Converting storage has {} tablets; exactly 1 tablet is required",
                            partition.id,
                            partition.tablets.len()
                        )));
                    }
                    if *from != StorageFormat::Row || *to != StorageFormat::Column {
                        return Err(HtapError::Unsupported(format!(
                            "partition {} has unsupported conversion direction: {from:?} -> {to:?}",
                            partition.id
                        )));
                    }

                    let tablet_id = partition.tablets[0];
                    let _tablet = cat_snap.tablet(tablet_id).ok_or_else(|| {
                        HtapError::Corruption(format!(
                            "tablet {tablet_id} referenced by partition {} not found in catalog",
                            partition.id
                        ))
                    })?;

                    let conv = partition.conversion.as_ref().ok_or_else(|| {
                        HtapError::Corruption(format!(
                            "partition {} in Converting state without conversion descriptor",
                            partition.id
                        ))
                    })?;

                    if conv.generation != *generation || conv.from != *from || conv.to != *to {
                        return Err(HtapError::Corruption(format!(
                            "partition {} conversion descriptor does not match Converting storage",
                            partition.id
                        )));
                    }

                    match conv.phase {
                        ConversionPhase::SnapshotPinned => {
                            // Manifest may or may not exist yet; if it does, it must not contradict
                            if let Ok(disk_manifest) = htap_convert::open(colstore_dir, tablet_id) {
                                if disk_manifest.generation != conv.generation
                                    || disk_manifest.base_version != conv.snapshot_version
                                {
                                    return Err(HtapError::Corruption(format!(
                                        "partition {} has disk manifest inconsistent with pinned conversion descriptor",
                                        partition.id
                                    )));
                                }
                            }
                        }
                        ConversionPhase::SegmentsWritten | ConversionPhase::ReadyToPublish => {
                            let disk_manifest = htap_convert::open(colstore_dir, tablet_id)?;
                            if disk_manifest.generation != conv.generation
                                || disk_manifest.base_version != conv.snapshot_version
                            {
                                return Err(HtapError::Corruption(format!(
                                    "partition {} manifest on disk does not match conversion descriptor",
                                    partition.id
                                )));
                            }
                        }
                    }
                }
                StorageDescriptor::Row => {
                    if let Some(&tablet_id) = partition.tablets.first() {
                        if let Some(tablet) = cat_snap.tablet(tablet_id) {
                            if tablet.column_manifest.is_some() && partition.conversion.is_none() {
                                return Err(HtapError::Corruption(format!(
                                    "partition {} is in Row storage but tablet {tablet_id} has column manifest in catalog",
                                    partition.id
                                )));
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

/// Borrowing façade providing server-integrated data movement operations.
///
/// Supplies the server-owned [`LocalCatalogStore`], [`Engine`], and [`TransactionManager`]
/// to [`LocalDataMover`] without exposing internal storage handles directly.
pub struct LocalServerDataMover<'a> {
    mover: &'a LocalDataMover,
    catalog: &'a LocalCatalogStore,
    engine: &'a Arc<Engine>,
    txn_manager: &'a TransactionManager,
}

impl<'a> LocalServerDataMover<'a> {
    /// Imports records from a generic CSV [`std::io::Read`] stream.
    ///
    /// Records are parsed according to table schema and committed in batches using
    /// the server's transaction manager.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on catalog lookup failure, schema mismatch, or storage error.
    pub fn copy_from_csv_reader<R: std::io::Read>(
        &self,
        options: &CopyOptions,
        reader: R,
    ) -> Result<CopyReport> {
        self.mover
            .copy_from_csv_reader(options, self.catalog, self.txn_manager, reader)
    }

    /// Imports records from a generic JSONLines [`std::io::Read`] stream.
    ///
    /// Records are parsed according to table schema and committed in batches using
    /// the server's transaction manager.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on catalog lookup failure, schema mismatch, or storage error.
    pub fn copy_from_jsonl_reader<R: std::io::Read>(
        &self,
        options: &CopyOptions,
        reader: R,
    ) -> Result<CopyReport> {
        self.mover
            .copy_from_jsonl_reader(options, self.catalog, self.txn_manager, reader)
    }

    /// Imports records from a CSV file specified in `options.path`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on file read failure, parse error, or transaction commit error.
    pub fn copy_from_csv(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover
            .copy_from_csv(options, self.catalog, self.txn_manager)
    }

    /// Imports records from a JSONLines file specified in `options.path`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on file read failure, parse error, or transaction commit error.
    pub fn copy_from_jsonl(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover
            .copy_from_jsonl(options, self.catalog, self.txn_manager)
    }

    /// Imports records from the file specified in `options.path` according to `options.format`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on file read failure, parse error, or transaction commit error.
    pub fn import(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover.import(options, self.catalog, self.txn_manager)
    }

    /// Exports partition records to a generic CSV [`std::io::Write`] stream.
    ///
    /// Pins the engine snapshot, collapses MVCC/tombstones, and outputs rows in
    /// deterministic primary-key order.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on catalog lookup failure, snapshot read failure, or stream write error.
    pub fn copy_to_csv_writer<W: std::io::Write>(
        &self,
        options: &CopyOptions,
        writer: W,
    ) -> Result<CopyReport> {
        self.mover
            .copy_to_csv_writer(options, self.catalog, self.engine.as_ref(), writer)
    }

    /// Exports partition records to a generic JSONLines [`std::io::Write`] stream.
    ///
    /// Pins the engine snapshot, collapses MVCC/tombstones, and outputs rows in
    /// deterministic primary-key order.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on catalog lookup failure, snapshot read failure, or stream write error.
    pub fn copy_to_jsonl_writer<W: std::io::Write>(
        &self,
        options: &CopyOptions,
        writer: W,
    ) -> Result<CopyReport> {
        self.mover
            .copy_to_jsonl_writer(options, self.catalog, self.engine.as_ref(), writer)
    }

    /// Exports partition records to a CSV file at `options.path`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on snapshot read failure, encoding error, or file write failure.
    pub fn copy_to_csv(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover
            .copy_to_csv(options, self.catalog, self.engine.as_ref())
    }

    /// Exports partition records to a JSONLines file at `options.path`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on snapshot read failure, encoding error, or file write failure.
    pub fn copy_to_jsonl(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover
            .copy_to_jsonl(options, self.catalog, self.engine.as_ref())
    }

    /// Exports partition records to the file specified in `options.path` according to `options.format`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on snapshot read failure, encoding error, or file write failure.
    pub fn export(&self, options: &CopyOptions) -> Result<CopyReport> {
        self.mover
            .export(options, self.catalog, self.engine.as_ref())
    }

    /// Clones a source tablet partition snapshot into a durable logical package.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] on catalog lookup error or package write failure.
    pub fn clone_tablet(&self, options: &TabletCloneOptions) -> Result<TabletPackageManifest> {
        self.mover
            .clone_tablet(options, self.catalog, self.engine.as_ref())
    }

    /// Verifies the integrity and consistency of a tablet clone package.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] if package manifest is corrupt or artifacts fail validation.
    pub fn verify_package(&self, options: &TabletCloneOptions) -> Result<TabletPackageManifest> {
        self.mover.verify_package(options, self.catalog)
    }

    /// Reconciles and repairs an unhealthy replica by validating the clone package and CAS-updating its health status.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] if validation fails or replica CAS update fails.
    pub fn repair_tablet(&self, options: &TabletCloneOptions) -> Result<ReplicaDescriptor> {
        self.mover.repair_tablet(options, self.catalog)
    }

    /// Returns the underlying [`LocalDataMover`] reference.
    pub fn mover(&self) -> &LocalDataMover {
        self.mover
    }

    /// Loads an existing movement job by `job_id` if present.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] if reading or decoding the job file fails.
    pub fn load_job(&self, job_id: &str) -> Result<Option<MovementJob>> {
        self.mover.load_job(job_id)
    }

    /// Resumes an existing movement job by `job_id`.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::NotFound`] if the job does not exist, or [`HtapError`] on I/O error.
    pub fn resume_job(&self, job_id: &str) -> Result<MovementJob> {
        self.mover.resume_job(job_id)
    }
}

impl<'a> std::ops::Deref for LocalServerDataMover<'a> {
    type Target = LocalDataMover;

    fn deref(&self) -> &Self::Target {
        self.mover
    }
}
