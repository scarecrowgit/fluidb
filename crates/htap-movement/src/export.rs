//! Streaming export execution for CSV and JSONLines data formats.
//!
//! # Consistency and Snapshot Semantics
//! - **Pinned Snapshot**: A rowstore MVCC [`Snapshot`] is pinned exactly once at the beginning
//!   of export (honoring [`CopyOptions::pinned_version`] if provided, or capturing the current
//!   visible version). All subsequent writes to the rowstore are strictly excluded from the scan.
//! - **MVCC & Tombstone Collapse**: Raw partition entries from [`Engine::scan_partition`] are
//!   collapsed by user key, taking the newest visible version. Deleted keys (tombstones) are
//!   discarded.
//! - **Deterministic Order**: Because partition entries are sorted ascending by encoded primary key,
//!   exported rows are output in deterministic encoded-PK order.
//! - **Atomic File Persistence**: File outputs write to a temporary file (`.tmp`), fsync
//!   the file data, atomically rename to the target path, and fsync parent directory metadata.
//! - **Terminal Idempotence**: Completed jobs immediately return their recorded report as a no-op
//!   without re-executing storage scans or disk writes.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use htap_catalog::store::CatalogStore;
use htap_common::{HtapError, Result, Row};
use htap_rowstore::{Engine, MemtableEntry, Snapshot, ValueKind};

use crate::codec::{encode_csv_header, encode_csv_record, encode_json_line};
use crate::import::resolve_table_topology;
use crate::job::{sync_dir, CopyOptions, CopyReport, DataFormat, LocalDataMover, MovementJobKind};

/// Collapses raw rowstore MVCC and tombstone entries for a partition into logical visible rows.
///
/// Entries from `scan_partition` are ordered by user key ascending and version descending.
/// The first entry encountered for any distinct user key represents its latest visible MVCC state.
/// If that entry is a [`ValueKind::Put`], its row is retained; if it is a [`ValueKind::Delete`],
/// the key is tombstoned and omitted. Subsequent older versions for the same key are discarded.
pub fn collapse_entries_to_rows(entries: &[MemtableEntry]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut current_user_key: Option<&[u8]> = None;

    for entry in entries {
        if current_user_key == Some(entry.key.user_key.as_slice()) {
            continue;
        }
        current_user_key = Some(entry.key.user_key.as_slice());
        match &entry.value {
            ValueKind::Put(row) => rows.push(row.clone()),
            ValueKind::Delete => {}
        }
    }

    rows
}

/// Export partition records to a generic CSV [`Write`] stream.
///
/// Pins the engine snapshot, collapses MVCC versions/tombstones, and formats
/// rows in deterministic encoded-PK order.
pub fn copy_to_csv_writer<W: Write>(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
    writer: W,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let _tablet_lease = mover.acquire_tablet_leases(&[options.tablet_id])?;
    let job = mover.start_copy(MovementJobKind::Export, options)?;
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

    // Pin MVCC snapshot once
    let snapshot = options
        .pinned_version
        .map(Snapshot::new)
        .unwrap_or_else(|| engine.snapshot());

    let entries = engine.scan_partition(topology.partition_id.as_u64(), snapshot)?;
    let rows = collapse_entries_to_rows(&entries);

    let mut wtr = csv::WriterBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(options.has_header)
        .from_writer(writer);

    if options.has_header {
        let header = encode_csv_header(&topology.table_desc.schema);
        wtr.write_record(&header)
            .map_err(|e| HtapError::Io(e.into()))?;
    }

    for row in &rows {
        let rec = encode_csv_record(&topology.table_desc.schema, row);
        wtr.write_record(&rec)
            .map_err(|e| HtapError::Io(e.into()))?;
    }
    wtr.flush().map_err(HtapError::Io)?;

    let row_count = rows.len() as u64;
    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: row_count,
        records_committed: row_count,
        rows_written: row_count,
        records_skipped: 0,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };

    mover.complete_job(&options.job_id, report.clone())?;
    Ok(report)
}

/// Export partition records to a generic JSONLines [`Write`] stream.
///
/// Pins the engine snapshot, collapses MVCC versions/tombstones, and formats
/// rows in deterministic encoded-PK order.
pub fn copy_to_jsonl_writer<W: Write>(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
    writer: W,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let _tablet_lease = mover.acquire_tablet_leases(&[options.tablet_id])?;
    let job = mover.start_copy(MovementJobKind::Export, options)?;
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

    // Pin MVCC snapshot once
    let snapshot = options
        .pinned_version
        .map(Snapshot::new)
        .unwrap_or_else(|| engine.snapshot());

    let entries = engine.scan_partition(topology.partition_id.as_u64(), snapshot)?;
    let rows = collapse_entries_to_rows(&entries);

    let mut buf_wtr = BufWriter::new(writer);
    for row in &rows {
        let line = encode_json_line(&topology.table_desc.schema, row)?;
        buf_wtr.write_all(line.as_bytes())?;
        buf_wtr.write_all(b"\n")?;
    }
    buf_wtr.flush()?;

    let row_count = rows.len() as u64;
    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: row_count,
        records_committed: row_count,
        rows_written: row_count,
        records_skipped: 0,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };

    mover.complete_job(&options.job_id, report.clone())?;
    Ok(report)
}

/// Export partition records to a CSV file at `options.path` with atomic temp+fsync+rename semantics.
pub fn copy_to_csv(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
) -> Result<CopyReport> {
    export_to_file(mover, options, catalog, engine, DataFormat::Csv)
}

/// Export partition records to a JSONLines file at `options.path` with atomic temp+fsync+rename semantics.
pub fn copy_to_jsonl(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
) -> Result<CopyReport> {
    export_to_file(mover, options, catalog, engine, DataFormat::JsonLines)
}

/// Export partition records according to [`CopyOptions::format`].
pub fn export(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
) -> Result<CopyReport> {
    match options.format {
        DataFormat::Csv => copy_to_csv(mover, options, catalog, engine),
        DataFormat::JsonLines => copy_to_jsonl(mover, options, catalog, engine),
    }
}

fn export_to_file(
    mover: &LocalDataMover,
    options: &CopyOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
    format: DataFormat,
) -> Result<CopyReport> {
    options.validate()?;
    let start_instant = Instant::now();

    let _tablet_lease = mover.acquire_tablet_leases(&[options.tablet_id])?;
    let job = mover.start_copy(MovementJobKind::Export, options)?;
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

    // 1. Pin snapshot and scan partition
    let snapshot = options
        .pinned_version
        .map(Snapshot::new)
        .unwrap_or_else(|| engine.snapshot());

    let entries = engine.scan_partition(topology.partition_id.as_u64(), snapshot)?;
    let rows = collapse_entries_to_rows(&entries);

    // 2. Prepare atomic destination and temporary file path
    let dest_path = &options.path;
    let parent_dir = dest_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent_dir)?;

    let file_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("export");
    let tmp_path = parent_dir.join(format!(".{file_name}.{}.tmp", options.job_id));

    // 3. Write temp file and fsync
    let write_res = (|| -> Result<()> {
        let file = File::create(&tmp_path)?;
        match format {
            DataFormat::Csv => {
                let mut wtr = csv::WriterBuilder::new()
                    .delimiter(options.delimiter)
                    .has_headers(options.has_header)
                    .from_writer(file);
                if options.has_header {
                    let header = encode_csv_header(&topology.table_desc.schema);
                    wtr.write_record(&header)
                        .map_err(|e| HtapError::Io(e.into()))?;
                }
                for row in &rows {
                    let rec = encode_csv_record(&topology.table_desc.schema, row);
                    wtr.write_record(&rec)
                        .map_err(|e| HtapError::Io(e.into()))?;
                }
                wtr.flush().map_err(HtapError::Io)?;
            }
            DataFormat::JsonLines => {
                let mut buf_wtr = BufWriter::new(file);
                for row in &rows {
                    let line = encode_json_line(&topology.table_desc.schema, row)?;
                    buf_wtr.write_all(line.as_bytes())?;
                    buf_wtr.write_all(b"\n")?;
                }
                buf_wtr.flush()?;
            }
        }

        let sync_file = File::open(&tmp_path)?;
        sync_file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(e);
    }

    // 4. Atomic rename
    if let Err(e) = fs::rename(&tmp_path, dest_path) {
        let _ = fs::remove_file(&tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(HtapError::Io(e));
    }

    // 5. Fsync directory metadata
    sync_dir(parent_dir)?;
    let _ = sync_dir(mover.root_dir());

    // 6. Complete job only after file is durably renamed
    let row_count = rows.len() as u64;
    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: row_count,
        records_committed: row_count,
        rows_written: row_count,
        records_skipped: 0,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };

    mover.complete_job(&options.job_id, report.clone())?;
    Ok(report)
}
