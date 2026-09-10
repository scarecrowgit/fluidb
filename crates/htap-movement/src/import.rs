//! Streaming import execution for CSV and JSONLines data formats.
//!
//! # Consistency and Crash-Recovery Semantics
//! - **Topology Validation**: Exactly one local partition, one local tablet, and one healthy
//!   leader replica are resolved from the catalog store.
//! - **Bounded Batching**: Storage mutations ([`Mutation::Put`]) are accumulated in bounded
//!   batches up to [`CopyOptions::batch_rows`] and committed synchronously through
//!   [`htap_txn::TransactionManager::commit_request`] with participant ID 1.
//! - **Durable Progress**: Job progress counters are committed to disk only **after** the
//!   transaction commit succeeds.
//! - **Commit-Before-Checkpoint Replay Window**: Because transactions commit in the rowstore
//!   before the job envelope checkpoint is written to disk, a crash between commit and
//!   checkpoint creates a replay window. On resume, uncheckpointed records will be replayed.
//!   Because imports apply primary-key upserts ([`Mutation::Put`]), replaying rows is
//!   semantically idempotent at the rowstore level. No exactly-once delivery across crashes
//!   is claimed.
//! - **Resume & Non-Seekable Streams**: File-based wrappers reopen the file on resume and
//!   discard exactly the checkpointed logical record count. Non-seekable reader streams
//!   return [`HtapError::Unsupported`] on resume.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::time::Instant;

use htap_catalog::store::CatalogStore;
use htap_catalog::{PartitionId, TableDescriptor, TableId, TabletId};
use htap_common::{HtapError, Mutation, Result, Row};
use htap_txn::{ParticipantId, ParticipantWork, TransactionManager, TransactionRequest};

use crate::codec::{decode_csv_record, decode_json_line, CsvHeaderMap};
use crate::job::{
    CopyOptions, CopyReport, DataFormat, JobCounters, LocalDataMover, MovementJobKind,
};

/// Resolved local table topology required for single-tablet data movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTopology {
    /// Target table metadata.
    pub table_desc: TableDescriptor,
    /// Target partition identifier.
    pub partition_id: PartitionId,
    /// Target tablet identifier.
    pub tablet_id: TabletId,
    /// Primary key column indices into table schema in order.
    pub pk_indices: Vec<usize>,
}

/// Validate and resolve single-tablet local topology from a [`CatalogStore`].
///
/// Ensures:
/// 1. Table exists.
/// 2. Table has exactly one partition.
/// 3. Partition has exactly one tablet matching `expected_tablet_id`.
/// 4. Tablet has exactly one healthy leader replica.
/// 5. Table has a non-empty primary key.
pub fn resolve_table_topology(
    catalog: &dyn CatalogStore,
    table_id: TableId,
    expected_tablet_id: TabletId,
) -> Result<ResolvedTopology> {
    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let table = snapshot
        .table(table_id)
        .ok_or_else(|| HtapError::NotFound(format!("table {table_id} not found")))?
        .clone();

    let partitions = snapshot.table_partitions(table.id);
    if partitions.len() != 1 {
        return Err(HtapError::InvalidArgument(format!(
            "table {table_id} must have exactly 1 partition for local data movement, found {}",
            partitions.len()
        )));
    }
    let partition = partitions[0];

    let tablets = snapshot.partition_tablets(partition.id);
    if tablets.len() != 1 {
        return Err(HtapError::InvalidArgument(format!(
            "partition {} must have exactly 1 tablet, found {}",
            partition.id,
            tablets.len()
        )));
    }
    let tablet = tablets[0];

    if tablet.id != expected_tablet_id {
        return Err(HtapError::InvalidArgument(format!(
            "requested tablet ID {} does not match partition tablet ID {}",
            expected_tablet_id, tablet.id
        )));
    }

    let replicas = snapshot.tablet_replicas(tablet.id);
    let healthy_leaders: Vec<_> = replicas
        .iter()
        .filter(|r| r.is_leader && r.healthy)
        .collect();

    if healthy_leaders.len() != 1 {
        return Err(HtapError::Internal(format!(
            "tablet {} must have exactly 1 healthy leader replica, found {}",
            tablet.id,
            healthy_leaders.len()
        )));
    }

    let pk_indices = if !table.primary_key.is_empty() {
        table.primary_key.clone()
    } else {
        let indices = table.schema.primary_key_indices();
        if indices.is_empty() {
            return Err(HtapError::InvalidArgument(format!(
                "table {table_id} has no primary key columns defined"
            )));
        }
        indices
    };

    Ok(ResolvedTopology {
        table_desc: table,
        partition_id: partition.id,
        tablet_id: tablet.id,
        pk_indices,
    })
}

/// Import records from a generic CSV [`Read`] stream.
///
/// # Headers
/// If [`CopyOptions::has_header`] is `true` (default), the first line is parsed as column headers
/// and matched against schema column names.
/// If [`CopyOptions::has_header`] is `false`, the first line is treated as data; fields are
/// interpreted in schema declaration order, requiring exactly `schema.len()` fields.
///
/// # Resume Behavior
/// If this job was previously started and checkpointed progress, resuming from a
/// non-seekable stream is unsupported and returns [`HtapError::Unsupported`].
pub fn copy_from_csv_reader<R: Read>(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    txn_manager: &TransactionManager,
    reader: R,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let job = mover.start_copy(MovementJobKind::Import, options)?;
    if job.is_complete() {
        return Ok(job.to_report(0));
    }
    if job.is_failed() {
        return Err(HtapError::Conflict(format!(
            "job '{}' previously failed: {}",
            job.job_id(),
            job.error.as_deref().unwrap_or("unknown error")
        )));
    }

    if job.counters.records_read > 0
        || job.counters.records_committed > 0
        || job.checkpoint.is_some()
    {
        return Err(HtapError::Unsupported(
            "resuming from a non-seekable reader is unsupported; use file-based import or submit a new job".into(),
        ));
    }

    let topology = resolve_table_topology(catalog, options.table_id, options.tablet_id)?;

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(options.has_header)
        .from_reader(reader);

    let header_map = if options.has_header {
        let raw_headers = rdr
            .headers()
            .map_err(|e| HtapError::InvalidArgument(format!("failed to read CSV header: {e}")))?;
        CsvHeaderMap::parse(&topology.table_desc.schema, raw_headers)?
    } else {
        CsvHeaderMap::from_schema(&topology.table_desc.schema)
    };

    let mut counters = job.counters;
    let mut batch = Vec::with_capacity(options.batch_rows);
    let mut record = csv::StringRecord::new();

    loop {
        match rdr.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {
                counters.records_read += 1;
                match decode_csv_record(&topology.table_desc.schema, &header_map, &record) {
                    Ok(row) => match build_mutation(&topology, row) {
                        Ok(mutation) => {
                            batch.push(mutation);
                            if batch.len() >= options.batch_rows {
                                commit_batch(
                                    mover,
                                    &options.job_id,
                                    txn_manager,
                                    &batch,
                                    &mut counters,
                                )?;
                                batch.clear();
                            }
                        }
                        Err(e) => {
                            if counters.records_skipped < options.max_errors as u64 {
                                counters.records_skipped += 1;
                            } else {
                                let _ = mover.fail_job(&options.job_id, e.to_string());
                                return Err(e);
                            }
                        }
                    },
                    Err(e) => {
                        if counters.records_skipped < options.max_errors as u64 {
                            counters.records_skipped += 1;
                        } else {
                            let _ = mover.fail_job(&options.job_id, e.to_string());
                            return Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                counters.records_read += 1;
                let err = HtapError::InvalidArgument(format!("malformed CSV record: {e}"));
                if counters.records_skipped < options.max_errors as u64 {
                    counters.records_skipped += 1;
                } else {
                    let _ = mover.fail_job(&options.job_id, err.to_string());
                    return Err(err);
                }
            }
        }
    }

    if !batch.is_empty() {
        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
        batch.clear();
    }

    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: counters.records_read,
        records_committed: counters.records_committed,
        rows_written: counters.rows_written,
        records_skipped: counters.records_skipped,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };
    mover.complete_job(&options.job_id, report.clone())?;

    Ok(report)
}

/// Import records from a generic JSONLines [`Read`] stream.
///
/// # Resume Behavior
/// If this job was previously started and checkpointed progress, resuming from a
/// non-seekable stream is unsupported and returns [`HtapError::Unsupported`].
pub fn copy_from_jsonl_reader<R: Read>(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    txn_manager: &TransactionManager,
    reader: R,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let job = mover.start_copy(MovementJobKind::Import, options)?;
    if job.is_complete() {
        return Ok(job.to_report(0));
    }
    if job.is_failed() {
        return Err(HtapError::Conflict(format!(
            "job '{}' previously failed: {}",
            job.job_id(),
            job.error.as_deref().unwrap_or("unknown error")
        )));
    }

    if job.counters.records_read > 0
        || job.counters.records_committed > 0
        || job.checkpoint.is_some()
    {
        return Err(HtapError::Unsupported(
            "resuming from a non-seekable reader is unsupported; use file-based import or submit a new job".into(),
        ));
    }

    let topology = resolve_table_topology(catalog, options.table_id, options.tablet_id)?;
    let mut buf_reader = BufReader::new(reader);

    let mut counters = job.counters;
    let mut batch = Vec::with_capacity(options.batch_rows);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let bytes_read = buf_reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        counters.records_read += 1;
        match decode_json_line(&topology.table_desc.schema, &line_buf) {
            Ok(row) => match build_mutation(&topology, row) {
                Ok(mutation) => {
                    batch.push(mutation);
                    if batch.len() >= options.batch_rows {
                        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
                        batch.clear();
                    }
                }
                Err(e) => {
                    if counters.records_skipped < options.max_errors as u64 {
                        counters.records_skipped += 1;
                    } else {
                        let _ = mover.fail_job(&options.job_id, e.to_string());
                        return Err(e);
                    }
                }
            },
            Err(e) => {
                if counters.records_skipped < options.max_errors as u64 {
                    counters.records_skipped += 1;
                } else {
                    let _ = mover.fail_job(&options.job_id, e.to_string());
                    return Err(e);
                }
            }
        }
    }

    if !batch.is_empty() {
        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
        batch.clear();
    }

    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: counters.records_read,
        records_committed: counters.records_committed,
        rows_written: counters.rows_written,
        records_skipped: counters.records_skipped,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };
    mover.complete_job(&options.job_id, report.clone())?;

    Ok(report)
}

/// Import records from a CSV file at `options.path`.
///
/// # Headers
/// If [`CopyOptions::has_header`] is `true` (default), the first line is parsed as column headers
/// and matched against schema column names.
/// If [`CopyOptions::has_header`] is `false`, the first line is treated as data; fields are
/// interpreted in schema declaration order, requiring exactly `schema.len()` fields.
///
/// On resume, reopens the source file and discards exactly the checkpointed logical records.
pub fn copy_from_csv(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    txn_manager: &TransactionManager,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let job = mover.start_copy(MovementJobKind::Import, options)?;
    if job.is_complete() {
        return Ok(job.to_report(0));
    }
    if job.is_failed() {
        return Err(HtapError::Conflict(format!(
            "job '{}' previously failed: {}",
            job.job_id(),
            job.error.as_deref().unwrap_or("unknown error")
        )));
    }

    let topology = resolve_table_topology(catalog, options.table_id, options.tablet_id)?;

    let file = File::open(&options.path)?;
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(options.has_header)
        .from_reader(file);

    let header_map = if options.has_header {
        let raw_headers = rdr
            .headers()
            .map_err(|e| HtapError::InvalidArgument(format!("failed to read CSV header: {e}")))?;
        CsvHeaderMap::parse(&topology.table_desc.schema, raw_headers)?
    } else {
        CsvHeaderMap::from_schema(&topology.table_desc.schema)
    };

    let mut counters = job.counters;
    let records_to_discard = job.counters.records_read;
    let mut record = csv::StringRecord::new();

    // Discard exactly checkpointed logical records
    for _ in 0..records_to_discard {
        match rdr.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => {
                return Err(HtapError::InvalidArgument(format!(
                    "error discarding checkpointed CSV record: {e}"
                )))
            }
        }
    }

    let mut batch = Vec::with_capacity(options.batch_rows);

    loop {
        match rdr.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {
                counters.records_read += 1;
                match decode_csv_record(&topology.table_desc.schema, &header_map, &record) {
                    Ok(row) => match build_mutation(&topology, row) {
                        Ok(mutation) => {
                            batch.push(mutation);
                            if batch.len() >= options.batch_rows {
                                commit_batch(
                                    mover,
                                    &options.job_id,
                                    txn_manager,
                                    &batch,
                                    &mut counters,
                                )?;
                                batch.clear();
                            }
                        }
                        Err(e) => {
                            if counters.records_skipped < options.max_errors as u64 {
                                counters.records_skipped += 1;
                            } else {
                                let _ = mover.fail_job(&options.job_id, e.to_string());
                                return Err(e);
                            }
                        }
                    },
                    Err(e) => {
                        if counters.records_skipped < options.max_errors as u64 {
                            counters.records_skipped += 1;
                        } else {
                            let _ = mover.fail_job(&options.job_id, e.to_string());
                            return Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                counters.records_read += 1;
                let err = HtapError::InvalidArgument(format!("malformed CSV record: {e}"));
                if counters.records_skipped < options.max_errors as u64 {
                    counters.records_skipped += 1;
                } else {
                    let _ = mover.fail_job(&options.job_id, err.to_string());
                    return Err(err);
                }
            }
        }
    }

    if !batch.is_empty() {
        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
        batch.clear();
    }

    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: counters.records_read,
        records_committed: counters.records_committed,
        rows_written: counters.rows_written,
        records_skipped: counters.records_skipped,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };
    mover.complete_job(&options.job_id, report.clone())?;

    Ok(report)
}

/// Import records from a JSONLines file at `options.path`.
///
/// On resume, reopens the source file and discards exactly the checkpointed logical records.
pub fn copy_from_jsonl(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    txn_manager: &TransactionManager,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let job = mover.start_copy(MovementJobKind::Import, options)?;
    if job.is_complete() {
        return Ok(job.to_report(0));
    }
    if job.is_failed() {
        return Err(HtapError::Conflict(format!(
            "job '{}' previously failed: {}",
            job.job_id(),
            job.error.as_deref().unwrap_or("unknown error")
        )));
    }

    let topology = resolve_table_topology(catalog, options.table_id, options.tablet_id)?;

    let file = File::open(&options.path)?;
    let mut buf_reader = BufReader::new(file);

    let mut counters = job.counters;
    let records_to_discard = job.counters.records_read;
    let mut line_buf = String::new();

    // Discard exactly checkpointed logical records
    for _ in 0..records_to_discard {
        line_buf.clear();
        if buf_reader.read_line(&mut line_buf)? == 0 {
            break;
        }
    }

    let mut batch = Vec::with_capacity(options.batch_rows);

    loop {
        line_buf.clear();
        let bytes_read = buf_reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        counters.records_read += 1;
        match decode_json_line(&topology.table_desc.schema, &line_buf) {
            Ok(row) => match build_mutation(&topology, row) {
                Ok(mutation) => {
                    batch.push(mutation);
                    if batch.len() >= options.batch_rows {
                        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
                        batch.clear();
                    }
                }
                Err(e) => {
                    if counters.records_skipped < options.max_errors as u64 {
                        counters.records_skipped += 1;
                    } else {
                        let _ = mover.fail_job(&options.job_id, e.to_string());
                        return Err(e);
                    }
                }
            },
            Err(e) => {
                if counters.records_skipped < options.max_errors as u64 {
                    counters.records_skipped += 1;
                } else {
                    let _ = mover.fail_job(&options.job_id, e.to_string());
                    return Err(e);
                }
            }
        }
    }

    if !batch.is_empty() {
        commit_batch(mover, &options.job_id, txn_manager, &batch, &mut counters)?;
        batch.clear();
    }

    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: counters.records_read,
        records_committed: counters.records_committed,
        rows_written: counters.rows_written,
        records_skipped: counters.records_skipped,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };
    mover.complete_job(&options.job_id, report.clone())?;

    Ok(report)
}

/// Execute import according to [`CopyOptions::format`].
pub fn import(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    txn_manager: &TransactionManager,
) -> Result<CopyReport> {
    match options.format {
        DataFormat::Csv => copy_from_csv(mover, options, catalog, txn_manager),
        DataFormat::JsonLines => copy_from_jsonl(mover, options, catalog, txn_manager),
    }
}

fn build_mutation(topology: &ResolvedTopology, row: Row) -> Result<Mutation> {
    let mut pk_values = Vec::with_capacity(topology.pk_indices.len());
    for &idx in &topology.pk_indices {
        let val = row.get(idx).ok_or_else(|| {
            HtapError::InvalidArgument(format!("row missing primary key column at index {idx}"))
        })?;
        pk_values.push(val.clone());
    }

    let key = htap_common::keycodec::encode_key(&pk_values)?;
    Ok(Mutation::Put {
        partition_id: topology.partition_id.as_u64(),
        key,
        row,
    })
}

fn commit_batch(
    mover: &LocalDataMover,
    job_id: &str,
    txn_manager: &TransactionManager,
    batch: &[Mutation],
    counters: &mut JobCounters,
) -> Result<()> {
    let payload = htap_txn::RowstoreParticipant::encode_payload(batch)?;
    let work = ParticipantWork::new(ParticipantId::new(1), payload);
    let req = TransactionRequest::new(vec![work])?;

    // Commit synchronously with participant 1
    txn_manager.commit_request(req)?;

    counters.records_committed += batch.len() as u64;
    counters.rows_written += batch.len() as u64;

    // Persist progress only after transaction successfully commits
    mover.commit_progress(job_id, *counters, Some(counters.records_read.to_string()))?;

    Ok(())
}
