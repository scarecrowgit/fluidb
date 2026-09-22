use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Barrier,
    },
    thread,
    time::Duration,
};

use htap_common::{HtapError, Row, Value, Version};
use htap_rowstore::{CompactionInput, Engine, EngineOptions, Mutation, Snapshot};

fn make_row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn flush_put(engine: &Engine, txn_id: u64, partition_id: u64, key: &[u8], value: i64) {
    engine
        .commit(
            txn_id,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id,
                key: key.to_vec(),
                row: make_row(value),
            }],
        )
        .unwrap();
    engine.flush().unwrap();
}

#[test]
fn test_concurrent_reads_do_not_deadlock_with_flush_and_compaction() {
    const READER_COUNT: usize = 4;
    const WRITER_ITERATIONS: usize = 32;
    const READER_ITERATIONS: usize = 1_000;

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(EngineOptions::new(dir.path())).unwrap());

    // Create enough equally sized SSTs for a first compaction and establish a
    // stable GC low-water that readers must continue to respect.
    flush_put(&engine, 1, 0, b"seed", 20);
    flush_put(&engine, 2, 0, b"seed", 30);
    flush_put(&engine, 3, 0, b"seed", 40);
    flush_put(&engine, 4, 0, b"seed", 50);

    let gc_horizon = Version::new(4);
    let initial_compaction = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon,
        })
        .unwrap();
    assert!(initial_compaction.compacted);

    let completed_operations = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(READER_COUNT + 1));
    let (done_tx, done_rx) = mpsc::channel();

    let supervisor_engine = Arc::clone(&engine);
    let supervisor_operations = Arc::clone(&completed_operations);
    let supervisor_start = Arc::clone(&start);

    thread::spawn(move || {
        let mut readers = Vec::new();

        for reader_id in 0..READER_COUNT {
            let engine = Arc::clone(&supervisor_engine);
            let operations = Arc::clone(&supervisor_operations);
            let start = Arc::clone(&supervisor_start);

            readers.push(thread::spawn(move || {
                start.wait();

                for iteration in 0..READER_ITERATIONS {
                    let partition_id = ((reader_id + iteration) % 4) as u64;
                    let key = format!("key-{}", iteration % WRITER_ITERATIONS);
                    let snapshot = engine.snapshot();

                    if engine.get(partition_id, key.as_bytes(), snapshot).is_ok() {
                        operations.fetch_add(1, Ordering::Relaxed);
                    }

                    if engine.scan_partition(partition_id, snapshot).is_ok() {
                        operations.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }

        let writer_engine = Arc::clone(&supervisor_engine);
        let writer_start = Arc::clone(&supervisor_start);
        let writer = thread::spawn(move || {
            writer_start.wait();

            for iteration in 0..WRITER_ITERATIONS {
                let partition_id = (iteration % 4) as u64;
                let key = format!("key-{iteration}");

                writer_engine
                    .commit(
                        1_000 + iteration as u64,
                        writer_engine.snapshot(),
                        vec![Mutation::Put {
                            partition_id,
                            key: key.into_bytes(),
                            row: make_row(iteration as i64),
                        }],
                    )
                    .unwrap();
                writer_engine.flush().unwrap();
                writer_engine
                    .compact_once(CompactionInput {
                        dropped_partition_ids: HashSet::new(),
                        protected_partition_ids: HashSet::new(),
                        explicit_sst_ids: None,
                        gc_horizon,
                    })
                    .unwrap();
            }
        });

        let writer_completed = writer.join().is_ok();
        let readers_completed = readers.into_iter().all(|reader| reader.join().is_ok());

        let result = if writer_completed && readers_completed {
            Ok(supervisor_operations.load(Ordering::Relaxed))
        } else {
            Err("a concurrent reader or writer thread panicked")
        };

        let _ = done_tx.send(result);
    });

    match done_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(operation_count)) => {
            assert!(
                operation_count > 0,
                "concurrent readers completed no successful operations"
            );
        }
        Ok(Err(message)) => panic!("{message}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("concurrent reads, flushes, and compactions timed out, likely due to deadlock")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("concurrent test supervisor exited without reporting completion")
        }
    }

    let error = engine
        .get(0, b"seed", Snapshot::new(Version::new(3)))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            HtapError::InvalidArgument(message)
                if message.contains("below GC low-water")
                    && message.contains(&gc_horizon.to_string())
        ),
        "expected snapshot-below-GC-low-water error for horizon {gc_horizon}, got {error}"
    );
}
