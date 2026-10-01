use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use htap_common::types::{Row, Value};
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

struct TestServer {
    root: PathBuf,
    server: Arc<LocalServer>,
}

impl TestServer {
    fn open() -> Self {
        let unique_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "htap-server-serializable-history-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        let server = Arc::new(LocalServer::open(&root).expect("server opens"));
        Self { root, server }
    }

    fn session(&self) -> Session {
        self.server.open_session().expect("session opens")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            if error.kind() != std::io::ErrorKind::NotFound {
                panic!(
                    "failed to remove test server directory {}: {error}",
                    self.root.display()
                );
            }
        }
    }
}

fn statement_result(session: &mut Session, sql: &str) -> Result<StatementResult, String> {
    let statement = htap_sql::parse_one(sql).expect("SQL parses");
    session
        .execute_statement(statement)
        .map_err(|error| error.to_string())
}

fn execute(session: &mut Session, sql: &str) {
    statement_result(session, sql).unwrap_or_else(|error| panic!("SQL failed: {sql}: {error}"));
}

fn query_rows(session: &mut Session, sql: &str) -> Vec<Row> {
    match statement_result(session, sql)
        .unwrap_or_else(|error| panic!("SQL failed: {sql}: {error}"))
    {
        StatementResult::Query(query) => query.rows,
        other => panic!("expected query result for {sql}, got {other:?}"),
    }
}

fn is_conflict(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("conflict")
        || lower.contains("serialization failure")
        || lower.contains("read-write dependency")
}

fn rollback_after_conflict(session: &mut Session) {
    let _ = statement_result(session, "ROLLBACK");
}

#[derive(Clone, Copy, Debug)]
enum Isolation {
    RepeatableRead,
    Serializable,
}

impl Isolation {
    fn begin_sql(self) -> &'static str {
        match self {
            Self::RepeatableRead => "START TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            Self::Serializable => "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        }
    }
}

#[derive(Clone, Debug)]
enum PlannedOp {
    PointRead { key: i32 },
    RangeCount { low: i32, high: i32 },
    Upsert { key: i32, value: i32 },
    Delete { key: i32 },
}

#[derive(Clone, Debug)]
enum RecordedOp {
    PointRead { key: i32, rows: Vec<Row> },
    RangeCount { low: i32, high: i32, rows: Vec<Row> },
    Upsert { key: i32, value: i32 },
    Delete { key: i32 },
}

#[derive(Debug)]
struct History {
    seed: u64,
    committed: Vec<Vec<RecordedOp>>,
    final_rows: Vec<Row>,
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next() % len as u64) as usize
    }

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.next() % denominator < numerator
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for end in (1..values.len()).rev() {
            let index = self.index(end + 1);
            values.swap(index, end);
        }
    }
}

fn initial_map() -> BTreeMap<i32, i32> {
    BTreeMap::from([(1, 101), (2, 202), (3, 303), (4, 404)])
}

fn planned_transactions(seed: u64, rng: &mut SplitMix64) -> Vec<Vec<PlannedOp>> {
    let mut keys = [1, 2, 3, 4, 5, 6];
    rng.shuffle(&mut keys);
    let write_keys = [keys[0], keys[1], keys[2]];
    let mut transactions = Vec::new();

    for transaction_id in 0..3 {
        let point_key = write_keys[(transaction_id + 1) % 3];
        let first = 1 + rng.index(6) as i32;
        let second = 1 + rng.index(6) as i32;
        let (low, high) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        let write_key = write_keys[transaction_id];
        let unique_value = 10_000 + (seed as i32 * 10) + transaction_id as i32;
        let write = if rng.chance(3, 4) {
            PlannedOp::Upsert {
                key: write_key,
                value: unique_value,
            }
        } else {
            PlannedOp::Delete { key: write_key }
        };
        let second_read = if rng.chance(1, 3) {
            PlannedOp::RangeCount { low, high }
        } else {
            PlannedOp::PointRead {
                key: keys[3 + rng.index(3)],
            }
        };
        let mut operations = vec![PlannedOp::PointRead { key: point_key }, second_read, write];

        // Most histories use the classic read-before-disjoint-write shape.
        if !rng.chance(4, 5) {
            rng.shuffle(&mut operations);
        }
        transactions.push(operations);
    }

    transactions
}

fn run_operation(session: &mut Session, operation: &PlannedOp) -> Result<RecordedOp, String> {
    match *operation {
        PlannedOp::PointRead { key } => {
            let sql = format!("SELECT value FROM kv WHERE id = {key}");
            match statement_result(session, &sql)? {
                StatementResult::Query(query) => Ok(RecordedOp::PointRead {
                    key,
                    rows: query.rows,
                }),
                other => panic!("expected query result for {sql}, got {other:?}"),
            }
        }
        PlannedOp::RangeCount { low, high } => {
            let sql = format!("SELECT COUNT(*) FROM kv WHERE id BETWEEN {low} AND {high}");
            match statement_result(session, &sql)? {
                StatementResult::Query(query) => Ok(RecordedOp::RangeCount {
                    low,
                    high,
                    rows: query.rows,
                }),
                other => panic!("expected query result for {sql}, got {other:?}"),
            }
        }
        PlannedOp::Upsert { key, value } => {
            let sql = format!("INSERT INTO kv (id, value) VALUES ({key}, {value})");
            statement_result(session, &sql)?;
            Ok(RecordedOp::Upsert { key, value })
        }
        PlannedOp::Delete { key } => {
            let sql = format!("DELETE FROM kv WHERE id = {key}");
            statement_result(session, &sql)?;
            Ok(RecordedOp::Delete { key })
        }
    }
}

fn run_history(seed: u64, isolation: Isolation, columnar: bool) -> History {
    let server = TestServer::open();
    let mut setup = server.session();
    execute(
        &mut setup,
        "CREATE TABLE kv (id INT PRIMARY KEY, value INT NOT NULL)",
    );
    execute(
        &mut setup,
        "INSERT INTO kv (id, value) VALUES \
         (1, 101), (2, 202), (3, 303), (4, 404)",
    );
    if columnar {
        assert!(server
            .server
            .convert_table_to_column("kv")
            .expect("converts")
            .is_success());
    }

    let mut rng = SplitMix64::new(seed);
    let planned = planned_transactions(seed, &mut rng);
    let mut sessions = (0..3).map(|_| server.session()).collect::<Vec<_>>();
    for session in &mut sessions {
        execute(session, isolation.begin_sql());
    }

    let mut schedule = vec![0, 0, 0, 1, 1, 1, 2, 2, 2];
    rng.shuffle(&mut schedule);
    let mut next = [0_usize; 3];
    let mut aborted = [false; 3];
    let mut recorded = [Vec::new(), Vec::new(), Vec::new()];

    for transaction_id in schedule {
        if aborted[transaction_id] {
            continue;
        }
        let operation = &planned[transaction_id][next[transaction_id]];
        next[transaction_id] += 1;
        match run_operation(&mut sessions[transaction_id], operation) {
            Ok(operation) => recorded[transaction_id].push(operation),
            Err(error) if is_conflict(&error) => {
                aborted[transaction_id] = true;
                rollback_after_conflict(&mut sessions[transaction_id]);
            }
            Err(error) => panic!(
                "seed {seed}, transaction {transaction_id}, operation {operation:?}: {error}"
            ),
        }
    }

    let mut commit_order = vec![0, 1, 2];
    rng.shuffle(&mut commit_order);
    let mut committed = Vec::new();
    for transaction_id in commit_order {
        if aborted[transaction_id] {
            continue;
        }
        match statement_result(&mut sessions[transaction_id], "COMMIT") {
            Ok(_) => committed.push(std::mem::take(&mut recorded[transaction_id])),
            Err(error) if is_conflict(&error) => {
                rollback_after_conflict(&mut sessions[transaction_id]);
            }
            Err(error) => panic!("seed {seed}, transaction {transaction_id}, COMMIT: {error}"),
        }
    }

    let mut observer = server.session();
    let final_rows = query_rows(&mut observer, "SELECT id, value FROM kv ORDER BY id");
    History {
        seed,
        committed,
        final_rows,
    }
}

fn transaction_matches(operations: &[RecordedOp], state: &mut BTreeMap<i32, i32>) -> bool {
    for operation in operations {
        match operation {
            RecordedOp::PointRead { key, rows } => {
                let expected = state
                    .get(key)
                    .map(|value| vec![Row::new(vec![Value::Int32(*value)])])
                    .unwrap_or_default();
                if *rows != expected {
                    return false;
                }
            }
            RecordedOp::RangeCount { low, high, rows } => {
                let count = state.range(*low..=*high).count() as i64;
                if *rows != vec![Row::new(vec![Value::Int64(count)])] {
                    return false;
                }
            }
            RecordedOp::Upsert { key, value } => {
                state.insert(*key, *value);
            }
            RecordedOp::Delete { key } => {
                state.remove(key);
            }
        }
    }
    true
}

fn final_rows(state: &BTreeMap<i32, i32>) -> Vec<Row> {
    state
        .iter()
        .map(|(id, value)| Row::new(vec![Value::Int32(*id), Value::Int32(*value)]))
        .collect()
}

fn permutation_matches(history: &History, order: &[usize]) -> bool {
    let mut state = initial_map();
    for &transaction_id in order {
        if !transaction_matches(&history.committed[transaction_id], &mut state) {
            return false;
        }
    }
    final_rows(&state) == history.final_rows
}

fn is_serializable(history: &History) -> bool {
    let count = history.committed.len();
    let mut order = (0..count).collect::<Vec<_>>();
    permutations_match(history, &mut order, 0)
}

fn permutations_match(history: &History, order: &mut [usize], start: usize) -> bool {
    if start == order.len() {
        return permutation_matches(history, order);
    }
    for index in start..order.len() {
        order.swap(start, index);
        if permutations_match(history, order, start + 1) {
            return true;
        }
        order.swap(start, index);
    }
    false
}

#[test]
fn serializable_histories_are_always_serializable() {
    let seed_count = 200;
    let mut total_committed = 0;
    let mut histories_with_multiple_commits = 0;

    for seed in 0..seed_count {
        let history = run_history(seed, Isolation::Serializable, false);
        total_committed += history.committed.len();
        if history.committed.len() >= 2 {
            histories_with_multiple_commits += 1;
        }
        assert!(
            is_serializable(&history),
            "non-serializable history for seed {}: {history:#?}",
            history.seed
        );
    }

    assert!(
        total_committed >= seed_count as usize,
        "expected at least {seed_count} committed transactions across {seed_count} seeds, got {total_committed}; histories with two or more commits: {histories_with_multiple_commits}"
    );
    assert!(
        histories_with_multiple_commits > 0,
        "expected at least one history with two or more committed transactions across {seed_count} seeds; total committed transactions: {total_committed}, histories with two or more commits: {histories_with_multiple_commits}"
    );
}

#[test]
fn snapshot_isolation_produces_non_serializable_histories() {
    for seed in 0..500 {
        let history = run_history(seed, Isolation::RepeatableRead, false);
        if !is_serializable(&history) {
            return;
        }
    }
    panic!("no non-serializable snapshot-isolation history found in seeds 0..500");
}

#[test]
fn serializable_histories_on_columnar_table() {
    let seed_count = 50;
    let mut total_committed = 0;
    let mut histories_with_multiple_commits = 0;

    for seed in 0..seed_count {
        let history = run_history(seed, Isolation::Serializable, true);
        total_committed += history.committed.len();
        if history.committed.len() >= 2 {
            histories_with_multiple_commits += 1;
        }
        assert!(
            is_serializable(&history),
            "non-serializable columnar history for seed {}: {history:#?}",
            history.seed
        );
    }

    assert!(
        total_committed >= seed_count as usize,
        "expected at least {seed_count} committed transactions across {seed_count} columnar seeds, got {total_committed}; histories with two or more commits: {histories_with_multiple_commits}"
    );
    assert!(
        histories_with_multiple_commits > 0,
        "expected at least one columnar history with two or more committed transactions across {seed_count} seeds; total committed transactions: {total_committed}, histories with two or more commits: {histories_with_multiple_commits}"
    );
}
