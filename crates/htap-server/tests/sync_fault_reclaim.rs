#![doc = "Regression proof that reclaim directory-sync failures are not swallowed."]

use std::cell::Cell;

use htap_catalog::{CatalogStore, LocalCatalogStore};
use htap_crashsim::{CrashHarness, SyncFault};
use htap_server::LocalServer;

const RECLAIM_COLSTORE_SYNC_SITE: &str = "server:reclaim_colstore_sync";
const AMBIGUOUS_AFTER_RENAME: &[(u64, &str, u64, &str)] = &[
    (
        2,
        "atomic_publish:dir_sync",
        1,
        "DROP returns Err after its catalog publish rename makes the table drop and pending reclaim visible; this is F12: an ambiguous outcome after the post-rename directory sync fails.",
    ),
    (
        8,
        "atomic_publish:dir_sync",
        2,
        "DROP returns Ok because reclaim errors are caught and logged, but the pending entry is already marked reclaimed before the post-rename sync fails; this is F12: an ambiguous outcome where the renamed state is visible in the catalog even though a subsequent fsync fails.",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableState {
    Live,
    Pending,
    Reclaimed,
}

fn open_server(root: &std::path::Path) -> LocalServer {
    LocalServer::open(root)
        .map(|server| server.with_scan_workers(1).with_query_parallelism(1))
        .unwrap()
}

fn table_state(root: &std::path::Path, table_name: &str) -> TableState {
    let catalog = LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default();

    if let Some(entry) = catalog
        .pending_reclaim
        .iter()
        .find(|entry| entry.table_name == table_name)
    {
        return if entry.colstore_and_movement_reclaimed {
            TableState::Reclaimed
        } else {
            TableState::Pending
        };
    }

    if catalog.table_by_name(table_name).is_some() {
        TableState::Live
    } else {
        TableState::Reclaimed
    }
}

fn prepare_columnar_table(server: &LocalServer) {
    server
        .execute("CREATE TABLE reclaim_sync_fault (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO reclaim_sync_fault (id, value) VALUES (1, 'one')")
        .unwrap();
    server
        .convert_table_to_column("reclaim_sync_fault")
        .unwrap();
}

fn sync_attempts_for_ordinal(
    attempts: &[(u64, u64, bool, TableState)],
    ordinal: u64,
) -> (u64, bool, TableState) {
    attempts
        .iter()
        .find_map(|(recorded_ordinal, attempts, returned_error, state)| {
            (*recorded_ordinal == ordinal).then_some((*attempts, *returned_error, *state))
        })
        .unwrap_or_else(|| panic!("ordinal={ordinal}: missing Nth-sweep sync-attempt record"))
}

#[test]
fn reclaim_sync_failure_is_never_swallowed() {
    let baseline = CrashHarness::new("reclaim_sync_fault_baseline").unwrap();
    let baseline_ok = Cell::new(false);
    let baseline_attempts = Cell::new(0);

    baseline
        .run_workload(|workload| {
            let server = open_server(workload.root());
            prepare_columnar_table(&server);

            baseline.set_sync_fault(SyncFault::None);
            baseline_ok.set(server.execute("DROP TABLE reclaim_sync_fault").is_ok());
            // Capture before the observer opens LocalCatalogStore, whose open syncs are unrelated.
            baseline_attempts.set(baseline.sync_attempts());
        })
        .unwrap();

    assert!(baseline_ok.get(), "baseline DROP must succeed");
    let attempts = baseline_attempts.get();
    assert!(
        attempts >= 1,
        "DROP reclaim path must perform at least one durable sync"
    );
    eprintln!("reclaim_sync_failure_is_never_swallowed: N={attempts}");

    let expected_ambiguous: Vec<u64> = AMBIGUOUS_AFTER_RENAME
        .iter()
        .map(|(ordinal, _, _, _)| *ordinal)
        .collect();

    let mut observed_ambiguous = Vec::new();
    let mut nth_sync_attempts = Vec::new();

    for nth in 1..=attempts {
        let harness = CrashHarness::new(format!("reclaim_sync_fault_nth_{nth}")).unwrap();
        let returned_error = Cell::new(false);
        let drop_attempts = Cell::new(0);
        let state = Cell::new(TableState::Live);

        harness
            .run_workload(|workload| {
                let server = open_server(workload.root());
                prepare_columnar_table(&server);

                harness.set_sync_fault(SyncFault::Nth(nth));
                returned_error.set(server.execute("DROP TABLE reclaim_sync_fault").is_err());
                // Capture before observing catalog state so this is the DROP-only total.
                drop_attempts.set(harness.sync_attempts());
                state.set(table_state(workload.root(), "reclaim_sync_fault"));
            })
            .unwrap();

        let region_attempts = drop_attempts.get();
        nth_sync_attempts.push((nth, region_attempts, returned_error.get(), state.get()));

        assert_eq!(
            harness.sync_faults_fired(),
            1,
            "nth={nth}: injected sync failure did not fire in DROP"
        );

        let is_expected_ambiguous = expected_ambiguous.contains(&nth);
        let is_ambiguous_outcome = if returned_error.get() {
            state.get() != TableState::Live
        } else {
            state.get() != TableState::Pending
        };

        if returned_error.get() {
            assert!(
                state.get() == TableState::Live || is_expected_ambiguous,
                "nth={nth}: DROP returned Err but state {:?} is not Live or a documented F12 exception",
                state.get()
            );
        } else {
            assert!(
                state.get() == TableState::Pending || is_expected_ambiguous,
                "nth={nth}: DROP returned Ok but state {:?} is not Pending or a documented F12 exception",
                state.get()
            );
        }

        if is_ambiguous_outcome {
            observed_ambiguous.push(nth);
        }

        eprintln!(
            "reclaim_sync_failure_is_never_swallowed: ordinal={nth}, drop_result={}, state={:?}, total={region_attempts}",
            if returned_error.get() { "Err" } else { "Ok" },
            state.get(),
        );
    }

    assert_eq!(
        observed_ambiguous, expected_ambiguous,
        "visible outcomes after DROP must match the documented F12 exceptions"
    );

    let colstore_site = CrashHarness::new("reclaim_sync_fault_colstore_site").unwrap();
    colstore_site
        .run_workload(|workload| {
            let server = open_server(workload.root());
            prepare_columnar_table(&server);

            colstore_site.set_sync_fault(SyncFault::Site {
                site: RECLAIM_COLSTORE_SYNC_SITE,
                occurrence: 1,
            });
            let _ = server.execute("DROP TABLE reclaim_sync_fault");
        })
        .unwrap();
    assert_eq!(
        colstore_site.sync_faults_fired(),
        1,
        "DROP must reach server:reclaim_colstore_sync occurrence 1"
    );

    for &(ordinal, site, occurrence_in_drop_region, reason) in AMBIGUOUS_AFTER_RENAME {
        let (expected_attempts, expected_returned_error, expected_state) =
            sync_attempts_for_ordinal(&nth_sync_attempts, ordinal);

        let harness = CrashHarness::new(format!("reclaim_sync_fault_f12_{ordinal}")).unwrap();
        let returned_error = Cell::new(false);
        let drop_attempts = Cell::new(0);
        let state = Cell::new(TableState::Live);

        harness
            .run_workload(|workload| {
                let server = open_server(workload.root());
                prepare_columnar_table(&server);

                harness.set_sync_fault(SyncFault::Site {
                    site,
                    occurrence: occurrence_in_drop_region,
                });
                returned_error.set(server.execute("DROP TABLE reclaim_sync_fault").is_err());
                // Keep the companion comparison limited to the DROP statement itself.
                drop_attempts.set(harness.sync_attempts());
                state.set(table_state(workload.root(), "reclaim_sync_fault"));
            })
            .unwrap();

        assert_eq!(
            harness.sync_faults_fired(),
            1,
            "ordinal={ordinal}: F12 companion fault at {site} occurrence {occurrence_in_drop_region} did not fire"
        );
        assert_eq!(
            drop_attempts.get(),
            expected_attempts,
            "ordinal={ordinal}: F12 companion fault at {site} occurrence {occurrence_in_drop_region} targeted DROP sync-attempt count {} instead of the Nth sweep's DROP-only count {expected_attempts}",
            drop_attempts.get()
        );
        assert_eq!(
            returned_error.get(),
            expected_returned_error,
            "ordinal={ordinal}: {reason}"
        );
        assert_eq!(state.get(), expected_state, "ordinal={ordinal}: {reason}");
    }
}
