use std::collections::HashSet;
use std::path::Path;

use htap_common::fs::{create_dir_all_durable, Op};
use htap_common::{HtapError, Mutation, Result, Row, Value, Version};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};
use htap_rowstore::{CompactionInput, Engine, EngineOptions, WalOptions};

const PARTITION_ID: u64 = 1;
const KEY_COUNT: usize = 3;

fn row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn open_engine(root: &Path) -> Engine {
    match Engine::open(EngineOptions::new(root.join("rowstore")).with_memtable_bytes(1024)) {
        Ok(engine) => engine,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening a crash image returned corruption: {error}");
        }
        Err(error) => {
            panic!("reopening a crash image failed: {error}");
        }
    }
}

fn serial_states() -> Vec<Vec<Option<Row>>> {
    vec![
        vec![None, None, None],
        vec![Some(row(10)), None, None],
        vec![Some(row(10)), Some(row(20)), None],
        vec![None, Some(row(20)), None],
        vec![None, Some(row(20)), Some(row(30))],
    ]
}

fn acked_commit_count(info: &CrashInfo) -> usize {
    info.acked_labels
        .iter()
        .filter(|label| label.starts_with("commit-"))
        .count()
}

fn recovered_state(engine: &Engine) -> Result<Vec<Option<Row>>> {
    let snapshot = engine.snapshot();
    (0..KEY_COUNT)
        .map(|key| engine.get(PARTITION_ID, &[key as u8], snapshot))
        .collect()
}

fn assert_reopens_consistent(root: &Path) -> (Vec<Option<Row>>, Version) {
    let first = open_engine(root);
    let first_state = recovered_state(&first).unwrap();
    let first_committed = first.committed_version();
    drop(first);

    let second = open_engine(root);
    assert_eq!(
        recovered_state(&second).unwrap(),
        first_state,
        "a second reopen changed the recovered state"
    );
    assert_eq!(
        second.committed_version(),
        first_committed,
        "a second reopen changed the durable commit watermark"
    );

    (first_state, first_committed)
}

fn flush_boundary_states() -> Vec<Vec<Option<Row>>> {
    vec![
        vec![None, None, None],
        vec![Some(row(10)), None, None],
        vec![Some(row(10)), Some(row(20)), None],
    ]
}

fn assert_prefix_consistent(root: &Path, info: &CrashInfo) {
    let (first_state, _) = assert_reopens_consistent(root);

    let states = serial_states();
    let recovered_prefix = states
        .iter()
        .position(|state| state == &first_state)
        .expect("recovered state is not a serial-history prefix");
    let acked = acked_commit_count(info);

    assert!(
        acked <= recovered_prefix,
        "recovered prefix {recovered_prefix} omitted {acked} acknowledged commits"
    );
}

fn assert_flush_boundary_prefix_consistent(root: &Path, info: &CrashInfo) {
    let (recovered, _) = assert_reopens_consistent(root);
    let states = flush_boundary_states();
    let recovered_prefix = states
        .iter()
        .position(|state| state == &recovered)
        .expect("recovered state is not a serial-history prefix");
    let acked = acked_commit_count(info);

    assert!(
        acked <= recovered_prefix,
        "recovered prefix {recovered_prefix} omitted {acked} acknowledged commits"
    );
}

fn run_commit_flush_compact_workload(workload: &htap_crashsim::WorkloadContext) {
    let root = workload.root().join("rowstore");
    create_dir_all_durable(&root).unwrap();
    let engine = Engine::open(EngineOptions::new(&root).with_memtable_bytes(1024)).unwrap();

    let mutations = [
        Mutation::Put {
            partition_id: PARTITION_ID,
            key: vec![0],
            row: row(10),
        },
        Mutation::Put {
            partition_id: PARTITION_ID,
            key: vec![1],
            row: row(20),
        },
        Mutation::Delete {
            partition_id: PARTITION_ID,
            key: vec![0],
        },
        Mutation::Put {
            partition_id: PARTITION_ID,
            key: vec![2],
            row: row(30),
        },
    ];

    for (index, mutation) in mutations.into_iter().enumerate() {
        let snapshot = engine.snapshot();
        engine
            .commit((index + 1) as u64, snapshot, vec![mutation])
            .unwrap();
        workload.ack(format!("commit-{}", index + 1));

        if index == 1 || index == 3 {
            engine.flush().unwrap();
            workload.ack(format!("flush-{}", index + 1));
        }
    }

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(HashSet::from([1, 2])),
            gc_horizon: engine.visible_version(),
        })
        .unwrap();
    assert!(report.compacted);
    workload.ack("compact");
}

#[test]
fn engine_commit_flush_compact_prefix_consistent() {
    let harness = CrashHarness::new("engine_commit_flush_compact_prefix_consistent").unwrap();

    harness
        .run_workload(run_commit_flush_compact_workload)
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 17,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();
                assert_prefix_consistent(root, info);
            })
            .unwrap();

        assert!(
            checked_count > 1,
            "expected multiple crash images to be checked"
        );
        assert!(
            checked_with_ack,
            "expected an acknowledged commit crash image to be checked"
        );
    }
}

#[test]
fn engine_wal_gc_after_flush_survives() {
    let harness = CrashHarness::new("engine_wal_gc_after_flush_survives").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("rowstore");
            create_dir_all_durable(&root).unwrap();
            let engine = Engine::open(
                EngineOptions::new(&root)
                    .with_memtable_bytes(1024)
                    .with_wal_options(
                        WalOptions::new(root.join("wal")).with_max_segment_bytes(512),
                    ),
            )
            .unwrap();

            for txn_id in 1..=5 {
                engine
                    .commit(
                        txn_id,
                        engine.snapshot(),
                        vec![Mutation::Put {
                            partition_id: PARTITION_ID,
                            key: vec![txn_id as u8],
                            row: row(txn_id as i64),
                        }],
                    )
                    .unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }

            engine.flush_roll_and_gc().unwrap();

            for txn_id in 6..=8 {
                engine
                    .commit(
                        txn_id,
                        engine.snapshot(),
                        vec![Mutation::Put {
                            partition_id: PARTITION_ID,
                            key: vec![txn_id as u8],
                            row: row(txn_id as i64),
                        }],
                    )
                    .unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }

            workload.ack("gc-done");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();

            let first = open_engine(root);
            let first_snapshot = first.snapshot();
            let first_committed = first.committed_version();

            for txn_id in info
                .acked_labels
                .iter()
                .filter_map(|label| label.strip_prefix("commit-"))
                .filter_map(|value| value.parse::<u64>().ok())
            {
                assert_eq!(
                    first
                        .get(PARTITION_ID, &[txn_id as u8], first_snapshot)
                        .unwrap(),
                    Some(row(txn_id as i64)),
                    "acknowledged row {txn_id} is missing"
                );
            }

            let first_state = (1..=8)
                .map(|key| first.get(PARTITION_ID, &[key], first_snapshot))
                .collect::<Result<Vec<_>>>()
                .unwrap();

            let states = (0..=8)
                .map(|prefix| {
                    (1..=8)
                        .map(|key| {
                            if key <= prefix {
                                Some(row(key as i64))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert!(
                states.iter().any(|state| state == &first_state),
                "recovered state is not a serial-history prefix"
            );

            drop(first);

            let second = open_engine(root);
            let second_snapshot = second.snapshot();
            let second_state = (1..=8)
                .map(|key| second.get(PARTITION_ID, &[key], second_snapshot))
                .collect::<Result<Vec<_>>>()
                .unwrap();

            assert_eq!(
                second_state, first_state,
                "a second reopen changed the recovered state"
            );
            assert_eq!(
                second.committed_version(),
                first_committed,
                "a second reopen changed the durable commit watermark"
            );
        })
        .unwrap();

    assert!(
        checked_count > 1,
        "expected multiple crash images to be checked"
    );
    assert!(
        checked_with_ack,
        "expected an acknowledged commit crash image to be checked"
    );
    assert!(
        harness.snapshot().unwrap().log.iter().any(|op| {
            matches!(
                op,
                Op::Unlink { path, .. } if path.to_string_lossy().ends_with(".wal")
            )
        }),
        "expected flush-roll-GC to unlink at least one WAL segment"
    );
}

#[test]
fn engine_fresh_open_dirs_durable() {
    let harness = CrashHarness::new("engine_fresh_open_dirs_durable").unwrap();

    harness
        .run_workload(|workload| {
            let engine = Engine::open(
                EngineOptions::new(workload.root().join("rowstore")).with_memtable_bytes(1024),
            )
            .unwrap();
            let version = engine
                .commit(
                    1,
                    engine.snapshot(),
                    vec![Mutation::Put {
                        partition_id: PARTITION_ID,
                        key: vec![0],
                        row: row(10),
                    }],
                )
                .unwrap();
            workload.ack(format!("commit-{}", version.get()));
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let commit_acked = info
                .acked_labels
                .iter()
                .any(|label| label.starts_with("commit-"));
            checked_with_ack |= commit_acked;

            let (state, _) = assert_reopens_consistent(root);

            if commit_acked {
                assert_eq!(state[0], Some(row(10)));
            }
        })
        .unwrap();

    assert!(
        checked_count > 1,
        "expected multiple crash images to be checked"
    );
    assert!(
        checked_with_ack,
        "expected an acknowledged commit crash image to be checked"
    );
}

#[test]
fn engine_flush_boundary_crash_depth1() {
    let harness = CrashHarness::new("engine_flush_boundary_crash_depth1").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("rowstore");
            create_dir_all_durable(&root).unwrap();
            let engine = Engine::open(EngineOptions::new(root).with_memtable_bytes(1024)).unwrap();

            for (txn_id, key) in [(1, 0), (2, 1)] {
                engine
                    .commit(
                        txn_id,
                        engine.snapshot(),
                        vec![Mutation::Put {
                            partition_id: PARTITION_ID,
                            key: vec![key],
                            row: row((txn_id * 10) as i64),
                        }],
                    )
                    .unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }

            engine.flush().unwrap();
            workload.ack("flush");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;
    let mut checked_incomplete_recovery = false;
    let mut found_sst_unlink = false;

    harness
        .enumerate_recovery(
            &CrashPolicy::Strict,
            |root| {
                let engine = open_engine(root);
                let _ = recovered_state(&engine).unwrap();
            },
            |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();
                checked_incomplete_recovery |= info
                    .recovery
                    .as_ref()
                    .is_some_and(|recovery| !recovery.completed);

                if let Some(recovery) = &info.recovery {
                    if recovery.completed {
                        found_sst_unlink |= recovery.ops.iter().any(|op| {
                            matches!(
                                op,
                                Op::Unlink { path, .. } if path.to_string_lossy().ends_with(".sst")
                            )
                        });
                    }
                }

                assert!(
                    info.recovery.is_some(),
                    "recovery enumeration did not identify the recovery stage"
                );
                assert_flush_boundary_prefix_consistent(root, info);
            },
        )
        .unwrap();

    assert!(
        checked_count > 1,
        "expected multiple crash images to be checked"
    );
    assert!(
        checked_with_ack,
        "expected an acknowledged commit crash image to be checked"
    );
    assert!(
        checked_incomplete_recovery,
        "expected an incomplete recovery-stage crash image to be checked"
    );
    assert!(
        found_sst_unlink,
        "expected at least one recovery stage with Unlink of .sst"
    );
}

#[test]
fn engine_visible_marker_monotonic() {
    let harness = CrashHarness::new("engine_visible_marker_monotonic").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("rowstore");
            create_dir_all_durable(&root).unwrap();
            let engine = Engine::open(EngineOptions::new(root).with_memtable_bytes(1024)).unwrap();

            for txn_id in 1..=3 {
                let version = engine
                    .commit(
                        txn_id,
                        engine.snapshot(),
                        vec![Mutation::Put {
                            partition_id: PARTITION_ID,
                            key: vec![txn_id as u8],
                            row: row(txn_id as i64),
                        }],
                    )
                    .unwrap();
                workload.ack(format!("txn-{txn_id}-version-{}", version.get()));
            }

            engine.flush().unwrap();
            workload.ack("flush");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 29,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();

                let first = open_engine(root);
                let first_snapshot = first.snapshot();
                let first_visible = first.visible_version();
                let first_committed = first.committed_version();
                let first_state = (1..=3)
                    .map(|key| first.get(PARTITION_ID, &[key], first_snapshot))
                    .collect::<Result<Vec<_>>>()
                    .unwrap();

                let acked_commits = info
                    .acked_labels
                    .iter()
                    .filter_map(|label| label.strip_prefix("txn-"))
                    .filter_map(|label| {
                        let (txn_id, version) = label.split_once("-version-")?;
                        Some((txn_id.parse::<u64>().ok()?, version.parse::<u64>().ok()?))
                    })
                    .collect::<Vec<_>>();
                let last_acked_version =
                    acked_commits.iter().map(|(_, version)| *version).max();

                assert!(
                    first_visible <= first_committed,
                    "visible version {first_visible} exceeds committed version {first_committed}"
                );

                if let Some(version) = last_acked_version {
                    assert!(
                        first_visible >= Version::new(version),
                        "visible version {first_visible} regressed below acknowledged version {version}"
                    );
                }

                for (txn_id, _) in &acked_commits {
                    assert_eq!(
                        first
                            .get(PARTITION_ID, &[*txn_id as u8], first_snapshot)
                            .unwrap(),
                        Some(row(*txn_id as i64)),
                        "acknowledged row {txn_id} is not visible"
                    );
                }

                drop(first);

                let second = open_engine(root);
                let second_snapshot = second.snapshot();
                let second_state = (1..=3)
                    .map(|key| second.get(PARTITION_ID, &[key], second_snapshot))
                    .collect::<Result<Vec<_>>>()
                    .unwrap();

                assert_eq!(
                    second_state, first_state,
                    "a second reopen changed the recovered state"
                );
                assert_eq!(
                    second.visible_version(),
                    first_visible,
                    "a second reopen changed the visible version"
                );
                assert_eq!(
                    second.committed_version(),
                    first_committed,
                    "a second reopen changed the committed version"
                );

                let states = [
                    vec![None, None, None],
                    vec![Some(row(1)), None, None],
                    vec![Some(row(1)), Some(row(2)), None],
                    vec![Some(row(1)), Some(row(2)), Some(row(3))],
                ];
                let _recovered_prefix = states
                    .iter()
                    .position(|state| state == &first_state)
                    .expect("recovered state is not a serial-history prefix");
            })
            .unwrap();

        assert!(
            checked_count > 1,
            "expected multiple crash images to be checked"
        );
        assert!(
            checked_with_ack,
            "expected an acknowledged version crash image to be checked"
        );
    }
}
