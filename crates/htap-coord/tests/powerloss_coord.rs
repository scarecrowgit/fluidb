use std::path::Path;

use htap_catalog::NodeId;
use htap_common::fs::{create_dir_all_durable, dur::create_dir_all};
use htap_common::{FencingToken, HtapError};
use htap_coord::{Coordinator, Leadership, LocalCoordinator};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

const SCOPE: &str = "tablet-1";
const NODE_ID: NodeId = NodeId::new(1);

fn open_coord_at(path: &Path) -> LocalCoordinator {
    match LocalCoordinator::open(path) {
        Ok(coord) => coord,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening a crash image returned corruption: {error}");
        }
        Err(error) => {
            panic!("reopening a crash image failed: {error}");
        }
    }
}

fn open_coord(root: &Path) -> LocalCoordinator {
    open_coord_at(&root.join("coord"))
}

fn acked_tokens(info: &CrashInfo) -> Vec<u64> {
    info.acked_labels
        .iter()
        .filter_map(|label| label.strip_prefix("token-"))
        .filter_map(|token| token.parse::<u64>().ok())
        .collect()
}

fn assert_acked_tokens_monotonic(info: &CrashInfo) {
    let tokens = acked_tokens(info);

    for pair in tokens.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "acknowledged fencing token {} regressed below {}",
            pair[1],
            pair[0]
        );
    }
}

fn assert_identical_reopens_at(path: &Path) -> Option<Leadership> {
    let first = open_coord_at(path);
    let first_nodes = first.list_nodes().unwrap();
    let first_leadership = first.current_leadership(SCOPE).unwrap();
    drop(first);

    let second = open_coord_at(path);
    assert_eq!(
        second.list_nodes().unwrap(),
        first_nodes,
        "a second reopen changed coordinator membership"
    );
    assert_eq!(
        second.current_leadership(SCOPE).unwrap(),
        first_leadership,
        "a second reopen changed coordinator leadership"
    );

    first_leadership
}

fn assert_identical_reopens(root: &Path) -> Option<Leadership> {
    assert_identical_reopens_at(&root.join("coord"))
}

fn assert_identical_reopens_and_recover_next_token_at(
    path: &Path,
) -> (Option<Leadership>, Vec<NodeId>, FencingToken) {
    let first = open_coord_at(path);
    let first_nodes = first.list_nodes().unwrap();
    let first_leadership = first.current_leadership(SCOPE).unwrap();
    drop(first);

    let second = open_coord_at(path);
    let second_nodes = second.list_nodes().unwrap();
    let second_leadership = second.current_leadership(SCOPE).unwrap();

    assert_eq!(
        second_nodes, first_nodes,
        "a second reopen changed coordinator membership"
    );
    assert_eq!(
        second_leadership, first_leadership,
        "a second reopen changed coordinator leadership"
    );

    if second_leadership.is_some() {
        second.release_leadership(SCOPE).unwrap();
    }

    let next_token = second.acquire_leadership(SCOPE, NODE_ID).unwrap().token();

    (first_leadership, first_nodes, next_token)
}

fn assert_leadership_prefix(leadership: &Option<Leadership>) {
    if let Some(leadership) = leadership {
        assert_eq!(
            leadership.scope(),
            SCOPE,
            "recovered leadership has the wrong scope"
        );
        assert_eq!(
            leadership.holder(),
            NODE_ID,
            "recovered leadership has the wrong holder"
        );
    }
}

fn recovered_next_token_at(path: &Path) -> FencingToken {
    let coord = open_coord_at(path);

    if coord.current_leadership(SCOPE).unwrap().is_some() {
        coord.release_leadership(SCOPE).unwrap();
    }

    coord.acquire_leadership(SCOPE, NODE_ID).unwrap().token()
}

fn recovered_next_token(root: &Path) -> FencingToken {
    recovered_next_token_at(&root.join("coord"))
}

#[test]
fn coord_fencing_token_never_regresses() {
    let harness = CrashHarness::new("coord_fencing_token_never_regresses").unwrap();

    harness
        .run_workload(|workload| {
            let coord_dir = workload.root().join("coord");
            create_dir_all_durable(&coord_dir).unwrap();

            let coord = LocalCoordinator::open(&coord_dir).unwrap();
            let mut previous_token = None;

            for attempt in 1..=4 {
                let leadership = coord.acquire_leadership(SCOPE, NODE_ID).unwrap();
                let token = leadership.token().get();

                if let Some(previous) = previous_token {
                    assert!(
                        token >= previous,
                        "fencing token {token} regressed below {previous}"
                    );
                }

                workload.ack(format!("token-{token}"));
                previous_token = Some(token);

                if attempt != 4 {
                    coord.release_leadership(SCOPE).unwrap();
                }
            }
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 37,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                assert_acked_tokens_monotonic(info);

                let acked = acked_tokens(info);
                checked_with_ack |= !acked.is_empty();

                let leadership = assert_identical_reopens(root);
                assert_leadership_prefix(&leadership);

                let recovered_token = recovered_next_token(root).get();

                for token in acked {
                    assert!(
                        recovered_token > token,
                        "next fencing token {recovered_token} did not advance beyond acknowledged token {token}"
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
            "expected an acknowledged fencing token crash image to be checked"
        );
    }
}

#[test]
fn coord_state_atomic() {
    let harness = CrashHarness::new("coord_state_atomic").unwrap();

    harness
        .run_workload(|workload| {
            let coord_dir = workload.root().join("coord");
            create_dir_all_durable(&coord_dir).unwrap();

            let coord = LocalCoordinator::open(&coord_dir).unwrap();
            coord.register_node(NODE_ID).unwrap();
            coord.acquire_leadership(SCOPE, NODE_ID).unwrap();
            workload.ack("state-ready");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 41,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;

                let leadership = assert_identical_reopens(root);
                let coord = open_coord(root);
                let nodes = coord.list_nodes().unwrap();
                let node_present = nodes.contains(&NODE_ID);

                if let Some(leadership) = &leadership {
                    assert_eq!(
                        leadership.scope(),
                        SCOPE,
                        "recovered leadership has the wrong scope"
                    );
                    assert_eq!(
                        leadership.holder(),
                        NODE_ID,
                        "recovered leadership has the wrong holder"
                    );
                    assert!(
                        node_present,
                        "leadership was recovered without its registered node"
                    );
                }

                let state_acked = info.acked_labels.iter().any(|label| label == "state-ready");
                checked_with_ack |= state_acked;

                if state_acked {
                    assert!(
                        node_present,
                        "acknowledged node registration was not recovered"
                    );
                    assert!(
                        leadership.is_some(),
                        "acknowledged leadership acquisition was not recovered"
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
            "expected an acknowledged coordinator state crash image to be checked"
        );
    }
}

#[test]
fn coord_preexisting_volatile_dir() {
    let harness = CrashHarness::new("coord_preexisting_volatile_dir").unwrap();
    const VOLATILE_COORD_DIR: &str = "volatile-coord";

    harness
        .run_workload(|workload| {
            let coord_path = workload.root().join(VOLATILE_COORD_DIR);
            create_dir_all(&coord_path).unwrap();

            let coord = LocalCoordinator::open(&coord_path).unwrap();
            coord.register_node(NODE_ID).unwrap();
            let leadership = coord.acquire_leadership(SCOPE, NODE_ID).unwrap();
            workload.ack(format!("token-{}", leadership.token().get()));
            workload.ack("volatile-dir-ready");
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
        let mut checked_with_token_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                checked_with_token_ack |= !acked_tokens(info).is_empty();
                let state_acked = info
                    .acked_labels
                    .iter()
                    .any(|label| label == "volatile-dir-ready");
                checked_with_ack |= state_acked;

                let coord_path = root.join(VOLATILE_COORD_DIR);
                let (leadership, nodes, recovered_token) =
                    assert_identical_reopens_and_recover_next_token_at(&coord_path);
                assert_leadership_prefix(&leadership);

                let node_present = nodes.contains(&NODE_ID);
                if let Some(leadership) = &leadership {
                    assert!(
                        node_present,
                        "leadership was recovered without its registered node"
                    );
                    assert_eq!(
                        leadership.scope(),
                        SCOPE,
                        "recovered leadership has the wrong scope"
                    );
                    assert_eq!(
                        leadership.holder(),
                        NODE_ID,
                        "recovered leadership has the wrong holder"
                    );
                }

                let recovered_token = recovered_token.get();
                for token in acked_tokens(info) {
                    assert!(
                        recovered_token > token,
                        "next fencing token {recovered_token} did not advance beyond acknowledged token {token}"
                    );
                }

                if state_acked {
                    assert!(
                        node_present,
                        "acknowledged node registration in a preexisting volatile directory was not recovered"
                    );
                    assert!(
                        leadership.is_some(),
                        "acknowledged leadership in a preexisting volatile directory was not recovered"
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
            "expected an acknowledged preexisting volatile coordinator directory crash image to be checked"
        );
        assert!(
            checked_with_token_ack,
            "expected an acknowledged fencing token crash image to be checked"
        );
    }
}

#[test]
fn coord_fresh_dir_durable() {
    let harness = CrashHarness::new("coord_fresh_dir_durable").unwrap();
    const FRESH_COORD_DIR: &str = "fresh-coord";

    harness
        .run_workload(|workload| {
            let coord_path = workload.root().join(FRESH_COORD_DIR);
            assert!(
                !coord_path.exists(),
                "fresh coordinator directory already exists"
            );

            let coord = LocalCoordinator::open(&coord_path).unwrap();
            coord.register_node(NODE_ID).unwrap();
            let leadership = coord.acquire_leadership(SCOPE, NODE_ID).unwrap();
            workload.ack(format!("token-{}", leadership.token().get()));
            workload.ack("fresh-dir-ready");
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
        let mut checked_with_token_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                checked_with_token_ack |= !acked_tokens(info).is_empty();
                let state_acked = info
                    .acked_labels
                    .iter()
                    .any(|label| label == "fresh-dir-ready");
                checked_with_ack |= state_acked;

                let coord_path = root.join(FRESH_COORD_DIR);
                let (leadership, nodes, recovered_token) =
                    assert_identical_reopens_and_recover_next_token_at(&coord_path);
                assert_leadership_prefix(&leadership);

                let node_present = nodes.contains(&NODE_ID);
                if let Some(leadership) = &leadership {
                    assert!(
                        node_present,
                        "leadership was recovered without its registered node"
                    );
                    assert_eq!(
                        leadership.scope(),
                        SCOPE,
                        "recovered leadership has the wrong scope"
                    );
                    assert_eq!(
                        leadership.holder(),
                        NODE_ID,
                        "recovered leadership has the wrong holder"
                    );
                }

                let recovered_token = recovered_token.get();
                for token in acked_tokens(info) {
                    assert!(
                        recovered_token > token,
                        "next fencing token {recovered_token} did not advance beyond acknowledged token {token}"
                    );
                }

                if state_acked {
                    assert!(
                        node_present,
                        "acknowledged node registration in a fresh directory was not recovered"
                    );
                    assert!(
                        leadership.is_some(),
                        "acknowledged leadership in a fresh directory was not recovered"
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
            "expected an acknowledged fresh coordinator directory crash image to be checked"
        );
        assert!(
            checked_with_token_ack,
            "expected an acknowledged fencing token crash image to be checked"
        );
    }
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_coord_open_parent_sync,
    site = "coord:open_parent_sync",
    body = coord_preexisting_volatile_dir
);
htap_crashsim::crashsim_witness!(
    witness_atomic_publish_dir_sync,
    site = "atomic_publish:dir_sync",
    body = coord_fresh_dir_durable
);
htap_crashsim::crashsim_survivor!(
    survivor_coord_open_dir_sync,
    site = "coord:open_dir_sync",
    body = coord_fresh_dir_durable
);
htap_crashsim::crashsim_control!(control_file, skip = File, body = coord_fresh_dir_durable);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = coord_fresh_dir_durable
);
