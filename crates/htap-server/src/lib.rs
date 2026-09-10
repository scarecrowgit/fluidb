//! Synchronous local single-node HTAP database server.
//!
//! Provides [`LocalServer`], an embedded synchronous server coordinating SQL
//! parsing, semantic binding, catalog topology updates, transaction management,
//! LSM rowstore storage, and data movement operations via [`LocalServerDataMover`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::encode_key;
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, Mutation, Row, Value};
use htap_movement::{
    CopyOptions, CopyReport, LocalDataMover, MovementJob, TabletCloneOptions, TabletPackageManifest,
};
use htap_rowstore::{Engine, EngineOptions, Snapshot};
use htap_sql::ast::{BoundStatement, CreateTable, DeleteByPrimaryKey, Insert, PointSelect};
use htap_sql::result::StatementResult;
use htap_sql::route::{classify_route, Route};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};
use parking_lot::Mutex;

/// Synchronous local database server.
///
/// Encapsulates catalog metadata management, transactional write logging,
/// LSM-based rowstore persistence, and data movement within a single local directory.
pub struct LocalServer {
    catalog: LocalCatalogStore,
    engine: Arc<Engine>,
    txn_manager: TransactionManager,
    data_mover: LocalDataMover,
    execution_lock: Mutex<()>,
}

impl LocalServer {
    /// Opens or recovers a local server instance rooted at `root`.
    ///
    /// The root directory layout contains:
    /// - `root/catalog` - Directory for durable catalog snapshots.
    /// - `root/rowstore` - Directory for rowstore LSM data (WAL, SSTs, manifest).
    /// - `root/txn.journal` - Journal file for 2PC transaction coordination.
    /// - `root/movement` - Directory for data movement jobs and tablet packages.
    ///
    /// Recovery registers a stable [`RowstoreParticipant`] with ID 1 and
    /// replays committed transactions from the transaction journal.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError`] if initialization, catalog loading, or transaction
    /// recovery fails.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;

        let catalog_dir = root.join("catalog");
        let catalog = LocalCatalogStore::open(catalog_dir)?;

        let rowstore_dir = root.join("rowstore");
        let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir))?);

        let txn_journal_path = root.join("txn.journal");
        let txn_manager = TransactionManager::open(txn_journal_path)?;

        let participant = Arc::new(RowstoreParticipant::new(
            ParticipantId::new(1),
            Arc::clone(&engine),
        ));
        txn_manager.register_participant(participant);

        txn_manager.recover()?;

        let movement_dir = root.join("movement");
        let data_mover = LocalDataMover::new(movement_dir)?;

        Ok(Self {
            catalog,
            engine,
            txn_manager,
            data_mover,
            execution_lock: Mutex::new(()),
        })
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
                let (table_desc, partition) =
                    self.resolve_single_partition_table(&insert.table, &catalog)?;
                let _route =
                    classify_route(&BoundStatement::Insert(insert.clone()), &partition.storage)?;
                self.execute_insert(insert, table_desc, partition.id)
            }
            BoundStatement::Delete(delete) => {
                let (_table_desc, partition) =
                    self.resolve_single_partition_table(&delete.table, &catalog)?;
                let _route =
                    classify_route(&BoundStatement::Delete(delete.clone()), &partition.storage)?;
                self.execute_delete(delete, partition.id)
            }
            BoundStatement::Select(select) => {
                let (table_desc, partition) =
                    self.resolve_single_partition_table(&select.table, &catalog)?;
                let route =
                    classify_route(&BoundStatement::Select(select.clone()), &partition.storage)?;
                let key = match route {
                    Route::RowstorePointRead { key } => key,
                    _ => unreachable!(),
                };
                self.execute_select(select, table_desc, partition.id, key)
            }
        }
    }

    fn resolve_single_partition_table<'a>(
        &self,
        table_name: &str,
        catalog: &'a CatalogSnapshot,
    ) -> Result<(&'a TableDescriptor, &'a PartitionDescriptor)> {
        let table_desc = catalog
            .table_by_name(table_name)
            .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))?;

        if table_desc.partitions.len() != 1 {
            return Err(HtapError::Unsupported(format!(
                "table '{table_name}' must have exactly one partition, found {}",
                table_desc.partitions.len()
            )));
        }

        let partition_id = table_desc.partitions[0];
        let partition = catalog.partition(partition_id).ok_or_else(|| {
            HtapError::Internal(format!(
                "partition {partition_id} referenced by table '{table_name}' not found in catalog"
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

        Ok((table_desc, partition))
    }

    fn execute_create_table(
        &self,
        create: CreateTable,
        catalog: &CatalogSnapshot,
    ) -> Result<StatementResult> {
        if catalog.table_by_name(&create.name).is_some() {
            return Err(HtapError::Conflict(format!(
                "table '{}' already exists",
                create.name
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
            .ok_or_else(|| HtapError::Internal("TableId overflow".into()))?;
        let table_id = TableId::new(next_table_id);

        let max_partition_id = catalog
            .partitions
            .iter()
            .map(|p| p.id.as_u64())
            .max()
            .unwrap_or(0);
        let next_partition_id = max_partition_id
            .checked_add(1)
            .ok_or_else(|| HtapError::Internal("PartitionId overflow".into()))?;
        let partition_id = PartitionId::new(next_partition_id);

        let max_tablet_id = catalog
            .tablets
            .iter()
            .map(|t| t.id.as_u64())
            .max()
            .unwrap_or(0);
        let next_tablet_id = max_tablet_id
            .checked_add(1)
            .ok_or_else(|| HtapError::Internal("TabletId overflow".into()))?;
        let tablet_id = TabletId::new(next_tablet_id);

        let max_replica_id = catalog
            .replicas
            .iter()
            .map(|r| r.id.as_u64())
            .max()
            .unwrap_or(0);
        let next_replica_id = max_replica_id
            .checked_add(1)
            .ok_or_else(|| HtapError::Internal("ReplicaId overflow".into()))?;
        let replica_id = ReplicaId::new(next_replica_id);

        let next_generation = catalog
            .generation
            .checked_add(1)
            .ok_or_else(|| HtapError::Internal("Catalog generation overflow".into()))?;

        let table_desc = TableDescriptor::new(
            table_id,
            create.name,
            create.schema,
            create.primary_key,
            vec![partition_id],
            next_generation,
        );

        let partition_desc = PartitionDescriptor::new(
            partition_id,
            table_id,
            "p0",
            StorageDescriptor::Row,
            vec![tablet_id],
            next_generation,
        );

        let tablet_desc = TabletDescriptor::new(
            tablet_id,
            partition_id,
            0,
            vec![replica_id],
            next_generation,
        );

        let replica_desc = ReplicaDescriptor::new(
            replica_id,
            tablet_id,
            NodeId::new(1),
            true,
            true,
            next_generation,
        );

        let mut tables = catalog.tables.clone();
        tables.push(table_desc);

        let mut partitions = catalog.partitions.clone();
        partitions.push(partition_desc);

        let mut tablets = catalog.tablets.clone();
        tablets.push(tablet_desc);

        let mut replicas = catalog.replicas.clone();
        replicas.push(replica_desc);

        let next_snapshot =
            CatalogSnapshot::new(next_generation, tables, partitions, tablets, replicas);

        self.catalog
            .compare_and_set(catalog.generation, next_snapshot)?;

        Ok(StatementResult::ddl(1))
    }

    fn execute_insert(
        &self,
        insert: Insert,
        table_desc: &TableDescriptor,
        partition_id: PartitionId,
    ) -> Result<StatementResult> {
        let mut mutations = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
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
