#![doc = "Evidence test for compaction tier-shift protection through tablet leases."]

use std::collections::HashSet;

use htap_catalog::{CatalogStore, LocalCatalogStore, PartitionId, TabletId};
use htap_server::LocalServer;
use tempfile::TempDir;

fn catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

fn partition_id(root: &std::path::Path, table_name: &str) -> PartitionId {
    let catalog = catalog(root);
    let table = catalog
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"));
    table.partitions[0]
}

fn tablet_id(root: &std::path::Path, table_name: &str) -> TabletId {
    let catalog = catalog(root);
    let partition = catalog
        .partition(partition_id(root, table_name))
        .unwrap_or_else(|| panic!("missing partition for table {table_name}"));
    partition.tablets[0]
}

fn manifest_sst_ids(root: &std::path::Path) -> HashSet<u64> {
    htap_rowstore::manifest::Manifest::read_from_file(&root.join("rowstore").join("MANIFEST"))
        .unwrap()
        .unwrap_or_default()
        .ssts
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

fn flush_rowstore(root: &std::path::Path, server: LocalServer) -> LocalServer {
    drop(server);

    let engine =
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(root.join("rowstore")))
            .unwrap();
    engine.flush().unwrap();
    drop(engine);

    LocalServer::open(root)
        .unwrap()
        .with_gc_horizon_retention_slack(0)
}

fn scalar_count(server: &LocalServer, table_name: &str) -> i64 {
    let result = server
        .execute(&format!("SELECT COUNT(*) FROM {table_name}"))
        .unwrap();
    let htap_sql::result::StatementResult::Query(result) = result else {
        panic!("COUNT query did not return rows");
    };
    match result.rows[0].values()[0] {
        htap_common::types::Value::Int64(value) => value,
        ref value => panic!("COUNT query returned unexpected value {value:?}"),
    }
}

#[test]
fn test_compaction_protects_ssts_containing_leased_tablet_partition() {
    let temp = TempDir::new().unwrap();
    let mut server = LocalServer::open(temp.path())
        .unwrap()
        .with_gc_horizon_retention_slack(0);

    server
        .execute("CREATE TABLE protected_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("CREATE TABLE unleased_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    // Closing the server flushes its active memtable. Reopen it before each
    // batch so every batch is written to a distinct SST on the next close.
    // Eight SSTs exceed the tier fanout and make compaction eligible.
    for id in 1..=8 {
        server
            .execute(&format!(
                "INSERT INTO protected_table (id, value) VALUES ({id}, 'protected-{id}')"
            ))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO unleased_table (id, value) VALUES ({id}, 'unleased-{id}')"
            ))
            .unwrap();

        server = flush_rowstore(temp.path(), server);
    }

    let protected_tablet = tablet_id(temp.path(), "protected_table");

    let ssts_before = manifest_sst_ids(temp.path());
    assert!(
        ssts_before.len() >= 8,
        "test setup must create a compactable tier of SSTs"
    );

    let data_mover = server.data_mover();
    let lease = data_mover
        .try_acquire_reclaim_lease(&[protected_tablet])
        .expect("protected tablet reclaim lease should be acquired");

    let report = server.compaction_tick().unwrap();
    assert!(report.ran);
    assert!(report
        .per_iteration_reports
        .iter()
        .all(|report| !report.compacted));

    let ssts_after = manifest_sst_ids(temp.path());
    assert_eq!(
        ssts_after, ssts_before,
        "leased partition must prevent rewriting SSTs that also contain unleased data"
    );

    for sst_id in &ssts_before {
        assert!(
            temp.path()
                .join("rowstore")
                .join("sst")
                .join(format!("{sst_id}.sst"))
                .is_file(),
            "protected SST {sst_id} must remain on disk"
        );
    }

    assert_eq!(scalar_count(&server, "unleased_table"), 8);
    assert_eq!(scalar_count(&server, "protected_table"), 8);

    drop(lease);

    let mut control_compacted = false;
    for _ in 0..32 {
        let control_report = server.compaction_tick().unwrap();
        assert!(control_report.ran);
        if control_report
            .per_iteration_reports
            .iter()
            .any(|report| report.compacted)
        {
            control_compacted = true;
            break;
        }
    }
    assert!(
        control_compacted,
        "compaction must proceed after the reclaim lease is released"
    );

    let ssts_after_control = manifest_sst_ids(temp.path());
    assert_ne!(
        ssts_after_control, ssts_before,
        "unleased compaction must rewrite the SST manifest"
    );
    assert!(
        ssts_before.iter().any(|sst_id| {
            !temp
                .path()
                .join("rowstore")
                .join("sst")
                .join(format!("{sst_id}.sst"))
                .exists()
        }),
        "compaction must delete at least one rewritten SST file"
    );

    assert_eq!(scalar_count(&server, "unleased_table"), 8);
    assert_eq!(scalar_count(&server, "protected_table"), 8);
}
