#![doc = "Evidence tests for deferred dropped-table reclamation."]

use std::io::Read;
use std::sync::mpsc;
use std::thread;

use htap_catalog::{CatalogStore, LocalCatalogStore, TabletId};
use htap_common::{Mutation, Row, Value};
use htap_convert::tablet_dir as convert_tablet_dir;
use htap_movement::{CopyOptions, DataFormat};
use htap_rowstore::{Engine, EngineOptions};
use htap_server::LocalServer;
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};
use tempfile::TempDir;

fn catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

fn tablet_id(root: &std::path::Path, table_name: &str) -> TabletId {
    let catalog = catalog(root);
    let table = catalog
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"));
    let partition_id = table.partitions[0];
    let partition = catalog
        .partition(partition_id)
        .unwrap_or_else(|| panic!("missing partition {partition_id} for table {table_name}"));

    partition.tablets[0]
}

fn table_id(root: &std::path::Path, table_name: &str) -> htap_catalog::TableId {
    catalog(root)
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"))
        .id
}

fn tablet_dir(server: &LocalServer, tablet_id: TabletId) -> std::path::PathBuf {
    convert_tablet_dir(
        server
            .colstore_dir()
            .expect("colstore directory must be available"),
        tablet_id,
    )
}

struct BlockingReader {
    started: mpsc::Sender<()>,
    data: mpsc::Receiver<Vec<u8>>,
    buffer: Option<std::io::Cursor<Vec<u8>>>,
}

impl Read for BlockingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buffer.is_none() {
            self.started.send(()).unwrap();
            self.buffer = Some(std::io::Cursor::new(self.data.recv().unwrap()));
        }
        self.buffer.as_mut().unwrap().read(buf)
    }
}

#[test]
fn test_drop_converted_table_reclaims_colstore_before_returning() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE dropped_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO dropped_table (id, value) VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    server.convert_table_to_column("dropped_table").unwrap();

    let dropped_tablet = tablet_id(temp.path(), "dropped_table");
    let dropped_dir = tablet_dir(&server, dropped_tablet);
    assert!(dropped_dir.is_dir());
    assert!(dropped_dir.join("MANIFEST").is_file());

    server.execute("DROP TABLE dropped_table").unwrap();

    assert!(
        !dropped_dir.exists(),
        "DROP must reclaim colstore before returning"
    );

    let catalog = catalog(temp.path());
    let pending = catalog
        .pending_reclaim
        .iter()
        .find(|entry| entry.table_name == "dropped_table")
        .expect("dropped table should have a pending reclaim entry");
    assert!(pending.colstore_and_movement_reclaimed);
    assert!(!pending.rowstore_purge_confirmed);

    let rowstore_files: Vec<_> = std::fs::read_dir(temp.path().join("rowstore"))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        !rowstore_files.is_empty(),
        "rowstore evidence must remain until rowstore purge is confirmed"
    );
}

#[test]
fn test_drop_then_create_same_name_isolates_new_artifacts_and_reclaims_old_after_lease_release() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE reused_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO reused_table (id, value) VALUES (1, 'old')")
        .unwrap();
    server.convert_table_to_column("reused_table").unwrap();

    // Recreating the table must allocate a fresh tablet ID.
    let old_tablet = tablet_id(temp.path(), "reused_table");
    let old_dir = tablet_dir(&server, old_tablet);
    let data_mover = server.data_mover().expect("data mover must be available");
    let reclaim_lease = data_mover
        .try_acquire_reclaim_lease(&[old_tablet])
        .expect("old tablet reclaim lease should be acquired");

    server.execute("DROP TABLE reused_table").unwrap();
    server
        .execute("CREATE TABLE reused_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO reused_table (id, value) VALUES (2, 'new')")
        .unwrap();
    server.convert_table_to_column("reused_table").unwrap();

    let new_tablet = tablet_id(temp.path(), "reused_table");
    let new_dir = tablet_dir(&server, new_tablet);
    assert_ne!(new_tablet, old_tablet);
    assert!(old_dir.join("MANIFEST").is_file());
    assert!(new_dir.join("MANIFEST").is_file());

    // The old tablet lease blocks reclamation; new-table artifacts stay untouched.
    let blocked = server.reclaim_tick().unwrap();
    assert_eq!(blocked.blocked_by_active_job.len(), 1);
    assert!(!blocked
        .blocked_by_active_job
        .contains(&table_id(temp.path(), "reused_table")));
    assert!(old_dir.exists());
    assert!(new_dir.exists());

    // Releasing the old lease permits reclaiming only the old artifacts.
    drop(reclaim_lease);

    let reclaimed = server.reclaim_tick().unwrap();
    assert!(reclaimed.blocked_by_active_job.is_empty());
    assert!(!old_dir.exists());
    assert!(new_dir.join("MANIFEST").is_file());
}

#[test]
fn test_drop_while_movement_job_holds_tablet_lease_stays_pending() {
    let temp = TempDir::new().unwrap();
    let server = std::sync::Arc::new(LocalServer::open(temp.path()).unwrap());

    server
        .execute("CREATE TABLE moving_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO moving_table (id, value) VALUES (1, 'old')")
        .unwrap();
    server.convert_table_to_column("moving_table").unwrap();

    let tablet = tablet_id(temp.path(), "moving_table");
    let table = table_id(temp.path(), "moving_table");
    let colstore = tablet_dir(&server, tablet);
    assert!(colstore.join("MANIFEST").is_file());

    let (started_tx, started_rx) = mpsc::channel();
    let (data_tx, data_rx) = mpsc::channel();
    let mover_server = std::sync::Arc::clone(&server);
    let copy_thread = thread::spawn(move || {
        mover_server
            .data_mover()
            .expect("data mover must be available")
            .copy_from_csv_reader(
                &CopyOptions::new(
                    "drop_while_moving",
                    table,
                    tablet,
                    DataFormat::Csv,
                    "/unused/moving.csv",
                ),
                BlockingReader {
                    started: started_tx,
                    data: data_rx,
                    buffer: None,
                },
            )
    });

    started_rx.recv().unwrap();
    server.execute("DROP TABLE moving_table").unwrap();

    let blocked = server.reclaim_tick().unwrap();
    assert_eq!(blocked.blocked_by_active_job, vec![table]);
    assert!(colstore.join("MANIFEST").is_file());
    assert!(catalog(temp.path())
        .pending_reclaim
        .iter()
        .any(|entry| { entry.table_id == table && !entry.colstore_and_movement_reclaimed }));

    data_tx.send(b"id,value\n2,done\n".to_vec()).unwrap();
    copy_thread.join().unwrap().unwrap();

    let reclaimed = server.reclaim_tick().unwrap();
    assert!(reclaimed.blocked_by_active_job.is_empty());
    assert!(!colstore.exists());
    assert!(catalog(temp.path())
        .pending_reclaim
        .iter()
        .any(|entry| { entry.table_id == table && entry.colstore_and_movement_reclaimed }));
}

struct CrashReader;

impl Read for CrashReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        panic!("CrashReader intentionally panics after the running job is persisted");
    }
}

#[test]
fn test_crash_orphaned_running_job_is_failed_and_reclaimed_at_open() {
    const CHILD_ENV: &str = "HTAP_RECLAIM_CRASH_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        let root = std::path::PathBuf::from(std::env::var("HTAP_RECLAIM_ROOT").unwrap());
        let tablet = TabletId::new(
            std::env::var("HTAP_RECLAIM_TABLET")
                .unwrap()
                .parse()
                .unwrap(),
        );
        let table = htap_catalog::TableId::new(
            std::env::var("HTAP_RECLAIM_TABLE")
                .unwrap()
                .parse()
                .unwrap(),
        );
        let server = LocalServer::open(root).unwrap();
        let result = server
            .data_mover()
            .expect("data mover must be available")
            .copy_from_csv_reader(
                &CopyOptions::new(
                    "crash_orphaned_job",
                    table,
                    tablet,
                    DataFormat::Csv,
                    "/unused/crash.csv",
                ),
                CrashReader,
            );
        panic!("CrashReader copy result: {result:?}");
    }

    let temp = TempDir::new().unwrap();
    let (tablet, table) = {
        let server = LocalServer::open(temp.path()).unwrap();
        server
            .execute("CREATE TABLE crashed_table (id INT PRIMARY KEY, value TEXT)")
            .unwrap();
        server
            .execute("INSERT INTO crashed_table (id, value) VALUES (1, 'old')")
            .unwrap();
        server.convert_table_to_column("crashed_table").unwrap();
        let tablet = tablet_id(temp.path(), "crashed_table");
        let table = table_id(temp.path(), "crashed_table");
        let colstore = tablet_dir(&server, tablet);
        assert!(colstore.join("MANIFEST").is_file());
        (tablet, table)
    };

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("test_crash_orphaned_running_job_is_failed_and_reclaimed_at_open")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env("HTAP_RECLAIM_ROOT", temp.path())
        .env("HTAP_RECLAIM_TABLET", tablet.as_u64().to_string())
        .env("HTAP_RECLAIM_TABLE", table.as_u64().to_string())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "child process must fail after CrashReader panics"
    );

    let server = LocalServer::open(temp.path()).unwrap();
    let job = server
        .data_mover()
        .expect("data mover must be available")
        .load_job("crash_orphaned_job")
        .unwrap()
        .expect("crashed movement job must exist before reclaim");
    assert!(job.is_running());
    let colstore = tablet_dir(&server, tablet);
    assert!(colstore.join("MANIFEST").is_file());
    let job_dir = server
        .data_mover()
        .expect("data mover must be available")
        .job_dir("crash_orphaned_job")
        .unwrap();

    server.execute("DROP TABLE crashed_table").unwrap();
    drop(server);

    let server = LocalServer::open(temp.path()).unwrap();

    assert!(
        server
            .data_mover()
            .expect("data mover must be available")
            .load_job("crash_orphaned_job")
            .unwrap()
            .is_none(),
        "open-time reclaim must delete the orphaned movement job record"
    );
    assert!(
        !job_dir.exists(),
        "open-time reclaim must delete the orphaned movement job directory"
    );
    assert!(
        !colstore.exists(),
        "open-time reclaim must delete the dropped tablet colstore artifacts"
    );
    assert!(catalog(temp.path())
        .pending_reclaim
        .iter()
        .any(|entry| { entry.table_id == table && entry.colstore_and_movement_reclaimed }));
}

#[test]
fn test_open_finalizes_and_compacts_oversized_transaction_journal() {
    let temp = TempDir::new().unwrap();
    let journal_path = temp.path().join("txn.journal");
    let engine = std::sync::Arc::new(
        Engine::open(EngineOptions::new(temp.path().join("rowstore"))).unwrap(),
    );
    let manager = TransactionManager::open(&journal_path)
        .unwrap()
        .with_checkpoint_trigger_bytes(u64::MAX);
    manager.register_participant(std::sync::Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        std::sync::Arc::clone(&engine),
    )));

    // Bootstrap opening permits this journal to exceed the normal configured limit.
    for id in 0..600u64 {
        let payload = RowstoreParticipant::encode_payload(&[Mutation::Put {
            partition_id: 99,
            key: id.to_be_bytes().to_vec(),
            row: Row::new(vec![Value::String("x".repeat(32 * 1024))]),
        }])
        .unwrap();
        manager
            .commit_request(
                TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
                    .unwrap(),
            )
            .unwrap();
    }
    let before = std::fs::metadata(&journal_path).unwrap().len();
    assert!(
        before > 64 * 1024 * 1024,
        "test journal must exceed the normal configured limit before server open"
    );
    drop(manager);
    drop(engine);

    let server = LocalServer::open(temp.path()).unwrap();
    let after = std::fs::metadata(&journal_path).unwrap().len();
    assert!(
        after < 64 * 1024 * 1024,
        "finalize_open must compact below the configured journal limit; before={before}, after={after}"
    );
    assert!(temp.path().join("txn.checkpoint").is_file());
    assert_eq!(
        server
            .txn_manager()
            .expect("transaction manager must be available")
            .visible_version()
            .get(),
        601
    );
}

#[test]
#[ignore = "No deterministic public hook can force only the best-effort open-time reclaim callback to fail."]
fn test_open_time_reclaim_failure_does_not_fail_open() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE reclaim_failure (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO reclaim_failure (id, value) VALUES (1, 'one')")
        .unwrap();
    server.convert_table_to_column("reclaim_failure").unwrap();

    let tablet = tablet_id(temp.path(), "reclaim_failure");
    let artifact_dir = tablet_dir(&server, tablet);
    assert!(artifact_dir.join("MANIFEST").is_file());

    let data_mover = server.data_mover().expect("data mover must be available");
    let lease = data_mover
        .try_acquire_reclaim_lease(&[tablet])
        .expect("lease must block reclamation");
    server.execute("DROP TABLE reclaim_failure").unwrap();
    drop(lease);
    drop(server);

    let reopened = LocalServer::open(temp.path()).unwrap();
    assert!(reopened.reclaim_tick().is_ok());
    assert!(!artifact_dir.exists());
}
