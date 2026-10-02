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
fn dir_reuse_without_root_sync_keeps_old_generation() {
    let harness = CrashHarness::new("dir_reuse_without_root_sync_keeps_old_generation").unwrap();
    harness
        .run_workload(|workload| {
            let directory = workload.root().join("d");

            htap_fs::dur::create_dir(&directory).unwrap();
            write_file(&directory.join("f"), b"old");
            htap_fs::fsync_file(directory.join("f")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_file(directory.join("f")).unwrap();
            htap_fs::dur::remove_dir(&directory).unwrap();
            htap_fs::dur::create_dir(&directory).unwrap();

            write_file(&directory.join("y"), b"new");
            htap_fs::fsync_file(directory.join("y")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
        })
        .unwrap();

    let final_point = harness.snapshot().unwrap().log.len();
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.crash_point != final_point {
                return;
            }

            let directory = root.join("d");
            assert!(directory.is_dir());
            assert_eq!(fs::read(directory.join("f")).unwrap(), b"old");
            assert!(!directory.join("y").exists());
        })
        .unwrap();
}

#[test]
fn dir_reuse_with_root_sync_shows_new_generation() {
    let harness = CrashHarness::new("dir_reuse_with_root_sync_shows_new_generation").unwrap();
    harness
        .run_workload(|workload| {
            let directory = workload.root().join("d");

            htap_fs::dur::create_dir(&directory).unwrap();
            write_file(&directory.join("f"), b"old");
            htap_fs::fsync_file(directory.join("f")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_file(directory.join("f")).unwrap();
            htap_fs::dur::remove_dir(&directory).unwrap();
            htap_fs::dur::create_dir(&directory).unwrap();

            write_file(&directory.join("y"), b"new");
            htap_fs::fsync_file(directory.join("y")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
        })
        .unwrap();

    let final_point = harness.snapshot().unwrap().log.len();
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.crash_point != final_point {
                return;
            }

            let directory = root.join("d");
            assert!(directory.is_dir());
            assert!(!directory.join("f").exists());
            assert_eq!(fs::read(directory.join("y")).unwrap(), b"new");
        })
        .unwrap();
}

#[test]
fn chaos_never_mixes_generations() {
    let harness = CrashHarness::new("chaos_never_mixes_generations").unwrap();
    harness
        .run_workload(|workload| {
            let directory = workload.root().join("d");

            htap_fs::dur::create_dir(&directory).unwrap();
            write_file(&directory.join("f"), b"old");
            htap_fs::fsync_file(directory.join("f")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::remove_file(directory.join("f")).unwrap();
            htap_fs::dur::remove_dir(&directory).unwrap();
            htap_fs::dur::create_dir(&directory).unwrap();

            write_file(&directory.join("y"), b"new");
            htap_fs::fsync_file(directory.join("y")).unwrap();
            htap_fs::sync_dir(&directory).unwrap();
        })
        .unwrap();

    for seed in 0..32 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, _| {
                let directory = root.join("d");
                assert!(
                    !(directory.join("f").exists() && directory.join("y").exists()),
                    "chaos mixed directory generations for seed {seed}"
                );
            })
            .unwrap();
    }
}

#[test]
fn chaos_rename_requires_source_identity() {
    let harness = CrashHarness::new("chaos_rename_requires_source_identity").unwrap();
    harness
        .run_workload(|workload| {
            let a = workload.root().join("a");
            let b = workload.root().join("b");
            let c = workload.root().join("c");

            write_file(&a, b"A");
            htap_fs::fsync_file(&a).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();

            htap_fs::dur::rename(&a, &b).unwrap();
            write_file(&a, b"B");
            htap_fs::fsync_file(&a).unwrap();
            htap_fs::dur::rename(&a, &c).unwrap();
        })
        .unwrap();

    for seed in 0..64 {
        harness
            .enumerate(&CrashPolicy::Chaos { seed }, |root, _| {
                let c = root.join("c");
                assert!(
                    !c.exists() || fs::read(&c).unwrap() != b"A",
                    "rename used a stale source identity for seed {seed}"
                );
            })
            .unwrap();
    }
}

fn run_nested_reuse(
    name: &str,
    sync_new_a: bool,
    sync_root: bool,
    expect_a: bool,
    expect_b: bool,
    expected_file: Option<&[u8]>,
) {
    let harness = CrashHarness::new(name).unwrap();
    harness
        .run_workload(|workload| {
            let a = workload.root().join("a");
            let b = a.join("b");
            let old = b.join("old");
            let new = b.join("new");

            htap_fs::dur::create_dir(&a).unwrap();
            htap_fs::sync_dir(workload.root()).unwrap();
            htap_fs::dur::create_dir(&b).unwrap();
            htap_fs::sync_dir(&a).unwrap();
            write_file(&old, b"old");
            htap_fs::fsync_file(&old).unwrap();
            htap_fs::sync_dir(&b).unwrap();

            htap_fs::dur::remove_file(&old).unwrap();
            htap_fs::dur::remove_dir(&b).unwrap();
            htap_fs::dur::remove_dir(&a).unwrap();
            htap_fs::dur::create_dir(&a).unwrap();
            htap_fs::dur::create_dir(&b).unwrap();
            write_file(&new, b"new");
            htap_fs::fsync_file(&new).unwrap();
            htap_fs::sync_dir(&b).unwrap();

            if sync_new_a {
                htap_fs::sync_dir(&a).unwrap();
            }
            if sync_root {
                htap_fs::sync_dir(workload.root()).unwrap();
            }
        })
        .unwrap();

    let final_point = harness.snapshot().unwrap().log.len();
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if info.crash_point != final_point {
                return;
            }

            let a = root.join("a");
            let b = a.join("b");
            let old = b.join("old");
            let new = b.join("new");

            assert_eq!(a.is_dir(), expect_a);
            assert_eq!(b.is_dir(), expect_b);
            match expected_file {
                Some(bytes) => {
                    let file = if bytes == b"old" { &old } else { &new };
                    assert_eq!(fs::read(file).unwrap(), bytes);
                }
                None => {
                    assert!(!old.exists());
                    assert!(!new.exists());
                }
            }
            if expected_file != Some(b"old") {
                assert!(!old.exists());
            }
            if expected_file != Some(b"new") {
                assert!(!new.exists());
            }
        })
        .unwrap();
}

#[test]
fn nested_reuse_without_parent_syncs_keeps_old() {
    run_nested_reuse(
        "nested_reuse_without_parent_syncs_keeps_old",
        false,
        false,
        true,
        true,
        Some(b"old"),
    );
}

#[test]
fn nested_reuse_with_new_a_synced_but_no_root_sync() {
    run_nested_reuse(
        "nested_reuse_with_new_a_synced_but_no_root_sync",
        true,
        false,
        true,
        true,
        Some(b"old"),
    );
}

#[test]
fn nested_reuse_with_all_syncs() {
    run_nested_reuse(
        "nested_reuse_with_all_syncs",
        true,
        true,
        true,
        true,
        Some(b"new"),
    );
}

#[test]
fn nested_reuse_root_sync_without_new_a_sync() {
    run_nested_reuse(
        "nested_reuse_root_sync_without_new_a_sync",
        false,
        true,
        true,
        false,
        None,
    );
}
