use std::collections::HashSet;
use std::path::Path;

use htap_catalog::{CatalogSnapshot, CatalogStore, LocalCatalogStore};
use htap_common::fs::{create_dir_all_durable, dur::create_dir_all};
use htap_common::HtapError;
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

const LAST_GENERATION: u64 = 4;

fn snapshot(generation: u64) -> CatalogSnapshot {
    CatalogSnapshot::new(generation, vec![], vec![], vec![], vec![])
}

fn open_catalog(root: &Path) -> LocalCatalogStore {
    match LocalCatalogStore::open(root.join("catalog")) {
        Ok(store) => store,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening a crash image returned corruption: {error}");
        }
        Err(error) => {
            panic!("reopening a crash image failed: {error}");
        }
    }
}

fn assert_reopens_identically(root: &Path) -> Option<CatalogSnapshot> {
    let first = open_catalog(root);
    let first_snapshot = first.load().unwrap();
    drop(first);

    let second = open_catalog(root);
    let second_snapshot = second.load().unwrap();

    assert_eq!(
        second_snapshot, first_snapshot,
        "a second reopen changed the recovered catalog"
    );

    first_snapshot
}

fn recovered_generation(snapshot: &Option<CatalogSnapshot>) -> u64 {
    snapshot
        .as_ref()
        .map(|snapshot| snapshot.generation)
        .unwrap_or(0)
}

fn acked_generation(info: &CrashInfo) -> u64 {
    info.acked_labels
        .iter()
        .filter_map(|label| label.strip_prefix("generation-"))
        .filter_map(|generation| generation.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

fn run_cas_sequence(workload: &htap_crashsim::WorkloadContext) {
    let catalog_dir = workload.root().join("catalog");
    create_dir_all_durable(&catalog_dir).unwrap();

    let store = LocalCatalogStore::open(&catalog_dir).unwrap();

    for generation in 1..=LAST_GENERATION {
        store
            .compare_and_set(generation - 1, snapshot(generation))
            .unwrap();
        workload.ack(format!("generation-{generation}"));
    }
}

#[test]
fn catalog_cas_sequence_prefix_consistent() {
    let harness = CrashHarness::new("catalog_cas_sequence_prefix_consistent").unwrap();

    harness.run_workload(run_cas_sequence).unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 41,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;
        let mut recovered_generations = HashSet::new();

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                checked_with_ack |= !info.acked_labels.is_empty();

                let recovered = assert_reopens_identically(root);
                let generation = recovered_generation(&recovered);
                let acknowledged = acked_generation(info);

                assert!(
                    generation <= LAST_GENERATION,
                    "recovered generation {generation} is not a prefix of the issued generations"
                );
                assert!(
                    acknowledged <= generation,
                    "recovered generation {generation} omitted acknowledged generation {acknowledged}"
                );

                recovered_generations.insert(generation);
            })
            .unwrap();

        assert!(
            checked_count > 1,
            "expected multiple crash images to be checked"
        );
        assert!(
            checked_with_ack,
            "expected an acknowledged CAS crash image to be checked"
        );

        if matches!(policy, CrashPolicy::Strict) {
            let max_recovered = recovered_generations
                .iter()
                .copied()
                .max()
                .expect("strict enumeration produced no crash images");

            for generation in 0..=max_recovered {
                assert!(
                    recovered_generations.contains(&generation),
                    "strict recovery generations are not contiguous from generation 0: \
                     missing generation {generation}"
                );
            }
        }
    }
}

#[test]
fn catalog_preexisting_volatile_dir() {
    let harness = CrashHarness::new("catalog_preexisting_volatile_dir").unwrap();

    harness
        .run_workload(|workload| {
            let catalog_dir = workload.root().join("catalog");
            create_dir_all(&catalog_dir).unwrap();

            let store = LocalCatalogStore::open(&catalog_dir).unwrap();
            store.compare_and_set(0, snapshot(1)).unwrap();
            workload.ack("generation-1");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 47,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;

                let recovered = assert_reopens_identically(root);
                let generation = recovered_generation(&recovered);
                let acknowledged = acked_generation(info);
                checked_with_ack |= acknowledged > 0;

                assert!(
                    generation <= 1,
                    "recovered generation {generation} was never issued"
                );
                assert!(
                    acknowledged <= generation,
                    "recovered generation {generation} omitted acknowledged generation {acknowledged}"
                );
            })
            .unwrap();

        assert!(
            checked_count > 1,
            "expected multiple crash images to be checked"
        );
        assert!(
            checked_with_ack,
            "expected an acknowledged CAS crash image to be checked"
        );
    }
}

#[test]
fn catalog_fresh_dir_durable() {
    let harness = CrashHarness::new("catalog_fresh_dir_durable").unwrap();

    harness
        .run_workload(|workload| {
            let catalog_dir = workload.root().join("catalog");

            // LocalCatalogStore creates the fresh directory; its entry must be durable before
            // the catalog file is published.
            let store = LocalCatalogStore::open(&catalog_dir).unwrap();
            store.compare_and_set(0, snapshot(1)).unwrap();
            workload.ack("generation-1");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 43,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;

                let recovered = assert_reopens_identically(root);
                let generation = recovered_generation(&recovered);
                let acknowledged = acked_generation(info);

                if acknowledged > 0 {
                    checked_with_ack = true;
                    assert!(
                        generation >= acknowledged,
                        "fresh catalog directory lost acknowledged generation {acknowledged}; \
                         recovered generation {generation}"
                    );
                }

                assert!(
                    generation <= 1,
                    "recovered generation {generation} was never issued"
                );
            })
            .unwrap();

        assert!(
            checked_count > 1,
            "expected multiple crash images to be checked"
        );
        assert!(
            checked_with_ack,
            "expected an acknowledged CAS crash image to be checked"
        );
    }
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_catalog_open_parent_sync,
    site = "catalog:open_parent_sync",
    body = catalog_preexisting_volatile_dir
);
htap_crashsim::crashsim_witness!(
    witness_atomic_publish_dir_sync,
    site = "atomic_publish:dir_sync",
    body = catalog_fresh_dir_durable
);
htap_crashsim::crashsim_witness!(
    witness_write_new_tmp_file_sync,
    site = "write_new_tmp_file:sync",
    body = catalog_fresh_dir_durable
);
htap_crashsim::crashsim_survivor!(
    survivor_catalog_open_dir_sync,
    site = "catalog:open_dir_sync",
    body = catalog_fresh_dir_durable
);
htap_crashsim::crashsim_control!(control_file, skip = File, body = catalog_fresh_dir_durable);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = catalog_fresh_dir_durable
);
