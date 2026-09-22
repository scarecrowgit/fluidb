#![doc = "Evidence test that confirmed dropped-rowstore purges cannot resurrect after restart."]

use htap_catalog::{CatalogStore, LocalCatalogStore, TabletId};
use htap_common::{encode_key, Value};
use htap_rowstore::{Engine, EngineOptions, Snapshot};
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
) -> (htap_catalog::TableId, u64, TabletId) {
    let catalog = catalog(root);
    let table = catalog
        .table_by_name(table_name)
        .unwrap_or_else(|| panic!("missing table {table_name}"));
    let partition_id = table.partitions[0];
    let partition = catalog
        .partition(partition_id)
        .unwrap_or_else(|| panic!("missing partition {partition_id} for table {table_name}"));

    (table.id, partition_id.into(), partition.tablets[0])
}

fn sst_ids(rowstore_dir: &std::path::Path) -> Vec<u64> {
    let mut ids = std::fs::read_dir(rowstore_dir.join("sst"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .path()
                .file_name()?
                .to_str()?
                .strip_suffix(".sst")?
                .parse::<u64>()
                .ok()
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

#[test]
fn test_confirmed_dropped_rowstore_purge_does_not_resurrect_after_restart() {
    let temp = TempDir::new().unwrap();
    let rowstore_dir = temp.path().join("rowstore");
    let table_name = "purged_table";
    let key = encode_key(&[Value::Int32(1)]).unwrap();

    let server = LocalServer::open(temp.path()).unwrap();

    server
        .execute("CREATE TABLE purged_table (id INT PRIMARY KEY, value TEXT)")
        .unwrap();
    server
        .execute("INSERT INTO purged_table (id, value) VALUES (1, 'deleted')")
        .unwrap();

    let (table_id, partition_id, _tablet_id) = table_and_partition(temp.path(), table_name);

    drop(server);

    let engine = Engine::open(EngineOptions::new(&rowstore_dir)).unwrap();
    assert!(engine
        .get(partition_id, &key, Snapshot::new(engine.visible_version()))
        .unwrap()
        .is_some());
    drop(engine);

    let server = LocalServer::open(temp.path()).unwrap();
    server.execute("DROP TABLE purged_table").unwrap();

    assert!(catalog(temp.path())
        .pending_reclaim
        .iter()
        .any(|entry| entry.table_id == table_id && !entry.rowstore_purge_confirmed));

    for _ in 0..32 {
        server.compaction_tick().unwrap();
        let catalog = catalog(temp.path());
        let pending = catalog
            .pending_reclaim
            .iter()
            .find(|entry| entry.table_id == table_id);
        if pending.is_none_or(|entry| entry.rowstore_purge_confirmed) {
            break;
        }
    }

    assert!(
        !catalog(temp.path())
            .pending_reclaim
            .iter()
            .any(|entry| entry.table_id == table_id && !entry.rowstore_purge_confirmed),
        "compaction ticks must confirm the rowstore purge"
    );

    drop(server);

    let ids_before_reopen = sst_ids(&rowstore_dir);

    let engine = Engine::open(EngineOptions::new(&rowstore_dir)).unwrap();
    assert_eq!(
        engine
            .get(partition_id, &key, Snapshot::new(engine.visible_version()))
            .unwrap(),
        None,
        "purged row must be gone before restart"
    );
    drop(engine);

    let server = LocalServer::open(temp.path()).unwrap();
    assert!(catalog(temp.path()).table_by_name(table_name).is_none());
    assert!(
        !catalog(temp.path())
            .pending_reclaim
            .iter()
            .any(|entry| entry.table_id == table_id),
        "confirmed reclaim entry must be removed"
    );
    drop(server);

    let engine = Engine::open(EngineOptions::new(&rowstore_dir)).unwrap();
    assert_eq!(
        engine
            .get(partition_id, &key, Snapshot::new(engine.visible_version()))
            .unwrap(),
        None,
        "purged row must not reappear after rowstore recovery"
    );
    drop(engine);

    assert_eq!(
        sst_ids(&rowstore_dir),
        ids_before_reopen,
        "reopening must not create SSTs that could resurrect purged rows"
    );
}
