use std::io::Write;

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{CrashHarness, CrashPolicy};

fn write_file(path: &std::path::Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn chaos_file_to_directory_type_conflicts_materialize() {
    let harness = CrashHarness::new("chaos_file_to_directory_type_conflicts_materialize").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("p");

            write_file(&path, b"file");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_file(&path).unwrap();
            htap_fs::dur::create_dir(&path).unwrap();
        })
        .unwrap();

    let mut files = 0;
    let mut directories = 0;
    for seed in 0..64 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, _| {
                let path = root.join("p");
                if path.is_file() {
                    files += 1;
                } else if path.is_dir() {
                    directories += 1;
                } else {
                    assert!(!path.exists());
                }
            })
            .unwrap();
    }

    assert!(files > 0);
    assert!(directories > 0);
}

#[test]
fn chaos_directory_to_file_type_conflicts_materialize() {
    let harness = CrashHarness::new("chaos_directory_to_file_type_conflicts_materialize").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("p");

            htap_fs::dur::create_dir(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_dir(&path).unwrap();
            write_file(&path, b"file");
        })
        .unwrap();

    let mut files = 0;
    let mut directories = 0;
    for seed in 0..64 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, _| {
                let path = root.join("p");
                if path.is_file() {
                    files += 1;
                } else if path.is_dir() {
                    directories += 1;
                } else {
                    assert!(!path.exists());
                }
            })
            .unwrap();
    }

    assert!(files > 0);
    assert!(directories > 0);
}

#[test]
#[should_panic(expected = "POWERLOSS_SEEDS must be at least 1")]
fn powerloss_seeds_zero_panics() {
    let harness = CrashHarness::new("powerloss_seeds_zero_panics").unwrap();
    harness.run_workload(|_| {}).unwrap();

    harness
        .enumerate_with_env(
            &CrashPolicy::Torn {
                seed: 0,
                sector_size: 4096,
            },
            [("POWERLOSS_SEEDS", "0")],
            |_, _| {},
        )
        .unwrap();
}
