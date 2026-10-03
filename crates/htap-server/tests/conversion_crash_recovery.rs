use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{ConversionDescriptor, ConversionPhase, StorageDescriptor, StorageFormat};
use htap_common::types::{Row, Value};
use htap_common::HtapError;
use htap_server::{ConversionAction, LocalServer};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

#[test]
fn test_server_open_serves_manifest_less_segments_written_and_ready_to_publish() {
    use std::sync::Arc;

    fn query_rows(result: StatementResult) -> Vec<Row> {
        match result {
            StatementResult::Query(result) => result.rows,
            other => panic!("expected Query, got {other:?}"),
        }
    }

    fn assert_table_contents(server: &LocalServer, expected: &[(i64, &str)]) {
        let aggregate = query_rows(
            server
                .execute("SELECT COUNT(*), SUM(id) FROM recovery_rows;")
                .unwrap(),
        );
        assert_eq!(aggregate.len(), 1);
        assert_eq!(
            aggregate[0].values(),
            &[
                Value::Int64(expected.len() as i64),
                Value::Int64(expected.iter().map(|(id, _)| *id).sum())
            ]
        );

        let rows = query_rows(
            server
                .execute("SELECT id, value FROM recovery_rows ORDER BY id;")
                .unwrap(),
        );
        let expected_rows: Vec<Row> = expected
            .iter()
            .map(|(id, value)| {
                Row::new(vec![Value::Int64(*id), Value::String((*value).to_string())])
            })
            .collect();
        assert_eq!(rows, expected_rows);
    }

    for phase in [
        ConversionPhase::SegmentsWritten,
        ConversionPhase::ReadyToPublish,
    ] {
        let dir = TempDir::new().unwrap();
        let colstore = dir.path().join("colstore");

        let (partition_id, tablet_id, manifest) = {
            let server = LocalServer::open(dir.path()).unwrap();
            server
                .execute(
                    "CREATE TABLE recovery_rows (
                        id BIGINT PRIMARY KEY,
                        value VARCHAR
                    );",
                )
                .unwrap();
            server
                .execute(
                    "INSERT INTO recovery_rows (id, value) VALUES
                        (1, 'one'), (2, 'two'), (3, 'three');",
                )
                .unwrap();
            server.convert_table_to_column("recovery_rows").unwrap();

            let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
            let catalog = catalog_store.load().unwrap().unwrap();
            let table = catalog.table_by_name("recovery_rows").unwrap();
            let partition_id = table.partitions[0];
            let partition = catalog.partition(partition_id).unwrap();
            let tablet_id = partition.tablets[0];
            let manifest = htap_convert::open(&colstore, tablet_id).unwrap();
            (partition_id, tablet_id, manifest)
        };

        // Do not compact between conversion and this rewrite: once the partition is Column,
        // no conversion descriptor pins the rowstore history needed by the recovered state.
        let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let current = catalog_store.load().unwrap().unwrap();
        let next_generation = current.generation.checked_add(1).unwrap();
        let mut converting = current.clone();
        converting.generation = next_generation;

        let partition = converting
            .partitions
            .iter_mut()
            .find(|partition| partition.id == partition_id)
            .unwrap();
        partition.generation = next_generation;
        partition.storage = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: manifest.generation,
        };
        partition.conversion = Some(ConversionDescriptor::new(
            manifest.generation,
            StorageFormat::Row,
            StorageFormat::Column,
            manifest.base_version,
            phase,
        ));

        let tablet = converting
            .tablets
            .iter_mut()
            .find(|tablet| tablet.id == tablet_id)
            .unwrap();
        tablet.generation = next_generation;
        tablet.column_manifest = None;

        converting.validate().unwrap();
        catalog_store
            .compare_and_set(current.generation, converting)
            .unwrap();
        drop(catalog_store);

        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        assert_table_contents(&server, &[(1, "one"), (2, "two"), (3, "three")]);

        let point_rows = query_rows(
            server
                .execute("SELECT id, value FROM recovery_rows WHERE id = 2;")
                .unwrap(),
        );
        assert_eq!(
            point_rows,
            vec![Row::new(vec![
                Value::Int64(2),
                Value::String("two".to_string()),
            ])]
        );

        server
            .execute("INSERT INTO recovery_rows (id, value) VALUES (4, 'four');")
            .unwrap();
        server
            .execute("UPDATE recovery_rows SET value = 'TWO' WHERE id = 2;")
            .unwrap();
        server
            .execute("DELETE FROM recovery_rows WHERE id = 1;")
            .unwrap();
        assert_table_contents(&server, &[(2, "TWO"), (3, "three"), (4, "four")]);

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        let before_insert = query_rows(
            session
                .execute("SELECT id, value FROM recovery_rows ORDER BY id;")
                .unwrap(),
        );
        assert_eq!(
            before_insert,
            vec![
                Row::new(vec![Value::Int64(2), Value::String("TWO".to_string()),]),
                Row::new(vec![Value::Int64(3), Value::String("three".to_string()),]),
                Row::new(vec![Value::Int64(4), Value::String("four".to_string()),]),
            ]
        );
        session
            .execute("INSERT INTO recovery_rows (id, value) VALUES (5, 'five');")
            .unwrap();
        let with_own_write = query_rows(
            session
                .execute("SELECT id, value FROM recovery_rows ORDER BY id;")
                .unwrap(),
        );
        assert_eq!(
            with_own_write,
            vec![
                Row::new(vec![Value::Int64(2), Value::String("TWO".to_string()),]),
                Row::new(vec![Value::Int64(3), Value::String("three".to_string()),]),
                Row::new(vec![Value::Int64(4), Value::String("four".to_string()),]),
                Row::new(vec![Value::Int64(5), Value::String("five".to_string()),]),
            ]
        );
        session.commit().unwrap();
        drop(session);

        for _ in 0..3 {
            server.compaction_tick().unwrap();
        }
        assert_table_contents(
            &server,
            &[(2, "TWO"), (3, "three"), (4, "four"), (5, "five")],
        );

        let drop_error = server.execute("DROP TABLE recovery_rows;").unwrap_err();
        assert!(
            matches!(drop_error, HtapError::Conflict(_)),
            "expected Conflict before conversion resume, got {drop_error:?}"
        );

        let tick = server.tick().unwrap();
        let report = tick
            .partition_reports()
            .into_iter()
            .find(|report| report.partition_id == partition_id)
            .expect("tick must report the recovered partition");
        assert_eq!(report.action, ConversionAction::Resumed);
        assert_eq!(report.final_storage, StorageDescriptor::Column);

        let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let catalog = catalog_store.load().unwrap().unwrap();
        let partition = catalog.partition(partition_id).unwrap();
        assert_eq!(partition.storage, StorageDescriptor::Column);
        assert!(partition.conversion.is_none());
        let manifest_ref = catalog
            .tablet(tablet_id)
            .unwrap()
            .column_manifest
            .as_ref()
            .expect("tick must publish the existing manifest");
        assert_eq!(manifest_ref.generation, manifest.generation);
        assert_eq!(manifest_ref.base_version, manifest.base_version);

        assert_table_contents(
            &server,
            &[(2, "TWO"), (3, "three"), (4, "four"), (5, "five")],
        );
        drop(catalog_store);
        drop(server);

        let reopened = LocalServer::open(dir.path()).unwrap();
        assert_table_contents(
            &reopened,
            &[(2, "TWO"), (3, "three"), (4, "four"), (5, "five")],
        );
        let reopened_catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let catalog = reopened_catalog.load().unwrap().unwrap();
        let partition = catalog.partition(partition_id).unwrap();
        assert_eq!(partition.storage, StorageDescriptor::Column);
        let manifest_ref = catalog
            .tablet(tablet_id)
            .unwrap()
            .column_manifest
            .as_ref()
            .unwrap();
        assert_eq!(manifest_ref.generation, manifest.generation);
        assert_eq!(manifest_ref.base_version, manifest.base_version);
    }
}

#[test]
fn test_server_open_rejects_converting_with_missing_manifest() {
    let dir = TempDir::new().unwrap();
    let colstore = dir.path().join("colstore");

    let (partition_id, tablet_id, manifest) = {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE missing_manifest (id BIGINT PRIMARY KEY, value VARCHAR);")
            .unwrap();
        server
            .execute(
                "INSERT INTO missing_manifest (id, value) VALUES
                    (1, 'one'), (2, 'two');",
            )
            .unwrap();
        server.convert_table_to_column("missing_manifest").unwrap();

        let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let catalog = catalog_store.load().unwrap().unwrap();
        let table = catalog.table_by_name("missing_manifest").unwrap();
        let partition_id = table.partitions[0];
        let partition = catalog.partition(partition_id).unwrap();
        let tablet_id = partition.tablets[0];
        let manifest = htap_convert::open(&colstore, tablet_id).unwrap();
        (partition_id, tablet_id, manifest)
    };

    // As in the recovery test, do not compact while no conversion descriptor pins history.
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let current = catalog_store.load().unwrap().unwrap();
    let next_generation = current.generation.checked_add(1).unwrap();
    let mut converting = current.clone();
    converting.generation = next_generation;

    let partition = converting
        .partitions
        .iter_mut()
        .find(|partition| partition.id == partition_id)
        .unwrap();
    partition.generation = next_generation;
    partition.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: manifest.generation,
    };
    partition.conversion = Some(ConversionDescriptor::new(
        manifest.generation,
        StorageFormat::Row,
        StorageFormat::Column,
        manifest.base_version,
        ConversionPhase::SegmentsWritten,
    ));

    let tablet = converting
        .tablets
        .iter_mut()
        .find(|tablet| tablet.id == tablet_id)
        .unwrap();
    tablet.generation = next_generation;
    tablet.column_manifest = None;

    converting.validate().unwrap();
    catalog_store
        .compare_and_set(current.generation, converting)
        .unwrap();
    drop(catalog_store);

    std::fs::remove_file(htap_convert::manifest_path(&colstore, tablet_id)).unwrap();

    // SegmentsWritten requires a durable manifest on open even though the catalog reference is
    // intentionally absent until final publish; a missing file therefore fails closed.
    let error = LocalServer::open(dir.path()).unwrap_err();
    assert!(
        matches!(error, HtapError::Io(_)),
        "open currently propagates the missing manifest as Io, got {error:?}"
    );
}
