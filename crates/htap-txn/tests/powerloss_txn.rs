use htap_crashsim::{CrashHarness, CrashPolicy};
use htap_txn::{JournalOptions, TransactionManager};

fn reopen_committed_txns(root: &std::path::Path) -> Vec<u64> {
    let journal_dir = root.join("journal");
    let rowstore_dir = root.join("rowstore");
    let participant_id = htap_txn::ParticipantId::new(1);

    let reopen_and_recover = || {
        let manager =
            TransactionManager::open_with_options(JournalOptions::new(&journal_dir)).unwrap();
        let engine = std::sync::Arc::new(
            htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir)).unwrap(),
        );
        let participant =
            std::sync::Arc::new(htap_txn::RowstoreParticipant::new(participant_id, engine));
        manager.register_participant(participant);

        manager
            .recover()
            .unwrap()
            .committed_txns
            .into_iter()
            .map(|txn_id| txn_id.as_u64())
            .collect::<Vec<_>>()
    };

    let first = reopen_and_recover();
    let second = reopen_and_recover();

    assert_eq!(
        second, first,
        "a second reopen changed the recovered transaction state"
    );

    first
}

#[test]
fn txn_recover_after_every_boundary() {
    let harness = CrashHarness::new("txn_recover_after_every_boundary").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let rowstore_dir = workload.root().join("rowstore");
            let manager =
                TransactionManager::open_with_options(JournalOptions::new(&journal_dir)).unwrap();

            let engine = std::sync::Arc::new(
                htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir))
                    .unwrap(),
            );
            let participant_id = htap_txn::ParticipantId::new(1);
            let participant = std::sync::Arc::new(htap_txn::RowstoreParticipant::new(
                participant_id,
                std::sync::Arc::clone(&engine),
            ));
            manager.register_participant(participant);

            manager.recover().unwrap();

            for txn_id in 1..=2 {
                let mut txn = manager.begin().unwrap();
                assert_eq!(txn.id().as_u64(), txn_id);

                let mutations = vec![htap_common::Mutation::Put {
                    partition_id: 0,
                    key: vec![txn_id as u8],
                    row: htap_common::Row::new(vec![htap_common::Value::Int64(txn_id as i64)]),
                }];
                let payload = htap_txn::RowstoreParticipant::encode_payload(&mutations).unwrap();
                txn.add_participant(participant_id, payload);

                manager.commit(&mut txn).unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();

            let recovered = reopen_committed_txns(root);
            assert_eq!(
                recovered,
                (1..=recovered.len() as u64).collect::<Vec<_>>(),
                "recovered transactions must form a contiguous serial history from one"
            );

            for label in &info.acked_labels {
                if let Some(txn_id) = label
                    .strip_prefix("commit-")
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    assert!(
                        recovered.contains(&txn_id),
                        "acked transaction {txn_id} is missing after recovery"
                    );
                }
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
fn txn_preexisting_volatile_journal_dir() {
    let harness = CrashHarness::new("txn_preexisting_volatile_journal_dir").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let journal_path = journal_dir.join("txn.journal");
            let rowstore_dir = workload.root().join("rowstore");

            let engine = std::sync::Arc::new(
                htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir))
                    .unwrap(),
            );

            // Simulate a directory created by an earlier process but not yet synced.
            htap_common::fs::dur::create_dir_all(&journal_dir).unwrap();

            let manager =
                TransactionManager::open_with_options(JournalOptions::new(&journal_path)).unwrap();
            let participant_id = htap_txn::ParticipantId::new(1);
            let participant = std::sync::Arc::new(htap_txn::RowstoreParticipant::new(
                participant_id,
                std::sync::Arc::clone(&engine),
            ));
            manager.register_participant(participant);

            manager.recover().unwrap();

            let mut txn = manager.begin().unwrap();
            assert_eq!(txn.id().as_u64(), 1);

            let mutations = vec![htap_common::Mutation::Put {
                partition_id: 0,
                key: vec![1],
                row: htap_common::Row::new(vec![htap_common::Value::Int64(1)]),
            }];
            let payload = htap_txn::RowstoreParticipant::encode_payload(&mutations).unwrap();
            txn.add_participant(participant_id, payload);

            manager.commit(&mut txn).unwrap();
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

            let journal_path = root.join("journal").join("txn.journal");

            if commit_acked {
                assert!(
                    root.join("journal").is_dir(),
                    "acked journal directory entry disappeared after the crash"
                );
            }

            let recover_with_participant = || {
                let rowstore_dir = root.join("rowstore");
                let engine = std::sync::Arc::new(
                    htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir))
                        .unwrap(),
                );
                let participant = std::sync::Arc::new(htap_txn::RowstoreParticipant::new(
                    htap_txn::ParticipantId::new(1),
                    std::sync::Arc::clone(&engine),
                ));
                let manager =
                    TransactionManager::open_with_options(JournalOptions::new(&journal_path))
                        .unwrap();
                manager.register_participant(participant);

                let committed = manager
                    .recover()
                    .unwrap()
                    .committed_txns
                    .into_iter()
                    .map(|txn_id| txn_id.as_u64())
                    .collect::<Vec<_>>();
                let snapshot = engine.snapshot();
                let row = engine.get(0, &[1], snapshot).unwrap();

                (committed, row)
            };

            let first = recover_with_participant();
            let second = recover_with_participant();

            assert_eq!(
                second, first,
                "a second reopen changed the recovered transaction state"
            );

            assert_eq!(
                first.0,
                (1..=first.0.len() as u64).collect::<Vec<_>>(),
                "recovered transactions must form a contiguous serial history from one"
            );

            if commit_acked {
                assert!(
                    first.0.contains(&1),
                    "recovered transaction history is missing transaction 1"
                );
                assert_eq!(
                    first.1,
                    Some(htap_common::Row::new(vec![htap_common::Value::Int64(1)])),
                    "acked transaction 1 mutation disappeared"
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

fn reopen_history(
    root: &std::path::Path,
    journal_path: &std::path::Path,
    txn_count: u64,
) -> (Vec<u64>, Vec<Option<htap_common::Row>>) {
    let rowstore_dir = root.join("rowstore");
    let manager = TransactionManager::open_with_options(JournalOptions::new(journal_path)).unwrap();
    let engine = std::sync::Arc::new(
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir)).unwrap(),
    );
    let participant = std::sync::Arc::new(htap_txn::RowstoreParticipant::new(
        htap_txn::ParticipantId::new(1),
        std::sync::Arc::clone(&engine),
    ));
    manager.register_participant(participant);

    let committed = match manager.recover() {
        Ok(recovery) => recovery
            .committed_txns
            .into_iter()
            .map(|txn_id| txn_id.as_u64())
            .collect(),
        Err(htap_common::HtapError::Corruption(error)) => {
            panic!("transaction journal corruption after recovery: {error}");
        }
        Err(error) => panic!("failed to recover transaction journal: {error}"),
    };

    let snapshot = engine.snapshot();
    let rows = (1..=txn_count)
        .map(|txn_id| engine.get(0, &[txn_id as u8], snapshot).unwrap())
        .collect();

    (committed, rows)
}

fn assert_history_prefix(
    root: &std::path::Path,
    journal_path: &std::path::Path,
    txn_count: u64,
    acked_labels: &[String],
) -> (Vec<u64>, Vec<Option<htap_common::Row>>) {
    let first = reopen_history(root, journal_path, txn_count);
    let second = reopen_history(root, journal_path, txn_count);

    assert_eq!(
        second, first,
        "a second reopen changed the recovered transaction outcome or rowstore state"
    );

    let recovered_prefix = first
        .1
        .iter()
        .enumerate()
        .take_while(|(index, row)| {
            **row
                == Some(htap_common::Row::new(vec![htap_common::Value::Int64(
                    (*index + 1) as i64,
                )]))
        })
        .count();

    assert!(
        first.1[recovered_prefix..].iter().all(Option::is_none),
        "recovered rowstore state is not a serial-history prefix"
    );
    assert!(
        first
            .0
            .iter()
            .all(|&txn_id| txn_id >= 1 && txn_id <= recovered_prefix as u64),
        "recovered committed transaction set contains an id outside the rowstore prefix"
    );

    for txn_id in acked_labels.iter().filter_map(|label| {
        label
            .strip_prefix("commit-")
            .and_then(|value| value.parse::<usize>().ok())
    }) {
        assert!(
            txn_id <= recovered_prefix,
            "acknowledged transaction {txn_id} is missing after recovery"
        );
    }

    first
}

fn run_history_workload(
    workload: &htap_crashsim::WorkloadContext,
    journal_path: &std::path::Path,
    first_txn: u64,
    last_txn: u64,
) {
    let rowstore_dir = workload.root().join("rowstore");
    let manager = TransactionManager::open_with_options(JournalOptions::new(journal_path)).unwrap();
    let engine = std::sync::Arc::new(
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir)).unwrap(),
    );
    let participant_id = htap_txn::ParticipantId::new(1);
    let participant =
        std::sync::Arc::new(htap_txn::RowstoreParticipant::new(participant_id, engine));
    manager.register_participant(participant);

    manager.recover().unwrap();

    for txn_id in first_txn..=last_txn {
        let mut txn = manager.begin().unwrap();
        let mutations = vec![htap_common::Mutation::Put {
            partition_id: 0,
            key: vec![txn_id as u8],
            row: htap_common::Row::new(vec![htap_common::Value::Int64(txn_id as i64)]),
        }];
        let payload = htap_txn::RowstoreParticipant::encode_payload(&mutations).unwrap();
        txn.add_participant(participant_id, payload);
        manager.commit(&mut txn).unwrap();
        workload.ack(format!("commit-{txn_id}"));
    }
}

#[test]
fn txn_commits_survive_journal_and_wal() {
    let harness = CrashHarness::new("txn_commits_survive_journal_and_wal").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let rowstore_dir = workload.root().join("rowstore");
            htap_common::fs::create_dir_all_durable(&journal_dir).unwrap();
            htap_common::fs::create_dir_all_durable(&rowstore_dir).unwrap();

            let journal_path = journal_dir.join("txn.journal");
            run_history_workload(workload, &journal_path, 1, 4);
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();

            let journal_path = root.join("journal").join("txn.journal");
            let (committed, rows) =
                assert_history_prefix(root, &journal_path, 4, &info.acked_labels);

            let row_prefix = rows.iter().take_while(|row| row.is_some()).count() as u64;
            assert_eq!(
                committed,
                (1..=row_prefix).collect::<Vec<_>>(),
                "journal outcome and rowstore durable state disagree"
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
}

#[test]
fn txn_checkpoint_rewrite_atomic() {
    let harness = CrashHarness::new("txn_checkpoint_rewrite_atomic").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let rowstore_dir = workload.root().join("rowstore");
            htap_common::fs::create_dir_all_durable(&journal_dir).unwrap();
            htap_common::fs::create_dir_all_durable(&rowstore_dir).unwrap();

            let journal_path = journal_dir.join("txn.journal");
            run_history_workload(workload, &journal_path, 1, 2);

            let manager =
                TransactionManager::open_with_options(JournalOptions::new(&journal_path)).unwrap();
            let engine = std::sync::Arc::new(
                htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(&rowstore_dir))
                    .unwrap(),
            );
            let participant = std::sync::Arc::new(htap_txn::RowstoreParticipant::new(
                htap_txn::ParticipantId::new(1),
                engine,
            ));
            manager.register_participant(participant);
            manager.recover().unwrap();
            manager.checkpoint().unwrap();

            for txn_id in 3..=4 {
                let mut txn = manager.begin().unwrap();
                let mutations = vec![htap_common::Mutation::Put {
                    partition_id: 0,
                    key: vec![txn_id as u8],
                    row: htap_common::Row::new(vec![htap_common::Value::Int64(txn_id as i64)]),
                }];
                let payload = htap_txn::RowstoreParticipant::encode_payload(&mutations).unwrap();
                txn.add_participant(htap_txn::ParticipantId::new(1), payload);
                manager.commit(&mut txn).unwrap();
                workload.ack(format!("commit-{txn_id}"));
            }
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            checked_with_ack |= !info.acked_labels.is_empty();

            let journal_path = root.join("journal").join("txn.journal");
            let (committed, rows) =
                assert_history_prefix(root, &journal_path, 4, &info.acked_labels);

            let prefix = rows.iter().take_while(|row| row.is_some()).count() as u64;
            let pre_checkpoint_history = (1..=prefix).collect::<Vec<_>>();
            let post_checkpoint_history = (3..=prefix).collect::<Vec<_>>();
            assert!(
                committed == pre_checkpoint_history || committed == post_checkpoint_history,
                "recovered committed transactions must be either the full pre-checkpoint \
                 journal history or the post-checkpoint suffix; got {committed:?}"
            );

            for txn_id in info.acked_labels.iter().filter_map(|label| {
                label
                    .strip_prefix("commit-")
                    .and_then(|value| value.parse::<u64>().ok())
            }) {
                if txn_id >= 3 {
                    assert!(
                        committed.contains(&txn_id),
                        "acknowledged post-checkpoint transaction {txn_id} is missing after recovery"
                    );
                }
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
fn txn_repair_torn_final_crash_depth1() {
    let harness = CrashHarness::new("txn_repair_torn_final_crash_depth1").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let rowstore_dir = workload.root().join("rowstore");
            htap_common::fs::create_dir_all_durable(&journal_dir).unwrap();
            htap_common::fs::create_dir_all_durable(&rowstore_dir).unwrap();

            let journal_path = journal_dir.join("txn.journal");
            run_history_workload(workload, &journal_path, 1, 6);
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;
    let pre_recovery_journal_size = std::cell::Cell::new(None);
    let torn_tail_repair_count = std::cell::Cell::new(0usize);

    harness
        .enumerate_recovery(
            // A 512-byte sector creates realistic partial-record tails while still
            // leaving enough complete journal records for recovery to repair.
            &CrashPolicy::Torn {
                seed: 8,
                sector_size: 512,
            },
            |root| {
                let journal_path = root.join("journal").join("txn.journal");
                pre_recovery_journal_size
                    .set(journal_path.metadata().ok().map(|metadata| metadata.len()));
                let _ = reopen_history(root, &journal_path, 6);
            },
            |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();

                if let Some(recovery) = &info.recovery {
                    if recovery.completed {
                        let journal_path = root.join("journal").join("txn.journal");
                        let post_recovery_journal_size =
                            journal_path.metadata().ok().map(|metadata| metadata.len());
                        let size_shrank = matches!(
                            (pre_recovery_journal_size.get(), post_recovery_journal_size),
                            (Some(pre), Some(post)) if pre > 0 && post < pre
                        );
                        let repair_file_id = recovery.ops.iter().find_map(|op| match op {
                            htap_common::fs::Op::FsyncFile {
                                file_id,
                                site: Some("txn:journal_repair_sync"),
                                ..
                            } => Some(*file_id),
                            _ => None,
                        });
                        let repaired_tail = repair_file_id.is_some_and(|journal_file_id| {
                            recovery.ops.iter().any(|op| {
                                matches!(
                                    op,
                                    htap_common::fs::Op::SetLen { file_id, .. }
                                        if *file_id == journal_file_id
                                )
                            })
                        });
                        let repair_synced = repair_file_id.is_some();

                        if size_shrank && repaired_tail && repair_synced {
                            torn_tail_repair_count.set(torn_tail_repair_count.get() + 1);
                        }
                    }
                }

                assert!(
                    info.recovery.is_some(),
                    "recovery enumeration must report a recovery stage"
                );

                let journal_path = root.join("journal").join("txn.journal");
                let (committed, rows) =
                    assert_history_prefix(root, &journal_path, 6, &info.acked_labels);
                assert_eq!(
                    committed,
                    (1..=committed.len() as u64).collect::<Vec<_>>(),
                    "recovered transactions must form a contiguous serial history from one"
                );

                for txn_id in info.acked_labels.iter().filter_map(|label| {
                    label
                        .strip_prefix("commit-")
                        .and_then(|value| value.parse::<usize>().ok())
                }) {
                    assert!(
                        committed.contains(&(txn_id as u64)),
                        "acknowledged transaction {txn_id} is missing after recovery"
                    );
                    assert_eq!(
                        rows[txn_id - 1],
                        Some(htap_common::Row::new(vec![htap_common::Value::Int64(
                            txn_id as i64,
                        )])),
                        "acknowledged transaction {txn_id} mutation disappeared"
                    );
                }
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
        torn_tail_repair_count.get() > 0,
        "expected at least one outer crash image with a torn journal tail repaired by recovery; found {}",
        torn_tail_repair_count.get()
    );
}

#[test]
fn txn_fresh_journal_entry_durable() {
    let harness = CrashHarness::new("txn_fresh_journal_entry_durable").unwrap();

    harness
        .run_workload(|workload| {
            let journal_dir = workload.root().join("journal");
            let rowstore_dir = workload.root().join("rowstore");
            htap_common::fs::create_dir_all_durable(&journal_dir).unwrap();
            htap_common::fs::create_dir_all_durable(&rowstore_dir).unwrap();

            // The directory is durable, but the manager itself creates txn.journal.
            let journal_path = journal_dir.join("txn.journal");
            run_history_workload(workload, &journal_path, 1, 1);
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let commit_acked = info.acked_labels.iter().any(|label| label == "commit-1");
            checked_with_ack |= commit_acked;

            let journal_path = root.join("journal").join("txn.journal");

            if commit_acked {
                assert!(
                    journal_path.is_file(),
                    "acked commit lost the freshly created journal directory entry"
                );
            }

            let (committed, rows) =
                assert_history_prefix(root, &journal_path, 1, &info.acked_labels);

            if commit_acked {
                assert_eq!(committed, vec![1], "acked journal outcome disappeared");
                assert_eq!(
                    rows,
                    vec![Some(htap_common::Row::new(vec![
                        htap_common::Value::Int64(1),
                    ]))],
                    "acked rowstore mutation disappeared"
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
