use htap_common::fs::dur::create_dir_all;
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

#[test]
fn wal_open_syncs_preexisting_volatile_dir() {
    let harness = CrashHarness::new("wal_open_syncs_preexisting_volatile_dir").unwrap();

    harness
        .run_workload(|workload| {
            let wal_dir = workload.root().join("wal");
            create_dir_all(&wal_dir).unwrap();

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

            let wal_dir = root.join("wal");
            let first = Wal::open(WalOptions::new(&wal_dir)).unwrap();
            let first_committed = Wal::replay(&wal_dir).unwrap().committed_txn_ids();
            drop(first);

            let second = Wal::open(WalOptions::new(&wal_dir)).unwrap();
            let second_committed = Wal::replay(&wal_dir).unwrap().committed_txn_ids();
            drop(second);

            assert_eq!(first_committed, second_committed);

            if commit_acked {
                assert!(first_committed.contains(&1));
                assert!(Wal::replay(&wal_dir)
                    .unwrap()
                    .committed_records()
                    .iter()
                    .any(|(_, record)| record == &put(1)));
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
    witness_wal_open_parent_sync,
    site = "wal:open_parent_sync",
    body = wal_open_syncs_preexisting_volatile_dir
);
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = wal_open_syncs_preexisting_volatile_dir
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = wal_open_syncs_preexisting_volatile_dir
);
