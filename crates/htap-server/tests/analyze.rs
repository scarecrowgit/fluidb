use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::error::HtapError;
use htap_common::types::Value;
use htap_server::LocalServer;
use tempfile::tempdir;

fn load_catalog(root: &std::path::Path) -> htap_catalog::CatalogSnapshot {
    LocalCatalogStore::open(root.join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap()
}

fn create_metrics_table(server: &LocalServer) {
    server
        .execute(
            "CREATE TABLE metrics (
                id BIGINT PRIMARY KEY,
                score BIGINT,
                label VARCHAR
            )",
        )
        .unwrap();
}

fn insert_metrics_rows(server: &LocalServer) {
    server
        .execute(
            "INSERT INTO metrics (id, score, label) VALUES
                (1, 10, 'zebra'),
                (2, NULL, NULL),
                (3, 5, 'apple')",
        )
        .unwrap();
}

#[test]
fn test_analyze_table_row_count_nulls_min_max() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    create_metrics_table(&server);
    insert_metrics_rows(&server);

    server.execute("ANALYZE TABLE metrics").unwrap();

    let catalog = load_catalog(dir.path());
    let table = catalog.table_by_name("metrics").unwrap();
    let stats = table.stats.as_ref().unwrap();

    assert_eq!(stats.row_count, 3);
    assert_eq!(stats.columns.len(), 3);

    assert_eq!(stats.columns[0].null_count, 0);
    assert_eq!(stats.columns[0].min, Some(Value::Int64(1)));
    assert_eq!(stats.columns[0].max, Some(Value::Int64(3)));

    assert_eq!(stats.columns[1].null_count, 1);
    assert_eq!(stats.columns[1].min, Some(Value::Int64(5)));
    assert_eq!(stats.columns[1].max, Some(Value::Int64(10)));

    assert_eq!(stats.columns[2].null_count, 1);
    assert_eq!(
        stats.columns[2].min,
        Some(Value::String("apple".to_string()))
    );
    assert_eq!(
        stats.columns[2].max,
        Some(Value::String("zebra".to_string()))
    );
}

#[test]
fn test_analyze_distinct_count_cap_falls_back_to_unknown() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path())
        .unwrap()
        .with_analyze_distinct_limit(2);

    server
        .execute(
            "CREATE TABLE values_to_analyze (
                id BIGINT PRIMARY KEY,
                value BIGINT
            )",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO values_to_analyze (id, value) VALUES
                (1, 10),
                (2, 20),
                (3, 30),
                (4, NULL)",
        )
        .unwrap();

    server.execute("ANALYZE TABLE values_to_analyze").unwrap();

    let catalog = load_catalog(dir.path());
    let stats = catalog
        .table_by_name("values_to_analyze")
        .unwrap()
        .stats
        .as_ref()
        .unwrap();

    assert_eq!(stats.row_count, 4);
    assert_eq!(stats.columns[0].null_count, 0);
    assert_eq!(stats.columns[0].min, Some(Value::Int64(1)));
    assert_eq!(stats.columns[0].max, Some(Value::Int64(4)));
    assert_eq!(stats.columns[0].distinct_count, None);

    assert_eq!(stats.columns[1].null_count, 1);
    assert_eq!(stats.columns[1].min, Some(Value::Int64(10)));
    assert_eq!(stats.columns[1].max, Some(Value::Int64(30)));
    assert_eq!(stats.columns[1].distinct_count, None);
}

#[test]
fn test_analyze_reopen_recovery() {
    let dir = tempdir().unwrap();

    {
        let server = LocalServer::open(dir.path()).unwrap();
        create_metrics_table(&server);
        insert_metrics_rows(&server);
        server.execute("ANALYZE TABLE metrics").unwrap();
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    let catalog = load_catalog(dir.path());
    let stats = catalog
        .table_by_name("metrics")
        .unwrap()
        .stats
        .as_ref()
        .unwrap();

    assert_eq!(stats.row_count, 3);
    assert_eq!(stats.columns[1].null_count, 1);
    assert_eq!(stats.columns[1].min, Some(Value::Int64(5)));
    assert_eq!(stats.columns[1].max, Some(Value::Int64(10)));

    reopened
        .execute("SELECT * FROM metrics WHERE id = 1")
        .unwrap();
}

#[test]
fn test_analyze_target_succeeds() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE analyze_target (
                id BIGINT PRIMARY KEY,
                value BIGINT
            )",
        )
        .unwrap();

    let values = (0..5_000)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(",");
    server
        .execute(&format!(
            "INSERT INTO analyze_target (id, value) VALUES {values}"
        ))
        .unwrap();

    server.execute("ANALYZE TABLE analyze_target").unwrap();

    let catalog = load_catalog(dir.path());
    let stats = catalog
        .table_by_name("analyze_target")
        .unwrap()
        .stats
        .as_ref()
        .unwrap();
    assert_eq!(stats.row_count, 5_000);
    assert_eq!(stats.columns[1].min, Some(Value::Int64(0)));
    assert_eq!(stats.columns[1].max, Some(Value::Int64(4_999)));
}

#[test]
fn test_analyze_concurrent_unrelated_catalog_mutation() {
    let dir = tempdir().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute(
            "CREATE TABLE target_table (
                id BIGINT PRIMARY KEY,
                value BIGINT
            )",
        )
        .unwrap();

    let values = (0..5_000)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(",");
    server
        .execute(&format!(
            "INSERT INTO target_table (id, value) VALUES {values}"
        ))
        .unwrap();

    let analyze_started = Arc::new(AtomicBool::new(false));
    let mutation_started = Arc::clone(&analyze_started);
    let mutation_server = Arc::clone(&server);

    thread::scope(|scope| {
        scope.spawn(move || {
            while !mutation_started.load(Ordering::Acquire) {
                thread::yield_now();
            }

            mutation_server
                .execute(
                    "CREATE TABLE other_table (
                        id BIGINT PRIMARY KEY
                    )",
                )
                .unwrap();
        });

        analyze_started.store(true, Ordering::Release);
        server.execute("ANALYZE TABLE target_table").unwrap();
    });

    let catalog = load_catalog(dir.path());
    let stats = catalog
        .table_by_name("target_table")
        .unwrap()
        .stats
        .as_ref()
        .unwrap();

    assert_eq!(stats.row_count, 5_000);
    assert_eq!(stats.columns[0].min, Some(Value::Int64(0)));
    assert_eq!(stats.columns[0].max, Some(Value::Int64(4_999)));
    assert_eq!(stats.columns[1].min, Some(Value::Int64(0)));
    assert_eq!(stats.columns[1].max, Some(Value::Int64(4_999)));
    assert!(catalog.table_by_name("other_table").is_some());
}

#[test]
fn test_analyze_missing_table_errors() {
    let dir = tempdir().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute(
            "CREATE TABLE disappearing_table (
                id BIGINT PRIMARY KEY,
                value BIGINT
            )",
        )
        .unwrap();

    let values = (0..5_000)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(",");
    server
        .execute(&format!(
            "INSERT INTO disappearing_table (id, value) VALUES {values}"
        ))
        .unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let drop_started = Arc::clone(&started);

    let error = thread::scope(|scope| {
        scope.spawn(|| {
            let root = dir.path().to_path_buf();

            while !drop_started.load(Ordering::Acquire) {
                thread::yield_now();
            }

            let store = LocalCatalogStore::open(root.join("catalog")).unwrap();
            loop {
                let snapshot = store.load().unwrap().unwrap();
                let table = snapshot.table_by_name("disappearing_table").unwrap();
                let table_id = table.id;
                let partition_ids: HashSet<_> = table.partitions.iter().copied().collect();
                let tablet_ids: HashSet<_> = snapshot
                    .tablets
                    .iter()
                    .filter(|tablet| partition_ids.contains(&tablet.partition_id))
                    .map(|tablet| tablet.id)
                    .collect();
                let replica_ids: HashSet<_> = snapshot
                    .replicas
                    .iter()
                    .filter(|replica| tablet_ids.contains(&replica.tablet_id))
                    .map(|replica| replica.id)
                    .collect();

                let mut next = snapshot.clone();
                next.generation = snapshot.generation.checked_add(1).unwrap();
                next.tables.retain(|candidate| candidate.id != table_id);
                next.partitions
                    .retain(|partition| !partition_ids.contains(&partition.id));
                next.tablets
                    .retain(|tablet| !tablet_ids.contains(&tablet.id));
                next.replicas
                    .retain(|replica| !replica_ids.contains(&replica.id));
                next.grants.retain(|grant| {
                    !matches!(
                        grant.scope,
                        htap_catalog::PrivilegeScope::Table(id) if id == table_id
                    )
                });

                if store.compare_and_set(snapshot.generation, next).is_ok() {
                    return;
                }
            }
        });

        started.store(true, Ordering::Release);
        server
            .execute("ANALYZE TABLE disappearing_table")
            .unwrap_err()
    });

    assert!(matches!(error, HtapError::NotFound(_)));
}
