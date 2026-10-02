use std::fs;
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
fn chaos_unforced_parent_mkdir_admits_forced_child() {
    let harness = CrashHarness::new("chaos_unforced_parent_mkdir_admits_forced_child").unwrap();
    harness
        .run_workload(|workload| {
            let directory = workload.root().join("a");
            let file = directory.join("f");

            htap_fs::dur::create_dir(&directory).unwrap();
            write_file(&file, b"fsynced child");
            htap_fs::fsync_file(&file).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
        })
        .unwrap();

    let final_point = harness.snapshot().unwrap().log.len();
    let mut admitted = 0;

    for seed in 0..64 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, info| {
                if info.crash_point != final_point {
                    return;
                }

                let directory = root.join("a");
                let file = directory.join("f");
                if file.exists() {
                    assert!(directory.is_dir());
                    assert_eq!(fs::read(&file).unwrap(), b"fsynced child");
                    admitted += 1;
                }
            })
            .unwrap();
    }

    assert!(
        admitted > 0,
        "chaos never retained an unforced parent mkdir with its forced child"
    );
}
