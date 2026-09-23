//! Owner-side Unix-domain-socket IPC listener.

use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use htap_common::error::Result;

use crate::{OwnedServer, Principal, Session};

use super::{
    read_frame, read_frame_until, write_frame, IpcRequest, IpcResponse, ResponsePayload,
    SessionStatus, WireError, IPC_PROTOCOL_VERSION,
};

const SOCKET_FILE_NAME: &str = "htap.sock";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Background IPC listener owned by an [`OwnedServer`].
pub struct IpcListener {
    socket_path: PathBuf,
    socket_directory: PathBuf,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    connection_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    live_connections: Arc<Mutex<HashMap<u64, UnixStream>>>,
}

impl IpcListener {
    /// Returns the well-known socket path used by this listener.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }

        let streams: Vec<UnixStream> = std::mem::take(&mut *self.live_connections.lock())
            .into_values()
            .collect();
        for stream in streams {
            let _ = stream.shutdown(Shutdown::Both);
        }

        let handles = std::mem::take(&mut *self.connection_threads.lock());
        for handle in handles {
            let _ = handle.join();
        }

        match std::fs::symlink_metadata(&self.socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                let _ = std::fs::remove_file(&self.socket_path);
            }
            _ => {}
        }

        let _ = std::fs::remove_dir(&self.socket_directory);
    }
}

/// Starts the owner-side IPC listener.
///
/// A bind failure is non-fatal because the owner lock remains authoritative for exclusive root
/// ownership. In that case this returns `Ok(None)` and the server continues in lock-only mode.
pub(crate) fn start(
    root: &Path,
    server: Arc<OwnedServer>,
    max_connections: usize,
) -> Result<Option<IpcListener>> {
    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "failed to canonicalize IPC root {}: {error}; continuing in lock-only mode",
                root.display()
            );
            return Ok(None);
        }
    };
    cleanup_stale_private_directories(&canonical_root);
    let socket_path = canonical_root.join(SOCKET_FILE_NAME);

    match std::fs::symlink_metadata(&socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if let Err(error) = std::fs::remove_file(&socket_path) {
                eprintln!(
                    "failed to remove stale IPC socket {}: {error}; continuing in lock-only mode",
                    socket_path.display()
                );
                return Ok(None);
            }
        }
        Ok(_) => {
            eprintln!(
                "IPC socket path {} exists but is not a socket; continuing in lock-only mode",
                socket_path.display()
            );
            return Ok(None);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let socket_directory = loop {
        let directory = canonical_root.join(format!(
            ".htap-ipc-{}-{}",
            std::process::id(),
            next_private_directory_id()
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => break directory,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                eprintln!(
                    "failed to create private IPC directory {}: {error}; continuing in lock-only mode",
                    directory.display()
                );
                return Ok(None);
            }
        }
    };
    let private_socket_path = socket_directory.join(SOCKET_FILE_NAME);

    let listener = match UnixListener::bind(&private_socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = std::fs::remove_dir(&socket_directory);
            eprintln!(
                "failed to bind private IPC socket {}: {error}; continuing in lock-only mode",
                private_socket_path.display()
            );
            return Ok(None);
        }
    };

    if let Err(error) =
        std::fs::set_permissions(&private_socket_path, std::fs::Permissions::from_mode(0o600))
    {
        let _ = std::fs::remove_file(&private_socket_path);
        let _ = std::fs::remove_dir(&socket_directory);
        eprintln!(
            "failed to secure IPC socket {}: {error}; continuing in lock-only mode",
            private_socket_path.display()
        );
        return Ok(None);
    }

    if let Err(error) = std::fs::rename(&private_socket_path, &socket_path) {
        let _ = std::fs::remove_file(&private_socket_path);
        let _ = std::fs::remove_dir(&socket_directory);
        eprintln!(
            "failed to publish IPC socket {}: {error}; continuing in lock-only mode",
            socket_path.display()
        );
        return Ok(None);
    }

    if let Err(error) = std::fs::remove_dir(&socket_directory) {
        eprintln!(
            "failed to remove private IPC directory {}: {error}; will retry during shutdown",
            socket_directory.display()
        );
    }

    if let Err(error) = listener.set_nonblocking(true) {
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&socket_directory);
        eprintln!(
            "failed to configure IPC socket {} as non-blocking: {error}; continuing in lock-only mode",
            socket_path.display()
        );
        return Ok(None);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let connection_threads = Arc::new(Mutex::new(Vec::new()));
    let live_connections = Arc::new(Mutex::new(HashMap::new()));
    let connection_count = Arc::new(AtomicUsize::new(0));
    let next_connection_id = Arc::new(AtomicU64::new(1));

    let accept_stop = Arc::clone(&stop);
    let accept_threads = Arc::clone(&connection_threads);
    let accept_connections = Arc::clone(&live_connections);
    let accept_count = Arc::clone(&connection_count);
    let accept_next_id = Arc::clone(&next_connection_id);
    let accept_thread =
        match thread::Builder::new()
            .name("htap-ipc-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    server,
                    canonical_root,
                    max_connections.max(1),
                    accept_stop,
                    accept_threads,
                    accept_connections,
                    accept_count,
                    accept_next_id,
                );
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = std::fs::remove_file(&socket_path);
                let _ = std::fs::remove_dir(&socket_directory);
                eprintln!(
                "failed to start IPC accept thread for {}: {error}; continuing in lock-only mode",
                socket_path.display()
            );
                return Ok(None);
            }
        };

    Ok(Some(IpcListener {
        socket_path,
        socket_directory,
        stop,
        accept_thread: Some(accept_thread),
        connection_threads,
        live_connections,
    }))
}

fn cleanup_stale_private_directories(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!(
                "failed to scan IPC root {} for stale private directories: {error}",
                root.display()
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(".htap-ipc-") else {
            continue;
        };
        let Some((pid, id)) = suffix.split_once('-') else {
            continue;
        };
        if pid.parse::<u32>().is_err()
            || id.is_empty()
            || id.contains('-')
            || id.parse::<u64>().is_err()
        {
            continue;
        }

        let directory = entry.path();
        let metadata = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.file_type().is_dir() {
            continue;
        }

        let socket_path = directory.join(SOCKET_FILE_NAME);
        match std::fs::symlink_metadata(&socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                let _ = std::fs::remove_file(&socket_path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => continue,
        }
        let _ = std::fs::remove_dir(&directory);
    }
}

fn next_private_directory_id() -> u64 {
    static NEXT_PRIVATE_DIRECTORY_ID: AtomicU64 = AtomicU64::new(1);

    NEXT_PRIVATE_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
}

#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listener: UnixListener,
    server: Arc<OwnedServer>,
    canonical_root: PathBuf,
    max_connections: usize,
    stop: Arc<AtomicBool>,
    connection_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    live_connections: Arc<Mutex<HashMap<u64, UnixStream>>>,
    connection_count: Arc<AtomicUsize>,
    next_connection_id: Arc<AtomicU64>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                connection_threads
                    .lock()
                    .retain(|handle| !handle.is_finished());

                if connection_count.load(Ordering::SeqCst) >= max_connections {
                    drop(stream);
                    continue;
                }

                let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
                let registered_stream = match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        eprintln!(
                            "failed to clone IPC stream for connection {connection_id}: {error}"
                        );
                        continue;
                    }
                };

                {
                    let mut connections = live_connections.lock();
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    connections.insert(connection_id, registered_stream);
                }
                connection_count.fetch_add(1, Ordering::SeqCst);

                let handler_server = Arc::clone(&server);
                let handler_root = canonical_root.clone();
                let handler_stop = Arc::clone(&stop);
                let handler_connections = Arc::clone(&live_connections);
                let handler_count = Arc::clone(&connection_count);
                match thread::Builder::new()
                    .name(format!("htap-ipc-connection-{connection_id}"))
                    .spawn(move || {
                        let _guard = ConnectionGuard {
                            connection_id,
                            live_connections: handler_connections,
                            connection_count: handler_count,
                        };
                        handle_connection(stream, handler_server, handler_root, handler_stop);
                    }) {
                    Ok(handle) => connection_threads.lock().push(handle),
                    Err(error) => {
                        connection_count.fetch_sub(1, Ordering::SeqCst);
                        live_connections.lock().remove(&connection_id);
                        eprintln!(
                            "failed to spawn IPC handler for connection {connection_id}: {error}"
                        );
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!("IPC accept failed: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

struct ConnectionGuard {
    connection_id: u64,
    live_connections: Arc<Mutex<HashMap<u64, UnixStream>>>,
    connection_count: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.live_connections.lock().remove(&self.connection_id);
        self.connection_count.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(
    mut stream: UnixStream,
    server: Arc<OwnedServer>,
    canonical_root: PathBuf,
    stop: Arc<AtomicBool>,
) {
    // Only the initial handshake is bounded; established sessions may remain idle indefinitely.
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let request = match read_frame_until::<IpcRequest>(&mut stream, handshake_deadline) {
        Ok(request) => request,
        Err(WireError::InvalidArgument(message)) => {
            let _ = write_response(
                &mut stream,
                response_status(None),
                Err(WireError::InvalidArgument(message)),
                false,
            );
            return;
        }
        Err(_) => return,
    };

    let IpcRequest::Handshake {
        protocol_version,
        canonical_root: requested_root,
    } = request
    else {
        let _ = write_response(
            &mut stream,
            response_status(None),
            Err(WireError::InvalidArgument(
                "IPC handshake must be the first request".into(),
            )),
            false,
        );
        return;
    };

    if protocol_version != IPC_PROTOCOL_VERSION {
        let _ = write_response(
            &mut stream,
            response_status(None),
            Err(WireError::InvalidArgument(format!(
                "IPC protocol version mismatch: expected {IPC_PROTOCOL_VERSION}, got {protocol_version}"
            ))),
            false,
        );
        return;
    }
    if requested_root != canonical_root.to_string_lossy() {
        let _ = write_response(
            &mut stream,
            response_status(None),
            Err(WireError::InvalidArgument(
                "IPC canonical root does not match server root".into(),
            )),
            false,
        );
        return;
    }

    if write_response(
        &mut stream,
        response_status(None),
        Ok(ResponsePayload::Empty),
        false,
    )
    .is_err()
    {
        return;
    }

    if stream.set_read_timeout(None).is_err() {
        return;
    }

    let mut session = server.open_session();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let request = match read_frame::<_, IpcRequest>(&mut stream) {
            Ok(request) => request,
            Err(WireError::InvalidArgument(message)) => {
                let _ = write_response(
                    &mut stream,
                    response_status(Some(&session)),
                    Err(WireError::InvalidArgument(message)),
                    false,
                );
                break;
            }
            Err(_) => break,
        };
        let result = dispatch(&server, &mut session, request);
        if write_response(&mut stream, response_status(Some(&session)), result, true).is_err() {
            break;
        }
    }

    let _ = session.rollback();
}

fn dispatch(
    server: &OwnedServer,
    session: &mut Session,
    request: IpcRequest,
) -> std::result::Result<ResponsePayload, WireError> {
    let check_session = |session_id| {
        if session.id().get() == session_id {
            Ok(())
        } else {
            Err(WireError::InvalidArgument(format!(
                "unknown IPC session {session_id}"
            )))
        }
    };

    match request {
        IpcRequest::Handshake { .. } => Err(WireError::InvalidArgument(
            "IPC handshake has already completed".into(),
        )),
        IpcRequest::OpenSession => Ok(ResponsePayload::SessionId(session.id().get())),
        IpcRequest::Execute { sql } => session
            .execute(&sql)
            .map(ResponsePayload::StatementResult)
            .map_err(WireError::from),
        IpcRequest::ExecuteBound { statement } => session
            .execute_statement(statement)
            .map(ResponsePayload::StatementResult)
            .map_err(WireError::from),
        IpcRequest::AuthenticateSession {
            session_id,
            username,
            scramble,
            auth_response,
        } => {
            check_session(session_id)?;
            session
                .change_user(&username, &scramble, &auth_response)
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::Begin { session_id } => {
            check_session(session_id)?;
            session
                .begin()
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::VisibilityCheck {
            session_id,
            statement,
        } => {
            check_session(session_id)?;
            session
                .check_statement_visible(&statement)
                .map(ResponsePayload::CatalogSnapshot)
                .map_err(WireError::from)
        }
        IpcRequest::CatalogSnapshot { session_id } => {
            check_session(session_id)?;
            session
                .catalog_snapshot()
                .map(ResponsePayload::CatalogSnapshot)
                .map_err(WireError::from)
        }
        IpcRequest::SetMaxAllowedPacket {
            session_id,
            max_allowed_packet,
        } => {
            check_session(session_id)?;
            session
                .set_max_allowed_packet(max_allowed_packet)
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::ChangeUser {
            session_id,
            username,
            scramble,
            auth_response,
        } => {
            check_session(session_id)?;
            session
                .change_user(&username, &scramble, &auth_response)
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::Commit { session_id } => {
            check_session(session_id)?;
            session
                .commit()
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::Rollback { session_id } => {
            check_session(session_id)?;
            session
                .rollback()
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::Reset { session_id } => {
            check_session(session_id)?;
            session
                .reset()
                .map(|()| ResponsePayload::Empty)
                .map_err(WireError::from)
        }
        IpcRequest::BootstrapRootAccount { password } => server
            .bootstrap_root_account(password.as_deref())
            .map(ResponsePayload::BootstrapReport)
            .map_err(WireError::from),
    }
}

fn response_status(session: Option<&Session>) -> SessionStatus {
    match session {
        Some(session) => SessionStatus {
            autocommit: session.autocommit(),
            in_transaction: session.in_transaction(),
            principal: session.principal().clone(),
        },
        None => SessionStatus {
            autocommit: true,
            in_transaction: false,
            principal: Principal::Superuser,
        },
    }
}

fn write_response(
    stream: &mut UnixStream,
    status: SessionStatus,
    result: std::result::Result<ResponsePayload, WireError>,
    dispatched: bool,
) -> std::result::Result<(), WireError> {
    let response = IpcResponse {
        status: status.clone(),
        result,
    };
    match write_frame(stream, &response) {
        Ok(()) => Ok(()),
        Err(WireError::InvalidArgument(message)) => {
            let error = if dispatched {
                WireError::Ambiguous(format!(
                    "IPC request completed, but its response could not be framed: {message}"
                ))
            } else {
                WireError::InvalidArgument(message)
            };
            let fallback = IpcResponse {
                status,
                result: Err(error),
            };
            write_frame(stream, &fallback)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use htap_common::types::{Row, Value};
    use htap_sql::result::StatementResult;

    use super::*;

    #[test]
    fn post_dispatch_serialization_failure_returns_ambiguous_outcome() {
        let (mut owner, mut client) = UnixStream::pair().expect("socket pair is created");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout is set");

        write_response(
            &mut owner,
            SessionStatus {
                autocommit: true,
                in_transaction: false,
                principal: Principal::Superuser,
            },
            Ok(ResponsePayload::StatementResult(StatementResult::query(
                Vec::new(),
                vec![Row::new(vec![Value::Float64(f64::NAN)])],
            ))),
            true,
        )
        .expect("fallback response is written");

        let decoded: IpcResponse = read_frame(&mut client).expect("fallback response is decoded");
        assert!(matches!(decoded.result, Err(WireError::Ambiguous(_))));
    }
}
