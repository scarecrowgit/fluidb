use htap_common::{HtapError, Version};
use htap_rowstore::manifest::{Manifest, ManifestLedgerEntry};

#[test]
fn test_refused_regression_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = Manifest {
        committed_version_high_water: Version::new(10),
        ..Default::default()
    };
    Manifest::atomic_publish(dir.path(), &manifest).unwrap();

    let path = dir.path().join("MANIFEST");
    let original_bytes = std::fs::read(&path).unwrap();

    manifest.committed_version_high_water = Version::new(9);
    let err = Manifest::atomic_publish(dir.path(), &manifest).unwrap_err();

    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
}

#[test]
fn test_refused_ledger_above_high_water() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = Manifest {
        committed_version_high_water: Version::new(10),
        ..Default::default()
    };
    Manifest::atomic_publish(dir.path(), &manifest).unwrap();

    let path = dir.path().join("MANIFEST");
    let original_bytes = std::fs::read(&path).unwrap();

    manifest
        .applied_txns
        .push(ManifestLedgerEntry::new(99, Version::new(11)));
    let err = Manifest::atomic_publish(dir.path(), &manifest).unwrap_err();

    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
}
