#![allow(dead_code)]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllowReason {
    SubsumedByLaterSync,
    CoveredByOtherSync,
    OracleGap,
    IdempotentResurrection,
    ErrorPathOnly,
    NonUnixOnly,
    NoProductionCaller,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurvivorProof {
    Macro {
        test_file: &'static str,
        survivor_fn: &'static str,
    },
    DataOnly {
        evidence: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllowEntry {
    pub site: &'static str,
    pub crate_scope: Option<&'static str>,
    pub reason: AllowReason,
    pub covered_by: &'static [&'static str],
    pub proof: SurvivorProof,
}

pub const SYNC_SITE_WITNESSES: &[(&str, &str, &str, &str)] = &[
    (
        "wal:append_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal.rs",
        "witness_wal_append_sync",
    ),
    (
        "wal:roll_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal.rs",
        "witness_wal_roll_sync",
    ),
    (
        "wal:roll_dir_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal.rs",
        "witness_wal_roll_dir_sync",
    ),
    (
        "wal:open_parent_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal_open.rs",
        "witness_wal_open_parent_sync",
    ),
    (
        "wal:open_dir_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal_segment.rs",
        "witness_wal_open_dir_sync",
    ),
    (
        "wal:open_adopt_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal_tail.rs",
        "witness_wal_open_adopt_sync",
    ),
    (
        "atomic_publish:dir_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_wal_tail.rs",
        "witness_atomic_publish_dir_sync",
    ),
    (
        "sst:write_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_engine.rs",
        "witness_sst_write_sync",
    ),
    (
        "engine:flush_sst_dir_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_engine.rs",
        "witness_engine_flush_sst_dir_sync",
    ),
    (
        "engine:compact_sst_dir_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_engine.rs",
        "witness_engine_compact_sst_dir_sync",
    ),
    (
        "engine:open_parent_sync",
        "htap-rowstore",
        "crates/htap-rowstore/tests/powerloss_engine_open.rs",
        "witness_engine_open_parent_sync",
    ),
    (
        "txn:journal_parent_sync",
        "htap-txn",
        "crates/htap-txn/tests/powerloss_txn.rs",
        "witness_txn_journal_parent_sync",
    ),
    (
        "txn:journal_sync",
        "htap-txn",
        "crates/htap-txn/tests/powerloss_txn.rs",
        "witness_txn_journal_sync",
    ),
    (
        "txn:journal_grandparent_sync",
        "htap-txn",
        "crates/htap-txn/tests/powerloss_txn.rs",
        "witness_txn_journal_grandparent_sync",
    ),
    (
        "catalog:open_parent_sync",
        "htap-catalog",
        "crates/htap-catalog/tests/powerloss_catalog.rs",
        "witness_catalog_open_parent_sync",
    ),
    (
        "atomic_publish:dir_sync",
        "htap-catalog",
        "crates/htap-catalog/tests/powerloss_catalog.rs",
        "witness_atomic_publish_dir_sync",
    ),
    (
        "write_new_tmp_file:sync",
        "htap-catalog",
        "crates/htap-catalog/tests/powerloss_catalog.rs",
        "witness_write_new_tmp_file_sync",
    ),
    (
        "coord:open_parent_sync",
        "htap-coord",
        "crates/htap-coord/tests/powerloss_coord.rs",
        "witness_coord_open_parent_sync",
    ),
    (
        "atomic_publish:dir_sync",
        "htap-coord",
        "crates/htap-coord/tests/powerloss_coord.rs",
        "witness_atomic_publish_dir_sync",
    ),
    (
        "colstore:segment_write_sync",
        "htap-colstore",
        "crates/htap-colstore/tests/powerloss_segment.rs",
        "witness_colstore_segment_write_sync",
    ),
    (
        "atomic_publish:dir_sync",
        "htap-convert",
        "crates/htap-convert/tests/powerloss_convert.rs",
        "witness_atomic_publish_dir_sync",
    ),
    (
        "sync_dir:sync",
        "htap-convert",
        "crates/htap-convert/tests/powerloss_convert.rs",
        "witness_sync_dir_sync",
    ),
    (
        "movement:open_parent_sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement.rs",
        "witness_movement_open_parent_sync",
    ),
    (
        "atomic_publish:dir_sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement.rs",
        "witness_atomic_publish_dir_sync",
    ),
    (
        "write_new_tmp_file:sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement.rs",
        "witness_write_new_tmp_file_sync",
    ),
    (
        "sync_dir:sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement.rs",
        "witness_sync_dir_sync",
    ),
    (
        "movement:export_write_sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement_export.rs",
        "witness_movement_export_write_sync",
    ),
    (
        "sync_ancestors:sync",
        "htap-movement",
        "crates/htap-movement/tests/powerloss_movement_export.rs",
        "witness_sync_ancestors_sync",
    ),
    (
        "sync_ancestors:sync",
        "htap-server",
        "crates/htap-server/tests/powerloss_server.rs",
        "witness_sync_ancestors_sync",
    ),
    (
        "server:reclaim_colstore_sync",
        "htap-server",
        "crates/htap-server/tests/powerloss_server_recovery.rs",
        "witness_server_reclaim_colstore_sync",
    ),
];

pub const UNATTRIBUTED_WITNESSES: &[(
    &str, /* site */
    &str, /* full relative test file */
    &str, /* witness fn */
    &str, /* reason */
)] = &[
    (
        "atomic_publish:dir_sync",
        "crates/htap-server/tests/powerloss_server.rs",
        "witness_atomic_publish_dir_sync",
        "reached only through component crates the server drives; htap-server has no direct call to the helper",
    ),
    (
        "sync_dir:sync",
        "crates/htap-server/tests/powerloss_server_recovery.rs",
        "witness_sync_dir_sync",
        "reached only through component crates the server drives; htap-server has no direct call to the helper",
    ),
    (
        "sync_dir:sync",
        "crates/htap-colstore/tests/powerloss_segment.rs",
        "witness_sync_dir_sync",
        "killed through the test fixture's own setup sync; not a production witness",
    ),
    (
        "create_dir_all_durable:parent_sync",
        "crates/htap-colstore/tests/powerloss_segment.rs",
        "witness_create_dir_all_durable_parent_sync",
        "killed through the test fixture's own setup sync; not a production witness",
    ),
    (
        "write_new_tmp_file:sync",
        "crates/htap-txn/tests/powerloss_txn.rs",
        "witness_write_new_tmp_file_sync",
        "killed through the test fixture's own setup sync; not a production witness",
    ),
];

pub const ALLOWLIST: &[AllowEntry] = &[
    AllowEntry {
        site: "wal:repair_sync",
        crate_scope: None,
        reason: AllowReason::IdempotentResurrection,
        covered_by: &["wal:append_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery sweep: only reaching test wal_recovery_repair_crash_depth1 fails its non-vacuity guard under the skip",
        },
    },
    AllowEntry {
        site: "wal:gc_sync",
        crate_scope: None,
        reason: AllowReason::IdempotentResurrection,
        covered_by: &[],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-rowstore/tests/powerloss_wal.rs",
            survivor_fn: "survivor_wal_gc_sync",
        },
    },
    AllowEntry {
        site: "engine:open_dir_sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["wal:open_parent_sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-rowstore/tests/powerloss_engine_open.rs",
            survivor_fn: "survivor_engine_open_dir_sync",
        },
    },
    AllowEntry {
        site: "txn:journal_open_sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["txn:journal_sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-txn/tests/powerloss_txn.rs",
            survivor_fn: "survivor_txn_journal_open_sync",
        },
    },
    AllowEntry {
        site: "txn:journal_repair_sync",
        crate_scope: None,
        reason: AllowReason::SubsumedByLaterSync,
        covered_by: &["txn:journal_open_sync", "txn:journal_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery sweep: only reaching test txn_repair_torn_final_crash_depth1 fails its non-vacuity guard under the skip",
        },
    },
    AllowEntry {
        site: "txn:journal_append_sync",
        crate_scope: None,
        reason: AllowReason::SubsumedByLaterSync,
        covered_by: &["txn:journal_sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-txn/tests/powerloss_txn.rs",
            survivor_fn: "survivor_txn_journal_append_sync",
        },
    },
    // F15: add a targeted workload that reopens a pre-existing directory with
    // unsynced children and acknowledges the loaded state.
    AllowEntry {
        site: "catalog:open_dir_sync",
        crate_scope: None,
        reason: AllowReason::OracleGap,
        covered_by: &[],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-catalog/tests/powerloss_catalog.rs",
            survivor_fn: "survivor_catalog_open_dir_sync",
        },
    },
    // F15: add a targeted workload that reopens a pre-existing directory with
    // unsynced children and acknowledges the loaded state.
    AllowEntry {
        site: "coord:open_dir_sync",
        crate_scope: None,
        reason: AllowReason::OracleGap,
        covered_by: &[],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-coord/tests/powerloss_coord.rs",
            survivor_fn: "survivor_coord_open_dir_sync",
        },
    },
    // F15: add a targeted workload that reopens a pre-existing directory with
    // unsynced children and acknowledges the loaded state.
    AllowEntry {
        site: "movement:open_dir_sync",
        crate_scope: None,
        reason: AllowReason::OracleGap,
        covered_by: &[],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-movement/tests/powerloss_movement.rs",
            survivor_fn: "survivor_movement_open_dir_sync",
        },
    },
    AllowEntry {
        site: "movement:package_data_dir_sync",
        crate_scope: None,
        reason: AllowReason::SubsumedByLaterSync,
        covered_by: &["sync_dir:sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-movement/tests/powerloss_movement_clone.rs",
            survivor_fn: "survivor_movement_package_data_dir_sync",
        },
    },
    AllowEntry {
        site: "server:open_parent_sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["txn:journal_grandparent_sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-server/tests/powerloss_server.rs",
            survivor_fn: "survivor_server_open_parent_sync",
        },
    },
    AllowEntry {
        site: "server:open_dir_sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &[
            "catalog:open_parent_sync",
            "engine:open_parent_sync",
            "txn:journal_parent_sync",
            "movement:open_parent_sync",
        ],
        proof: SurvivorProof::DataOnly {
            evidence: "survivor proof exceeds the 30 s budget (38.9 s); C5 discovery sweep shows no kill",
        },
    },
    AllowEntry {
        site: "fsync_file:sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["colstore:segment_write_sync"],
        proof: SurvivorProof::Macro {
            test_file: "crates/htap-convert/tests/powerloss_convert.rs",
            survivor_fn: "survivor_fsync_file_sync",
        },
    },
    AllowEntry {
        site: "fsync_file:sync",
        crate_scope: Some("htap-convert"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["colstore:segment_write_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "convert lib.rs fsyncs the segment tmp file right after SegmentWriter's own colstore:segment_write_sync",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: None,
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &[
            "catalog:open_parent_sync",
            "wal:open_parent_sync",
            "txn:journal_parent_sync",
            "movement:open_parent_sync",
        ],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: every production caller's fresh-dir test survives the skip; the component's own open_parent_sync covers the new entry",
        },
    },
    AllowEntry {
        site: "atomic_publish:dir_sync",
        crate_scope: Some("htap-txn"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["txn:journal_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: no txn test killed; checkpoint rewrite is followed by journal reopen",
        },
    },
    AllowEntry {
        site: "sync_dir:sync",
        crate_scope: Some("htap-catalog"),
        reason: AllowReason::NoProductionCaller,
        covered_by: &[],
        proof: SurvivorProof::DataOnly {
            evidence: "only caller is the unused pub wrapper htap_catalog::local::sync_dir",
        },
    },
    AllowEntry {
        site: "sync_dir:sync",
        crate_scope: Some("htap-coord"),
        reason: AllowReason::NoProductionCaller,
        covered_by: &[],
        proof: SurvivorProof::DataOnly {
            evidence: "only caller is the unused pub wrapper htap_coord::sync_dir",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-catalog"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["catalog:open_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-convert"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["sync_dir:sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-coord"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["coord:open_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-movement"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["movement:open_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-rowstore"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["wal:open_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-server"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["txn:journal_grandparent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
    AllowEntry {
        site: "create_dir_all_durable:parent_sync",
        crate_scope: Some("htap-txn"),
        reason: AllowReason::CoveredByOtherSync,
        covered_by: &["txn:journal_parent_sync"],
        proof: SurvivorProof::DataOnly {
            evidence: "C5 discovery: survives in the crate's suites",
        },
    },
];

pub const HELPER_SITES: &[(&str, &str)] = &[
    ("atomic_publish", "atomic_publish:dir_sync"),
    ("write_new_tmp_file", "write_new_tmp_file:sync"),
    ("sync_dir", "sync_dir:sync"),
    ("sync_ancestors_best_effort", "sync_ancestors:sync"),
    (
        "create_dir_all_durable",
        "create_dir_all_durable:parent_sync",
    ),
    ("fsync_file", "fsync_file:sync"),
];

pub const CONTROL_EXEMPTIONS: &[(&str, &str)] = &[];
