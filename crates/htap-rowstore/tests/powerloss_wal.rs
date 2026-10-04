use htap_common::fs::Op;
use htap_common::{Row, Value, Version};
use htap_crashsim::{CrashHarness, CrashPolicy};
use htap_rowstore::{Wal, WalOptions, WalRecord};

fn put(txn_id: u64) -> WalRecord {
    WalRecord::Put {
        txn_id,
        partition_id: 1,
        key: txn_id.to_be_bytes().to_vec(),
        row: Row::new(vec![Value::Int64(txn_id as i64)]),
        version: Version::new(txn_id),
    }
}

fn commit(txn_id: u64) -> WalRecord {
    WalRecord::Commit {
        txn_id,
        version: Version::new(txn_id),
    }
}

fn acked_txns(labels: &[String]) -> Vec<u64> {
    labels
        .iter()
        .filter_map(|label| label.strip_prefix("commit-")?.parse().ok())
        .collect()
}

fn reopen_commits(root: &std::path::Path, issued: u64) -> Vec<u64> {
    let wal_dir = root.join("wal");
    let _wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();
    let first = Wal::replay(&wal_dir).unwrap().committed_txn_ids();

    assert!(
        first.len() as u64 <= issued,
        "recovery exposed more transactions than were issued"
    );

    let _wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();
    let second = Wal::replay(&wal_dir).unwrap().committed_txn_ids();
    assert_eq!(
        second, first,
        "a second reopen changed the recovered WAL state"
    );

    let mut committed = first.into_iter().collect::<Vec<_>>();
    committed.sort_unstable();
    committed
}

fn assert_acked_commits(root: &std::path::Path, labels: &[String]) {
    let wal_dir = root.join("wal");
    let _wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();
    let replay = Wal::replay(&wal_dir).unwrap();
    let committed = replay.committed_records();

    for txn_id in acked_txns(labels) {
        assert!(
            replay.committed_txn_ids().contains(&txn_id),
            "acked transaction {txn_id} is missing its commit marker"
        );
        assert!(
            committed.iter().any(|(_, record)| record == &put(txn_id)),
            "acked transaction {txn_id} is missing its expected value"
        );
    }
}

fn append_acked_commits(workload: &htap_crashsim::WorkloadContext, count: u64) {
    let wal_dir = workload.root().join("wal");
    htap_common::fs::create_dir_all_durable(&wal_dir).unwrap();
    let mut wal = Wal::open(WalOptions::new(wal_dir)).unwrap();

    for txn_id in 1..=count {
        wal.append(&put(txn_id)).unwrap();
        wal.append_commit(&commit(txn_id)).unwrap();
        workload.ack(format!("commit-{txn_id}"));
    }
}

#[test]
fn wal_acked_commits_survive_strict() {
    let harness = CrashHarness::new("wal_acked_commits_survive_strict").unwrap();

    harness
        .run_workload(|workload| append_acked_commits(workload, 4))
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();
            let recovered = reopen_commits(root, 4);
            assert_eq!(
                recovered,
                (1..=recovered.len() as u64).collect::<Vec<_>>(),
                "recovered transactions must form a contiguous serial history from one"
            );
            assert_acked_commits(root, &info.acked_labels);
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
fn wal_acked_commits_survive_torn() {
    let harness = CrashHarness::new("wal_acked_commits_survive_torn").unwrap();

    harness
        .run_workload(|workload| append_acked_commits(workload, 4))
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(
            &CrashPolicy::Torn {
                seed: 17,
                sector_size: 4096,
            },
            |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();

                let recovered = reopen_commits(root, 4);
                assert_eq!(
                    recovered,
                    (1..=recovered.len() as u64).collect::<Vec<_>>(),
                    "recovered transactions must form a contiguous serial history from one"
                );

                assert_acked_commits(root, &info.acked_labels);
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
}

#[test]
fn wal_segment_roll_and_gc_survive() {
    let harness = CrashHarness::new("wal_segment_roll_and_gc_survive").unwrap();
    const ISSUED: u64 = 12;

    harness
        .run_workload(|workload| {
            let wal_dir = workload.root().join("wal");
            htap_common::fs::create_dir_all_durable(&wal_dir).unwrap();
            let mut wal = Wal::open(WalOptions::new(wal_dir).with_max_segment_bytes(1024)).unwrap();

            for txn_id in 1..=ISSUED {
                wal.append(&put(txn_id)).unwrap();
                wal.append_commit(&commit(txn_id)).unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }

            assert!(
                wal.segment_paths().len() > 1,
                "small segment size should force WAL segment rolls"
            );
            workload.ack("gc-begin");
            assert!(
                wal.gc(Version::new(8)).unwrap() > 0,
                "GC should remove WAL segments covered by the horizon"
            );
            wal.sync().unwrap();
            workload.ack("gc");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();

            let recovered = reopen_commits(root, ISSUED);
            let gc_acked = info.acked_labels.iter().any(|label| label == "gc-begin");

            let relevant_txns = recovered
                .iter()
                .copied()
                .filter(|txn_id| !gc_acked || *txn_id > 8)
                .collect::<Vec<_>>();

            if !gc_acked {
                assert_eq!(
                    relevant_txns,
                    (1..=relevant_txns.len() as u64).collect::<Vec<_>>(),
                    "recovered transactions must form a contiguous serial history from one"
                );
                assert_acked_commits(root, &info.acked_labels);
            } else if let Some(first_txn_id) = relevant_txns.first() {
                let last_txn_id = relevant_txns.last().unwrap();
                assert_eq!(
                    relevant_txns,
                    (*first_txn_id..=*last_txn_id).collect::<Vec<_>>(),
                    "recovered transactions above the GC horizon must be contiguous"
                );
                let retained_acks = info
                    .acked_labels
                    .iter()
                    .filter_map(|label| label.strip_prefix("commit-")?.parse::<u64>().ok())
                    .filter(|txn_id| *txn_id > 8)
                    .map(|txn_id| format!("commit-{txn_id}"))
                    .collect::<Vec<_>>();
                assert_acked_commits(root, &retained_acks);
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
fn wal_recovery_repair_crash_depth1() {
    let harness = CrashHarness::new("wal_recovery_repair_crash_depth1").unwrap();

    harness
        .run_workload(|workload| append_acked_commits(workload, 4))
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;
    let mut checked_incomplete_recovery = false;
    let mut found_repair_sync = false;

    for seed in [17, 19, 21] {
        harness
            .enumerate_recovery(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: 4096,
                },
                |root| {
                    let wal_dir = root.join("wal");
                    let _wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();
                    let _replay = Wal::replay(&wal_dir).unwrap();
                },
                |root, info| {
                    checked_count += 1;
                    checked_with_ack |= !info.acked_labels.is_empty();
                    checked_incomplete_recovery |= info
                        .recovery
                        .as_ref()
                        .is_some_and(|recovery| !recovery.completed);

                    if let Some(recovery) = &info.recovery {
                        if !recovery.ops.is_empty() {
                            let saw_set_len = recovery
                                .ops
                                .iter()
                                .any(|op| matches!(op, Op::SetLen { .. }));
                            let saw_repair_sync = recovery.ops.iter().any(|op| {
                                matches!(
                                    op,
                                    Op::FsyncFile {
                                        site: Some("wal:repair_sync"),
                                        ..
                                    }
                                )
                            });

                            if !recovery.completed {
                                found_repair_sync |= saw_set_len && saw_repair_sync;
                            }
                        }
                    }

                    assert!(
                        info.recovery.is_some(),
                        "recovery enumeration must report a recovery stage"
                    );
                    let recovered = reopen_commits(root, 4);
                    assert_eq!(
                        recovered,
                        (1..=recovered.len() as u64).collect::<Vec<_>>(),
                        "recovered transactions must form a contiguous serial history from one"
                    );
                    assert_acked_commits(root, &info.acked_labels);
                },
            )
            .unwrap();
    }

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
        found_repair_sync,
        "expected a strictly materialized recovery image after the repair truncate and sync to be checked"
    );
}

#[test]
fn wal_fresh_dir_entry_durable() {
    let harness = CrashHarness::new("wal_fresh_dir_entry_durable").unwrap();

    harness
        .run_workload(|workload| {
            let wal_dir = workload.root().join("wal");
            let mut wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();

            wal.append(&put(1)).unwrap();
            wal.append_commit(&commit(1)).unwrap();
            workload.ack("commit-1");
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let commit_acked = info.acked_labels.iter().any(|label| label == "commit-1");
            checked_with_ack |= commit_acked;

            let recovered = reopen_commits(root, 1);
            assert_eq!(
                recovered,
                (1..=recovered.len() as u64).collect::<Vec<_>>(),
                "recovered transactions must form a contiguous serial history from one"
            );

            if commit_acked {
                let wal_dir = root.join("wal");
                assert!(
                    wal_dir.is_dir(),
                    "acked WAL directory entry disappeared after the crash"
                );

                let replay = Wal::replay(&wal_dir).unwrap();
                assert!(
                    replay.committed_txn_ids().contains(&1),
                    "acked transaction disappeared with its fresh WAL directory"
                );
                assert!(
                    replay
                        .committed_records()
                        .iter()
                        .any(|(_, record)| record == &put(1)),
                    "acked transaction has an incorrect recovered value"
                );
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

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_wal_append_sync,
    site = "wal:append_sync",
    body = wal_acked_commits_survive_strict
);
htap_crashsim::crashsim_witness!(
    witness_wal_roll_sync,
    site = "wal:roll_sync",
    body = wal_segment_roll_and_gc_survive
);
htap_crashsim::crashsim_witness!(
    witness_wal_roll_dir_sync,
    site = "wal:roll_dir_sync",
    body = wal_fresh_dir_entry_durable
);
htap_crashsim::crashsim_survivor!(
    survivor_wal_gc_sync,
    site = "wal:gc_sync",
    body = wal_segment_roll_and_gc_survive
);
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = wal_acked_commits_survive_strict
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = wal_acked_commits_survive_strict
);
