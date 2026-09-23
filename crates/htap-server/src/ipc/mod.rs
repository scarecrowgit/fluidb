//! Unix-domain-socket IPC protocol support.
//!
//! Frames use a four-byte big-endian body length followed by a JSON payload.

mod client;
mod owner;
mod protocol;

pub use client::{IpcClient, IpcConnection};
pub(crate) use owner::start;
pub use owner::IpcListener;
pub(crate) use protocol::read_frame_until;
pub use protocol::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, SessionStatus, WireError,
    IPC_PROTOCOL_VERSION, MAX_FRAME_SIZE,
};
