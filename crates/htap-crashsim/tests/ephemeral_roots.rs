use std::fs;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use htap_common::fs::{self as htap_fs, DurOpenOptions};
use htap_crashsim::{CrashHarness, CrashPolicy};

const DATA_SYNC_SITE: &str = "ephemeral_roots:data_sync";
const DIR_SYNC_SITE: &str = "ephemeral_roots:dir_sync";

fn create_durable_file(path: &Path, contents: &[u8]) {
    let mut options = DurOpenOptions::new();
    options.write(true).create(true).truncate(true);

    let mut file = options.open(path).unwrap();
    file.write_all(contents).unwrap();
    file.sync_all_site(DATA_SYNC_SITE).unwrap();
    htap_fs::dur::sync_dir_site(path.parent().unwrap(), DIR_SYNC_SITE).unwrap();
}

#[cfg(unix)]
#[test]
fn ephemeral_root_ignores_lock_spill_sock_and_ipc_dirs_under_nested_root() {
    use std::os::unix::net::UnixListener;

    let harness = CrashHarness::new("ephemeral_roots_nested")
        .unwrap()
        .with_data_root("a/b/srv");

    harness
        .run_workload(|workload| {
            let srv = workload.root().join("a/b/srv");
            htap_fs::create_dir_all_durable(&srv).unwrap();

            create_durable_file(&srv.join("data"), b"durable payload");

            fs::write(srv.join("LOCK"), b"unrecorded lock").unwrap();
            fs::create_dir(srv.join("spill")).unwrap();
            fs::write(srv.join("spill/x"), b"unrecorded spill").unwrap();

            let _listener = UnixListener::bind(srv.join("htap.sock")).unwrap();

            fs::create_dir(srv.join(".htap-ipc-123")).unwrap();
            fs::write(srv.join(".htap-ipc-123/f"), b"unrecorded ipc").unwrap();

            workload.ack("published");
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    for op in &snapshot.log {
        let rendered = format!("{op:?}");
        assert!(
            !rendered.contains("LOCK"),
            "ignored LOCK was recorded: {op:?}"
        );
        assert!(
            !rendered.contains("spill"),
            "ignored spill path was recorded: {op:?}"
        );
        assert!(
            !rendered.contains("htap.sock"),
            "ignored socket was recorded: {op:?}"
        );
        assert!(
            !rendered.contains(".htap-ipc-123"),
            "ignored IPC directory was recorded: {op:?}"
        );
    }

    let mut checked = 0;
    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            if !info.acked_labels.iter().any(|label| label == "published") {
                return;
            }

            checked += 1;
            assert_eq!(
                fs::read(root.join("a/b/srv/data")).unwrap(),
                b"durable payload"
            );
        })
        .unwrap();

    assert!(checked > 0, "no acknowledged image was checked");
}

#[test]
fn ephemeral_root_does_not_hide_other_files() {
    let cases = [
        ("ephemeral_roots_outer_lock", "a/LOCK"),
        ("ephemeral_roots_nested_lock", "a/b/srv/sub/LOCK"),
        ("ephemeral_roots_ordinary_file", "a/b/srv/data.bin"),
    ];

    for (name, raw_relative_path) in cases {
        let harness = CrashHarness::new(name).unwrap().with_data_root("a/b/srv");

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            harness
                .run_workload(|workload| {
                    let srv = workload.root().join("a/b/srv");
                    htap_fs::create_dir_all_durable(&srv).unwrap();

                    if raw_relative_path.contains("/sub/") {
                        htap_fs::create_dir_all_durable(srv.join("sub")).unwrap();
                    }

                    fs::write(workload.root().join(raw_relative_path), b"raw mutation").unwrap();
                })
                .unwrap();
        }));

        assert!(
            result.is_err(),
            "{raw_relative_path} should not be hidden by the ephemeral root"
        );
    }
}

#[cfg(unix)]
#[test]
fn ephemeral_root_default_root_still_ignores_lock_and_spill() {
    use std::os::unix::net::UnixListener;

    let harness = CrashHarness::new("ephemeral_roots_default_root").unwrap();

    harness
        .run_workload(|workload| {
            fs::write(workload.root().join("LOCK"), b"unrecorded lock").unwrap();

            fs::create_dir(workload.root().join("spill")).unwrap();
            fs::write(workload.root().join("spill/x"), b"unrecorded spill").unwrap();

            let _listener = UnixListener::bind(workload.root().join("htap.sock")).unwrap();
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    for op in &snapshot.log {
        let rendered = format!("{op:?}");
        assert!(
            !rendered.contains("LOCK"),
            "ignored LOCK was recorded: {op:?}"
        );
        assert!(
            !rendered.contains("spill"),
            "ignored spill path was recorded: {op:?}"
        );
        assert!(
            !rendered.contains("htap.sock"),
            "ignored socket was recorded: {op:?}"
        );
    }
}
