use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use htap_catalog::TabletId;
use htap_movement::{LocalDataMover, TabletReclaimOutcome};

fn test_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "htap-movement-leasing-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

#[test]
fn reclaim_leases_are_mutually_exclusive_and_release_after_drop() {
    let root = test_root("mutual-exclusion");
    let mover = LocalDataMover::new(&root).unwrap();
    let tablet_id = TabletId::new(11);

    let lease = mover.try_acquire_reclaim_lease(&[tablet_id]).unwrap();
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_none());

    drop(lease);

    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reclaim_lease_releases_after_callback_success_error_and_panic() {
    let root = test_root("release-paths");
    let mover = LocalDataMover::new(&root).unwrap();
    let tablet_id = TabletId::new(12);

    let outcome = mover
        .reclaim_tablet_artifacts(tablet_id, || Ok(()))
        .unwrap();
    assert_eq!(outcome, TabletReclaimOutcome::Reclaimed { tablet_id });
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());

    let error = mover
        .reclaim_tablet_artifacts(tablet_id, || {
            Err(htap_common::HtapError::Internal("expected failure".into()))
        })
        .unwrap_err();
    assert!(error.to_string().contains("expected failure"));
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());

    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = mover.reclaim_tablet_artifacts(tablet_id, || -> htap_common::Result<()> {
            panic!("expected panic");
        });
    }));
    assert!(panic_result.is_err());
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn best_effort_reclaim_reports_denied_tablets() {
    let root = test_root("best-effort");
    let mover = LocalDataMover::new(&root).unwrap();
    let leased_tablet = TabletId::new(21);
    let available_tablet = TabletId::new(22);

    let held = mover.try_acquire_reclaim_lease(&[leased_tablet]).unwrap();
    let (best_effort, denied) =
        mover.acquire_reclaim_leases_best_effort(&[leased_tablet, available_tablet]);

    assert!(denied.contains(&leased_tablet));
    assert!(!denied.contains(&available_tablet));
    assert!(best_effort.tablet_ids().contains(&available_tablet));
    assert!(!best_effort.tablet_ids().contains(&leased_tablet));

    drop(best_effort);
    drop(held);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_reclaim_holds_lease_while_callback_runs() {
    let root = test_root("concurrent");
    let mover = Arc::new(LocalDataMover::new(&root).unwrap());
    let tablet_id = TabletId::new(31);
    let barrier = Arc::new(Barrier::new(2));

    let worker_mover = Arc::clone(&mover);
    let worker_barrier = Arc::clone(&barrier);
    let worker = thread::spawn(move || {
        worker_mover
            .reclaim_tablet_artifacts(tablet_id, || {
                worker_barrier.wait();
                worker_barrier.wait();
                Ok(())
            })
            .unwrap()
    });

    barrier.wait();
    assert_eq!(
        mover
            .reclaim_tablet_artifacts(tablet_id, || Ok(()))
            .unwrap(),
        TabletReclaimOutcome::Skipped { leased: true }
    );
    barrier.wait();

    assert_eq!(
        worker.join().unwrap(),
        TabletReclaimOutcome::Reclaimed { tablet_id }
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reclaim_callback_deletes_tablet_artifacts_while_lease_is_held() {
    let root = test_root("artifact-delete");
    let mover = LocalDataMover::new(&root).unwrap();
    let tablet_id = TabletId::new(41);
    let artifact_dir = mover.tablets_dir().join(tablet_id.as_u64().to_string());
    fs::create_dir_all(&artifact_dir).unwrap();
    fs::write(artifact_dir.join("DATA"), b"artifact").unwrap();

    let artifact_dir_for_callback = artifact_dir.clone();
    let outcome = mover
        .reclaim_tablet_artifacts(tablet_id, move || {
            assert!(artifact_dir_for_callback.join("DATA").exists());
            fs::remove_dir_all(&artifact_dir_for_callback)?;
            Ok(())
        })
        .unwrap();

    assert_eq!(outcome, TabletReclaimOutcome::Reclaimed { tablet_id });
    assert!(!artifact_dir.exists());
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_movement_and_reclaim_lease_mutual_exclusion() {
    use std::io::{Cursor, Read};
    use std::sync::mpsc;

    use htap_catalog::local::LocalCatalogStore;
    use htap_catalog::store::CatalogStore;
    use htap_catalog::{
        CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
        StorageDescriptor, TableDescriptor, TableId, TabletDescriptor,
    };
    use htap_common::{ColumnDef, DataType, HtapError, Schema};
    use htap_movement::{CopyOptions, DataFormat};
    use htap_rowstore::{Engine, EngineOptions};
    use htap_txn::{ParticipantId, RowstoreParticipant, TransactionManager};

    struct BlockingReader {
        started: mpsc::Sender<()>,
        data: mpsc::Receiver<Vec<u8>>,
        buffer: Option<Cursor<Vec<u8>>>,
    }

    impl Read for BlockingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.buffer.is_none() {
                self.started.send(()).unwrap();
                self.buffer = Some(Cursor::new(self.data.recv().unwrap()));
            }
            self.buffer.as_mut().unwrap().read(buf)
        }
    }

    let root = test_root("movement-reclaim-mutual-exclusion");
    let cat_store = Arc::new(LocalCatalogStore::open(root.join("catalog")).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(root.join("rowstore"))).unwrap());
    let txn_manager = Arc::new(TransactionManager::open(root.join("txn.journal")).unwrap());
    txn_manager.register_participant(Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    )));
    let mover = Arc::new(LocalDataMover::new(root.join("movement")).unwrap());

    let table_id = TableId::new(1);
    let partition_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);
    let schema = Schema::new(vec![
        ColumnDef {
            name: "c_id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "c_str".into(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let table = TableDescriptor::new(
        table_id,
        "test_table",
        schema,
        vec![0],
        vec![partition_id],
        1,
    );
    let partition = PartitionDescriptor::new(
        partition_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, partition_id, 0, vec![replica_id], 1);
    let replica = ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, 1);
    cat_store
        .compare_and_set(
            0,
            CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]),
        )
        .unwrap();

    // Part 1: the movement job holds its tablet lease before the reader receives data.
    let (started_tx, started_rx) = mpsc::channel();
    let (data_tx, data_rx) = mpsc::channel();
    let options = CopyOptions::new(
        "movement_holds_lease",
        table_id,
        tablet_id,
        DataFormat::Csv,
        "/unused/stream.csv",
    );

    let copy_mover = Arc::clone(&mover);
    let copy_catalog = Arc::clone(&cat_store);
    let copy_txn_manager = Arc::clone(&txn_manager);
    let copy_options = options.clone();
    let copy_thread = thread::spawn(move || {
        copy_mover.copy_from_csv_reader(
            &copy_options,
            copy_catalog.as_ref(),
            copy_txn_manager.as_ref(),
            BlockingReader {
                started: started_tx,
                data: data_rx,
                buffer: None,
            },
        )
    });

    started_rx.recv().unwrap();
    assert_eq!(
        mover
            .reclaim_tablet_artifacts(tablet_id, || Ok(()))
            .unwrap(),
        TabletReclaimOutcome::Skipped { leased: true }
    );

    data_tx.send(b"c_id,c_str\n1,test\n".to_vec()).unwrap();
    let report = copy_thread.join().unwrap().unwrap();
    assert_eq!(report.records_committed, 1);

    assert_eq!(
        mover
            .reclaim_tablet_artifacts(tablet_id, || Ok(()))
            .unwrap(),
        TabletReclaimOutcome::Reclaimed { tablet_id }
    );

    // Part 2: a reclaim lease prevents an actual CSV movement job from starting.
    let reclaim_lease = mover.try_acquire_reclaim_lease(&[tablet_id]).unwrap();
    let blocked_options = CopyOptions::new(
        "movement_blocked_by_reclaim",
        table_id,
        tablet_id,
        DataFormat::Csv,
        "/unused/stream.csv",
    );
    let blocked_mover = Arc::clone(&mover);
    let blocked_catalog = Arc::clone(&cat_store);
    let blocked_txn_manager = Arc::clone(&txn_manager);
    let blocked_thread = thread::spawn(move || {
        blocked_mover.copy_from_csv_reader(
            &blocked_options,
            blocked_catalog.as_ref(),
            blocked_txn_manager.as_ref(),
            Cursor::new(b"c_id,c_str\n2,blocked\n".to_vec()),
        )
    });

    let err = blocked_thread.join().unwrap().unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    drop(reclaim_lease);

    let retry_options = CopyOptions::new(
        "movement_after_reclaim",
        table_id,
        tablet_id,
        DataFormat::Csv,
        "/unused/stream.csv",
    );
    let report = mover
        .copy_from_csv_reader(
            &retry_options,
            cat_store.as_ref(),
            txn_manager.as_ref(),
            Cursor::new(b"c_id,c_str\n2,after_reclaim\n".to_vec()),
        )
        .unwrap();
    assert_eq!(report.records_committed, 1);

    fs::remove_dir_all(root).unwrap();
}
