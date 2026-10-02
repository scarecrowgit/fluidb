use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{CrashHarness, CrashPolicy};

fn write_file(path: &Path, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options.open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true);
    let mut file = options.open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn torn_zero_only_extension_reachable() {
    let harness = CrashHarness::new("torn_zero_only_extension_reachable").unwrap();
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");

            write_file(&path, b"abcd");
            htap_fs::fsync_file(&path).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            write_at(&path, 4, b"efghijkl");
            workload.ack("extended");
        })
        .unwrap();

    let mut zero_only_extensions = 0;
    for seed in 0..128 {
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: 4,
                },
                |root, info| {
                    if info.acked_labels.iter().any(|label| label == "extended") {
                        let bytes = fs::read(root.join("data")).unwrap();
                        if bytes.len() == 12
                            && bytes[..4] == *b"abcd"
                            && bytes[4..].iter().all(|byte| *byte == 0)
                        {
                            zero_only_extensions += 1;
                        }
                    }
                },
            )
            .unwrap();
    }

    assert!(
        zero_only_extensions > 0,
        "torn writes never produced a zero-only extension"
    );
}
