use std::fs;
use std::io::{self, Cursor, Write};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{ColumnDef, DataType, HtapError, Schema};
use htap_movement::{CopyOptions, DataFormat, LocalDataMover};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{ParticipantId, RowstoreParticipant, TransactionManager};

fn test_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "htap-movement-export-leasing-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

struct BlockingWriter {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    blocked: bool,
}

impl Write for BlockingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.blocked {
            self.blocked = true;
            self.started
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test receiver dropped"))?;
            self.release
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test sender dropped"))?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn exports_hold_tablet_leases_against_reclaim() {
    let root = test_root("exports-hold-leases");
    let catalog = Arc::new(LocalCatalogStore::open(root.join("catalog")).unwrap());
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
    catalog
        .compare_and_set(
            0,
            CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]),
        )
        .unwrap();

    let import_options = CopyOptions::new(
        "seed-export-data",
        table_id,
        tablet_id,
        DataFormat::Csv,
        root.join("seed.csv"),
    );
    mover
        .copy_from_csv_reader(
            &import_options,
            catalog.as_ref(),
            txn_manager.as_ref(),
            Cursor::new(b"c_id,c_str\n1,exported\n".to_vec()),
        )
        .unwrap();

    let output_path = root.join("export.csv");
    let export_options = CopyOptions::new(
        "blocked-export",
        table_id,
        tablet_id,
        DataFormat::Csv,
        &output_path,
    );

    let reclaim_lease = mover.try_acquire_reclaim_lease(&[tablet_id]).unwrap();
    let err = mover
        .copy_to_csv(&export_options, catalog.as_ref(), engine.as_ref())
        .unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
    assert!(!output_path.exists());

    drop(reclaim_lease);

    let report = mover
        .copy_to_csv(&export_options, catalog.as_ref(), engine.as_ref())
        .unwrap();
    assert_eq!(report.records_committed, 1);
    assert!(output_path.exists());

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer_options = CopyOptions::new(
        "writer-holds-export-lease",
        table_id,
        tablet_id,
        DataFormat::Csv,
        root.join("unused-writer-path.csv"),
    );
    let export_mover = Arc::clone(&mover);
    let export_catalog = Arc::clone(&catalog);
    let export_engine = Arc::clone(&engine);
    let export_thread = thread::spawn(move || {
        export_mover.copy_to_csv_writer(
            &writer_options,
            export_catalog.as_ref(),
            export_engine.as_ref(),
            BlockingWriter {
                started: started_tx,
                release: release_rx,
                blocked: false,
            },
        )
    });

    started_rx.recv().unwrap();
    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_none());

    release_tx.send(()).unwrap();
    let report = export_thread.join().unwrap().unwrap();
    assert_eq!(report.records_committed, 1);

    assert!(mover.try_acquire_reclaim_lease(&[tablet_id]).is_some());
    fs::remove_dir_all(root).unwrap();
}
