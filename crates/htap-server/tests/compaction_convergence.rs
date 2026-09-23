#![doc = "Evidence test for lease-protected dropped-table compaction."]

use std::collections::HashSet;
use std::path::Path;

use htap_catalog::{CatalogStore, LocalCatalogStore, TableId, TabletId};
use htap_rowstore::Manifest;
use htap_server::LocalServer;
use tempfile::TempDir;

fn catalog(root: &Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap_or_default()
}

fn flush_rowstore(root: &Path, server: LocalServer) -> LocalServer {
    drop(server);

    let engine =
        htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(root.join("rowstore")))
            .unwrap();
    engine.flush().unwrap();
    drop(engine);

    LocalServer::open(root).unwrap()
}

fn rowstore_sst_ids(root: &Path) -> HashSet<u64> {
    Manifest::read_from_file(&root.join("rowstore").join("MANIFEST"))
        .unwrap()
        .unwrap_or_default()
        .ssts
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

fn table_and_tablet(root: &Path, name: &str) -> (TableId, TabletId) {
    let snapshot = catalog(root);
    let table = snapshot
        .table_by_name(name)
        .unwrap_or_else(|| panic!("missing table {name}"));
    let partition = snapshot
        .partition(table.partitions[0])
        .unwrap_or_else(|| panic!("missing partition for table {name}"));
    (table.id, partition.tablets[0])
}

#[test]
fn leased_dropped_tablets_are_all_protected_in_one_tick() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut server = LocalServer::open(root).unwrap();
    let names = ["leased_a", "leased_b", "leased_c"];

    for name in names {
        server
            .execute(&format!(
                "CREATE TABLE {name} (id INT PRIMARY KEY, value TEXT)"
            ))
            .unwrap();
    }

    let mut known_sst_ids = rowstore_sst_ids(root);
    let mut table_ssts: Vec<(String, HashSet<u64>)> = names
        .iter()
        .map(|name| ((*name).to_string(), HashSet::new()))
        .collect();

    // Flush each table separately to create interleaved, attributable SSTs.
    for value in 1..=6 {
        for (index, name) in names.iter().enumerate() {
            if value == 1 {
                server
                    .execute(&format!(
                        "INSERT INTO {name} (id, value) VALUES (1, '{name}-{value}')"
                    ))
                    .unwrap();
            } else {
                server
                    .execute(&format!(
                        "UPDATE {name} SET value = '{name}-{value}' WHERE id = 1"
                    ))
                    .unwrap();
            }

            server = flush_rowstore(root, server);
            let current = rowstore_sst_ids(root);
            table_ssts[index]
                .1
                .extend(current.difference(&known_sst_ids).copied());
            known_sst_ids = current;
        }
    }

    let tablets: Vec<TabletId> = names
        .iter()
        .map(|name| table_and_tablet(root, name).1)
        .collect();
    let mover = server.data_mover().expect("data mover must be available");
    let leases = mover
        .try_acquire_reclaim_lease(&tablets)
        .expect("failed to lease dropped tablets");

    for name in names {
        server.execute(&format!("DROP TABLE {name}")).unwrap();
    }

    let report = server
        .compaction_tick()
        .expect("a tick with several denied tablets must not fail");
    assert!(
        report.denied_tablet_count >= tablets.len(),
        "all leased tablets must be reported as denied: {report:?}"
    );

    let after_tick_sst_ids = rowstore_sst_ids(root);
    for (name, sst_ids) in &table_ssts {
        for id in sst_ids {
            assert!(
                after_tick_sst_ids.contains(id),
                "leased table {name} SST {id} must remain in the manifest"
            );
            assert!(
                root.join("rowstore")
                    .join("sst")
                    .join(format!("{id}.sst"))
                    .is_file(),
                "leased table {name} SST file {id}.sst must remain on disk"
            );
        }
    }

    drop(leases);

    for _ in 0..32 {
        server.compaction_tick().unwrap();
        let snapshot = catalog(root);
        if snapshot.pending_reclaim.is_empty() {
            return;
        }
    }

    panic!("dropped tables were not purged after releasing their leases");
}
