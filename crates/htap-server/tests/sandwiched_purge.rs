#![doc = "Evidence test for exact partition presence during dropped-table purge."]

use std::collections::HashSet;

use htap_catalog::{CatalogStore, LocalCatalogStore, PartitionId, TabletId};
use htap_common::Value;
use htap_rowstore::Manifest;
use htap_server::LocalServer;
use tempfile::TempDir;

fn catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

fn table_and_partition(
    root: &std::path::Path,
    table_name: &str,
) -> (htap_catalog::TableId, PartitionId, TabletId) {
    let catalog = catalog(root);
    let table = catalog
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"));
    let partition_id = table.partitions[0];
    let partition = catalog
        .partition(partition_id)
        .unwrap_or_else(|| panic!("missing partition {partition_id} for table {table_name}"));
    (table.id, partition_id, partition.tablets[0])
}

fn flush_rowstore(root: &std::path::Path, server: LocalServer) -> LocalServer {
    drop(server);

    let engine =
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(root.join("rowstore")))
            .unwrap();
    engine.flush().unwrap();
    drop(engine);

    LocalServer::open(root).unwrap()
}

fn rowstore_sst_ids(root: &std::path::Path) -> HashSet<u64> {
    Manifest::read_from_file(&root.join("rowstore").join("MANIFEST"))
        .unwrap()
        .unwrap_or_default()
        .ssts
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

fn query_value(server: &LocalServer, table: &str, id: i32) -> String {
    let result = server
        .execute(&format!("SELECT value FROM {table} WHERE id = {id}"))
        .unwrap();
    let htap_sql::result::StatementResult::Query(query) = result else {
        panic!("expected query result for {table}");
    };
    assert_eq!(
        query.rows.len(),
        1,
        "expected one row in {table} for id {id}"
    );
    match &query.rows[0].values()[0] {
        Value::String(value) => value.clone(),
        other => panic!("unexpected query value {other:?}"),
    }
}

#[test]
fn dropping_sandwiched_partition_preserves_unrelated_tables() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut server = LocalServer::open(root).unwrap();

    server
        .execute("CREATE TABLE lower_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("CREATE TABLE middle_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("CREATE TABLE upper_rows (id INT PRIMARY KEY, value TEXT)")
        .unwrap();

    let (lower_table, lower_partition, _) = table_and_partition(root, "lower_rows");
    let (middle_table, middle_partition, _) = table_and_partition(root, "middle_rows");
    let (upper_table, upper_partition, _) = table_and_partition(root, "upper_rows");

    assert!(
        lower_partition < middle_partition && middle_partition < upper_partition,
        "the dropped partition must be sandwiched in manifest key order: \
         lower={lower_partition}, middle={middle_partition}, upper={upper_partition}"
    );
    assert_ne!(lower_table, middle_table);
    assert_ne!(middle_table, upper_table);

    for id in 1..=6 {
        server
            .execute(&format!(
                "INSERT INTO lower_rows (id, value) VALUES ({id}, 'lower-{id}')"
            ))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO middle_rows (id, value) VALUES ({id}, 'middle-{id}')"
            ))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO upper_rows (id, value) VALUES ({id}, 'upper-{id}')"
            ))
            .unwrap();
    }

    // One flush puts all three adjacent partitions into the same SST.
    server = flush_rowstore(root, server);

    let before_drop_ssts = rowstore_sst_ids(root);
    assert!(
        !before_drop_ssts.is_empty(),
        "the three tables must be persisted before the purge"
    );
    for id in 1..=6 {
        assert_eq!(
            query_value(&server, "lower_rows", id),
            format!("lower-{id}")
        );
        assert_eq!(
            query_value(&server, "middle_rows", id),
            format!("middle-{id}")
        );
        assert_eq!(
            query_value(&server, "upper_rows", id),
            format!("upper-{id}")
        );
    }

    server.execute("DROP TABLE middle_rows").unwrap();

    let pending = catalog(root)
        .pending_reclaim
        .into_iter()
        .find(|entry| entry.table_id == middle_table)
        .expect("DROP must create a pending reclaim entry");
    assert!(
        !pending.rowstore_purge_confirmed,
        "the middle table must not be confirmed before compaction"
    );

    let mut saw_confirmation = false;
    let mut removed = false;
    for _ in 0..32 {
        let report = server.compaction_tick().unwrap();
        assert!(report.ran, "compaction tick did not run: {report:?}");

        for id in 1..=6 {
            assert_eq!(
                query_value(&server, "lower_rows", id),
                format!("lower-{id}"),
                "purging the middle partition must preserve the lower partition"
            );
            assert_eq!(
                query_value(&server, "upper_rows", id),
                format!("upper-{id}"),
                "purging the middle partition must preserve the upper partition"
            );
        }

        let current = catalog(root);
        match current
            .pending_reclaim
            .iter()
            .find(|entry| entry.table_id == middle_table)
        {
            Some(entry) => {
                saw_confirmation |= entry.rowstore_purge_confirmed;
            }
            None => {
                removed = true;
                break;
            }
        }
    }

    assert!(
        saw_confirmation || removed,
        "the middle partition purge must be confirmed before reclamation completes"
    );
    assert!(
        removed,
        "the middle table's pending reclaim entry must eventually be removed"
    );

    drop(server);

    let after_purge_ssts = rowstore_sst_ids(root);
    assert!(
        !after_purge_ssts.is_empty(),
        "purging the sandwiched partition must leave SSTs for adjacent partitions"
    );
    for id in &after_purge_ssts {
        assert!(
            root.join("rowstore")
                .join("sst")
                .join(format!("{id}.sst"))
                .is_file(),
            "manifest-listed SST file {id}.sst must exist after the purge"
        );
    }

    let server = LocalServer::open(root).unwrap();
    for id in 1..=6 {
        assert_eq!(
            query_value(&server, "lower_rows", id),
            format!("lower-{id}"),
            "lower-table data must survive reopening after the purge"
        );
        assert_eq!(
            query_value(&server, "upper_rows", id),
            format!("upper-{id}"),
            "upper-table data must survive reopening after the purge"
        );
    }

    let final_catalog = catalog(root);
    assert!(
        final_catalog.table(lower_table).is_some(),
        "the lower table must remain in the catalog"
    );
    assert!(
        final_catalog.table(upper_table).is_some(),
        "the upper table must remain in the catalog"
    );
    assert!(
        final_catalog
            .pending_reclaim
            .iter()
            .all(|entry| entry.table_id != middle_table),
        "the middle table's reclaim entry must be removed"
    );
}
