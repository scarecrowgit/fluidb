//! Synchronous local single-node HTAP database server.
//!
//! Provides [`LocalServer`], an embedded synchronous server coordinating SQL
//! parsing, semantic binding, catalog topology updates, transaction management,
//! LSM rowstore storage, and data movement operations via [`LocalServerDataMover`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod olap;
mod query_exec;
mod session;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
pub use htap_catalog::{
    CatalogSnapshot, ColumnManifestRef, ConversionDescriptor, ConversionPhase, IdHighWater,
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
    PointSelect, UpdateStatement, UpdateTarget,
};
use htap_sql::ast::{DropTableStatement, ShowStatement};
use htap_sql::expr::VariableLookup;
use htap_sql::result::StatementResult;
use htap_sql::route::{classify_route, Route};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, Transaction, TransactionManager,
    TransactionRequest,
};
use parking_lot::Mutex;
use session::{DefaultVariables, WriteSet};
pub use session::{Session, SessionId};

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
    next_session_id: AtomicU64,
}

impl std::fmt::Debug for LocalServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServer")
            .field("colstore_dir", &self.colstore_dir)
            .field("scan_workers", &self.scan_workers)
            .finish()
    }
}

/// Read/write context for [`LocalServer::dispatch_bound`]: either an autocommit statement, where
/// each write commits its own 2PC transaction immediately exactly as before Phase 10, or one
/// statement inside an open [`Session`] transaction, where reads observe a pinned snapshot
/// overlaid with the session's buffered write set and writes are buffered into it rather than
/// committed.
///
/// Always constructed and consumed with `LocalServer::execution_lock` already held by the caller
/// (Phase 10 plan amendment A1); [`LocalServer::dispatch_bound`] never re-locks.
enum ExecMode<'a> {
    /// Every write commits immediately through `TransactionManager::commit_request`.
    Autocommit,
    /// Part of an open session transaction.
    Txn {
        /// MVCC snapshot pinned when the transaction began.
        snapshot: Snapshot,
        /// The transaction's accumulated write set; writes merge into it instead of committing.
        write_set: &'a mut WriteSet,
    },
}

impl ExecMode<'_> {
    /// Read-side view: the MVCC snapshot to read at, and the write set to overlay below
    /// relational operators (`None` in autocommit mode, which is byte-for-byte the pre-Phase-10
    /// read path).
    fn read_view(&self, server: &LocalServer) -> (Snapshot, Option<&WriteSet>) {
        match self {
            ExecMode::Autocommit => (Snapshot::new(server.txn_manager.visible_version()), None),
            ExecMode::Txn {
                snapshot,
                write_set,
            } => (*snapshot, Some(&**write_set)),
        }
    }
}

/// Rejects a duplicate `(partition_id, key)` within one statement's own mutation batch, with the
/// same error `Engine::prepare`/`RowstoreParticipant::prepare` gives an autocommit statement
/// (storage-reviewer finding F6): `WriteSet::insert`'s `(partition_id, key)`-keyed map would
/// otherwise silently keep only the last row inside an explicit transaction, unlike autocommit.
fn reject_duplicate_mutation_keys(mutations: &[Mutation]) -> Result<()> {
    let mut seen = std::collections::HashSet::with_capacity(mutations.len());
    for m in mutations {
        let (partition_id, key) = match m {
            Mutation::Put {
                partition_id, key, ..
            } => (*partition_id, key.as_slice()),
            Mutation::Delete { partition_id, key } => (*partition_id, key.as_slice()),
        };
        if !seen.insert((partition_id, key)) {
            return Err(HtapError::InvalidArgument(format!(
                "duplicate key in mutation batch: partition {partition_id}, key {key:?}"
            )));
        }
    }
    Ok(())
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
        Self::migrate_legacy_id_high_water(&catalog, &colstore_dir)?;

        Ok(Self {
            _lock: lock_guard,
            catalog,
            engine,
            txn_manager,
            data_mover,
            execution_lock: Mutex::new(()),
            colstore_dir,
            scan_workers: DEFAULT_SCAN_WORKERS,
            next_session_id: AtomicU64::new(1),
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

        self.dispatch_bound(bound, &catalog, ExecMode::Autocommit, &DefaultVariables)
    }

    /// Returns the server's transaction manager.
    ///
    /// Exposed for tests that need to inject commit-path failures via
    /// [`htap_txn::TransactionManager::set_commit_append_hook`] /
    /// [`htap_txn::TransactionManager::set_commit_sync_hook`] to exercise `DurablePending`
    /// handling in [`Session::commit`].
    pub fn txn_manager(&self) -> &TransactionManager {
        &self.txn_manager
    }

    /// Binds and routes a single statement, either autocommit or inside an open [`Session`]
    /// transaction.
    ///
    /// Always called with `execution_lock` already held by the caller ([`Self::execute`] or
    /// [`Session::execute`]/[`Session::commit`]); never re-locks (Phase 10 plan amendment A1).
    ///
    /// `variables` resolves `@name`/`@@name` expressions (Phase 10 task 7): [`Session::execute`]
    /// passes the session's own user variables and live state; [`Self::execute`] (no session)
    /// passes [`DefaultVariables`], so `SELECT @@autocommit` still resolves (to the process
    /// default) even without a session, while a user variable is always `NULL`.
    fn dispatch_bound(
        &self,
        bound: BoundStatement,
        catalog: &CatalogSnapshot,
        mut mode: ExecMode,
        variables: &dyn VariableLookup,
    ) -> Result<StatementResult> {
        match bound {
            BoundStatement::CreateTable(create) => {
                let _route = classify_route(
                    &BoundStatement::CreateTable(create.clone()),
                    &StorageDescriptor::Row,
                )?;
                self.execute_create_table(create, catalog)
            }
            BoundStatement::Insert(insert) => {
                let table_desc = self.resolve_table(&insert.table, catalog)?;
                self.execute_insert(insert, table_desc, catalog, &mut mode)
            }
            BoundStatement::Delete(delete) => {
                let table_desc = self.resolve_table(&delete.table, catalog)?;
                let partition_id = self.route_pk_to_partition(table_desc, &delete.key, catalog)?;
                let partition = self.validate_partition(table_desc, partition_id, catalog)?;
                let _route =
                    classify_route(&BoundStatement::Delete(delete.clone()), &partition.storage)?;
                self.execute_delete(delete, table_desc.id, partition.id, &mut mode)
            }
            BoundStatement::Select(select) => {
                let table_desc = self.resolve_table(&select.table, catalog)?;
                let partition_id = self.route_pk_to_partition(table_desc, &select.key, catalog)?;
                let partition = self.validate_partition(table_desc, partition_id, catalog)?;
                let route =
                    classify_route(&BoundStatement::Select(select.clone()), &partition.storage)?;
                let key = match route {
                    Route::RowstorePointRead { key } => key,
                    _ => unreachable!(),
                };
                self.execute_select(select, table_desc, partition.id, key, &mode)
            }
            BoundStatement::AnalyticSelect(select) => {
                let (table_desc, partitions) =
                    self.resolve_table_and_all_partitions(&select.table, catalog)?;
                for partition in &partitions {
                    let _route = classify_route(
                        &BoundStatement::AnalyticSelect(select.clone()),
                        &partition.storage,
                    )?;
                }
                self.execute_analytic_select(select, table_desc, &partitions, catalog, &mode)
            }
            BoundStatement::AlterPartitions(alter) => {
                let _route = classify_route(
                    &BoundStatement::AlterPartitions(alter.clone()),
                    &StorageDescriptor::Row,
                )?;
                self.alter_partitions_internal(&alter.table, &alter.alteration)
            }
            BoundStatement::Query(query) => {
                // Every base slot must be a known table; storage descriptors of all
                // partitions are accepted (rowstore, columnar, converting).
                for slot in Self::general_base_tables(&query) {
                    let (_, partitions) = self.resolve_table_and_all_partitions(&slot, catalog)?;
                    for partition in partitions {
                        let _route = classify_route(
                            &BoundStatement::Query(query.clone()),
                            &partition.storage,
                        )?;
                    }
                }
                let (snapshot, write_set) = mode.read_view(self);
                query_exec::execute_query(
                    self,
                    &query,
                    catalog,
                    snapshot,
                    write_set,
                    Some(variables),
                )
            }
            BoundStatement::Update(update) => {
                let table_desc = self.resolve_table(&update.table, catalog)?;
                match &update.target {
                    UpdateTarget::PrimaryKey(key_values) => {
                        let partition_id =
                            self.route_pk_to_partition(table_desc, key_values, catalog)?;
                        let partition =
                            self.validate_partition(table_desc, partition_id, catalog)?;
                        let route = classify_route(
                            &BoundStatement::Update(update.clone()),
                            &partition.storage,
                        )?;
                        let key = match route {
                            Route::RowstoreUpdate { key: Some(key) } => key,
                            _ => unreachable!(),
                        };
                        self.execute_update_by_key(
                            &update,
                            table_desc,
                            partition.id,
                            key,
                            &mut mode,
                            variables,
                        )
                    }
                    UpdateTarget::Filter(filter) => {
                        let (_, partitions) =
                            self.resolve_table_and_all_partitions(&update.table, catalog)?;
                        for partition in &partitions {
                            let _route = classify_route(
                                &BoundStatement::Update(update.clone()),
                                &partition.storage,
                            )?;
                        }
                        self.execute_update_by_filter(
                            &update,
                            table_desc,
                            filter.as_ref(),
                            catalog,
                            variables,
                            &mut mode,
                        )
                    }
                }
            }
            BoundStatement::DropTable(drop) => {
                let _route = classify_route(
                    &BoundStatement::DropTable(drop.clone()),
                    &StorageDescriptor::Row,
                )?;
                self.execute_drop_table(&drop, catalog)
            }
            BoundStatement::Show(show) => {
                let _route =
                    classify_route(&BoundStatement::Show(show.clone()), &StorageDescriptor::Row)?;
                Self::execute_show(&show, catalog)
            }
        }
    }

    /// Names of every base table referenced anywhere in a bound query (including derived
    /// tables, CTEs, subqueries, and set-operation branches).
    fn general_base_tables(query: &htap_sql::BoundQuery) -> Vec<String> {
        fn walk(query: &htap_sql::BoundQuery, out: &mut Vec<String>) {
            for sub in &query.subqueries {
                walk(sub, out);
            }
            match &query.body {
                htap_sql::QueryBody::Select(sel) => {
                    for slot in &sel.slots {
                        match slot {
                            htap_sql::TableSlot::Base { table, .. } => {
                                if !out.contains(table) {
                                    out.push(table.clone());
                                }
                            }
                            htap_sql::TableSlot::Derived { query, .. } => walk(query, out),
                        }
                    }
                }
                htap_sql::QueryBody::SetOp { left, right, .. } => {
                    walk(left, out);
                    walk(right, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(query, &mut out);
        out
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

        let mut high_water = catalog.id_high_water();
        let table_id = high_water.allocate_table()?;

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
            CatalogSnapshot::new(next_generation, tables, partitions, tablets, replicas)
                .with_id_high_water(high_water);

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
        self.alter_partitions_internal(table_name, &alteration)
    }

    fn alter_partitions_internal(
        &self,
        table_name: &str,
        alteration: &PartitionAlteration,
    ) -> Result<StatementResult> {
        let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);

        let source_partitions = catalog.source_partitions_for_alteration(table_name, alteration)?;

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

        let candidate = catalog.apply_partition_alteration(table_name, alteration)?;

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
        mode: &mut ExecMode,
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

        // A blind `INSERT` never reads, but autocommit's own 2PC commit still needs the snapshot
        // it would have read at (F4): taken here, before the commit below, exactly like a
        // statement that does read.
        let (statement_snapshot, _) = mode.read_view(self);
        self.commit_or_buffer(table_desc.id, mutations, mode, statement_snapshot)
    }

    fn execute_delete(
        &self,
        delete: DeleteByPrimaryKey,
        table_id: TableId,
        partition_id: PartitionId,
        mode: &mut ExecMode,
    ) -> Result<StatementResult> {
        let key = encode_key(&delete.key)?;
        let mutation = Mutation::Delete {
            partition_id: partition_id.as_u64(),
            key,
        };

        // See `execute_insert`: a blind `DELETE` never reads either, but still needs a statement
        // snapshot for autocommit's own commit (F4).
        let (statement_snapshot, _) = mode.read_view(self);
        self.commit_or_buffer(table_id, vec![mutation], mode, statement_snapshot)
    }

    /// `DROP TABLE`: removes the table and its partitions, tablets, and replicas from the
    /// catalog in one CAS. Storage is not reclaimed: rowstore data and columnar segments of
    /// the dropped partitions stay on disk, unreachable because their identifiers are never
    /// reissued (see [`IdHighWater`]). Tables with a partition in `Converting` state cannot be
    /// dropped until the conversion finishes.
    fn execute_drop_table(
        &self,
        drop: &DropTableStatement,
        catalog: &CatalogSnapshot,
    ) -> Result<StatementResult> {
        let Some(table_desc) = catalog.table_by_name(&drop.table) else {
            if drop.if_exists {
                return Ok(StatementResult::ddl(0));
            }
            return Err(HtapError::NotFound(format!(
                "table '{}' not found",
                drop.table
            )));
        };
        let table_id = table_desc.id;
        let dropped_partitions: Vec<&PartitionDescriptor> = catalog
            .partitions
            .iter()
            .filter(|p| p.table_id == table_id)
            .collect();
        if let Some(converting) = dropped_partitions
            .iter()
            .find(|p| matches!(p.storage, StorageDescriptor::Converting { .. }))
        {
            return Err(HtapError::Conflict(format!(
                "table '{}' cannot be dropped while partition '{}' is converting",
                drop.table, converting.name
            )));
        }
        let tablet_ids: std::collections::HashSet<TabletId> = dropped_partitions
            .iter()
            .flat_map(|p| p.tablets.iter().copied())
            .collect();
        let replica_ids: std::collections::HashSet<ReplicaId> = catalog
            .tablets
            .iter()
            .filter(|t| tablet_ids.contains(&t.id))
            .flat_map(|t| t.replicas.iter().copied())
            .collect();
        let next_generation =
            catalog
                .generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;
        let next = CatalogSnapshot::new(
            next_generation,
            catalog
                .tables
                .iter()
                .filter(|t| t.id != table_id)
                .cloned()
                .collect(),
            catalog
                .partitions
                .iter()
                .filter(|p| p.table_id != table_id)
                .cloned()
                .collect(),
            catalog
                .tablets
                .iter()
                .filter(|t| !tablet_ids.contains(&t.id))
                .cloned()
                .collect(),
            catalog
                .replicas
                .iter()
                .filter(|r| !replica_ids.contains(&r.id))
                .cloned()
                .collect(),
        )
        .with_id_high_water(catalog.id_high_water());
        self.catalog.compare_and_set(catalog.generation, next)?;
        Ok(StatementResult::ddl(1))
    }

    /// `SHOW TABLES / DATABASES / COLUMNS` and `DESCRIBE`, answered from the catalog.
    fn execute_show(show: &ShowStatement, catalog: &CatalogSnapshot) -> Result<StatementResult> {
        let string_col = |name: &str, nullable: bool| ColumnDef {
            name: name.to_string(),
            data_type: htap_common::types::DataType::String,
            nullable,
            primary_key: false,
        };
        match show {
            ShowStatement::Tables { like } => {
                let mut names: Vec<&str> = catalog
                    .tables
                    .iter()
                    .map(|t| t.name.as_str())
                    .filter(|n| {
                        like.as_deref()
                            .is_none_or(|p| htap_sql::expr::like_match(n, p))
                    })
                    .collect();
                names.sort_unstable();
                let rows = names
                    .into_iter()
                    .map(|n| Row::new(vec![Value::String(n.to_string())]))
                    .collect();
                Ok(StatementResult::query(
                    vec![string_col("Tables_in_htap", false)],
                    rows,
                ))
            }
            ShowStatement::Databases => Ok(StatementResult::query(
                vec![string_col("Database", false)],
                vec![Row::new(vec![Value::String("htap".into())])],
            )),
            ShowStatement::Columns { table } | ShowStatement::Describe { table } => {
                let table_desc = catalog
                    .table_by_name(table)
                    .ok_or_else(|| HtapError::NotFound(format!("table '{table}' not found")))?;
                let columns = vec![
                    string_col("Field", false),
                    string_col("Type", false),
                    string_col("Null", false),
                    string_col("Key", false),
                    string_col("Default", true),
                    string_col("Extra", false),
                ];
                let rows = table_desc
                    .schema
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(idx, c)| {
                        Row::new(vec![
                            Value::String(c.name.clone()),
                            Value::String(c.data_type.name().to_string()),
                            Value::String(if c.nullable { "YES" } else { "NO" }.into()),
                            Value::String(
                                if table_desc.primary_key.contains(&idx) {
                                    "PRI"
                                } else {
                                    ""
                                }
                                .into(),
                            ),
                            Value::Null,
                            Value::String(String::new()),
                        ])
                    })
                    .collect();
                Ok(StatementResult::query(columns, rows))
            }
        }
    }

    /// Applies UPDATE assignments left to right against the progressively updated row and
    /// enforces NOT NULL constraints on the result.
    fn apply_assignments(
        update: &UpdateStatement,
        table_desc: &TableDescriptor,
        row: &Row,
        variables: &dyn VariableLookup,
    ) -> Result<Row> {
        let mut values = row.values().to_vec();
        for (col_idx, expr) in &update.assignments {
            let ctx = htap_sql::EvalContext {
                row: &values,
                aggregates: &[],
                output: None,
                subqueries: &[],
                variables: Some(variables),
            };
            let value = expr.eval(&ctx)?;
            let col = table_desc.schema.column(*col_idx).ok_or_else(|| {
                HtapError::Internal(format!("assignment column index {col_idx} out of bounds"))
            })?;
            if value.is_null() && !col.nullable {
                return Err(HtapError::InvalidArgument(format!(
                    "column '{}' is NOT NULL",
                    col.name
                )));
            }
            if let Some(dt) = value.data_type() {
                if dt != col.data_type {
                    return Err(HtapError::InvalidArgument(format!(
                        "type mismatch for column '{}': expected {}, found {}",
                        col.name,
                        col.data_type.name(),
                        dt.name()
                    )));
                }
            }
            values[*col_idx] = value;
        }
        Ok(Row::new(values))
    }

    /// Commits (autocommit) or buffers (open session transaction) a batch of mutations as one
    /// unit, all belonging to `table_id`.
    ///
    /// `statement_snapshot` is the MVCC snapshot this statement actually read at — in autocommit
    /// mode, [`ExecMode::read_view`]'s `Snapshot::new(server.txn_manager.visible_version())` from
    /// before this statement did any reading, or, for a blind `INSERT`/`DELETE` that never reads,
    /// the equally valid snapshot taken just for this purpose (see the call sites). In
    /// [`ExecMode::Txn`] mode this is always `open_txn.snapshot` (the transaction's own pinned
    /// snapshot) and is ignored here (only autocommit's own 2PC commit needs it).
    ///
    /// In autocommit mode this commits with `Transaction::new(.., statement_snapshot.version)` +
    /// `TransactionManager::commit`, exactly like a session's own commit (plan amendment A3) —
    /// never `TransactionManager::commit_request`, whose internal `begin()` would instead pin
    /// `read_version` at the *current* visible version at commit time, which can have moved
    /// forward past `statement_snapshot` if a concurrent writer (e.g. an import, which does not
    /// take `execution_lock`) committed between this statement's read and its commit; that would
    /// defeat `Engine::prepare`'s first-writer-wins check and silently overwrite the concurrent
    /// writer's row (storage-reviewer finding F4: previously observable as a lost update on an
    /// autocommit `UPDATE`). A first-writer-wins conflict caught here is therefore correct,
    /// intentional behavior, not a regression: an autocommit `INSERT`/`UPDATE`/`DELETE` whose
    /// snapshot predates a concurrent writer that already touched the same key must now lose,
    /// exactly like an explicit transaction would.
    ///
    /// In [`ExecMode::Txn`] mode the mutations are staged into a statement-local [`WriteSet`]
    /// delta and merged into the transaction's write set only if the whole delta fits the payload
    /// cap (`WriteSet::try_merge`), so a statement that would overflow it fails without polluting
    /// the write set with a partial delta. A duplicate `(partition_id, key)` within this one
    /// statement's own `mutations` is rejected here with the same error
    /// `Engine::prepare`/`RowstoreParticipant::prepare` would give autocommit (storage-reviewer
    /// finding F6): without this check, `WriteSet`'s `(partition_id, key)`-keyed map would
    /// silently keep only the last row, unlike autocommit's `Engine::prepare`, which rejects the
    /// whole batch.
    fn commit_or_buffer(
        &self,
        table_id: TableId,
        mutations: Vec<Mutation>,
        mode: &mut ExecMode,
        statement_snapshot: Snapshot,
    ) -> Result<StatementResult> {
        if mutations.is_empty() {
            return Ok(StatementResult::dml(0, None));
        }
        let affected = mutations.len() as u64;
        match mode {
            ExecMode::Autocommit => {
                let payload = RowstoreParticipant::encode_payload(&mutations)?;
                let work = ParticipantWork::new(ParticipantId::new(1), payload);
                let request = TransactionRequest::new(vec![work])?;
                let mut txn =
                    Transaction::new(self.txn_manager.next_txn_id()?, statement_snapshot.version);
                txn.set_request(request);
                let committed = self.txn_manager.commit(&mut txn)?;
                Ok(StatementResult::dml(affected, Some(committed.version)))
            }
            ExecMode::Txn { write_set, .. } => {
                reject_duplicate_mutation_keys(&mutations)?;
                let mut delta = WriteSet::new();
                for mutation in mutations {
                    delta.insert(table_id, mutation)?;
                }
                write_set.try_merge(delta, self.txn_manager.max_frame_size())?;
                Ok(StatementResult::dml(affected, None))
            }
        }
    }

    /// Reads one row by exact key, consulting an open transaction's write set first
    /// ("read your own writes") before falling back to the MVCC snapshot.
    fn read_with_overlay(
        &self,
        partition_id: u64,
        key: &[u8],
        snapshot: Snapshot,
        write_set: Option<&WriteSet>,
    ) -> Result<Option<Row>> {
        if let Some(ws) = write_set {
            if let Some(buffered) = ws.get(partition_id, key) {
                return Ok(match &buffered.mutation {
                    Mutation::Put { row, .. } => Some(row.clone()),
                    Mutation::Delete { .. } => None,
                });
            }
        }
        self.engine.get(partition_id, key, snapshot)
    }

    /// Point UPDATE: read the row at the current snapshot (overlaid with any open transaction's
    /// write set), apply the assignments, and write the new version of the row under the same
    /// key.
    fn execute_update_by_key(
        &self,
        update: &UpdateStatement,
        table_desc: &TableDescriptor,
        partition_id: PartitionId,
        key: Vec<u8>,
        mode: &mut ExecMode,
        variables: &dyn VariableLookup,
    ) -> Result<StatementResult> {
        let (snapshot, write_set) = mode.read_view(self);
        let Some(row) = self.read_with_overlay(partition_id.as_u64(), &key, snapshot, write_set)?
        else {
            return Ok(StatementResult::dml(0, None));
        };
        let updated = Self::apply_assignments(update, table_desc, &row, variables)?;
        self.commit_or_buffer(
            table_desc.id,
            vec![Mutation::Put {
                partition_id: partition_id.as_u64(),
                key,
                row: updated,
            }],
            mode,
            snapshot,
        )
    }

    /// Scan UPDATE: read every partition at one snapshot (overlaid with any open transaction's
    /// write set), evaluate the filter, and commit or buffer all rewritten rows as one unit
    /// (bounded by the transaction payload cap).
    fn execute_update_by_filter(
        &self,
        update: &UpdateStatement,
        table_desc: &TableDescriptor,
        filter: Option<&htap_sql::Expr>,
        catalog: &CatalogSnapshot,
        variables: &dyn VariableLookup,
        mode: &mut ExecMode,
    ) -> Result<StatementResult> {
        let (snapshot, write_set) = mode.read_view(self);
        let ctx = query_exec::ExecContext {
            server: self,
            catalog,
            snapshot,
            write_set,
            variables: Some(variables),
        };
        let all_columns: std::collections::BTreeSet<usize> = (0..table_desc.schema.len()).collect();
        let rows = query_exec::scan_base_table(&ctx, &update.table, &all_columns, 0, &[])?;
        let mut mutations = Vec::new();
        for values in rows {
            if let Some(f) = filter {
                let filter_ctx = htap_sql::EvalContext {
                    row: &values,
                    aggregates: &[],
                    output: None,
                    subqueries: &[],
                    variables: Some(variables),
                };
                if !f.eval_predicate(&filter_ctx)? {
                    continue;
                }
            }
            let row = Row::new(values);
            let updated = Self::apply_assignments(update, table_desc, &row, variables)?;
            let pk_values: Vec<Value> = table_desc
                .primary_key
                .iter()
                .map(|&idx| {
                    updated.get(idx).cloned().ok_or_else(|| {
                        HtapError::Internal(format!("row missing primary key column index {idx}"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let partition_id = self.route_pk_to_partition(table_desc, &pk_values, catalog)?;
            mutations.push(Mutation::Put {
                partition_id: partition_id.as_u64(),
                key: encode_key(&pk_values)?,
                row: updated,
            });
        }
        self.commit_or_buffer(table_desc.id, mutations, mode, snapshot)
    }

    fn execute_select(
        &self,
        select: PointSelect,
        table_desc: &TableDescriptor,
        partition_id: PartitionId,
        key: Vec<u8>,
        mode: &ExecMode,
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

        let (snapshot, write_set) = mode.read_view(self);
        let row_opt = self.read_with_overlay(partition_id.as_u64(), &key, snapshot, write_set)?;

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
        mode: &ExecMode,
    ) -> Result<StatementResult> {
        // 1. Freeze catalog, snapshot, and plan
        let selected_partitions =
            olap::prune_partitions(table_desc, partitions, select.filter.as_ref());
        let (snapshot, write_set) = mode.read_view(self);
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
                        write_set,
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
                    let ws = write_set;

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
                                ws,
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
pub(crate) fn scan_partition_compact(
    engine: &Engine,
    colstore_dir: &Path,
    catalog: &CatalogSnapshot,
    partition: &PartitionDescriptor,
    snapshot: Snapshot,
    source_columns: &[usize],
    primary_key: &[usize],
    pushdown_predicate: Option<&Predicate>,
    write_set: Option<&WriteSet>,
) -> Result<Vec<Row>> {
    let tablet_id = partition.tablets[0];
    let tablet = catalog
        .tablet(tablet_id)
        .ok_or_else(|| HtapError::Internal(format!("tablet {tablet_id} not found")))?;

    // When a session write set overlay is active, every base row must carry its primary key
    // columns so it can be matched against buffered mutations (keyed by encoded primary key),
    // even if the statement itself did not project them. In autocommit (`write_set == None`)
    // `internal_columns` is exactly `source_columns`, so that path's scan request and output are
    // byte-for-byte unchanged from before Phase 10.
    let internal_columns: Vec<usize> = match write_set {
        Some(_) => {
            let mut cols: std::collections::BTreeSet<usize> =
                source_columns.iter().copied().collect();
            cols.extend(primary_key.iter().copied());
            cols.into_iter().collect()
        }
        None => source_columns.to_vec(),
    };

    let base_rows = match &partition.storage {
        StorageDescriptor::Row => {
            let entries = engine.scan_partition(partition.id.as_u64(), snapshot)?;
            let rows = htap_convert::collapse_entries_to_rows(&entries);
            let mut compact_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values = Vec::with_capacity(internal_columns.len());
                for &idx in &internal_columns {
                    let val = row.get(idx).cloned().ok_or_else(|| {
                        HtapError::Internal(format!("row missing column index {idx}"))
                    })?;
                    values.push(val);
                }
                compact_rows.push(Row::new(values));
            }
            compact_rows
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
            // Stale-snapshot-vs-columnar-base conflict (Phase 10 task 6a; storage-reviewer
            // finding F5).
            //
            // `cat_manifest.base_version` is the rowstore MVCC version at which this columnar
            // base was materialized: every row committed at or before it is baked into the
            // segment files. `htap_convert::read_column_partition[_compact]_core` already knows
            // how to answer a read whose snapshot predates `base_version` correctly *today*: it
            // falls back to reading the rowstore directly ("rowstore is authoritative for
            // historical reads before manifest base_version"), which works only because this
            // engine never compacts or garbage-collects old rowstore MVCC versions (see
            // docs/LIMITATIONS.md) — nothing has ever thrown away the data that predates the
            // columnar base.
            //
            // A one-shot autocommit read cannot rely on that indefinitely, though: conversion
            // pins `base_version` from `rowstore.visible_version()` (`htap_convert`), which reads
            // a *different* version counter than `TransactionManager::visible_version()` (the
            // source of an autocommit read's `snapshot`); the two are not updated atomically
            // together, so during a narrow window around a concurrent import's commit the
            // rowstore-side counter this conversion read can briefly be ahead of the manager-side
            // counter an autocommit statement's snapshot was just taken from. That made
            // `snapshot.version < base_version` observably possible for an autocommit read too
            // (contradicting an earlier version of this comment, which claimed it could
            // "structurally never hold" there) — a real bug, spuriously conflicting a plain
            // autocommit read with no open transaction to roll back. Autocommit therefore keeps
            // the prior fallback behavior unconditionally (fall through to the rowstore-backed
            // read below, which is correct precisely because this engine never compacts old
            // versions) and never runs this check at all.
            //
            // A session's explicit transaction (`write_set.is_some()`, i.e. `ExecMode::Txn`) is
            // different: it pins its snapshot once at `BEGIN` and can stay open across many
            // statements and an arbitrary amount of wall-clock time, during which another
            // connection can convert the table to columnar. Continuing to serve such a read via
            // the rowstore fallback would make the transaction's correctness depend forever on
            // "the rowstore never compacts old versions" — exactly the invariant a future
            // compaction/GC feature would break, with no pinned-snapshot registry (see DECISIONS
            // ADR-018) to keep the data reachable for as long as the transaction needs it. Rather
            // than take on that indefinite dependency, a stale read inside an explicit
            // transaction is treated as a hard, retryable conflict instead: the transaction is
            // poisoned and must be rolled back and retried against a fresher snapshot.
            if write_set.is_some() && snapshot.version < cat_manifest.base_version {
                return Err(HtapError::Conflict(format!(
                    "snapshot version {} predates columnar base version {} for partition {}: \
                     the table was converted to columnar storage after this transaction's \
                     snapshot was pinned; roll back and retry the transaction",
                    snapshot.version, cat_manifest.base_version, partition.id
                )));
            }
            let compact_res = htap_convert::read_column_partition_compact_core(
                catalog,
                engine,
                colstore_dir,
                partition.id,
                snapshot,
                &internal_columns,
                primary_key,
                pushdown_predicate.cloned(),
            )?;
            compact_res.rows
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
                // Same stale-snapshot-vs-columnar-base conflict as the `Column` branch above
                // (storage-reviewer finding F5: only checked for a transaction read); see its
                // comment for the full justification. A `Converting` partition already has a
                // manifest on disk once it reaches `SegmentsWritten`/`ReadyToPublish`, and that
                // manifest's `base_version` carries exactly the same meaning.
                if write_set.is_some() && snapshot.version < cat_manifest.base_version {
                    return Err(HtapError::Conflict(format!(
                        "snapshot version {} predates columnar base version {} for converting \
                         partition {}: the table was converted to columnar storage after this \
                         transaction's snapshot was pinned; roll back and retry the transaction",
                        snapshot.version, cat_manifest.base_version, partition.id
                    )));
                }
                let compact_res = htap_convert::read_column_partition_compact_core(
                    catalog,
                    engine,
                    colstore_dir,
                    partition.id,
                    snapshot,
                    &internal_columns,
                    primary_key,
                    pushdown_predicate.cloned(),
                )?;
                compact_res.rows
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
                        let mut values = Vec::with_capacity(internal_columns.len());
                        for &idx in &internal_columns {
                            let val = row.get(idx).cloned().ok_or_else(|| {
                                HtapError::Internal(format!("row missing column index {idx}"))
                            })?;
                            values.push(val);
                        }
                        compact_rows.push(Row::new(values));
                    }
                    compact_rows
                } else {
                    return Err(HtapError::InvalidArgument(format!(
                        "partition {} is converting without manifest but phase is not SnapshotPinned",
                        partition.id
                    )));
                }
            }
        },
    };

    match write_set {
        None => Ok(base_rows),
        Some(ws) => session::overlay_rows(
            base_rows,
            &internal_columns,
            source_columns,
            primary_key,
            partition.id.as_u64(),
            ws,
        ),
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

    /// Seeds the identifier high-water mark of a catalog written before the mark existed
    /// (format version 1, mark all zeros). Such catalogs may already have removed partitions
    /// (`ALTER TABLE ... DROP PARTITION`) whose tablet directories still exist under
    /// `colstore/`, so the live maximum is not enough: the tablet counter is raised to the
    /// highest tablet directory found on disk. Rowstore data of partitions dropped under
    /// version 1 is always logically empty (DROP PARTITION requires it), so partition ids
    /// only need the live maximum. The result is persisted with a CAS so the next
    /// allocation starts above every id that has ever been used.
    fn migrate_legacy_id_high_water(
        catalog: &LocalCatalogStore,
        colstore_dir: &Path,
    ) -> Result<()> {
        let Some(snapshot) = catalog.load()? else {
            return Ok(());
        };
        if snapshot.id_high_water != IdHighWater::default() {
            return Ok(());
        }
        let mut physical_tablet_max = 0u64;
        for entry in std::fs::read_dir(colstore_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let id = name
                .strip_prefix("tablet-")
                .or_else(|| name.strip_prefix("tablet_"))
                .unwrap_or(name);
            if let Ok(id) = id.parse::<u64>() {
                physical_tablet_max = physical_tablet_max.max(id);
            }
        }
        let mut mark = snapshot.id_high_water();
        mark.tablet = mark.tablet.max(physical_tablet_max);
        if mark == IdHighWater::default() {
            return Ok(());
        }
        let next_generation =
            snapshot
                .generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;
        let mut next = snapshot.clone();
        next.generation = next_generation;
        next.id_high_water = mark;
        catalog.compare_and_set(snapshot.generation, next)?;
        Ok(())
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
