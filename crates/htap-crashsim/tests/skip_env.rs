use std::ffi::OsString;
use std::io::Write;

use htap_common::fs::{self as htap_fs, with_skip_sync, DurOpenOptions, Op, SkipSync};
use htap_crashsim::CrashHarness;

const DATA: &[u8] = b"durable contents";
const DATA_SYNC_SITE: &str = "skip_controls:data_sync";
const ROOT_SYNC_SITE: &str = "skip_controls:root_sync";
const SKIP_SYNC_ENV: &str = "POWERLOSS_SKIP_SYNC";

struct EnvGuard(Option<OsString>);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(SKIP_SYNC_ENV, value),
            None => std::env::remove_var(SKIP_SYNC_ENV),
        }
    }
}

fn run_durable_workload(harness: &CrashHarness) {
    harness
        .run_workload(|workload| {
            let path = workload.root().join("data");
            let mut options = DurOpenOptions::new();
            options.write(true).create(true).truncate(true);

            let mut file = options.open(&path).unwrap();
            file.write_all(DATA).unwrap();
            file.sync_all_site(DATA_SYNC_SITE).unwrap();

            htap_fs::dur::sync_dir_site(workload.root(), ROOT_SYNC_SITE).unwrap();
            workload.ack("published");
        })
        .unwrap();
}

#[test]
fn scope_overrides_env() {
    let previous = std::env::var_os(SKIP_SYNC_ENV);
    std::env::set_var(SKIP_SYNC_ENV, "site:skip_controls:env_only");
    let _env_guard = EnvGuard(previous);

    let harness = CrashHarness::new("skip_controls_scope_overrides_env").unwrap();
    with_skip_sync(SkipSync::Site(DATA_SYNC_SITE), || {
        run_durable_workload(&harness);
    });

    let snapshot = harness.snapshot().unwrap();
    assert!(!snapshot.log.iter().any(|op| {
        matches!(
            op,
            Op::FsyncFile { site, .. } if *site == Some(DATA_SYNC_SITE)
        )
    }));
}
