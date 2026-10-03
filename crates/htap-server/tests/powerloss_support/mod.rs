#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_common::fs::Op;
use htap_common::Value;
use htap_crashsim::{CrashInfo, CrashPolicy, WorkloadContext};
use htap_server::LocalServer;
use htap_sql::StatementResult;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableState {
    Absent,
    Rows(BTreeMap<i64, String>),
}

#[derive(Clone, Copy, Debug)]
pub enum Step {
    Create,
    Insert12,
    Update1,
    Delete2,
    Transaction34,
    Restart,
    Insert5,
}

impl Step {
    fn apply(self, state: &mut TableState) {
        match self {
            Self::Create => {
                assert_eq!(*state, TableState::Absent);
                *state = TableState::Rows(BTreeMap::new());
            }
            Self::Insert12 => {
                let rows = rows_mut(state);
                assert_eq!(rows.insert(1, "a".to_owned()), None);
                assert_eq!(rows.insert(2, "b".to_owned()), None);
            }
            Self::Update1 => {
                let rows = rows_mut(state);
                assert_eq!(rows.insert(1, "a2".to_owned()), Some("a".to_owned()));
            }
            Self::Delete2 => {
                let rows = rows_mut(state);
                assert_eq!(rows.remove(&2), Some("b".to_owned()));
            }
            Self::Transaction34 => {
                let rows = rows_mut(state);
                assert_eq!(rows.insert(3, "c".to_owned()), None);
                assert_eq!(rows.insert(4, "d".to_owned()), None);
            }
            Self::Restart => {}
            Self::Insert5 => {
                let rows = rows_mut(state);
                assert_eq!(rows.insert(5, "e".to_owned()), None);
            }
        }
    }
}

fn rows_mut(state: &mut TableState) -> &mut BTreeMap<i64, String> {
    match state {
        TableState::Rows(rows) => rows,
        TableState::Absent => panic!("model step requires table t to exist"),
    }
}

pub fn history(steps: &[Step]) -> Vec<TableState> {
    let mut states = vec![TableState::Absent];
    let mut state = TableState::Absent;

    for step in steps {
        step.apply(&mut state);
        states.push(state.clone());
    }

    states
}

pub fn open_server(root: &Path) -> LocalServer {
    LocalServer::open(root)
        .map(|server| server.with_scan_workers(1).with_query_parallelism(1))
        .unwrap_or_else(|error| {
            panic!(
                "LocalServer::open({}) failed; Corruption/open error: {error:?}",
                root.display()
            )
        })
}

pub fn read_table_state(server: &LocalServer) -> TableState {
    let tables = query_rows(
        server.execute("SHOW TABLES").unwrap_or_else(|error| {
            panic!("SHOW TABLES failed; Corruption/query error: {error:?}")
        }),
        "SHOW TABLES",
    );

    let exists = tables.iter().any(|row| {
        matches!(
            row.values(),
            [Value::String(name)] if name.as_str() == "t"
        )
    });
    if !exists {
        return TableState::Absent;
    }

    let result = server
        .execute("SELECT id, v FROM t ORDER BY id")
        .unwrap_or_else(|error| {
            panic!("SELECT id, v FROM t failed; Corruption/query error: {error:?}")
        });
    let rows = query_rows(result, "SELECT id, v FROM t ORDER BY id");
    let mut normalized = BTreeMap::new();

    for row in rows {
        match row.values() {
            [Value::Int64(id), Value::String(value)] => {
                assert!(
                    normalized.insert(*id, value.clone()).is_none(),
                    "recovered table contains duplicate primary key {id}"
                );
            }
            values => panic!("unexpected recovered row values: {values:?}"),
        }
    }

    TableState::Rows(normalized)
}

fn query_rows(result: StatementResult, sql: &str) -> Vec<htap_common::Row> {
    match result {
        StatementResult::Query(query) => query.rows,
        other => panic!("{sql} returned a non-query result: {other:?}"),
    }
}

pub fn reopen_twice(path: &Path) -> TableState {
    let first = {
        let server = open_server(path);
        read_table_state(&server)
    };
    let second = {
        let server = open_server(path);
        read_table_state(&server)
    };

    assert_eq!(
        second, first,
        "recovery drifted between consecutive LocalServer opens"
    );
    first
}

pub fn issue<T, E>(
    workload: &WorkloadContext,
    index: usize,
    call: impl FnOnce() -> Result<T, E>,
) -> T
where
    E: std::fmt::Debug,
{
    workload.ack(format!("issue-{index}"));
    let value = call().unwrap_or_else(|error| {
        panic!("workload step {index} failed; Corruption/workload error: {error:?}")
    });
    workload.ack(format!("done-{index}"));
    value
}

pub fn execute_step(
    workload: &WorkloadContext,
    server: &mut Option<Arc<LocalServer>>,
    index: usize,
    step: Step,
) {
    match step {
        Step::Create => {
            issue(workload, index, || {
                current(server).execute("CREATE TABLE t(id BIGINT PRIMARY KEY, v VARCHAR)")
            });
        }
        Step::Insert12 => {
            issue(workload, index, || {
                current(server).execute("INSERT INTO t(id, v) VALUES (1, 'a'), (2, 'b')")
            });
        }
        Step::Update1 => {
            issue(workload, index, || {
                current(server).execute("UPDATE t SET v = 'a2' WHERE id = 1")
            });
        }
        Step::Delete2 => {
            issue(workload, index, || {
                current(server).execute("DELETE FROM t WHERE id = 2")
            });
        }
        Step::Transaction34 => {
            issue(workload, index, || {
                let server = Arc::clone(current(server));
                let mut session = server.open_session()?;
                session.begin()?;
                session.execute("INSERT INTO t(id, v) VALUES (3, 'c')")?;
                session.execute("INSERT INTO t(id, v) VALUES (4, 'd')")?;
                session.commit()
            });
        }
        Step::Restart => {
            workload.ack(format!("issue-{index}"));
            drop(server.take());
            *server = Some(Arc::new(open_server(workload.root())));
            workload.ack(format!("done-{index}"));
        }
        Step::Insert5 => {
            issue(workload, index, || {
                current(server).execute("INSERT INTO t(id, v) VALUES (5, 'e')")
            });
        }
    }
}

fn current(server: &Option<Arc<LocalServer>>) -> &Arc<LocalServer> {
    server
        .as_ref()
        .expect("recorded workload has no open LocalServer")
}

pub fn issued_and_done(info: &CrashInfo, step_count: usize) -> (usize, usize) {
    let labels: BTreeSet<_> = info.acked_labels.iter().map(String::as_str).collect();

    let issue = contiguous_prefix(&labels, "issue", step_count);
    let done = contiguous_prefix(&labels, "done", step_count);

    assert_eq!(
        labels.len(),
        issue + done,
        "image contains a non-contiguous or unknown acknowledgement label: {:?}",
        info.acked_labels
    );
    assert!(
        done <= issue,
        "done prefix {done} exceeds issue prefix {issue}"
    );
    assert!(
        issue <= done + 1,
        "serial workload has more than one issued-but-incomplete step: issue={issue}, done={done}"
    );

    (issue, done)
}

fn contiguous_prefix(labels: &BTreeSet<&str>, prefix: &str, step_count: usize) -> usize {
    let count = (0..step_count)
        .take_while(|index| labels.contains(format!("{prefix}-{index}").as_str()))
        .count();

    for index in count..step_count {
        assert!(
            !labels.contains(format!("{prefix}-{index}").as_str()),
            "{prefix} acknowledgement labels are not a contiguous prefix"
        );
    }

    count
}

pub fn check_image(recovered: &TableState, info: &CrashInfo, history: &[TableState]) -> ImageMatch {
    let step_count = history
        .len()
        .checked_sub(1)
        .expect("history must contain its initial state");
    let (issue, done) = issued_and_done(info, step_count);

    let matched = (done..=issue)
        .find(|index| history[*index] == *recovered)
        .unwrap_or_else(|| {
            panic!(
                "recovered state is outside the allowed prefix window [{done}, {issue}]: \
                 recovered={recovered:?}"
            )
        });

    ImageMatch {
        issue,
        done,
        matched,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ImageMatch {
    pub issue: usize,
    pub done: usize,
    pub matched: usize,
}

#[derive(Default)]
pub struct Flags {
    images: usize,
    saw_done_all: bool,
    saw_strictly_behind_issue: bool,
    history_indices: BTreeSet<usize>,
    torn_images: usize,
    strict_trees: BTreeSet<u64>,
    differing_torn_trees: usize,
}

impl Flags {
    pub fn observe(
        &mut self,
        tree_hash: u64,
        info: &CrashInfo,
        matched: ImageMatch,
        step_count: usize,
    ) {
        self.images += 1;
        self.saw_done_all |= matched.done == step_count;
        self.saw_strictly_behind_issue |= matched.matched < matched.issue;
        self.history_indices.insert(matched.matched);

        match info.policy {
            CrashPolicy::Strict => {
                assert_eq!(
                    self.torn_images, 0,
                    "Strict image observed after Torn enumeration began"
                );
                self.strict_trees.insert(tree_hash);
            }
            CrashPolicy::Torn { .. } => {
                assert!(
                    !self.strict_trees.is_empty(),
                    "Torn image observed before any Strict image"
                );
                self.torn_images += 1;
                if !self.strict_trees.contains(&tree_hash) {
                    self.differing_torn_trees += 1;
                }
            }
            CrashPolicy::Chaos { .. } => panic!("server suite must not use Chaos"),
        }
    }

    pub fn assert_complete(&self) {
        assert!(
            self.images > 50,
            "expected more than 50 crash images, checked {}",
            self.images
        );
        assert!(
            self.saw_done_all,
            "no image acknowledged every workload step"
        );
        assert!(
            self.saw_strictly_behind_issue,
            "no image exercised a real crash window behind its issued step"
        );
        assert!(
            self.history_indices.len() >= 4,
            "expected at least four distinct recovered history indices, saw {:?}",
            self.history_indices
        );
        assert!(self.torn_images > 0, "no Torn images were checked");
        // Measured even under exhaustive runs: this workload's Torn trees all match Strict
        // trees. Per-crate suites own Torn coverage for individual files.
        eprintln!(
            "POWERLOSS_TORN_DISTINCT={} of {}",
            self.differing_torn_trees, self.torn_images
        );
    }
}

pub fn tree_hash(root: &Path) -> u64 {
    fn hash_directory(path: &Path, relative: &Path, hasher: &mut impl Hasher) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
            .map(|entry| entry.unwrap())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            if relative.as_os_str().is_empty() {
                let name = name.to_string_lossy();
                if name == "LOCK"
                    || name == "htap.sock"
                    || name == "spill"
                    || name.starts_with(".htap-ipc-")
                {
                    continue;
                }
            }

            let relative = relative.join(&name);
            relative.hash(hasher);

            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                0_u8.hash(hasher);
                hash_directory(&entry.path(), &relative, hasher);
            } else {
                1_u8.hash(hasher);
                fs::read(entry.path()).unwrap().hash(hasher);
            }
        }
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_directory(root, Path::new(""), &mut hasher);
    hasher.finish()
}

#[allow(dead_code)]
pub fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

#[allow(dead_code)]
pub fn op_is_torn(policy: &CrashPolicy) -> bool {
    matches!(policy, CrashPolicy::Torn { .. })
}

#[allow(dead_code)]
pub fn acked(info: &CrashInfo, label: &str) -> bool {
    info.acked_labels.iter().any(|value| value == label)
}

#[allow(dead_code)]
pub fn contains_ack(ops: &[Op], expected: &str) -> bool {
    ops.iter()
        .any(|op| matches!(op, Op::Ack { label } if label == expected))
}
