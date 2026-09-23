#![cfg(unix)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use htap_server::ipc::{read_frame, IpcResponse, WireError};
use htap_server::LocalServer;

#[test]
fn undecodable_frame_returns_error_then_closes() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    let mut stream = UnixStream::connect(root.path().join("htap.sock")).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout sets");

    let body = b"null";
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .expect("length writes");
    stream.write_all(body).expect("undecodable body writes");
    stream.flush().expect("frame flushes");

    // Closing without a reply would make the client report an unknown outcome for a statement that
    // provably never ran.
    let response: IpcResponse = read_frame(&mut stream).expect("error response");
    assert!(matches!(
        response.result,
        Err(WireError::InvalidArgument(_))
    ));
    assert!(read_frame::<_, IpcResponse>(&mut stream).is_err());

    drop(server);
}
