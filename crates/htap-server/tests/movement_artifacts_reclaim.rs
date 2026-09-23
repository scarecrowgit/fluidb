#![doc = "Evidence test for dropped-table movement artifact reclamation."]

use htap_catalog::{CatalogStore, LocalCatalogStore, NodeId, ReplicaDescriptor, ReplicaId};
use htap_common::Version;
use htap_movement::{CopyOptions, DataFormat, TabletCloneOptions};
use htap_server::LocalServer;
use tempfile::TempDir;

fn catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

#[test]
fn test_dropped_table_reclaim_deletes_movement_package_and_job_artifacts() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE movement_reclaim (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    let snapshot = catalog(temp.path());
    let table = snapshot
        .table_by_name("movement_reclaim")
        .expect("created table must exist");
    let partition = snapshot
        .partition(table.partitions[0])
        .expect("created table partition must exist");
    let tablet_id = partition.tablets[0];

    let import_path = temp.path().join("movement_reclaim.csv");
    std::fs::write(&import_path, "id,value\n1,one\n").unwrap();

    let job_id = "movement-reclaim-import";
    server
        .data_mover()
        .expect("data mover must be available")
        .copy_from_csv(&CopyOptions::new(
            job_id,
            table.id,
            tablet_id,
            DataFormat::Csv,
            &import_path,
        ))
        .unwrap();

    let catalog_store = LocalCatalogStore::open(temp.path().join("catalog")).unwrap();
    let mut snapshot = catalog_store.load().unwrap().unwrap();
    let current_generation = snapshot.generation;
    let follower_replica_id = ReplicaId::new(2);
    snapshot.replicas.push(ReplicaDescriptor::new(
        follower_replica_id,
        tablet_id,
        NodeId::new(2),
        false,
        false,
        snapshot.generation + 1,
    ));
    snapshot
        .tablets
        .iter_mut()
        .find(|tablet| tablet.id == tablet_id)
        .expect("created tablet must exist")
        .replicas
        .push(follower_replica_id);
    snapshot.id_high_water.replica = snapshot
        .id_high_water
        .replica
        .max(follower_replica_id.as_u64());
    snapshot.generation += 1;
    catalog_store
        .compare_and_set(current_generation, snapshot)
        .unwrap();

    let package_job_id = "movement-reclaim-package";
    let package_options = TabletCloneOptions::new(package_job_id, tablet_id, follower_replica_id)
        .with_table_id(table.id)
        .with_pinned_version(Version::new(
            server
                .txn_manager()
                .expect("transaction manager is available")
                .visible_version()
                .get(),
        ));
    server
        .data_mover()
        .expect("data mover must be available")
        .clone_tablet(&package_options)
        .unwrap();

    let movement_dir = temp.path().join("movement");
    let tablet_artifacts_dir = movement_dir
        .join("tablets")
        .join(tablet_id.as_u64().to_string());
    let import_job_dir = movement_dir.join("jobs").join(job_id);
    let package_job_dir = movement_dir.join("jobs").join(package_job_id);

    assert!(
        tablet_artifacts_dir.is_dir(),
        "tablet movement package artifacts must exist before reclamation"
    );
    assert!(
        import_job_dir.is_dir(),
        "import movement job directory must exist before reclamation"
    );
    assert!(
        package_job_dir.is_dir(),
        "clone movement job directory must exist before reclamation"
    );
    assert!(
        std::fs::read_dir(&tablet_artifacts_dir)
            .unwrap()
            .next()
            .is_some(),
        "tablet movement package artifacts must be non-empty before reclamation"
    );
    assert!(
        std::fs::read_dir(&import_job_dir).unwrap().next().is_some(),
        "import movement job directory must be non-empty before reclamation"
    );
    assert!(
        std::fs::read_dir(&package_job_dir)
            .unwrap()
            .next()
            .is_some(),
        "clone movement job directory must be non-empty before reclamation"
    );

    server.execute("DROP TABLE movement_reclaim").unwrap();

    for _ in 0..4 {
        server.reclaim_tick().unwrap();
        let snapshot = catalog(temp.path());
        if snapshot
            .pending_reclaim
            .iter()
            .any(|entry| entry.table_id == table.id && entry.colstore_and_movement_reclaimed)
        {
            break;
        }
    }

    assert!(
        !tablet_artifacts_dir.exists(),
        "tablet movement package artifacts must be deleted during reclamation"
    );
    assert!(
        !import_job_dir.exists(),
        "import movement job directory must be deleted during reclamation"
    );
    assert!(
        !package_job_dir.exists(),
        "clone movement job directory must be deleted during reclamation"
    );

    let snapshot = catalog(temp.path());
    assert!(
        snapshot
            .pending_reclaim
            .iter()
            .any(|entry| { entry.table_id == table.id && entry.colstore_and_movement_reclaimed }),
        "reclamation must record that colstore and movement artifacts were reclaimed"
    );
}
