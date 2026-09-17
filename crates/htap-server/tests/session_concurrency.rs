//! Cross-cutting concurrency evidence for Phase 10 sessions: real multi-threaded write-write
//! conflicts with no lost updates, and the R5 point-read fast path bypassing analytics both
//! inside and outside an explicit transaction (task 10).

use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::types::{Mutation, Row, Value};
use htap_common::HtapError;
use htap_rowstore::{Engine, EngineOptions};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, Transaction, TransactionManager,
    TransactionRequest,
};
use tempfile::TempDir;

fn as_rows(result: StatementResult) -> Vec<Row> {
    match result {
        StatementResult::Query(q) => q.rows,
        other => panic!("expected Query result, got {other:?}"),
    }
}

/// N threads each run `BEGIN` / read-modify-write the same counter row / `COMMIT`, retrying on
/// `Conflict`, for several iterations. First-writer-wins (Phase 10 task 1) means every
/// concurrent commit either succeeds outright or is cleanly rejected and retried against a
/// fresh snapshot; none is silently lost. The final counter must equal the total number of
/// increments every thread eventually got committed — `threads * iterations` by construction of
/// the retry loop, so this also proves no update is ever lost or double-applied.
#[test]
fn test_concurrent_sessions_write_write_conflict_first_committer_wins() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE counter (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO counter (id, v) VALUES (1, 0);")
        .unwrap();

    const THREADS: usize = 8;
    const ITERATIONS: usize = 5;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    let mut attempts = 0u32;
                    loop {
                        attempts += 1;
                        assert!(
                            attempts < 10_000,
                            "an increment never committed; retry loop is not making progress"
                        );

                        let mut session = server.open_session();
                        session.begin().unwrap();
                        let current = match session.execute("SELECT v FROM counter WHERE id = 1;") {
                            Ok(result) => match as_rows(result)[0].get(0) {
                                Some(Value::Int32(v)) => *v,
                                other => panic!("unexpected value {other:?}"),
                            },
                            Err(HtapError::Conflict(_)) => continue,
                            Err(e) => panic!("unexpected read error: {e}"),
                        };
                        let sql = format!("UPDATE counter SET v = {} WHERE id = 1;", current + 1);
                        if let Err(HtapError::Conflict(_)) = session.execute(&sql) {
                            continue;
                        }
                        match session.commit() {
                            Ok(()) => break,
                            Err(HtapError::Conflict(_)) => continue,
                            Err(e) => panic!("unexpected commit error: {e}"),
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let got = as_rows(
        server
            .execute("SELECT v FROM counter WHERE id = 1;")
            .unwrap(),
    );
    assert_eq!(
        got,
        vec![Row::new(vec![Value::Int32((THREADS * ITERATIONS) as i32)])],
        "every successfully committed increment must be reflected exactly once: no lost updates"
    );
}

/// R5 pin, end to end (not just route classification): a complete-primary-key `SELECT` on a
/// `Column`-storage table never reads columnar storage at all, inside or outside an explicit
/// transaction. Evidence: after converting the table to columnar storage, the on-disk column
/// manifest is corrupted in place, so any read that actually touches columnar storage
/// (`htap_convert::open`) fails with `HtapError::Corruption`. A complete-PK point read still
/// succeeds and returns the correct row despite the corruption; a narrow analytic scan of the
/// same table fails, confirming the corruption is real and the point path's success isn't an
/// accident of some other fallback.
#[test]
fn test_r5_point_read_still_bypasses_analytics_inside_and_outside_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    assert!(server.convert_table_to_column("t").unwrap().is_success());

    // Corrupt the on-disk column manifest for `t`'s single tablet so any read that actually
    // opens it fails.
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cat = cat_store.load().unwrap().unwrap();
    let tablet_id = cat.partitions[0].tablets[0];
    let manifest_path = htap_convert::manifest_path(server.colstore_dir(), tablet_id);
    assert!(manifest_path.is_file());
    std::fs::write(&manifest_path, b"CORRUPTED_GARBAGE_BYTES_MANIFEST").unwrap();

    // Outside a transaction (autocommit): the analytic path fails on the corrupted manifest...
    let analytic_err = server
        .execute("SELECT id, v FROM t WHERE v > 0 ORDER BY id;")
        .unwrap_err();
    assert!(
        matches!(analytic_err, HtapError::Corruption(_)),
        "expected Corruption from the corrupted column manifest, got {analytic_err:?}"
    );
    // ...but a complete-PK point read is unaffected: it never opens the column manifest.
    assert_eq!(
        as_rows(server.execute("SELECT v FROM t WHERE id = 2;").unwrap()),
        vec![Row::new(vec![Value::Int32(20)])]
    );

    // Inside an explicit transaction: same story. The point path still bypasses the corrupted
    // columnar storage entirely, including for a row this same transaction never wrote.
    let mut session = server.open_session();
    session.begin().unwrap();
    let txn_analytic_err = session
        .execute("SELECT id, v FROM t WHERE v > 0 ORDER BY id;")
        .unwrap_err();
    assert!(matches!(txn_analytic_err, HtapError::Corruption(_)));
    // The failed analytic read does not poison the transaction (it isn't a `Conflict`); the
    // point path still works in the same transaction afterwards.
    assert_eq!(
        session
            .execute("SELECT v FROM t WHERE id = 3;")
            .map(as_rows)
            .unwrap(),
        vec![Row::new(vec![Value::Int32(30)])]
    );
    session.rollback().unwrap();
}

/// Storage-reviewer finding F4 (autocommit lost update): `LocalServer::commit_or_buffer`'s
/// `ExecMode::Autocommit` arm must commit an autocommit `UPDATE`/`INSERT`/`DELETE` against the
/// snapshot this statement actually read at (`Transaction::new(.., statement_snapshot.version)` +
/// `TransactionManager::commit`), never `TransactionManager::commit_request` (whose internal
/// `begin()` instead pins `read_version` at the *current* visible version at commit time). Using
/// `commit_request` there is unsafe: a concurrent import — `htap-movement`'s import path commits
/// via `TransactionManager::commit_request` directly and never takes `LocalServer.execution_lock`
/// (see `htap-movement/src/import.rs`), so it really can land between an autocommit statement's
/// own read and its own commit in production — would be silently overwritten by a write computed
/// from stale data, with no conflict ever raised.
///
/// A full reproduction through two real concurrent threads calling `LocalServer::execute`
/// directly is not deterministic: the only interleaving that matters is between this statement's
/// own snapshot read and the `prepare` call inside its own `TransactionManager::commit` (where
/// the first-writer-wins check runs), and `TransactionManager` exposes no hook that fires in that
/// window (`set_commit_append_hook`/`set_commit_sync_hook` both fire *after* `prepare` already
/// ran). This test instead exercises the same two commit shapes `commit_or_buffer` chooses
/// between, directly against `RowstoreParticipant`/`TransactionManager` — exactly the layer
/// `commit_or_buffer` itself sits on — with a real concurrent import committed in between:
///
/// - The pre-fix shape (`TransactionManager::commit_request`) silently succeeds and destroys the
///   import's write: a lost update.
/// - The fixed shape (`Transaction::new(.., statement_snapshot)` + `TransactionManager::commit`,
///   exactly what `commit_or_buffer` does today) is rejected with `Conflict` instead, leaving the
///   import's write intact — first-writer-wins, exactly like an explicit transaction.
#[test]
fn test_autocommit_update_conflicts_with_concurrent_import_instead_of_losing_it() {
    fn make_row(v: i64) -> Row {
        Row::new(vec![Value::Int64(v)])
    }

    fn put_request(participant_id: ParticipantId, key: &[u8], v: i64) -> TransactionRequest {
        let mutations = vec![Mutation::Put {
            partition_id: 0,
            key: key.to_vec(),
            row: make_row(v),
        }];
        let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
        TransactionRequest::new(vec![ParticipantWork::new(participant_id, payload)]).unwrap()
    }

    let journal_dir = tempfile::tempdir().unwrap();
    let rowstore_dir = tempfile::tempdir().unwrap();
    let journal_path = journal_dir.path().join("txn.journal");
    let participant_id = ParticipantId::new(1);

    let manager = TransactionManager::open(&journal_path).unwrap();
    let engine = Arc::new(Engine::open(EngineOptions::new(rowstore_dir.path())).unwrap());
    manager.register_participant(Arc::new(RowstoreParticipant::new(
        participant_id,
        Arc::clone(&engine),
    )));

    // Seed row k=1, v=10.
    manager
        .commit_request(put_request(participant_id, b"k", 10))
        .unwrap();

    // --- Buggy shape: `TransactionManager::commit_request` ---
    //
    // An autocommit `UPDATE`'s statement reads v=10 at snapshot `s0` and computes "v = v + 1"
    // (11) from that stale read. Before it commits, a concurrent import lands, bulk-setting
    // v=20 (representing a real, current value the UPDATE never saw).
    let s0 = manager.visible_version();
    manager
        .commit_request(put_request(participant_id, b"k", 20)) // concurrent import
        .unwrap();

    // The buggy commit path ignores `s0` entirely and commits successfully anyway, silently
    // discarding the import's v=20 in favor of a value computed from the stale v=10 read.
    manager
        .commit_request(put_request(participant_id, b"k", 11))
        .expect("commit_request ignores the statement's own read snapshot: this is the bug");
    assert_eq!(
        engine.get(0, b"k", engine.snapshot()).unwrap(),
        Some(make_row(11)),
        "bug reproduced: the import's v=20 was silently lost"
    );
    let _ = s0; // s0 was never actually used by the buggy path — that's the point.

    // --- Fixed shape: `Transaction::new(.., statement_snapshot)` + `TransactionManager::commit`
    // ---
    //
    // Same story, replayed with the fix `commit_or_buffer` now uses. The statement reads v=11 at
    // snapshot `s1`, computes "v = v + 1" (12) from that read. A concurrent import lands first,
    // bulk-setting v=99.
    let s1 = manager.visible_version();
    manager
        .commit_request(put_request(participant_id, b"k", 99)) // concurrent import
        .unwrap();

    // The fix commits against the transaction's OWN pinned read snapshot `s1`, so
    // `Engine::prepare`'s first-writer-wins check sees the import's newer committed version and
    // rejects it — the stale-based write never lands.
    let mut txn = Transaction::new(manager.next_txn_id().unwrap(), s1);
    txn.set_request(put_request(participant_id, b"k", 12));
    let err = manager.commit(&mut txn).unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
    assert_eq!(
        engine.get(0, b"k", engine.snapshot()).unwrap(),
        Some(make_row(99)),
        "fixed: the import's v=99 stands; the stale-based write was rejected, not silently lost"
    );
}
