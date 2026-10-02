use std::path::Path;

use htap_common::fs::dur::create_dir_all;
use htap_common::{Mutation, Row, Value};
use htap_crashsim::{CrashHarness, CrashPolicy};
use htap_rowstore::{Engine, EngineOptions};

const PARTITION_ID: u64 = 1;

fn row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn open_engine(root: &Path) -> Engine {
    Engine::open(EngineOptions::new(root.join("rowstore")).with_memtable_bytes(1024)).unwrap()
}

#[test]
fn engine_open_syncs_preexisting_volatile_dir() {
    let harness = CrashHarness::new("engine_open_syncs_preexisting_volatile_dir").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("rowstore");
            create_dir_all(&root).unwrap();

            let engine = Engine::open(EngineOptions::new(&root).with_memtable_bytes(1024)).unwrap();
            engine
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

            let first_engine = open_engine(root);
            let first_value = first_engine
                .get(PARTITION_ID, &[0], first_engine.snapshot())
                .unwrap();
            drop(first_engine);

            let second_engine = open_engine(root);
            let second_value = second_engine
                .get(PARTITION_ID, &[0], second_engine.snapshot())
                .unwrap();

            assert_eq!(first_value, second_value);

            if commit_acked {
                assert_eq!(first_value, Some(row(10)));
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
