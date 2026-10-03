mod powerloss_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_common::fs::{dur, Op};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

use powerloss_support::{
    acked, check_image, execute_step, history, open_server, read_table_state, reopen_twice,
    tree_hash, Flags, Step, TableState,
};

const STEPS: &[Step] = &[
    Step::Create,
    Step::Insert12,
    Step::Update1,
    Step::Delete2,
    Step::Transaction34,
    Step::Restart,
    Step::Insert5,
];

fn run_acked_sql_workload(workload: &htap_crashsim::WorkloadContext) -> usize {
    let mut server = Some(Arc::new(open_server(workload.root())));
    let mut issued = 0;

    for (index, step) in STEPS.iter().copied().enumerate() {
        execute_step(workload, &mut server, index, step);
        issued += 1;
    }

    drop(server);
    issued
}

#[test]
fn server_acked_sql_survives_power_loss() {
    let harness = CrashHarness::new("srv_sql_ack").unwrap();
    let issued = std::cell::Cell::new(0);

    harness
        .run_workload(|workload| issued.set(run_acked_sql_workload(workload)))
        .unwrap();

    let history = history(STEPS);
    assert_eq!(issued.get(), STEPS.len(), "workload omitted a model step");
    assert_eq!(
        STEPS.len(),
        history.len() - 1,
        "issued == checked invariant failed"
    );

    let flags = std::cell::RefCell::new(Flags::default());

    harness
        .enumerate(&CrashPolicy::Strict, |image_root, info| {
            let hash = tree_hash(image_root);
            let recovered = reopen_twice(image_root);
            let matched = check_image(&recovered, info, &history);
            flags.borrow_mut().observe(hash, info, matched, STEPS.len());
        })
        .unwrap();

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 11,
                sector_size: 4096,
            },
            |image_root, info| {
                let hash = tree_hash(image_root);
                let recovered = reopen_twice(image_root);
                let matched = check_image(&recovered, info, &history);
                flags.borrow_mut().observe(hash, info, matched, STEPS.len());
            },
        )
        .unwrap();

    flags.borrow().assert_complete();
}

const BOOTSTRAP_LABELS: &[&str] = &["open", "bootstrapped", "table", "row"];
const BOOTSTRAP_STEPS: usize = BOOTSTRAP_LABELS.len();

fn bootstrap_acked_prefix(info: &CrashInfo) -> usize {
    let prefix = BOOTSTRAP_LABELS
        .iter()
        .take_while(|label| acked(info, label))
        .count();

    assert_eq!(
        info.acked_labels.len(),
        prefix,
        "{}: bootstrap acknowledgements are not an ordered prefix: {:?}",
        info.harness_name,
        info.acked_labels
    );
    for (index, label) in BOOTSTRAP_LABELS.iter().enumerate() {
        assert_eq!(
            acked(info, label),
            index < prefix,
            "{}: bootstrap acknowledgements are not an ordered prefix: {:?}",
            info.harness_name,
            info.acked_labels
        );
    }

    prefix
}

#[derive(Default)]
struct BootstrapFlags {
    images: usize,
    torn_images: usize,
    saw_open_acked: bool,
    saw_all_done: bool,
}

impl BootstrapFlags {
    fn observe(&mut self, info: &CrashInfo) {
        let acked_prefix = bootstrap_acked_prefix(info);

        self.images += 1;
        self.torn_images += usize::from(matches!(info.policy, CrashPolicy::Torn { .. }));
        self.saw_open_acked |= acked_prefix >= 1;
        self.saw_all_done |= acked_prefix == BOOTSTRAP_STEPS;
    }

    fn assert_complete(&self, harness_name: &str) {
        assert!(
            self.images > 10,
            "{harness_name}: expected more than 10 crash images, checked {}",
            self.images
        );
        assert!(
            self.torn_images > 0,
            "{harness_name}: no Torn crash images were checked"
        );
        assert!(
            self.saw_open_acked,
            "{harness_name}: no crash image acknowledged server open"
        );
        assert!(
            self.saw_all_done,
            "{harness_name}: no crash image acknowledged every bootstrap workload step"
        );
    }
}

fn run_bootstrap_workload(
    workload: &htap_crashsim::WorkloadContext,
    server_rel: &Path,
    precreate_volatile_root: bool,
) -> usize {
    let server_root = workload.root().join(server_rel);

    if precreate_volatile_root {
        dur::create_dir_all(&server_root).unwrap_or_else(|error| {
            panic!(
                "volatile server root creation at {} failed: {error:?}",
                server_root.display()
            )
        });
    }

    let mut executed = 0;

    let server = open_server(&server_root);
    workload.ack("open");
    executed += 1;

    server
        .bootstrap_root_account(Some("pw"))
        .unwrap_or_else(|error| {
            panic!(
                "bootstrap_root_account({}) failed; Corruption/bootstrap error: {error:?}",
                server_root.display()
            )
        });
    workload.ack("bootstrapped");
    executed += 1;

    server
        .execute("CREATE TABLE t(id BIGINT PRIMARY KEY, v VARCHAR)")
        .unwrap_or_else(|error| {
            panic!(
                "CREATE TABLE on {} failed; Corruption/query error: {error:?}",
                server_root.display()
            )
        });
    workload.ack("table");
    executed += 1;

    server
        .execute("INSERT INTO t(id, v) VALUES (1, 'a')")
        .unwrap_or_else(|error| {
            panic!(
                "INSERT on {} failed; Corruption/query error: {error:?}",
                server_root.display()
            )
        });
    workload.ack("row");
    executed += 1;

    drop(server);
    executed
}

fn check_bootstrap_image(image_root: &Path, server_rel: &Path, info: &CrashInfo) {
    let server_root = image_root.join(server_rel);
    bootstrap_acked_prefix(info);

    if acked(info, "open") {
        assert!(
            server_root.is_dir(),
            "{}: open was acknowledged but server root {} is absent",
            info.harness_name,
            server_root.display()
        );
        assert!(
            server_root.join("txn.journal").is_file(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.join("txn.journal").display()
        );
        assert!(
            server_root.join("catalog").is_dir(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.join("catalog").display()
        );
        assert!(
            server_root.join("rowstore").is_dir(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.join("rowstore").display()
        );
        assert!(
            server_root.join("colstore").is_dir(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.join("colstore").display()
        );
    }

    if !server_root.is_dir() {
        assert!(
            !acked(info, "bootstrapped") && !acked(info, "table") && !acked(info, "row"),
            "{}: later workload step was acknowledged without a durable server root",
            info.harness_name
        );
        return;
    }

    let first = open_server(&server_root);
    if acked(info, "bootstrapped") {
        let report = first
            .bootstrap_root_account(Some("pw"))
            .unwrap_or_else(|error| {
                panic!(
                    "bootstrap verification on {} failed; Corruption/bootstrap error: {error:?}",
                    server_root.display()
                )
            });
        assert!(
            !report.created_root,
            "{}: acknowledged bootstrap recreated the root account",
            info.harness_name
        );
        assert_eq!(
            report.config_password_matches_root,
            Some(true),
            "{}: acknowledged bootstrap password did not survive",
            info.harness_name
        );
    }
    let first_state = read_table_state(&first);
    drop(first);

    let second = open_server(&server_root);
    if acked(info, "bootstrapped") {
        let report = second
            .bootstrap_root_account(Some("pw"))
            .unwrap_or_else(|error| {
                panic!(
                    "second bootstrap verification on {} failed; Corruption/bootstrap error: \
                     {error:?}",
                    server_root.display()
                )
            });
        assert!(
            !report.created_root,
            "{}: second reopen recreated the acknowledged root account",
            info.harness_name
        );
        assert_eq!(
            report.config_password_matches_root,
            Some(true),
            "{}: second reopen lost the acknowledged bootstrap password",
            info.harness_name
        );
    }
    let second_state = read_table_state(&second);
    drop(second);

    assert_eq!(
        second_state, first_state,
        "{}: recovery state drifted between consecutive LocalServer opens",
        info.harness_name
    );

    if acked(info, "table") {
        assert_ne!(
            first_state,
            TableState::Absent,
            "{}: acknowledged table creation did not survive",
            info.harness_name
        );
    }

    if first_state != TableState::Absent {
        assert!(
            acked(info, "bootstrapped"),
            "{}: recovered a table before bootstrap was acknowledged",
            info.harness_name
        );
    }

    if acked(info, "row") {
        let expected = TableState::Rows(std::collections::BTreeMap::from([(1, "a".to_owned())]));
        assert_eq!(
            first_state, expected,
            "{}: acknowledged row did not survive",
            info.harness_name
        );
    }
}

fn run_bootstrap_powerloss_case(name: &str, server_rel: &Path, precreate_volatile_root: bool) {
    let harness = CrashHarness::new(name).unwrap().with_data_root(server_rel);
    let issued = std::cell::Cell::new(0);

    harness
        .run_workload(|workload| {
            issued.set(run_bootstrap_workload(
                workload,
                server_rel,
                precreate_volatile_root,
            ));
        })
        .unwrap();

    assert_eq!(
        issued.get(),
        BOOTSTRAP_STEPS,
        "{name}: workload omitted a bootstrap model step"
    );

    let mut flags = BootstrapFlags::default();

    harness
        .enumerate(&CrashPolicy::Strict, |image_root, info| {
            check_bootstrap_image(image_root, server_rel, info);
            flags.observe(info);
        })
        .unwrap();

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 11,
                sector_size: 4096,
            },
            |image_root, info| {
                check_bootstrap_image(image_root, server_rel, info);
                flags.observe(info);
            },
        )
        .unwrap();

    flags.assert_complete(name);
}

#[test]
fn server_fresh_root_bootstrap_durable() {
    run_bootstrap_powerloss_case("srv_fresh_1", Path::new("srv"), false);
    run_bootstrap_powerloss_case("srv_fresh_3", Path::new("a/b/srv"), false);
}

#[test]
fn server_preexisting_volatile_root_durable() {
    run_bootstrap_powerloss_case("srv_vol_1", Path::new("srv"), true);
    run_bootstrap_powerloss_case("srv_vol_3", Path::new("a/b/srv"), true);
}

#[test]
fn server_reopen_syncs_volatile_root_children() {
    const SERVER_REL: &str = "a/b/srv";
    const CHILDREN: &[&str] = &["catalog", "rowstore", "movement", "colstore"];

    let server_rel = Path::new(SERVER_REL);
    let harness = CrashHarness::new("srv_reopen_sync")
        .unwrap()
        .with_data_root(server_rel);

    harness
        .run_workload(|workload| {
            let server_root = workload.root().join(server_rel);
            for child in CHILDREN {
                dur::create_dir_all(server_root.join(child)).unwrap_or_else(|error| {
                    panic!(
                        "volatile child creation at {} failed: {error:?}",
                        server_root.join(child).display()
                    )
                });
            }

            let server = open_server(&server_root);
            workload.ack("open");
            drop(server);
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let open_ack = snapshot
        .log
        .iter()
        .position(|op| matches!(op, Op::Ack { label } if label == "open"))
        .expect("server_reopen_syncs_volatile_root_children: open ack is absent");
    let last_mkdir = snapshot.log[..open_ack]
        .iter()
        .enumerate()
        .filter_map(|(index, op)| match op {
            Op::Mkdir { path } if CHILDREN.iter().any(|child| path == &server_rel.join(child)) => {
                Some(index)
            }
            _ => None,
        })
        .max()
        .expect("server_reopen_syncs_volatile_root_children: standard child mkdirs are absent");

    assert!(
        snapshot.log[last_mkdir + 1..open_ack].iter().any(|op| {
            matches!(
                op,
                Op::FsyncDir {
                    path,
                    site: Some("server:open_dir_sync"),
                } if path == server_rel
            )
        }),
        "server_reopen_syncs_volatile_root_children: no final server-root sync followed the \
         volatile child mkdirs before open ack"
    );

    let check_image = |image_root: &Path, info: &CrashInfo| {
        if !acked(info, "open") {
            return;
        }

        let server_root = image_root.join(server_rel);
        assert!(
            server_root.is_dir(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.display()
        );
        for child in CHILDREN {
            assert!(
                server_root.join(child).is_dir(),
                "{}: open was acknowledged but {} is absent",
                info.harness_name,
                server_root.join(child).display()
            );
        }
        assert!(
            server_root.join("txn.journal").is_file(),
            "{}: open was acknowledged but {} is absent",
            info.harness_name,
            server_root.join("txn.journal").display()
        );
    };

    let mut images = 0;
    let mut saw_open_acked = false;

    harness
        .enumerate(&CrashPolicy::Strict, |image_root, info| {
            images += 1;
            saw_open_acked |= acked(info, "open");
            check_image(image_root, info);
        })
        .unwrap();

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 11,
                sector_size: 4096,
            },
            |image_root, info| {
                images += 1;
                saw_open_acked |= acked(info, "open");
                check_image(image_root, info);
            },
        )
        .unwrap();

    assert!(
        images > 5,
        "server_reopen_syncs_volatile_root_children: expected more than 5 crash images, checked \
         {images}"
    );
    assert!(
        saw_open_acked,
        "server_reopen_syncs_volatile_root_children: no image acknowledged server open"
    );
}

#[test]
fn server_open_syncs_colstore_entry_before_ack() {
    let harness = CrashHarness::new("srv_colsync").unwrap();

    harness
        .run_workload(|workload| {
            let server = open_server(workload.root());
            workload.ack("open");
            drop(server);
        })
        .unwrap();

    let snapshot = harness.snapshot().unwrap();
    let open_ack = snapshot
        .log
        .iter()
        .position(|op| matches!(op, Op::Ack { label } if label == "open"))
        .expect("server_open_syncs_colstore_entry_before_ack: open ack is absent");

    let colstore_mkdir = snapshot.log[..open_ack]
        .iter()
        .rposition(|op| matches!(op, Op::Mkdir { path } if path == Path::new("colstore")))
        .expect(
            "server_open_syncs_colstore_entry_before_ack: colstore mkdir is absent before open ack",
        );

    assert!(
        snapshot.log[colstore_mkdir + 1..open_ack]
            .iter()
            .any(|op| matches!(
                op,
                Op::FsyncDir { path, .. } if path.as_os_str().is_empty()
            )),
        "server_open_syncs_colstore_entry_before_ack: colstore mkdir was not followed by a \
         server-root directory sync before open ack"
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NormalizedFile {
    Path(PathBuf),
    Id(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NormalizedOp {
    Create(PathBuf),
    Mkdir(PathBuf),
    Write(NormalizedFile, usize),
    SetLen(NormalizedFile, u64),
    Rename(PathBuf, PathBuf),
    Unlink(PathBuf),
    Rmdir(PathBuf),
    FsyncFile(NormalizedFile, Option<String>),
    FsyncDir(PathBuf, Option<String>),
    Ack(String),
}

fn normalize_log(root: &Path, log: &[Op]) -> Vec<NormalizedOp> {
    let mut file_paths = std::collections::BTreeMap::new();
    let mut normalized = Vec::with_capacity(log.len());

    for op in log {
        let op = match op {
            Op::Create { path, file_id } => {
                let path = normalize_path(root, path);
                file_paths.insert(*file_id, path.clone());
                NormalizedOp::Create(path)
            }
            Op::Mkdir { path } => NormalizedOp::Mkdir(normalize_path(root, path)),
            Op::Write {
                file_id,
                bytes_written,
                ..
            } => {
                // Compare only the length so nondeterministic runtime bytes cannot invalidate repros.
                NormalizedOp::Write(normalize_file(*file_id, &file_paths), bytes_written.len())
            }
            Op::SetLen { file_id, size } => {
                NormalizedOp::SetLen(normalize_file(*file_id, &file_paths), *size)
            }
            Op::Rename {
                from, to, file_id, ..
            } => {
                let from = normalize_path(root, from);
                let to = normalize_path(root, to);
                if file_paths.contains_key(file_id) {
                    file_paths.insert(*file_id, to.clone());
                }
                NormalizedOp::Rename(from, to)
            }
            Op::Unlink { path, .. } => NormalizedOp::Unlink(normalize_path(root, path)),
            Op::Rmdir { path } => NormalizedOp::Rmdir(normalize_path(root, path)),
            Op::FsyncFile { file_id, site } => NormalizedOp::FsyncFile(
                normalize_file(*file_id, &file_paths),
                site.map(str::to_owned),
            ),
            Op::FsyncDir { path, site } => {
                NormalizedOp::FsyncDir(normalize_path(root, path), site.map(str::to_owned))
            }
            Op::Ack { label } => NormalizedOp::Ack(label.clone()),
        };
        normalized.push(op);
    }

    normalized
}

fn normalize_file(
    file_id: u64,
    file_paths: &std::collections::BTreeMap<u64, PathBuf>,
) -> NormalizedFile {
    file_paths
        .get(&file_id)
        .cloned()
        .map(NormalizedFile::Path)
        .unwrap_or(NormalizedFile::Id(file_id))
}

fn normalize_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn record_normalized_log(name: &str) -> Vec<NormalizedOp> {
    let harness = CrashHarness::new(name).unwrap();
    let issued = std::cell::Cell::new(0);

    harness
        .run_workload(|workload| issued.set(run_acked_sql_workload(workload)))
        .unwrap();

    let history = history(STEPS);
    assert_eq!(issued.get(), STEPS.len(), "workload omitted a model step");
    assert_eq!(
        STEPS.len(),
        history.len() - 1,
        "issued == checked invariant failed"
    );

    let snapshot = harness.snapshot().unwrap();
    normalize_log(harness.root(), &snapshot.log)
}

#[test]
fn server_workload_logs_are_deterministic() {
    let first = record_normalized_log("srv_log_a");
    let second = record_normalized_log("srv_log_b");

    assert_eq!(
        first, second,
        "normalized server workload logs differ; POWERLOSS_REPRO would not be deterministic"
    );
}
