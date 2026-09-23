#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use htap_common::{HtapError, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

const CHILD_TIMEOUT: Duration = Duration::from_secs(5);
const THREAD_TIMEOUT: Duration = Duration::from_secs(5);

fn child_binary() -> std::path::PathBuf {
    let current = std::env::current_exe().expect("test executable path resolves");
    let target_dir = current
        .parent()
        .and_then(Path::parent)
        .expect("test executable is below target directory");
    let binary = target_dir.join(format!("server_ipc_child{}", std::env::consts::EXE_SUFFIX));

    if binary.exists() {
        return binary;
    }

    let status = Command::new("cargo")
        .args(["build", "-p", "htap-server", "--bin", "server_ipc_child"])
        .status()
        .expect("child binary build starts");
    assert!(status.success(), "child binary builds successfully");
    binary
}

fn spawn_owner(root: &Path) -> Child {
    Command::new(child_binary())
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("IPC owner child starts")
}

fn wait_for_owner(child: &mut Child) {
    let stdout = child.stdout.take().expect("child stdout is captured");
    let (ready, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line);
        let _ = ready.send((result, line));
    });

    let (result, line) = ready_rx
        .recv_timeout(CHILD_TIMEOUT)
        .expect("child reports IPC readiness within timeout");
    assert!(
        result.expect("child readiness line reads") > 0,
        "child stdout closes before readiness"
    );
    assert_eq!(line.trim(), "IPC_OWNER_READY");
}

fn kill_and_reap(child: &mut Child) {
    if child.try_wait().expect("child status checks").is_none() {
        child.kill().expect("child receives kill signal");
    }

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if child.try_wait().expect("child status checks").is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child does not exit within timeout"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn join_with_timeout<T: Send + 'static>(handle: thread::JoinHandle<T>, timeout: Duration) -> T {
    let (done, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(handle.join());
    });
    done_rx
        .recv_timeout(timeout)
        .expect("thread completes within timeout")
        .expect("thread does not panic")
}

#[test]
fn first_opener_owner_second_opener_client_visible_write() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let owner = LocalServer::open(root.path()).expect("first server opens");
    assert!(owner.is_owner());

    owner
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .expect("table creates");

    let client = LocalServer::open(root.path()).expect("second server opens");
    assert!(!client.is_owner());

    client
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect("client insert succeeds");

    let fresh_client = LocalServer::open(root.path()).expect("fresh client opens");
    assert!(!fresh_client.is_owner());
    let result = fresh_client
        .execute("SELECT id, v FROM t WHERE id = 1")
        .expect("fresh client query succeeds");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };
    assert_eq!(query.rows.len(), 1);
    assert_eq!(query.rows[0].values(), &[Value::Int32(1), Value::Int32(99)]);
}

#[test]
fn startup_race() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let mut child = spawn_owner(root.path());

    wait_for_owner(&mut child);

    let client = LocalServer::open(root.path()).expect("client connects after owner startup");
    assert!(!client.is_owner());
    client.execute("SELECT 1").expect("client query succeeds");

    kill_and_reap(&mut child);
}

#[test]
fn kill_signal_mid_connection() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let mut child = spawn_owner(root.path());
    wait_for_owner(&mut child);

    let client = Arc::new(LocalServer::open(root.path()).expect("client opens"));
    assert!(!client.is_owner());

    let (started, started_rx) = mpsc::channel();
    let (finished, finished_rx) = mpsc::channel();
    let request_client = Arc::clone(&client);
    thread::spawn(move || {
        started.send(()).expect("request start reports");
        let result = request_client.execute("SELECT 1");
        let _ = finished.send(result);
    });

    started_rx
        .recv_timeout(THREAD_TIMEOUT)
        .expect("request thread starts");
    thread::sleep(Duration::from_millis(200));
    kill_and_reap(&mut child);

    // This real-process scenario cannot deterministically exercise every branch the fake-listener
    // tests cover; it proves the end-to-end disconnect path returns safely without hanging.
    let result = finished_rx
        .recv_timeout(THREAD_TIMEOUT)
        .expect("in-flight request completes after owner death");
    assert!(
        matches!(
            result,
            Ok(_) | Err(HtapError::Conflict(_)) | Err(HtapError::Ambiguous(_))
        ),
        "in-flight request either completes before owner death or returns a safe disconnect error"
    );
}

#[test]
fn post_kill_takeover() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let mut child = spawn_owner(root.path());
    wait_for_owner(&mut child);

    kill_and_reap(&mut child);

    // This proves lock release and stale-socket cleanup, not recovery correctness.
    let server = LocalServer::open(root.path()).expect("new owner opens after child death");
    assert!(server.is_owner());
}

#[test]
fn eight_simultaneous_openers() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let root = Arc::new(root);
    let barrier = Arc::new(Barrier::new(8));
    let (results, results_rx) = mpsc::channel();
    let mut threads = Vec::new();

    for _ in 0..8 {
        let root = Arc::clone(&root);
        let barrier = Arc::clone(&barrier);
        let results = results.clone();

        threads.push(thread::spawn(move || {
            barrier.wait();
            let result = LocalServer::open(root.path());
            let _ = results.send(result);
        }));
    }
    drop(results);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut servers = Vec::new();
    let mut owner_count = 0;
    let mut client_count = 0;

    for _ in 0..8 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let server = results_rx
            .recv_timeout(remaining)
            .expect("parallel opener completes within total timeout")
            .expect("parallel opener succeeds without error");

        if server.is_owner() {
            owner_count += 1;
        } else {
            client_count += 1;
        }
        servers.push(server);
    }

    assert_eq!(owner_count, 1, "exactly one parallel opener becomes owner");
    assert_eq!(client_count, 7, "all other parallel openers become clients");

    for thread in threads {
        let remaining = deadline.saturating_duration_since(Instant::now());
        join_with_timeout(thread, remaining);
    }

    assert!(
        Instant::now() < deadline,
        "parallel opens exceed bounded total time"
    );

    drop(servers);
}
