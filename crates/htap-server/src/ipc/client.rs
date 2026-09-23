//! Client-side Unix-domain-socket IPC connector.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use htap_common::error::{HtapError, Result};

use super::{
    read_frame, read_frame_until, write_frame, IpcRequest, IpcResponse, ResponsePayload,
    SessionStatus, WireError, IPC_PROTOCOL_VERSION,
};

const SOCKET_FILE_NAME: &str = "htap.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const MAX_BACKOFF: Duration = Duration::from_millis(250);

/// Client-side connector for an owner process serving `root`.
#[derive(Debug, Clone)]
pub struct IpcClient {
    canonical_root: String,
    socket_path: PathBuf,
}

impl IpcClient {
    /// Connects to the owner serving `root` and completes the IPC handshake.
    ///
    /// A missing socket and a refused connection are retried while an owner is starting. All
    /// other connection errors, including permission failures, are returned immediately.
    pub fn connect(root: &Path) -> Result<Self> {
        let canonical_root = root.canonicalize()?;
        let socket_path = canonical_root.join(SOCKET_FILE_NAME);
        let canonical_root_string = canonical_root.to_string_lossy().into_owned();

        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut backoff = INITIAL_BACKOFF;
        let mut last_error: io::Error;

        loop {
            match UnixStream::connect(&socket_path) {
                Ok(mut stream) => {
                    stream.set_read_timeout(Some(remaining_until(deadline)))?;
                    stream.set_write_timeout(Some(remaining_until(deadline)))?;

                    let handshake = IpcRequest::Handshake {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        canonical_root: canonical_root_string.clone(),
                    };
                    if write_frame(&mut stream, &handshake).is_err() {
                        return Err(unreachable_owner_error(&socket_path, None));
                    }

                    let response = match read_frame_until::<IpcResponse>(&mut stream, deadline) {
                        Ok(response) => response,
                        Err(_) => return Err(unreachable_owner_error(&socket_path, None)),
                    };
                    match response.result {
                        Ok(ResponsePayload::Empty) => {
                            stream.set_read_timeout(None)?;
                            stream.set_write_timeout(None)?;
                            return Ok(Self {
                                canonical_root: canonical_root_string,
                                socket_path,
                            });
                        }
                        Ok(_) => {
                            return Err(HtapError::Conflict(
                                "locked but unreachable: IPC handshake returned an invalid response"
                                    .into(),
                            ));
                        }
                        Err(error) => {
                            return Err(HtapError::Conflict(format!(
                                "locked but unreachable: IPC handshake rejected by owner: {}",
                                HtapError::from(error)
                            )));
                        }
                    }
                }
                Err(error) if is_transient_connect_error(&error) => {
                    last_error = error;
                }
                Err(error) => {
                    return Err(unreachable_owner_error(&socket_path, Some(error)));
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(unreachable_owner_error(&socket_path, Some(last_error)));
            }
            thread::sleep(backoff.min(deadline.saturating_duration_since(now)));
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Opens a persistent connection suitable for one client session.
    pub fn open_connection(&self) -> Result<IpcConnection> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|error| unreachable_owner_error(&self.socket_path, Some(error)))?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        stream.set_read_timeout(Some(remaining_until(deadline)))?;
        stream.set_write_timeout(Some(remaining_until(deadline)))?;
        complete_handshake(
            &mut stream,
            &self.canonical_root,
            &self.socket_path,
            deadline,
        )?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(IpcConnection {
            stream,
            dead: false,
        })
    }

    /// Executes one request using a short-lived connection.
    pub fn request(&self, request: IpcRequest) -> Result<(SessionStatus, ResponsePayload)> {
        let mut connection = self.open_connection()?;
        let (status, result) = connection.request(request).map_err(HtapError::from)?;
        result
            .map(|payload| (status, payload))
            .map_err(HtapError::from)
    }

    /// Executes SQL using a short-lived connection.
    pub fn execute(&self, sql: &str) -> Result<(SessionStatus, ResponsePayload)> {
        self.request(IpcRequest::Execute {
            sql: sql.to_string(),
        })
    }

    /// Bootstraps the root account using a short-lived connection.
    pub fn bootstrap_root_account(
        &self,
        password: Option<&str>,
    ) -> Result<(SessionStatus, ResponsePayload)> {
        self.request(IpcRequest::BootstrapRootAccount {
            password: password.map(str::to_owned),
        })
    }
}

/// One persistent IPC connection.
///
/// The caller keeps this value for the lifetime of its corresponding server session.
pub struct IpcConnection {
    stream: UnixStream,
    dead: bool,
}

impl IpcConnection {
    /// Sends one request and returns the owner response.
    ///
    /// Once a non-read-only request has begun writing, a later write or response failure is
    /// ambiguous: the owner may have received and applied it. Catalog snapshots and visibility
    /// checks are statically read-only, so their response failures are safe to retry.
    pub fn request(
        &mut self,
        request: IpcRequest,
    ) -> std::result::Result<
        (
            SessionStatus,
            std::result::Result<ResponsePayload, WireError>,
        ),
        WireError,
    > {
        if self.dead {
            return Err(WireError::OwnerUnreachable(
                "IPC connection is no longer usable after a transport failure".into(),
            ));
        }

        let read_only = matches!(
            request,
            IpcRequest::CatalogSnapshot { .. } | IpcRequest::VisibilityCheck { .. }
        );
        {
            let mut writer = CountingWriter {
                inner: &mut self.stream,
                bytes_written: 0,
            };

            if let Err(error) = write_frame(&mut writer, &request) {
                let bytes_written = writer.bytes_written;
                if bytes_written > 0 {
                    self.dead = true;
                }
                return Err(classify_transport_failure(read_only, bytes_written, error));
            }
        }

        match read_frame::<_, IpcResponse>(&mut self.stream) {
            Ok(response) => Ok((response.status, response.result)),
            Err(error) => {
                self.dead = true;
                Err(classify_transport_failure(read_only, 1, error))
            }
        }
    }
}

struct CountingWriter<'a> {
    inner: &'a mut UnixStream,
    bytes_written: usize,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.inner.write(buffer) {
            Ok(written) => {
                self.bytes_written += written;
                Ok(written)
            }
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn complete_handshake(
    stream: &mut UnixStream,
    canonical_root: &str,
    socket_path: &Path,
    deadline: Instant,
) -> Result<()> {
    let handshake = IpcRequest::Handshake {
        protocol_version: IPC_PROTOCOL_VERSION,
        canonical_root: canonical_root.to_string(),
    };
    write_frame(stream, &handshake).map_err(|_| unreachable_owner_error(socket_path, None))?;
    let response = read_frame_until::<IpcResponse>(stream, deadline)
        .map_err(|_| unreachable_owner_error(socket_path, None))?;
    match response.result {
        Ok(ResponsePayload::Empty) => Ok(()),
        Ok(_) => Err(HtapError::Conflict(
            "locked but unreachable: IPC handshake returned an invalid response".into(),
        )),
        Err(error) => Err(HtapError::Conflict(format!(
            "locked but unreachable: IPC handshake rejected by owner: {}",
            HtapError::from(error)
        ))),
    }
}

fn classify_transport_failure(
    read_only: bool,
    bytes_written: usize,
    error: WireError,
) -> WireError {
    if bytes_written == 0 && matches!(error, WireError::InvalidArgument(_)) {
        return error;
    }

    if read_only || bytes_written == 0 {
        WireError::OwnerUnreachable(format!(
            "IPC request was not completed: {}",
            HtapError::from(error)
        ))
    } else {
        WireError::Ambiguous(format!(
            "IPC request outcome is ambiguous because request bytes may have reached the owner: {}",
            HtapError::from(error)
        ))
    }
}

fn is_transient_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

fn unreachable_owner_error(socket_path: &Path, error: Option<io::Error>) -> HtapError {
    let detail = error.map(|error| format!(": {error}")).unwrap_or_default();
    HtapError::Conflict(format!(
        "locked but unreachable: could not connect to IPC socket {}{}",
        socket_path.display(),
        detail
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::super::MAX_FRAME_SIZE;
    use super::*;

    fn status() -> SessionStatus {
        SessionStatus {
            autocommit: true,
            in_transaction: false,
            principal: super::super::super::Principal::Superuser,
        }
    }

    fn client_root() -> TempDir {
        tempfile::tempdir().expect("temporary root is created")
    }

    fn bind_listener(root: &Path) -> UnixListener {
        UnixListener::bind(root.join(SOCKET_FILE_NAME)).expect("listener binds")
    }

    fn serve_handshake(stream: &mut UnixStream) {
        let _: IpcRequest = read_frame(stream).expect("handshake is received");
        write_frame(
            stream,
            &IpcResponse {
                status: status(),
                result: Ok(ResponsePayload::Empty),
            },
        )
        .expect("handshake response is written");
    }

    #[test]
    fn connect_to_missing_or_gone_socket_returns_conflict() {
        let root = client_root();
        let socket_path = root.path().join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket_path).expect("listener binds");
        drop(listener);

        // A caller can safely report or retry a lock-owner startup failure, not an unknown write.
        let error = IpcClient::connect(root.path()).expect_err("connection must fail");
        assert!(matches!(error, HtapError::Conflict(_)));
    }

    #[test]
    fn connect_handshake_that_never_answers_returns_conflict_within_deadline() {
        let root = client_root();
        let listener = bind_listener(root.path());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client connects");
            let _: IpcRequest = read_frame(&mut stream).expect("handshake is received");

            let mut byte = [0u8; 1];
            let _ = stream.read(&mut byte);
        });

        let started = Instant::now();
        // A caller must not mistake a timed-out owner handshake for a connected owner.
        let error = IpcClient::connect(root.path()).expect_err("handshake must time out");
        assert!(matches!(error, HtapError::Conflict(_)));
        assert!(started.elapsed() <= CONNECT_TIMEOUT + Duration::from_secs(1));

        server.join().expect("server exits after client closes");
    }

    #[test]
    fn fully_written_execute_without_response_is_ambiguous() {
        let root = client_root();
        let listener = bind_listener(root.path());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("connect handshake client connects");
            serve_handshake(&mut stream);

            let (mut stream, _) = listener.accept().expect("session client connects");
            serve_handshake(&mut stream);
            let _: IpcRequest = read_frame(&mut stream).expect("execute request is received");
        });

        let client = IpcClient::connect(root.path()).expect("client connects");
        let mut connection = client.open_connection().expect("session connection opens");

        // Retrying this as harmless could execute the SQL twice after the owner already received it.
        let error = connection
            .request(IpcRequest::Execute {
                sql: "CREATE TABLE t (id INT)".into(),
            })
            .expect_err("closed response must fail");
        assert!(matches!(error, WireError::Ambiguous(_)));

        server.join().expect("server exits");
    }

    #[test]
    fn closed_peer_before_request_write_returns_conflict() {
        let root = client_root();
        let listener = bind_listener(root.path());
        let (closed, closed_receiver) = mpsc::channel();
        let (request_complete, wait_for_request) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("connect handshake client connects");
            serve_handshake(&mut stream);

            let (mut stream, _) = listener.accept().expect("session client connects");
            serve_handshake(&mut stream);
            drop(stream);
            closed.send(()).expect("closure is reported");
            wait_for_request
                .recv_timeout(Duration::from_secs(2))
                .expect("client completes request");
        });

        let client = IpcClient::connect(root.path()).expect("client connects");
        let mut connection = client.open_connection().expect("session connection opens");
        closed_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("peer closes");
        thread::sleep(Duration::from_millis(20));

        connection
            .stream
            .shutdown(std::net::Shutdown::Write)
            .expect("client write half closes");
        // A caller may retry because no request byte was accepted for delivery to the owner.
        let error = connection
            .request(IpcRequest::Execute {
                sql: "SELECT 1".into(),
            })
            .expect_err("write must fail before sending bytes");
        assert!(matches!(error, WireError::OwnerUnreachable(_)));

        request_complete.send(()).expect("server may exit");
        server.join().expect("server exits");
    }

    #[test]
    fn catalog_snapshot_mid_response_returns_conflict() {
        let root = client_root();
        let listener = bind_listener(root.path());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("connect handshake client connects");
            serve_handshake(&mut stream);

            let (mut stream, _) = listener.accept().expect("session client connects");
            serve_handshake(&mut stream);
            let _: IpcRequest = read_frame(&mut stream).expect("snapshot request is received");

            stream
                .write_all(&16u32.to_be_bytes())
                .expect("response prefix is written");
            stream
                .write_all(br#"{"status":"#)
                .expect("partial response is written");
        });

        let client = IpcClient::connect(root.path()).expect("client connects");
        let mut connection = client.open_connection().expect("session connection opens");

        // Catalog snapshots are statically read-only, so retrying cannot duplicate a mutation.
        let error = connection
            .request(IpcRequest::CatalogSnapshot { session_id: 7 })
            .expect_err("partial response must fail");
        assert!(matches!(error, WireError::OwnerUnreachable(_)));

        server.join().expect("server exits");
    }

    #[test]
    fn malformed_or_oversized_response_marks_connection_dead_and_preserves_outcome_classification()
    {
        let root = client_root();
        let listener = bind_listener(root.path());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("connect handshake client connects");
            serve_handshake(&mut stream);

            let (mut stream, _) = listener.accept().expect("execute client connects");
            serve_handshake(&mut stream);
            let _: IpcRequest = read_frame(&mut stream).expect("execute request is received");
            stream
                .write_all(&1u32.to_be_bytes())
                .expect("malformed response length is written");
            stream
                .write_all(b"!")
                .expect("malformed response is written");

            let (mut stream, _) = listener.accept().expect("snapshot client connects");
            serve_handshake(&mut stream);
            let _: IpcRequest = read_frame(&mut stream).expect("snapshot request is received");
            stream
                .write_all(&(MAX_FRAME_SIZE as u32 + 1).to_be_bytes())
                .expect("oversized response length is written");
        });

        let client = IpcClient::connect(root.path()).expect("client connects");

        let mut execute_connection = client.open_connection().expect("execute connection opens");
        let error = execute_connection
            .request(IpcRequest::Execute {
                sql: "CREATE TABLE t (id INT)".into(),
            })
            .expect_err("malformed response fails");
        assert!(matches!(error, WireError::Ambiguous(_)));
        assert!(matches!(
            execute_connection.request(IpcRequest::Execute {
                sql: "SELECT 1".into(),
            }),
            Err(WireError::OwnerUnreachable(_))
        ));
        drop(execute_connection);

        let mut snapshot_connection = client.open_connection().expect("snapshot connection opens");
        let error = snapshot_connection
            .request(IpcRequest::CatalogSnapshot { session_id: 7 })
            .expect_err("oversized response fails");
        assert!(matches!(error, WireError::OwnerUnreachable(_)));
        assert!(matches!(
            snapshot_connection.request(IpcRequest::CatalogSnapshot { session_id: 7 }),
            Err(WireError::OwnerUnreachable(_))
        ));

        server.join().expect("server exits");
    }

    #[test]
    fn partial_request_write_marks_connection_dead() {
        let (stream, _peer) = UnixStream::pair().expect("socket pair is created");
        let mut connection = IpcConnection {
            stream,
            dead: false,
        };
        let bytes_written = 1;
        let error = WireError::Io {
            kind: "BrokenPipe".into(),
        };

        if bytes_written > 0 {
            connection.dead = true;
        }
        let error = classify_transport_failure(false, bytes_written, error);

        assert!(connection.dead);
        assert!(matches!(error, WireError::Ambiguous(_)));
        assert!(matches!(
            connection.request(IpcRequest::Execute {
                sql: "SELECT 1".into(),
            }),
            Err(WireError::OwnerUnreachable(_))
        ));
    }

    #[test]
    fn transport_failure_classification_obeys_write_boundary() {
        let error = WireError::Io {
            kind: "BrokenPipe".into(),
        };

        // Misclassifying an unwritten request as ambiguous would unnecessarily block safe retries.
        assert!(matches!(
            classify_transport_failure(false, 0, error.clone()),
            WireError::OwnerUnreachable(_)
        ));
        // Misclassifying a partially written mutation as retryable could duplicate its effects.
        assert!(matches!(
            classify_transport_failure(false, 1, error.clone()),
            WireError::Ambiguous(_)
        ));
        // Misclassifying a read-only request as ambiguous would prevent a safe retry.
        assert!(matches!(
            classify_transport_failure(true, 0, error.clone()),
            WireError::OwnerUnreachable(_)
        ));
        assert!(matches!(
            classify_transport_failure(true, 1, error),
            WireError::OwnerUnreachable(_)
        ));
    }
}
