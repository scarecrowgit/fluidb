use htap_catalog::{ReplicaId, TabletId};
use htap_movement::LocalDataMover;

#[test]
fn reclaim_succeeds_when_jobs_directory_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let mover = LocalDataMover::new(temp.path().join("movement")).unwrap();
    let tablet_id = TabletId::new(100);
    let package_dir = mover
        .tablet_package_dir(tablet_id, ReplicaId::new(1000), "missing-jobs-dir")
        .unwrap();

    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::remove_dir_all(mover.jobs_dir()).unwrap();

    let reclaim_lease = mover.try_acquire_reclaim_lease(&[tablet_id]).unwrap();
    mover.delete_tablet_movement_artifacts(tablet_id).unwrap();

    assert!(!package_dir.exists());

    drop(reclaim_lease);
}
