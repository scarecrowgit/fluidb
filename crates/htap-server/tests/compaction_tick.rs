#![doc = "Evidence tests for compaction GC horizons, dropped-rowstore purge, and leases."]

use std::collections::HashSet;
use std::io::Read;
use std::sync::mpsc;
use std::thread;

use htap_catalog::{CatalogStore, LocalCatalogStore, PartitionId, TabletId};
use htap_common::Value;
use htap_movement::{CopyOptions, DataFormat};
use htap_rowstore::Manifest;
use htap_server::{GcHorizonSource, LocalServer};
use tempfile::TempDir;

fn catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

fn table_and_partition(
    root: &std::path::Path,
    table_name: &str,
) -> (htap_catalog::TableId, PartitionId, TabletId) {
    let catalog = catalog(root);
    let table = catalog
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"));
    let partition_id = table.partitions[0];
    let partition = catalog
        .partition(partition_id)
        .unwrap_or_else(|| panic!("missing partition {partition_id} for table {table_name}"));
    (table.id, partition_id, partition.tablets[0])
}

fn flush_rowstore(root: &std::path::Path, server: LocalServer) -> LocalServer {
    drop(server);

    let engine =
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(root.join("rowstore")))
            .unwrap();
    engine.flush().unwrap();
    drop(engine);

    LocalServer::open(root).unwrap()
}

fn rowstore_sst_ids(root: &std::path::Path) -> HashSet<u64> {
    Manifest::read_from_file(&root.join("rowstore").join("MANIFEST"))
        .unwrap()
        .unwrap_or_default()
        .ssts
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

fn query_value(session: &mut htap_server::Session, sql: &str) -> String {
    let result = session.execute(sql).unwrap();
    let htap_sql::result::StatementResult::Query(query) = result else {
        panic!("expected query result for {sql}");
    };
    assert_eq!(query.rows.len(), 1, "expected exactly one row for {sql}");
    match &query.rows[0].values()[0] {
        Value::String(value) => value.clone(),
        Value::Int32(value) => value.to_string(),
        Value::Int64(value) => value.to_string(),
        other => panic!("unexpected query value {other:?}"),
    }
}

struct BlockingReader {
    started: mpsc::Sender<()>,
    data: mpsc::Receiver<Vec<u8>>,
    buffer: Option<std::io::Cursor<Vec<u8>>>,
}

impl Read for BlockingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buffer.is_none() {
            self.started.send(()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "movement test stopped waiting for the reader",
                )
            })?;
            let data = self.data.recv().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "movement test closed the data channel",
                )
            })?;
            self.buffer = Some(std::io::Cursor::new(data));
        }
        self.buffer.as_mut().unwrap().read(buf)
    }
}

#[test]
fn horizon_protects_an_open_transaction_snapshot() {
    let temp = TempDir::new().unwrap();
    let server = std::sync::Arc::new(LocalServer::open(temp.path()).unwrap());

    server
        .execute("CREATE TABLE protected_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO protected_rows (id, value) VALUES (1, 'before')")
        .unwrap();

    let mut session = server.open_session().unwrap();
    session.begin().unwrap();
    let pinned_version = server.txn_manager().unwrap().visible_version();
    assert_eq!(
        query_value(
            &mut session,
            "SELECT value FROM protected_rows WHERE id = 1"
        ),
        "before"
    );

    server
        .execute("UPDATE protected_rows SET value = 'after' WHERE id = 1")
        .unwrap();
    let report = server.compaction_tick().unwrap();

    assert!(report.ran, "compaction tick did not run: {report:?}");
    assert!(
        report.gc_horizon <= pinned_version,
        "GC horizon must preserve the pinned snapshot: {report:?}"
    );
    assert_eq!(
        report.gc_horizon_limiting_source,
        GcHorizonSource::PinnedSession(session.id()),
        "the open session must limit the GC horizon: {report:?}"
    );
    assert_eq!(
        query_value(
            &mut session,
            "SELECT value FROM protected_rows WHERE id = 1"
        ),
        "before",
        "the old row version must remain readable through the pinned transaction"
    );
    session.rollback().unwrap();
}

#[test]
fn commit_and_rollback_unregister_snapshot_and_allow_versions_to_collapse() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut server = LocalServer::open(root).unwrap();

    server
        .execute("CREATE TABLE versioned_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO versioned_rows (id, value) VALUES (1, 'v1')")
        .unwrap();
    server = flush_rowstore(root, server);

    // Persist each version in a separate SST so compaction has versions to collapse.
    for value in 2..=8 {
        server
            .execute(&format!(
                "UPDATE versioned_rows SET value = 'v{value}' WHERE id = 1"
            ))
            .unwrap();
        server = flush_rowstore(root, server);
    }

    let server = std::sync::Arc::new(server);
    let mut session = server.open_session().unwrap();
    session.begin().unwrap();
    let pinned_version = server.txn_manager().unwrap().visible_version();

    session.commit().unwrap();
    drop(session);

    // Advance the visible version beyond the snapshot that was pinned by the session.
    server
        .execute("UPDATE versioned_rows SET value = 'v9' WHERE id = 1")
        .unwrap();

    let after_commit = server.compaction_tick().unwrap();
    let after_commit_follow_up = server.compaction_tick().unwrap();
    assert!(
        after_commit.gc_horizon > pinned_version,
        "COMMIT must unregister the pinned snapshot: {after_commit:?}"
    );
    assert_eq!(
        after_commit.gc_horizon_limiting_source,
        GcHorizonSource::VisibleVersion,
        "the visible version must limit GC after COMMIT: {after_commit:?}"
    );
    let collapsed_after_commit: u64 = after_commit
        .per_iteration_reports
        .iter()
        .chain(after_commit_follow_up.per_iteration_reports.iter())
        .map(|report| report.collapsed_versions)
        .sum();
    assert!(
        collapsed_after_commit > 0,
        "unregistering on COMMIT must allow obsolete versions to collapse: \
         first={after_commit:?}, second={after_commit_follow_up:?}"
    );

    drop(server);
    let mut server = LocalServer::open(root).unwrap();

    // Build another persisted version before testing rollback unregistration.
    server
        .execute("UPDATE versioned_rows SET value = 'v10' WHERE id = 1")
        .unwrap();
    server = flush_rowstore(root, server);

    let server = std::sync::Arc::new(server);
    let mut rollback_session = server.open_session().unwrap();
    rollback_session.begin().unwrap();
    let rollback_pin = server.txn_manager().unwrap().visible_version();

    server
        .execute("UPDATE versioned_rows SET value = 'v11' WHERE id = 1")
        .unwrap();
    rollback_session.rollback().unwrap();
    drop(rollback_session);

    let after_rollback = server.compaction_tick().unwrap();
    assert!(
        after_rollback.gc_horizon > rollback_pin,
        "ROLLBACK must unregister the pinned snapshot: {after_rollback:?}"
    );
    assert_eq!(
        after_rollback.gc_horizon_limiting_source,
        GcHorizonSource::VisibleVersion,
        "the visible version must limit GC after ROLLBACK: {after_rollback:?}"
    );
}

#[test]
fn dropped_table_is_purged_from_rowstore_before_pending_entry_is_removed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let server = LocalServer::open(root).unwrap();

    server
        .execute("CREATE TABLE purged_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO purged_rows (id, value) VALUES (1, 'value-1')")
        .unwrap();

    let (table_id, _, _) = table_and_partition(root, "purged_rows");
    server.execute("DROP TABLE purged_rows").unwrap();

    let before = catalog(root);
    let pending = before
        .pending_reclaim
        .iter()
        .find(|entry| entry.table_id == table_id)
        .expect("DROP must create a pending reclaim entry");
    assert!(!pending.rowstore_purge_confirmed);

    let mut saw_entries_purged = false;
    let mut saw_rowstore_confirmation = false;
    let mut removed = false;

    for _ in 0..32 {
        let report = server.compaction_tick().unwrap();
        saw_entries_purged |= report.entries_purged > 0;

        let current = catalog(root);
        match current
            .pending_reclaim
            .iter()
            .find(|entry| entry.table_id == table_id)
        {
            Some(entry) => {
                saw_rowstore_confirmation |= entry.rowstore_purge_confirmed;
            }
            None => {
                removed = true;
                break;
            }
        }
    }

    assert!(
        saw_entries_purged,
        "a compaction tick must report purging dropped rowstore entries"
    );
    assert!(
        saw_rowstore_confirmation || removed,
        "catalog reclamation must not complete without rowstore purge confirmation"
    );
    assert!(
        removed,
        "the pending reclaim entry must be removed after reclamation completes"
    );
    assert!(
        catalog(root)
            .pending_reclaim
            .iter()
            .all(|entry| entry.table_id != table_id),
        "only after both reclamation flags are confirmed may the pending entry be removed"
    );
}

/// If the tick stops protecting movement-leased partitions, compaction will replace or delete at
/// least one recorded moving-table SST, so its manifest-membership or file-existence assertion fails.
#[test]
fn movement_lease_blocks_compaction_rewrite_of_busy_tablet() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut server = LocalServer::open(root).unwrap();

    server
        .execute("CREATE TABLE moving_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("CREATE TABLE other_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    let mut known_sst_ids = rowstore_sst_ids(root);
    let mut moving_sst_ids = HashSet::new();

    // Exceed the tier threshold, flushing each table separately so the leased table's SSTs
    // have identities that can be checked after compaction.
    for value in 1..=6 {
        if value == 1 {
            server
                .execute("INSERT INTO moving_rows (id, value) VALUES (1, 'moving-1')")
                .unwrap();
        } else {
            server
                .execute(&format!(
                    "UPDATE moving_rows SET value = 'moving-{value}' WHERE id = 1"
                ))
                .unwrap();
        }
        server = flush_rowstore(root, server);
        let after_moving_flush = rowstore_sst_ids(root);
        moving_sst_ids.extend(after_moving_flush.difference(&known_sst_ids).copied());

        if value == 1 {
            server
                .execute("INSERT INTO other_rows (id, value) VALUES (1, 'other-1')")
                .unwrap();
        } else {
            server
                .execute(&format!(
                    "UPDATE other_rows SET value = 'other-{value}' WHERE id = 1"
                ))
                .unwrap();
        }
        server = flush_rowstore(root, server);
        known_sst_ids = rowstore_sst_ids(root);
    }
    assert!(
        moving_sst_ids.len() >= 4,
        "the movement-leased table must have several SSTs before compaction"
    );

    let (moving_table, _, moving_tablet) = table_and_partition(root, "moving_rows");
    let server = std::sync::Arc::new(server);

    let (started_tx, started_rx) = mpsc::channel();
    let (data_tx, data_rx) = mpsc::channel();
    let mover_server = std::sync::Arc::clone(&server);
    let worker = thread::spawn(move || {
        mover_server.data_mover().unwrap().copy_from_csv_reader(
            &CopyOptions::new(
                "compaction_busy_tablet",
                moving_table,
                moving_tablet,
                DataFormat::Csv,
                "/unused/compaction-busy.csv",
            ),
            BlockingReader {
                started: started_tx,
                data: data_rx,
                buffer: None,
            },
        )
    });

    started_rx
        .recv()
        .expect("movement worker exited before reading input");
    let report = server.compaction_tick().unwrap();

    // Always release and join the worker before asserting so a failure cannot strand it.
    if let Err(send_error) = data_tx.send(b"id,value\n2,imported\n".to_vec()) {
        let worker_result = worker.join().expect("movement worker panicked");
        panic!(
            "movement reader closed unexpectedly: {send_error}; worker result: {worker_result:?}"
        );
    }
    let movement = worker
        .join()
        .expect("movement worker panicked")
        .expect("movement failed");
    assert_eq!(movement.records_committed, 1);

    assert!(
        report.denied_tablet_count > 0,
        "compaction must observe and deny work for the movement-leased tablet: {report:?}"
    );

    let result = server
        .execute("SELECT value FROM moving_rows WHERE id = 2")
        .unwrap();
    let htap_sql::result::StatementResult::Query(query) = result else {
        panic!("expected query result");
    };
    assert_eq!(query.rows[0].values()[0], Value::String("imported".into()));

    drop(server);
    let after_tick_sst_ids = rowstore_sst_ids(root);
    for id in moving_sst_ids {
        assert!(
            after_tick_sst_ids.contains(&id),
            "movement-leased SST {id} must remain listed in the manifest"
        );
        assert!(
            root.join("rowstore")
                .join("sst")
                .join(format!("{id}.sst"))
                .is_file(),
            "movement-leased SST file {id}.sst must remain on disk"
        );
    }
}

/// If the tick stops protecting the reclaim-leased partition, its dropped-table SSTs will be
/// removed; the busy-SST manifest/file assertions fail while the other-SST assertions prove the
/// tick actually compacted eligible work rather than preserving everything.
#[test]
fn best_effort_compaction_runs_bounded_iterations_with_busy_tablet() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut server = LocalServer::open(root).unwrap();

    server
        .execute("CREATE TABLE busy_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("CREATE TABLE other_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    let mut known_sst_ids = rowstore_sst_ids(root);
    let mut busy_sst_ids = HashSet::new();
    let mut other_sst_ids = HashSet::new();

    for value in 1..=6 {
        if value == 1 {
            server
                .execute("INSERT INTO busy_rows (id, value) VALUES (1, 'busy-1')")
                .unwrap();
        } else {
            server
                .execute(&format!(
                    "UPDATE busy_rows SET value = 'busy-{value}' WHERE id = 1"
                ))
                .unwrap();
        }
        server = flush_rowstore(root, server);
        let after_busy_flush = rowstore_sst_ids(root);
        busy_sst_ids.extend(after_busy_flush.difference(&known_sst_ids).copied());
        known_sst_ids = after_busy_flush;

        if value == 1 {
            server
                .execute("INSERT INTO other_rows (id, value) VALUES (1, 'other-1')")
                .unwrap();
        } else {
            server
                .execute(&format!(
                    "UPDATE other_rows SET value = 'other-{value}' WHERE id = 1"
                ))
                .unwrap();
        }
        server = flush_rowstore(root, server);
        let after_other_flush = rowstore_sst_ids(root);
        other_sst_ids.extend(after_other_flush.difference(&known_sst_ids).copied());
        known_sst_ids = after_other_flush;
    }

    let (busy_table, _, busy_tablet) = table_and_partition(root, "busy_rows");
    let (other_table, _, other_tablet) = table_and_partition(root, "other_rows");
    assert_ne!(busy_tablet, other_tablet);

    let data_mover = server.data_mover().unwrap();
    let lease = data_mover
        .try_acquire_reclaim_lease(&[other_tablet])
        .expect("failed to acquire reclaim lease for protected tablet");

    server.execute("DROP TABLE busy_rows").unwrap();
    server.execute("DROP TABLE other_rows").unwrap();

    let report = server.compaction_tick().unwrap();

    assert!(report.ran, "compaction tick did not run: {report:?}");
    assert!(
        report.denied_tablet_count >= 1,
        "the refused forced lease must be reported as denied: {report:?}"
    );
    assert!(
        report
            .per_iteration_reports
            .iter()
            .any(|iteration| iteration.compacted),
        "the tick must compact at least one eligible iteration: {report:?}"
    );

    let pending = catalog(root)
        .pending_reclaim
        .into_iter()
        .find(|entry| entry.table_id == other_table)
        .expect("the leased dropped table must remain pending");
    assert!(
        !pending.rowstore_purge_confirmed,
        "a lease-protected dropped partition must not be purge-confirmed"
    );

    drop(lease);
    drop(server);

    let after_tick_sst_ids = rowstore_sst_ids(root);
    for &id in &other_sst_ids {
        assert!(
            after_tick_sst_ids.contains(&id),
            "reclaim-leased other-table SST {id} must remain listed in the manifest"
        );
        assert!(
            root.join("rowstore")
                .join("sst")
                .join(format!("{id}.sst"))
                .is_file(),
            "reclaim-leased other-table SST file {id}.sst must remain on disk"
        );
    }
    assert!(
        busy_sst_ids
            .iter()
            .any(|id| !after_tick_sst_ids.contains(id)),
        "at least one eligible busy-table SST must be compacted away"
    );

    let server = LocalServer::open(root).unwrap();
    for _ in 0..32 {
        server.compaction_tick().unwrap();
        if catalog(root)
            .pending_reclaim
            .iter()
            .all(|entry| entry.table_id != busy_table && entry.table_id != other_table)
        {
            return;
        }
    }

    panic!("the dropped tables were not purged after the lease was released");
}

#[test]
fn compaction_tick_refuses_while_transaction_recovery_is_required() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE recovery_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    server.txn_manager().unwrap().set_commit_sync_hook(|_| {
        Err(htap_common::HtapError::DurablePending {
            txn_id: 999,
            version: htap_common::Version::new(999),
            reason: "injected recovery requirement".into(),
        })
    });

    let _ = server.execute("INSERT INTO recovery_rows (id, value) VALUES (1, 'pending')");
    let report = server.compaction_tick().unwrap();

    assert!(
        !report.ran,
        "compaction must not run while recovery is pending: {report:?}"
    );
    assert_eq!(
        report.reason,
        Some("recovery pending"),
        "compaction must report the recovery requirement: {report:?}"
    );
    assert!(
        report.per_iteration_reports.is_empty(),
        "a skipped tick must not contain iteration reports: {report:?}"
    );
}
