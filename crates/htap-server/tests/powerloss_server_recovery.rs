mod powerloss_support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{CatalogSnapshot, StorageDescriptor, TabletId};
use htap_common::fs::Op;
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

use powerloss_support::{
    check_image, issued_and_done, open_server, reopen_twice, tree_hash, Flags, TableState,
};

#[derive(Clone, Copy, Debug)]
enum ConversionStep {
    Create,
    Insert14,
    Convert,
    Insert5,
    Drop,
    Compact,
    Recreate,
    Insert9,
}

impl ConversionStep {
    fn apply(self, state: &mut TableState) {
        match self {
            Self::Create | Self::Recreate => {
                assert_eq!(*state, TableState::Absent);
                *state = TableState::Rows(BTreeMap::new());
            }
            Self::Insert14 => {
                let rows = rows_mut(state);
                for (id, value) in [(1, "a"), (2, "b"), (3, "c"), (4, "d")] {
                    assert_eq!(rows.insert(id, value.to_owned()), None);
                }
            }
            Self::Convert | Self::Compact => {}
            Self::Insert5 => {
                assert_eq!(rows_mut(state).insert(5, "e".to_owned()), None);
            }
            Self::Drop => {
                assert!(matches!(state, TableState::Rows(_)));
                *state = TableState::Absent;
            }
            Self::Insert9 => {
                assert_eq!(rows_mut(state).insert(9, "z".to_owned()), None);
            }
        }
    }
}

const CONVERSION_STEPS: &[ConversionStep] = &[
    ConversionStep::Create,
    ConversionStep::Insert14,
    ConversionStep::Convert,
    ConversionStep::Insert5,
    ConversionStep::Drop,
    ConversionStep::Compact,
    ConversionStep::Recreate,
    ConversionStep::Insert9,
];

#[derive(Clone, Copy, Debug)]
enum RecoveryStep {
    Create,
    Insert14,
    Convert,
    Insert5,
    Drop,
}

impl RecoveryStep {
    fn apply(self, state: &mut TableState) {
        match self {
            Self::Create => {
                assert_eq!(*state, TableState::Absent);
                *state = TableState::Rows(BTreeMap::new());
            }
            Self::Insert14 => {
                let rows = rows_mut(state);
                for (id, value) in [(1, "a"), (2, "b"), (3, "c"), (4, "d")] {
                    assert_eq!(rows.insert(id, value.to_owned()), None);
                }
            }
            Self::Convert => {}
            Self::Insert5 => {
                assert_eq!(rows_mut(state).insert(5, "e".to_owned()), None);
            }
            Self::Drop => {
                assert!(matches!(state, TableState::Rows(_)));
                *state = TableState::Absent;
            }
        }
    }
}

const RECOVERY_STEPS: &[RecoveryStep] = &[
    RecoveryStep::Create,
    RecoveryStep::Insert14,
    RecoveryStep::Convert,
    RecoveryStep::Insert5,
    RecoveryStep::Drop,
];

fn rows_mut(state: &mut TableState) -> &mut BTreeMap<i64, String> {
    match state {
        TableState::Rows(rows) => rows,
        TableState::Absent => panic!("model step requires table t to exist"),
    }
}

fn conversion_history() -> Vec<TableState> {
    let mut history = vec![TableState::Absent];
    let mut state = TableState::Absent;

    for step in CONVERSION_STEPS {
        step.apply(&mut state);
        history.push(state.clone());
    }

    history
}

fn recovery_history() -> Vec<TableState> {
    let mut history = vec![TableState::Absent];
    let mut state = TableState::Absent;

    for step in RECOVERY_STEPS {
        step.apply(&mut state);
        history.push(state.clone());
    }

    history
}

fn issue_step<T, E>(
    workload: &htap_crashsim::WorkloadContext,
    index: usize,
    call: impl FnOnce() -> Result<T, E>,
) -> T
where
    E: std::fmt::Debug,
{
    workload.ack(format!("issue-{index}"));
    let result = call().unwrap_or_else(|error| {
        panic!("workload step {index} failed; Corruption/workload error: {error:?}")
    });
    workload.ack(format!("done-{index}"));
    result
}

fn load_catalog(root: &Path) -> CatalogSnapshot {
    let store = LocalCatalogStore::open(root.join("catalog")).unwrap_or_else(|error| {
        panic!(
            "LocalCatalogStore::open({}) failed; Corruption/catalog open error: {error:?}",
            root.join("catalog").display()
        )
    });
    store
        .load()
        .unwrap_or_else(|error| {
            panic!(
                "catalog load at {} failed; Corruption/catalog load error: {error:?}",
                root.display()
            )
        })
        .unwrap_or_else(CatalogSnapshot::empty)
}

fn table_tablet(snapshot: &CatalogSnapshot, table_name: &str) -> Option<TabletId> {
    let table = snapshot.table_by_name(table_name)?;
    assert_eq!(
        table.partitions.len(),
        1,
        "table {table_name} must have exactly one partition"
    );
    let partition = snapshot
        .partition(table.partitions[0])
        .unwrap_or_else(|| panic!("table {table_name} references a missing partition"));
    assert_eq!(
        partition.tablets.len(),
        1,
        "table {table_name} must have exactly one tablet"
    );
    Some(partition.tablets[0])
}

fn run_conversion_workload(
    workload: &htap_crashsim::WorkloadContext,
    old_tablet: &Cell<Option<TabletId>>,
) -> usize {
    let server = open_server(workload.root());
    let mut issued = 0;

    issue_step(workload, 0, || {
        server.execute("CREATE TABLE t(id BIGINT PRIMARY KEY, v VARCHAR)")
    });
    issued += 1;

    let snapshot = load_catalog(workload.root());
    old_tablet.set(Some(
        table_tablet(&snapshot, "t").expect("newly created table t has no tablet"),
    ));

    issue_step(workload, 1, || {
        server.execute(
            "INSERT INTO t(id, v) VALUES \
             (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
        )
    });
    issued += 1;

    issue_step(workload, 2, || {
        let report = server.convert_table_to_column("t")?;
        assert!(
            report.is_success(),
            "conversion returned an unsuccessful report: {report:?}"
        );
        Ok::<_, htap_common::HtapError>(())
    });
    issued += 1;

    issue_step(workload, 3, || {
        server.execute("INSERT INTO t(id, v) VALUES (5, 'e')")
    });
    issued += 1;

    issue_step(workload, 4, || server.execute("DROP TABLE t"));
    issued += 1;

    issue_step(workload, 5, || {
        let old_tablet = old_tablet
            .get()
            .expect("original tablet id was not recorded before reclamation");

        for _ in 0..32 {
            server.compaction_tick()?;

            let snapshot = load_catalog(workload.root());
            let reclaim_pending = snapshot.pending_reclaim.iter().any(|entry| {
                entry
                    .dropped
                    .iter()
                    .any(|artifact| artifact.tablet_id == old_tablet)
            });
            if !reclaim_pending {
                return Ok::<(), htap_common::HtapError>(());
            }
        }

        panic!(
            "reclamation did not remove the pending_reclaim entry for old tablet \
             {old_tablet} after 32 compaction ticks"
        );
    });
    issued += 1;

    issue_step(workload, 6, || {
        server.execute("CREATE TABLE t(id BIGINT PRIMARY KEY, v VARCHAR)")
    });
    issued += 1;

    issue_step(workload, 7, || {
        server.execute("INSERT INTO t(id, v) VALUES (9, 'z')")
    });
    issued += 1;

    drop(server);
    issued
}

fn check_conversion_image(
    image_root: &Path,
    info: &CrashInfo,
    history: &[TableState],
    old_tablet: TabletId,
    flags: &RefCell<Flags>,
) {
    let (issue_count, done_count) = issued_and_done(info, CONVERSION_STEPS.len());
    let old_tablet_dir = htap_convert::tablet_dir(&image_root.join("colstore"), old_tablet);

    // DROP returns only after reclaiming the old column-store and movement artifacts.
    if done_count >= 5 {
        assert!(
            !old_tablet_dir.exists(),
            "{}: acknowledged DROP left old tablet directory {}",
            info.repro_string(),
            old_tablet_dir.display()
        );
    }

    let hash = tree_hash(image_root);
    let recovered = reopen_twice(image_root);
    let matched = check_image(&recovered, info, history);
    let snapshot = load_catalog(image_root);

    // Conversion is durable and externally readable until DROP begins.
    if done_count >= 3 && issue_count < 5 {
        let table = snapshot
            .table_by_name("t")
            .expect("acknowledged conversion lost table t before DROP was issued");
        assert_eq!(table.partitions.len(), 1);
        let partition = snapshot
            .partition(table.partitions[0])
            .expect("converted table references a missing partition");
        assert_eq!(
            partition.storage,
            StorageDescriptor::Column,
            "acknowledged conversion did not recover as Column storage"
        );
        assert_eq!(partition.tablets, vec![old_tablet]);

        htap_convert::open(&image_root.join("colstore"), old_tablet).unwrap_or_else(|error| {
            panic!(
                "{}: converted tablet manifest failed to open; Corruption/convert error: \
                 {error:?}",
                info.repro_string()
            )
        });

        assert_eq!(
            recovered, history[matched.matched],
            "converted rows do not equal the matched model history"
        );
    }

    // Once the replacement CREATE is durable, identifiers must not be reused and old rows
    // must not become reachable through the new table.
    if done_count >= 7 {
        let new_tablet = table_tablet(&snapshot, "t")
            .expect("acknowledged replacement CREATE did not recover table t");
        assert!(
            new_tablet.as_u64() > old_tablet.as_u64(),
            "replacement table reused old tablet id {old_tablet}; new id is {new_tablet}"
        );

        match &recovered {
            TableState::Absent => {
                panic!("acknowledged replacement CREATE recovered without table t")
            }
            TableState::Rows(rows) => {
                assert!(
                    rows.keys().all(|id| *id == 9),
                    "replacement table resurrected rows from the dropped table: {rows:?}"
                );
            }
        }
    }

    // An acknowledged compaction confirms the rowstore purge and allows the completed
    // pending-reclaim entry to be removed permanently.
    if done_count >= 6 {
        assert!(
            snapshot.pending_reclaim.iter().all(|entry| {
                entry
                    .dropped
                    .iter()
                    .all(|artifact| artifact.tablet_id != old_tablet)
            }),
            "acked reclaim reintroduced pending_reclaim for old tablet {old_tablet}"
        );
    } else if done_count >= 5 {
        for entry in snapshot.pending_reclaim.iter().filter(|entry| {
            entry
                .dropped
                .iter()
                .any(|artifact| artifact.tablet_id == old_tablet)
        }) {
            assert!(
                entry.colstore_and_movement_reclaimed,
                "acknowledged DROP retained an unreclaimed colstore/movement entry"
            );
        }
    }

    flags
        .borrow_mut()
        .observe(hash, info, matched, CONVERSION_STEPS.len());
}

#[test]
fn server_conversion_and_reclaim_survive() {
    let harness = CrashHarness::new("srv_conv_reclaim").unwrap();
    let issued = Cell::new(0);
    let old_tablet = Cell::new(None);

    harness
        .run_workload(|workload| {
            issued.set(run_conversion_workload(workload, &old_tablet));
        })
        .unwrap();

    let history = conversion_history();
    assert_eq!(
        issued.get(),
        CONVERSION_STEPS.len(),
        "workload omitted a model step"
    );
    assert_eq!(
        CONVERSION_STEPS.len(),
        history.len() - 1,
        "issued == checked invariant failed"
    );

    let old_tablet = old_tablet
        .get()
        .expect("workload did not record the original tablet id");
    let flags = RefCell::new(Flags::default());

    harness
        .enumerate(&CrashPolicy::Strict, |image_root, info| {
            check_conversion_image(image_root, info, &history, old_tablet, &flags);
        })
        .unwrap();

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 11,
                sector_size: 4096,
            },
            |image_root, info| {
                check_conversion_image(image_root, info, &history, old_tablet, &flags);
            },
        )
        .unwrap();

    flags.borrow().assert_complete();
}

fn run_recovery_workload(workload: &htap_crashsim::WorkloadContext) -> usize {
    let server = open_server(workload.root());
    let mut issued = 0;

    issue_step(workload, 0, || {
        server.execute("CREATE TABLE t(id BIGINT PRIMARY KEY, v VARCHAR)")
    });
    issued += 1;

    issue_step(workload, 1, || {
        server.execute(
            "INSERT INTO t(id, v) VALUES \
             (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
        )
    });
    issued += 1;

    issue_step(workload, 2, || {
        let report = server.convert_table_to_column("t")?;
        assert!(
            report.is_success(),
            "conversion returned an unsuccessful report: {report:?}"
        );
        Ok::<_, htap_common::HtapError>(())
    });
    issued += 1;

    issue_step(workload, 3, || {
        server.execute("INSERT INTO t(id, v) VALUES (5, 'e')")
    });
    issued += 1;

    issue_step(workload, 4, || server.execute("DROP TABLE t"));
    issued += 1;

    drop(server);
    issued
}

#[derive(Default)]
struct RecoveryProofFlags {
    saw_incomplete_nonempty_recovery: bool,
    saw_torn_repair: bool,
    saw_reclaim: bool,
    saw_journal_replay: bool,
}

impl RecoveryProofFlags {
    fn observe(&mut self, info: &CrashInfo) {
        let recovery = info
            .recovery
            .as_ref()
            .expect("enumerate_recovery checker received no recovery stage");

        self.saw_incomplete_nonempty_recovery |= !recovery.completed && !recovery.ops.is_empty();

        if matches!(info.policy, CrashPolicy::Torn { .. }) {
            let saw_set_len = recovery
                .ops
                .iter()
                .any(|op| matches!(op, Op::SetLen { .. }));
            let saw_repair_sync = recovery.ops.iter().any(|op| {
                matches!(
                    op,
                    Op::FsyncFile {
                        site: Some(site),
                        ..
                    } if site.contains("repair_sync")
                )
            });
            self.saw_torn_repair |= saw_set_len && saw_repair_sync;
        }

        self.saw_reclaim |= recovery.ops.iter().any(|op| match op {
            Op::Unlink { path, .. } | Op::Rmdir { path } => {
                path_is_under(path, "colstore") || path_is_under(path, "movement")
            }
            _ => false,
        });

        self.saw_journal_replay |= recovery_writes_named_file(recovery.ops, |path| {
            path == Path::new("txn.journal") || path.ends_with("VISIBLE.tmp")
        });
    }

    fn assert_complete(&self) {
        assert!(
            self.saw_incomplete_nonempty_recovery,
            "no non-completed recovery image contained recovery operations"
        );
        assert!(
            self.saw_torn_repair,
            "no Torn outer image recovery log contained both SetLen and repair_sync FsyncFile"
        );
        assert!(
            self.saw_reclaim,
            "no recovery log unlinked or removed an entry under colstore/ or movement/"
        );
        assert!(
            self.saw_journal_replay,
            "no recovery log wrote txn.journal or VISIBLE.tmp"
        );
    }
}

fn path_is_under(path: &Path, directory: &str) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Normal(component)) if component == directory
    )
}

fn recovery_writes_named_file(ops: &[Op], mut expected: impl FnMut(&Path) -> bool) -> bool {
    let mut paths_by_id = BTreeMap::<u64, PathBuf>::new();

    for op in ops {
        match op {
            Op::Create { path, file_id } => {
                paths_by_id.insert(*file_id, path.clone());
            }
            Op::Rename { to, file_id, .. } => {
                paths_by_id.insert(*file_id, to.clone());
            }
            Op::Unlink { file_id, .. } => {
                paths_by_id.remove(file_id);
            }
            Op::Write { file_id, .. } => {
                if paths_by_id.get(file_id).is_some_and(|path| expected(path)) {
                    return true;
                }
            }
            Op::Mkdir { .. }
            | Op::SetLen { .. }
            | Op::FsyncFile { .. }
            | Op::FsyncDir { .. }
            | Op::Rmdir { .. }
            | Op::Ack { .. } => {}
        }
    }

    false
}

fn check_recovery_image(
    image_root: &Path,
    info: &CrashInfo,
    history: &[TableState],
    flags: &RefCell<Flags>,
    proof: &RefCell<RecoveryProofFlags>,
) {
    let hash = tree_hash(image_root);
    let recovered = reopen_twice(image_root);
    let matched = check_image(&recovered, info, history);

    flags
        .borrow_mut()
        .observe(hash, info, matched, RECOVERY_STEPS.len());
    proof.borrow_mut().observe(info);
}

fn enumerate_recovery_policy(
    harness: &CrashHarness,
    policy: &CrashPolicy,
    history: &[TableState],
    flags: &RefCell<Flags>,
    proof: &RefCell<RecoveryProofFlags>,
) {
    harness
        .enumerate_recovery(
            policy,
            |root| {
                let server = open_server(root);
                drop(server);
            },
            |image_root, info| {
                check_recovery_image(image_root, info, history, flags, proof);
            },
        )
        .unwrap();
}

#[test]
fn server_recovery_crash_depth1() {
    let harness = CrashHarness::new("srv_recover_d1").unwrap();
    let issued = Cell::new(0);

    harness
        .run_workload(|workload| issued.set(run_recovery_workload(workload)))
        .unwrap();

    let history = recovery_history();
    assert_eq!(
        issued.get(),
        RECOVERY_STEPS.len(),
        "workload omitted a model step"
    );
    assert_eq!(
        RECOVERY_STEPS.len(),
        history.len() - 1,
        "issued == checked invariant failed"
    );

    let flags = RefCell::new(Flags::default());
    let proof = RefCell::new(RecoveryProofFlags::default());

    enumerate_recovery_policy(&harness, &CrashPolicy::Strict, &history, &flags, &proof);
    enumerate_recovery_policy(
        &harness,
        &CrashPolicy::Torn {
            seed: 23,
            sector_size: 4096,
        },
        &history,
        &flags,
        &proof,
    );

    flags.borrow().assert_complete();
    proof.borrow().assert_complete();
}
