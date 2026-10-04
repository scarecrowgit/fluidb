use htap_common::{Row, Value, Version};
use htap_crashsim::{CrashHarness, CrashPolicy};
use htap_rowstore::{Engine, EngineOptions, Mutation, WalOptions};

#[test]
fn adopted_unsynced_wal_tail_is_synced_before_publish() {
    let harness = CrashHarness::new("adopted_unsynced_wal_tail_is_synced_before_publish").unwrap();

    harness
        .run_workload(|workload| {
            {
                // sync_on_commit(false) stands in for a process killed between the WAL write and its fsync.
                let engine = Engine::open(EngineOptions::new(workload.root()).with_wal_options(
                    WalOptions::new(workload.root().join("wal")).with_sync_on_commit(false),
                ))
                .unwrap();

                engine
                    .apply_external(
                        1,
                        Version::new(2),
                        vec![Mutation::Put {
                            partition_id: 1,
                            key: b"key".to_vec(),
                            row: Row::new(vec![Value::Int64(2)]),
                        }],
                    )
                    .unwrap();
            }

            {
                let engine = Engine::open(EngineOptions::new(workload.root())).unwrap();

                engine
                    .apply_external(
                        1,
                        Version::new(2),
                        vec![Mutation::Put {
                            partition_id: 1,
                            key: b"key".to_vec(),
                            row: Row::new(vec![Value::Int64(2)]),
                        }],
                    )
                    .unwrap();
                engine.publish(Version::new(2)).unwrap();
                workload.ack("published-T");
            }
        })
        .unwrap();

    let mut checked_count = 0;
    let mut checked_with_ack = false;

    harness
        .enumerate(&CrashPolicy::Strict, |root, info| {
            checked_count += 1;
            let published = info.acked_labels.iter().any(|label| label == "published-T");
            checked_with_ack |= published;

            let first = Engine::open(EngineOptions::new(root)).unwrap();
            let first_snapshot = first.snapshot();
            let first_state = first.get(1, b"key", first_snapshot).unwrap();
            let first_visible = first.visible_version();
            let first_committed = first.committed_version();
            drop(first);

            let second = Engine::open(EngineOptions::new(root)).unwrap();
            assert_eq!(
                second.get(1, b"key", second.snapshot()).unwrap(),
                first_state,
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

            if first_visible >= Version::new(2) {
                assert_eq!(first_state, Some(Row::new(vec![Value::Int64(2)])));
            }

            if published {
                assert_eq!(first_state, Some(Row::new(vec![Value::Int64(2)])));
            }
        })
        .unwrap();

    assert!(
        checked_count > 1,
        "expected multiple crash images to be checked"
    );
    assert!(
        checked_with_ack,
        "expected an acknowledged publish crash image to be checked"
    );
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_wal_open_adopt_sync,
    site = "wal:open_adopt_sync",
    body = adopted_unsynced_wal_tail_is_synced_before_publish
);
htap_crashsim::crashsim_witness!(
    witness_atomic_publish_dir_sync,
    site = "atomic_publish:dir_sync",
    body = adopted_unsynced_wal_tail_is_synced_before_publish
);
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = adopted_unsynced_wal_tail_is_synced_before_publish
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = adopted_unsynced_wal_tail_is_synced_before_publish
);
