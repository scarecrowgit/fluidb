use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn rows(server: &LocalServer, sql: &str) -> Vec<Row> {
    match server.execute(sql) {
        Ok(StatementResult::Query(query)) => query.rows,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn sorted_rows(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn test_spill_uses_server_root_and_reopen_sweeps_abandoned_files() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_root_left ( \
                 id BIGINT PRIMARY KEY, join_key INT, value VARCHAR(256) \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE spill_root_right ( \
                 id BIGINT PRIMARY KEY, join_key INT, value VARCHAR(256) \
             );",
        )
        .unwrap();

    let left_payload = "x".repeat(192);
    let right_payload = "y".repeat(192);
    let left_values = (1..=256)
        .map(|id| format!("({id}, {id}, 'left-{id:04}-{left_payload}')"))
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=256)
        .map(|id| format!("({}, {id}, 'right-{id:04}-{right_payload}')", id + 1_000))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_root_left (id, join_key, value) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_root_right (id, join_key, value) VALUES {right_values};"
        ))
        .unwrap();

    server.set_query_memory_budget(32 * 1024);
    let result = rows(
        &server,
        "SELECT l.id, r.id \
         FROM spill_root_left l \
         JOIN spill_root_right r ON l.join_key = r.join_key;",
    );
    assert_eq!(result.len(), 256);
    assert!(server.last_query_hash_join_spilled());

    let spill_dir = root.path().join("spill");
    assert!(spill_dir.exists());
    assert!(
        std::fs::read_dir(&spill_dir).unwrap().next().is_none(),
        "completed statement left spill directories behind"
    );

    drop(server);

    let statement_dir = spill_dir.join("abandoned-statement");
    let nested_dir = statement_dir.join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(statement_dir.join("hash-join.spill"), b"abandoned").unwrap();
    std::fs::write(nested_dir.join("sort.spill"), b"abandoned").unwrap();

    let reopened = LocalServer::open(root.path()).unwrap();
    assert!(!spill_dir.exists());
    drop(reopened);
}

#[test]
fn test_hash_join_spills_under_tiny_budget_same_result() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_left ( \
                 id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(64) \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE spill_right ( \
                 id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(64) \
             );",
        )
        .unwrap();

    let left_values = (1..=256)
        .map(|id| {
            format!(
                "({id}, {}, 'left-payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                id % 37
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=320)
        .map(|id| {
            format!(
                "({}, {}, 'right-payload-{id:04}-yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy')",
                id + 1_000,
                id % 37
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_left (id, join_key, payload) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_right (id, join_key, payload) VALUES {right_values};"
        ))
        .unwrap();

    let sql = "SELECT l.id, r.id, l.join_key, l.payload, r.payload \
               FROM spill_left l \
               INNER JOIN spill_right r ON l.join_key = r.join_key;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert!(!in_memory.is_empty());

    server.set_query_memory_budget(32 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_hash_join_spilled());
}

#[test]
fn test_order_by_spills_preserves_order_and_nulls() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_order ( \
                 id BIGINT PRIMARY KEY, score INT, name VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| {
            let score = if id % 11 == 0 {
                "NULL".to_string()
            } else {
                (id % 73).to_string()
            };
            let name = format!(
                "name-{:03}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                id % 29
            );
            format!("({id}, {score}, '{name}')")
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_order (id, score, name) VALUES {values};"
        ))
        .unwrap();

    // Single-table sorts take the analytic path, which is not budgeted.
    let sql = "SELECT l.score, l.name FROM spill_order l \
               JOIN spill_order r ON l.id = r.id \
               ORDER BY l.score ASC NULLS FIRST, l.name DESC;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = rows(&server, sql);
    assert_eq!(in_memory.len(), 4_096);
    assert!(in_memory
        .first()
        .is_some_and(|row| row.values().first() == Some(&Value::Null)));

    server.set_query_memory_budget(32 * 1024);
    let spilled = rows(&server, sql);

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_sort_spilled());
}

#[test]
fn test_select_distinct_spill_deduplicates() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute("CREATE TABLE spill_distinct (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();

    let values = (1..=4_096)
        .map(|id| format!("({id}, {})", id % 3_072))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_distinct (id, value) VALUES {values};"
        ))
        .unwrap();

    let sql = "SELECT DISTINCT value FROM spill_distinct;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 3_072);

    server.set_query_memory_budget(32 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert_eq!(spilled.len(), 3_072);
    assert!(server.last_query_distinct_spilled());
    for value in 0..3_072 {
        assert_eq!(
            spilled
                .iter()
                .filter(|row| row == &&Row::new(vec![Value::Int32(value)]))
                .count(),
            1
        );
    }
}

#[test]
fn test_except_intersect_all_spill_preserves_multiplicity() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute("CREATE TABLE spill_set_left (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE spill_set_right (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();

    let left_values = (1..=512)
        .map(|id| format!("({id}, {})", id % 97))
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=384)
        .map(|id| format!("({id}, {})", id % 73))
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_set_left (id, value) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_set_right (id, value) VALUES {right_values};"
        ))
        .unwrap();

    let queries = [
        "SELECT value FROM spill_set_left \
         EXCEPT ALL \
         SELECT value FROM spill_set_right;",
        "SELECT value FROM spill_set_left \
         INTERSECT ALL \
         SELECT value FROM spill_set_right;",
    ];

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();

    server.set_query_memory_budget(32 * 1024);
    let spilled = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_set_operation_spilled());

    for value in 0..97 {
        let left_count = (1..=512).filter(|id| id % 97 == value).count();
        let right_count = (1..=384).filter(|id| id % 73 == value).count();
        let except_count = spilled[0]
            .iter()
            .filter(|row| row == &&Row::new(vec![Value::Int32(value)]))
            .count();
        let intersect_count = spilled[1]
            .iter()
            .filter(|row| row == &&Row::new(vec![Value::Int32(value)]))
            .count();

        assert_eq!(except_count, left_count.saturating_sub(right_count));
        assert_eq!(intersect_count, left_count.min(right_count));
    }
}

#[test]
fn test_union_distinct_spill_deduplicates() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute("CREATE TABLE spill_union_left (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE spill_union_right (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();

    let left_values = (1..=4_096)
        .map(|id| format!("({id}, {})", id % 3_072))
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=4_096)
        .map(|id| format!("({id}, {})", (id + 1_024) % 3_072))
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_union_left (id, value) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_union_right (id, value) VALUES {right_values};"
        ))
        .unwrap();

    let sql = "SELECT value FROM spill_union_left \
               UNION \
               SELECT value FROM spill_union_right;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 3_072);

    server.set_query_memory_budget(32 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert_eq!(spilled.len(), 3_072);
    assert!(server.last_query_set_operation_spilled());
    for value in 0..3_072 {
        assert_eq!(
            spilled
                .iter()
                .filter(|row| row == &&Row::new(vec![Value::Int32(value)]))
                .count(),
            1
        );
    }
}

#[test]
fn test_hash_join_mixed_int_float_keys_normalizes_correctly() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE mixed_int_keys ( \
                 id INT PRIMARY KEY, int_key INT, bigint_key BIGINT, payload VARCHAR(256) \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE mixed_float_keys ( \
                 id BIGINT PRIMARY KEY, int_key DOUBLE, bigint_key DOUBLE, payload VARCHAR(256) \
             );",
        )
        .unwrap();

    let int_payload = "x".repeat(192);
    let float_payload = "y".repeat(192);
    let int_values = (1..=192)
        .map(|id| {
            let int_key = id % 29;
            let bigint_key = 1_000_000 + (id % 31) as i64;
            format!("({id}, {int_key}, {bigint_key}, 'int-{id:04}-{int_payload}')")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let float_values = (1..=224)
        .map(|id| {
            let int_key = id % 29;
            let bigint_key = 1_000_000 + (id % 31) as i64;
            format!(
                "({}, {int_key}.0, {bigint_key}.0, 'float-{id:04}-{float_payload}')",
                id + 1_000
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO mixed_int_keys (id, int_key, bigint_key, payload) VALUES {int_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO mixed_float_keys (id, int_key, bigint_key, payload) VALUES {float_values};"
        ))
        .unwrap();

    let sql = "SELECT i.id, f.id, i.int_key, i.bigint_key \
               FROM mixed_int_keys i \
               INNER JOIN mixed_float_keys f \
                 ON i.int_key = f.int_key \
                AND i.bigint_key = f.bigint_key;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert!(!in_memory.is_empty());

    server.set_query_memory_budget(32 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);

    assert!(spilled.iter().any(|row| {
        row == &Row::new(vec![
            Value::Int32(1),
            Value::Int64(1_001),
            Value::Int32(1),
            Value::Int64(1_000_001),
        ])
    }));
    assert!(server.last_query_hash_join_spilled());
}

#[test]
fn test_ungrouped_aggregate_spill_returns_one_row_including_empty_input() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute("CREATE TABLE spill_ungrouped (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();

    let values = (1..=4_096)
        .map(|id| format!("({id}, {})", id % 97))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_ungrouped (id, value) VALUES {values};"
        ))
        .unwrap();

    let sql = "SELECT COUNT(*), SUM(l.value), MIN(l.value) \
               FROM spill_ungrouped l \
               JOIN spill_ungrouped r ON l.id = r.id;";
    let empty_sql = "SELECT COUNT(*), SUM(l.value), MIN(l.value) \
                     FROM spill_ungrouped l \
                     JOIN spill_ungrouped r ON l.id = r.id \
                     WHERE l.id < 0 AND r.id < 0;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = rows(&server, sql);
    let empty_in_memory = rows(&server, empty_sql);
    assert_eq!(in_memory.len(), 1);
    assert_eq!(empty_in_memory.len(), 1);

    server.set_query_memory_budget(32 * 1024);
    let spilled = rows(&server, sql);
    let empty_spilled = rows(&server, empty_sql);

    assert_eq!(spilled, in_memory);
    assert_eq!(empty_spilled, empty_in_memory);
    assert_eq!(spilled.len(), 1);
    assert_eq!(empty_spilled.len(), 1);
    assert_eq!(
        empty_spilled,
        vec![Row::new(vec![Value::Int64(0), Value::Null, Value::Null])]
    );
    assert!(!server.last_query_group_by_spilled());
}

#[test]
fn test_group_by_spills_under_tiny_budget_same_result() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_groups ( \
                 id BIGINT PRIMARY KEY, grp INT, amount INT, distinct_value VARCHAR(32) \
             );",
        )
        .unwrap();

    let values = (1..=2_048)
        .map(|id| {
            let grp = id % 512;
            let amount = id % 97;
            let distinct_value = format!("value-{}-xxxxxxxxxxxxxxxx", id % 4);
            format!("({id}, {grp}, {amount}, '{distinct_value}')")
        })
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_groups (id, grp, amount, distinct_value) VALUES {values};"
        ))
        .unwrap();

    let sql = "SELECT grp, AVG(amount), COUNT(DISTINCT distinct_value) \
               FROM spill_groups GROUP BY grp;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 512);

    server.set_query_memory_budget(32 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_group_by_spilled());
}

#[test]
fn test_skewed_hash_join_spill_exceeds_partition_budget_cleanly() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE skew_left ( \
                 id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE skew_right ( \
                 id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let left_values = (1..=4_096)
        .map(|id| format!("({id}, 1, 'left-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"))
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=4_096)
        .map(|id| {
            format!(
                "({}, 1, 'right-{id:04}-yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy')",
                id + 10_000
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO skew_left (id, join_key, payload) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO skew_right (id, join_key, payload) VALUES {right_values};"
        ))
        .unwrap();

    server.set_query_memory_budget(32 * 1024);
    let error = server
        .execute(
            "SELECT l.id, r.id FROM skew_left l \
             JOIN skew_right r ON l.join_key = r.join_key;",
        )
        .unwrap_err();

    assert!(
        matches!(&error, htap_common::HtapError::InvalidArgument(_)),
        "{error}"
    );
    assert!(
        error.to_string().to_lowercase().contains("budget"),
        "{error}"
    );
    assert!(server.last_query_hash_join_spilled());
}

#[test]
fn test_spilled_hash_join_matches_across_row_column_and_converting_tables() {
    use htap_catalog::store::CatalogStore;

    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    for table in ["r", "c", "k"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} ( \
                     id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(96) \
                 );"
            ))
            .unwrap();

        let values = (1..=512)
            .map(|id| {
                format!(
                    "({id}, {}, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                    id % 61
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO {table} (id, join_key, payload) VALUES {values};"
            ))
            .unwrap();
    }

    assert!(server.convert_table_to_column("c").unwrap().is_success());

    let catalog_store =
        htap_catalog::local::LocalCatalogStore::open(root.path().join("catalog")).unwrap();
    let current = catalog_store.load().unwrap().unwrap();
    let partition_id = current.table_by_name("k").unwrap().partitions[0];
    let mut next = current.clone();
    next.generation += 1;
    let partition = next
        .partitions
        .iter_mut()
        .find(|partition| partition.id == partition_id)
        .unwrap();
    partition.generation = next.generation;
    partition.storage = htap_catalog::StorageDescriptor::Converting {
        from: htap_catalog::StorageFormat::Row,
        to: htap_catalog::StorageFormat::Column,
        generation: next.generation,
    };
    partition.conversion = Some(htap_catalog::ConversionDescriptor::new(
        next.generation,
        htap_catalog::StorageFormat::Row,
        htap_catalog::StorageFormat::Column,
        htap_common::version::Version::new(1),
        htap_catalog::ConversionPhase::SnapshotPinned,
    ));
    catalog_store
        .compare_and_set(current.generation, next)
        .unwrap();
    drop(catalog_store);

    let queries = [
        "SELECT l.id, r.id, l.payload, r.payload \
         FROM r l JOIN c r ON l.join_key = r.join_key;",
        "SELECT l.id, r.id, l.payload, r.payload \
         FROM r l JOIN k r ON l.join_key = r.join_key;",
        "SELECT l.id, r.id, l.payload, r.payload \
         FROM c l JOIN k r ON l.join_key = r.join_key;",
    ];

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();
    assert!(!in_memory[0].is_empty());
    assert_eq!(in_memory[0], in_memory[1]);
    assert_eq!(in_memory[0], in_memory[2]);

    server.set_query_memory_budget(32 * 1024);
    let spilled = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_hash_join_spilled());
}

#[test]
fn test_window_row_number_spills_under_tiny_budget_same_result() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_window_row_number ( \
                 id BIGINT PRIMARY KEY, grp INT, score INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| {
            format!(
                "({id}, {}, {}, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                id % 97,
                id % 73
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_window_row_number (id, grp, score, payload) VALUES {values};"
        ))
        .unwrap();

    // Single-table window queries take the analytic path, which is not budgeted.
    let sql = "SELECT l.id, l.grp, l.score, \
               ROW_NUMBER() OVER (PARTITION BY l.grp ORDER BY l.score, l.id) \
               FROM spill_window_row_number l \
               JOIN spill_window_row_number r ON l.id = r.id;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 4_096);

    // The full joined input is about 1.7 MiB; each of the 97 partitions is about 18 KiB.
    // 128 KiB therefore forces bounded spilling while allowing an individual partition to fit.
    server.set_query_memory_budget(128 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_window_spilled());
}

#[test]
fn test_large_join_order_by_spills_and_preserves_order() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_join_order_left ( \
                 id BIGINT PRIMARY KEY, join_key INT, score INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE spill_join_order_right ( \
                 id BIGINT PRIMARY KEY, join_key INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let left_values = (1..=4_096)
        .map(|id| {
            let score = if id % 11 == 0 {
                "NULL".to_string()
            } else {
                (id % 73).to_string()
            };
            format!(
                "({id}, {id}, {score}, \
                 'left-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=4_096)
        .map(|id| {
            format!(
                "({}, {id}, 'right-{id:04}-yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy')",
                id + 10_000
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_join_order_left (id, join_key, score, payload) \
             VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_join_order_right (id, join_key, payload) \
             VALUES {right_values};"
        ))
        .unwrap();

    let sql = "SELECT l.id, r.id, l.score, l.payload, r.payload \
               FROM spill_join_order_left l \
               JOIN spill_join_order_right r ON l.join_key = r.join_key \
               ORDER BY l.score ASC NULLS FIRST, l.payload DESC;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = rows(&server, sql);
    assert_eq!(in_memory.len(), 4_096);
    assert!(in_memory
        .first()
        .is_some_and(|row| row.values().get(2) == Some(&Value::Null)));

    server.set_query_memory_budget(32 * 1024);
    let spilled = rows(&server, sql);

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_sort_spilled());
}

#[test]
fn test_window_without_partition_by_fails_when_its_input_exceeds_budget() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_window_unpartitioned ( \
                 id BIGINT PRIMARY KEY, score INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| {
            format!(
                "({id}, {}, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                id % 73
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_window_unpartitioned (id, score, payload) VALUES {values};"
        ))
        .unwrap();

    server.set_query_memory_budget(256 * 1024);
    let error = server
        .execute(
            "SELECT l.id, ROW_NUMBER() OVER (ORDER BY l.score, l.id) \
             FROM spill_window_unpartitioned l \
             JOIN spill_window_unpartitioned r ON l.id = r.id;",
        )
        .unwrap_err();

    assert!(
        matches!(&error, htap_common::HtapError::InvalidArgument(_)),
        "{error}"
    );
    assert!(
        error.to_string().to_lowercase().contains("budget"),
        "{error}"
    );
    assert!(server.last_query_window_spilled());
}

#[test]
fn test_window_rank_spills_preserves_ties_and_nulls() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_window_rank ( \
                 id BIGINT PRIMARY KEY, grp INT, score INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| {
            let score = if id % 17 == 0 {
                "NULL".to_string()
            } else {
                (id % 31).to_string()
            };
            format!(
                "({id}, {}, {score}, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                id % 23
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_window_rank (id, grp, score, payload) VALUES {values};"
        ))
        .unwrap();

    let sql = "SELECT l.id, l.grp, l.score, \
               RANK() OVER (PARTITION BY l.grp ORDER BY l.score ASC NULLS FIRST) \
               FROM spill_window_rank l \
               JOIN spill_window_rank r ON l.id = r.id;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 4_096);

    // The full joined input is about 1.7 MiB; the largest of 23 partitions is about 75 KiB.
    // 256 KiB forces spilling but leaves enough room for the largest logical partition.
    server.set_query_memory_budget(256 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_window_spilled());
}

#[test]
fn test_window_running_sum_spills_under_tiny_budget_same_result() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_window_sum ( \
                 id BIGINT PRIMARY KEY, grp INT, amount INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| {
            format!(
                "({id}, {}, {}, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')",
                id % 41,
                id % 97
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_window_sum (id, grp, amount, payload) VALUES {values};"
        ))
        .unwrap();

    let sql = "SELECT l.id, l.grp, l.amount, \
               SUM(l.amount) OVER ( \
                   PARTITION BY l.grp \
                   ORDER BY l.id \
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
               ) \
               FROM spill_window_sum l \
               JOIN spill_window_sum r ON l.id = r.id;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = sorted_rows(rows(&server, sql));
    assert_eq!(in_memory.len(), 4_096);

    // The full joined input is about 1.7 MiB; the largest of 41 partitions is about 43 KiB.
    // 256 KiB forces spilling while allowing each logical partition to be evaluated in memory.
    server.set_query_memory_budget(256 * 1024);
    let spilled = sorted_rows(rows(&server, sql));

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_window_spilled());
}

#[test]
fn test_window_skewed_partition_fails_while_other_queries_succeed() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_window_skew ( \
                 id BIGINT PRIMARY KEY, grp INT, payload VARCHAR(96) \
             );",
        )
        .unwrap();

    let values = (1..=4_096)
        .map(|id| format!("({id}, 1, 'payload-{id:04}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx')"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO spill_window_skew (id, grp, payload) VALUES {values};"
        ))
        .unwrap();

    // A simple query succeeds under the same budget, but the one 1.7 MiB window partition does
    // not fit within 128 KiB and must fail rather than recursively spilling.
    server.set_query_memory_budget(128 * 1024);
    assert_eq!(
        rows(&server, "SELECT id FROM spill_window_skew WHERE id = 1;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    let error = server
        .execute(
            "SELECT l.id, ROW_NUMBER() OVER (PARTITION BY l.grp ORDER BY l.id) \
             FROM spill_window_skew l \
             JOIN spill_window_skew r ON l.id = r.id;",
        )
        .unwrap_err();
    assert!(
        matches!(&error, htap_common::HtapError::InvalidArgument(_)),
        "{error}"
    );
    assert!(
        error.to_string().to_lowercase().contains("budget"),
        "{error}"
    );
    assert!(server.last_query_window_spilled());
}

#[test]
fn test_spilled_set_operations_match_in_memory_for_doubles() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE spill_double_left ( \
                 id BIGINT PRIMARY KEY, value DOUBLE \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE spill_double_right ( \
                 id BIGINT PRIMARY KEY, value DOUBLE \
             );",
        )
        .unwrap();

    // Integer literals convert exactly to DOUBLE, leaving this test focused on spill behavior.
    let left_values = (1..=4_096)
        .map(|id| format!("({id}, {})", id % 3_072))
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=4_096)
        .map(|id| format!("({id}, {})", (id + 1_024) % 3_072))
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO spill_double_left (id, value) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO spill_double_right (id, value) VALUES {right_values};"
        ))
        .unwrap();

    let queries = [
        "SELECT value FROM spill_double_left \
         UNION \
         SELECT value FROM spill_double_right;",
        "SELECT value FROM spill_double_left \
         EXCEPT ALL \
         SELECT value FROM spill_double_right;",
        "SELECT value FROM spill_double_left \
         INTERSECT ALL \
         SELECT value FROM spill_double_right;",
    ];

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();

    server.set_query_memory_budget(32 * 1024);
    let spilled = queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();

    assert_eq!(spilled, in_memory);
    assert!(server.last_query_set_operation_spilled());
}

#[test]
fn test_spilled_set_operations_and_distinct_preserve_awkward_doubles() {
    let root = TempDir::new().unwrap();
    let mut server = LocalServer::open(root.path()).unwrap();
    server.set_query_memory_budget(256 * 1024 * 1024);

    server
        .execute(
            "CREATE TABLE awkward_double_left ( \
                 id BIGINT PRIMARY KEY, value DOUBLE \
             );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE awkward_double_right ( \
                 id BIGINT PRIMARY KEY, value DOUBLE \
             );",
        )
        .unwrap();

    let awkward_values = [
        0.1_f64,
        -0.0_f64,
        1.000_000_000_000_000_2_f64,
        1.234_567_890_123_456_7_f64,
        1e-308_f64,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
    ];
    let sql_value = |value: f64| {
        if value == 0.0 && value.is_sign_negative() {
            "-0.0".to_string()
        } else {
            value.to_string()
        }
    };
    let left_values = (1..=512)
        .map(|id| {
            format!(
                "({id}, {})",
                sql_value(awkward_values[id as usize % awkward_values.len()])
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let right_values = (1..=512)
        .map(|id| {
            format!(
                "({}, {})",
                id + 10_000,
                sql_value(awkward_values[(id as usize + 3) % awkward_values.len()])
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO awkward_double_left (id, value) VALUES {left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO awkward_double_right (id, value) VALUES {right_values};"
        ))
        .unwrap();

    let additional_left_values = (513..=1_536)
        .map(|id| {
            format!(
                "({id}, {})",
                sql_value(awkward_values[id as usize % awkward_values.len()])
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let additional_right_values = (513..=1_536)
        .map(|id| {
            format!(
                "({}, {})",
                id + 10_000,
                sql_value(awkward_values[(id as usize + 3) % awkward_values.len()])
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    server
        .execute(&format!(
            "INSERT INTO awkward_double_left (id, value) VALUES {additional_left_values};"
        ))
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO awkward_double_right (id, value) VALUES {additional_right_values};"
        ))
        .unwrap();

    let set_queries = [
        "SELECT value FROM awkward_double_left \
         UNION \
         SELECT value FROM awkward_double_right;",
        "SELECT value FROM awkward_double_left \
         EXCEPT ALL \
         SELECT value FROM awkward_double_right;",
        "SELECT value FROM awkward_double_left \
         INTERSECT ALL \
         SELECT value FROM awkward_double_right;",
    ];
    let distinct_sql = "SELECT DISTINCT value FROM awkward_double_left;";

    server.set_query_memory_budget(256 * 1024 * 1024);
    let in_memory_sets = set_queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();
    let in_memory_distinct = sorted_rows(rows(&server, distinct_sql));

    // NOTES: 64 KiB accommodates the set-operation working buffer while still forcing spills.
    server.set_query_memory_budget(64 * 1024);
    let spilled_sets = set_queries
        .iter()
        .map(|sql| sorted_rows(rows(&server, sql)))
        .collect::<Vec<_>>();
    assert!(server.last_query_set_operation_spilled());
    let spilled_distinct = sorted_rows(rows(&server, distinct_sql));
    assert!(server.last_query_distinct_spilled());

    assert_eq!(spilled_sets, in_memory_sets);
    assert_eq!(spilled_distinct, in_memory_distinct);
    assert_eq!(spilled_distinct.len(), awkward_values.len());
    for value in awkward_values {
        assert!(spilled_distinct.iter().any(|row| {
            matches!(row.get(0), Some(Value::Float64(actual)) if actual.to_bits() == value.to_bits())
        }));
    }
}
