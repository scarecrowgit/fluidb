use std::fs;

use htap_catalog::TabletId;
use htap_movement::LocalDataMover;

fn test_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "htap-movement-corrupt-job-isolation-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

#[test]
fn corrupt_unrelated_job_does_not_block_tablet_artifact_reclamation() {
    let root = test_root();
    let mover = LocalDataMover::new(root.join("movement")).unwrap();
    let target_tablet = TabletId::new(41);

    let target_artifacts = mover.tablets_dir().join(target_tablet.as_u64().to_string());
    fs::create_dir_all(&target_artifacts).unwrap();
    fs::write(target_artifacts.join("DATA"), b"target artifact").unwrap();

    let corrupt_job_path = mover.job_file_path("corrupt-unrelated-job").unwrap();
    fs::create_dir_all(corrupt_job_path.parent().unwrap()).unwrap();
    fs::write(&corrupt_job_path, b"not a movement job envelope").unwrap();

    let outcome = mover
        .reclaim_tablet_artifacts(target_tablet, || {
            fs::remove_dir_all(&target_artifacts)?;
            mover.delete_tablet_movement_artifacts(target_tablet)
        })
        .unwrap();

    assert_eq!(
        outcome,
        htap_movement::TabletReclaimOutcome::Reclaimed {
            tablet_id: target_tablet
        }
    );
    assert!(!target_artifacts.exists());
    assert!(
        corrupt_job_path.is_file(),
        "unrelated corrupt job file must be left untouched"
    );

    fs::remove_dir_all(root).unwrap();
}
