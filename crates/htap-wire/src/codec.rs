//! Packet framing and length-encoded primitives of the MySQL protocol.
//!
//! A packet is a 3-byte little-endian payload length, a 1-byte sequence id, and the payload.
//! Payloads of `0xFF_FFFF` bytes signal a multi-packet continuation, which this MVP does not
//! implement: such packets are rejected with an error and the caller must close the connection.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum payload length carried by a single packet.
pub const MAX_PACKET_PAYLOAD: usize = 0xFF_FFFF;

/// Packet sequence id counter.
///
/// Sequence ids start at 0 for every new client command and increment (wrapping) for every
/// packet exchanged while processing that command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqCounter(u8);

impl SeqCounter {
    /// Creates a counter starting at 0.
    pub fn new() -> Self {
        Self(0)
    }

    /// Returns the current sequence id and advances the counter.
    pub fn advance(&mut self) -> u8 {
        let v = self.0;
        self.0 = self.0.wrapping_add(1);
        v
    }

    /// Resets the counter to 0 (start of a new command).
    pub fn reset(&mut self) {
        self.0 = 0;
    }

    /// Sets the counter to the value that follows a received packet's sequence id.
    pub fn continue_after(&mut self, received_seq: u8) {
        self.0 = received_seq.wrapping_add(1);
    }
}

/// Error kind used to signal a clean shutdown request observed at a packet boundary.
pub const SHUTDOWN_ERROR_KIND: io::ErrorKind = io::ErrorKind::Interrupted;

/// Reads exactly `buf.len()` bytes.
///
/// When `stop` is provided, a read timeout (`WouldBlock` / `TimedOut`) is used as an opportunity
/// to observe the stop flag, but only while no byte of `buf` has been received yet. Once a
/// partial read has happened the function keeps reading until the buffer is complete, so a
/// packet is never torn by a shutdown request.
pub fn read_fully_or_stop<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed by peer",
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if filled == 0 {
                    if let Some(flag) = stop {
                        if flag.load(Ordering::SeqCst) {
                            return Err(io::Error::new(SHUTDOWN_ERROR_KIND, "server shutdown"));
                        }
                    }
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads one packet, returning its sequence id and payload.
///
/// See [`read_fully_or_stop`] for the meaning of `stop`.
pub fn read_packet_with_stop<R: Read>(
    reader: &mut R,
    stop: Option<&AtomicBool>,
) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    read_fully_or_stop(reader, &mut header, stop)?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let seq = header[3];
    if len >= MAX_PACKET_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "multi-packet payloads (>= 16MB) are not supported",
        ));
    }
    let mut payload = vec![0u8; len];
    read_fully_or_stop(reader, &mut payload, None)?;
    Ok((seq, payload))
}

/// Reads one packet without shutdown observation.
pub fn read_packet<R: Read>(reader: &mut R) -> io::Result<(u8, Vec<u8>)> {
    read_packet_with_stop(reader, None)
}

/// Writes one packet with the given sequence id.
pub fn write_packet<W: Write>(writer: &mut W, seq: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() >= MAX_PACKET_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large for a single packet (>= 16MB)",
        ));
    }
    let len = payload.len() as u32;
    let header = [
        (len & 0xff) as u8,
        ((len >> 8) & 0xff) as u8,
        ((len >> 16) & 0xff) as u8,
        seq,
    ];
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Appends a length-encoded integer.
pub fn write_lenenc_int(buf: &mut Vec<u8>, v: u64) {
    if v < 251 {
        buf.push(v as u8);
    } else if v < (1 << 16) {
        buf.push(0xfc);
        buf.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v < (1 << 24) {
        buf.push(0xfd);
        buf.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

fn short(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("truncated packet while reading {what}"),
    )
}

/// Reads a length-encoded integer at `*pos`, advancing it.
pub fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let first = *buf.get(*pos).ok_or_else(|| short("lenenc int"))?;
    *pos += 1;
    match first {
        0..=250 => Ok(first as u64),
        0xfc => {
            let b = buf
                .get(*pos..*pos + 2)
                .ok_or_else(|| short("lenenc int (2)"))?;
            *pos += 2;
            Ok(u16::from_le_bytes([b[0], b[1]]) as u64)
        }
        0xfd => {
            let b = buf
                .get(*pos..*pos + 3)
                .ok_or_else(|| short("lenenc int (3)"))?;
            *pos += 3;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64)
        }
        0xfe => {
            let b = buf
                .get(*pos..*pos + 8)
                .ok_or_else(|| short("lenenc int (8)"))?;
            *pos += 8;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(b);
            Ok(u64::from_le_bytes(arr))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid lenenc int prefix 0x{first:02x}"),
        )),
    }
}

/// Appends a length-encoded byte string.
pub fn write_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

/// Reads a length-encoded byte string at `*pos`, advancing it.
pub fn read_lenenc_str<'a>(buf: &'a [u8], pos: &mut usize) -> io::Result<&'a [u8]> {
    let len = read_lenenc_int(buf, pos)? as usize;
    let s = buf
        .get(*pos..*pos + len)
        .ok_or_else(|| short("lenenc string"))?;
    *pos += len;
    Ok(s)
}

/// Appends a NUL-terminated string.
pub fn write_null_terminated(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(s);
    buf.push(0);
}

/// Reads a NUL-terminated string at `*pos`, advancing past the terminator.
pub fn read_null_terminated<'a>(buf: &'a [u8], pos: &mut usize) -> io::Result<&'a [u8]> {
    let rest = buf.get(*pos..).ok_or_else(|| short("nul string"))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| short("nul terminator"))?;
    let s = &rest[..end];
    *pos += end + 1;
    Ok(s)
}

/// Reads a fixed number of bytes at `*pos`, advancing it.
pub fn read_fixed<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> io::Result<&'a [u8]> {
    let s = buf
        .get(*pos..*pos + n)
        .ok_or_else(|| short("fixed bytes"))?;
    *pos += n;
    Ok(s)
}

/// Reads a little-endian `u16`.
pub fn read_u16(buf: &[u8], pos: &mut usize) -> io::Result<u16> {
    let b = read_fixed(buf, pos, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

/// Reads a little-endian `u32`.
pub fn read_u32(buf: &[u8], pos: &mut usize) -> io::Result<u32> {
    let b = read_fixed(buf, pos, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn packet_round_trip() {
        let mut out = Vec::new();
        write_packet(&mut out, 7, b"hello").unwrap();
        assert_eq!(out, [5, 0, 0, 7, b'h', b'e', b'l', b'l', b'o']);
        let mut cur = Cursor::new(out);
        let (seq, payload) = read_packet(&mut cur).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn oversize_packet_rejected() {
        let mut out = Vec::new();
        let big = vec![0u8; MAX_PACKET_PAYLOAD];
        assert!(write_packet(&mut out, 0, &big).is_err());
        let header = [0xff, 0xff, 0xff, 0];
        let mut cur = Cursor::new(header.to_vec());
        let err = read_packet(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn lenenc_int_round_trip() {
        for v in [
            0u64,
            1,
            250,
            251,
            65535,
            65536,
            (1 << 24) - 1,
            1 << 24,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write_lenenc_int(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_lenenc_int(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
        let mut pos = 0;
        assert!(read_lenenc_int(&[0xfb], &mut pos).is_err());
        let mut pos = 0;
        assert!(read_lenenc_int(&[0xfc, 1], &mut pos).is_err());
    }

    #[test]
    fn lenenc_str_and_nul_str_round_trip() {
        let mut buf = Vec::new();
        write_lenenc_str(&mut buf, b"abc");
        write_null_terminated(&mut buf, b"def");
        write_lenenc_str(&mut buf, &[0u8; 300]);
        let mut pos = 0;
        assert_eq!(read_lenenc_str(&buf, &mut pos).unwrap(), b"abc");
        assert_eq!(read_null_terminated(&buf, &mut pos).unwrap(), b"def");
        assert_eq!(read_lenenc_str(&buf, &mut pos).unwrap().len(), 300);
        assert_eq!(pos, buf.len());
        assert!(read_null_terminated(b"no-terminator", &mut 0).is_err());
    }

    #[test]
    fn seq_counter_wraps_and_resets() {
        let mut c = SeqCounter::new();
        assert_eq!(c.advance(), 0);
        assert_eq!(c.advance(), 1);
        c.continue_after(255);
        assert_eq!(c.advance(), 0);
        c.reset();
        assert_eq!(c.advance(), 0);
    }

    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        timeouts_before_first_byte: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.timeouts_before_first_byte > 0 {
                self.timeouts_before_first_byte -= 1;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "timeout"));
            }
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(1);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn read_fully_handles_partial_reads_and_stop_at_boundary_only() {
        let mut r = ChunkedReader {
            data: vec![3, 0, 0, 1, 9, 9, 9],
            pos: 0,
            timeouts_before_first_byte: 2,
        };
        let stop = AtomicBool::new(false);
        let (seq, payload) = read_packet_with_stop(&mut r, Some(&stop)).unwrap();
        assert_eq!((seq, payload), (1, vec![9, 9, 9]));

        let mut r = ChunkedReader {
            data: vec![3, 0, 0, 1, 9, 9, 9],
            pos: 0,
            timeouts_before_first_byte: 1,
        };
        let stop = AtomicBool::new(true);
        let err = read_packet_with_stop(&mut r, Some(&stop)).unwrap_err();
        assert_eq!(err.kind(), SHUTDOWN_ERROR_KIND);
    }
}
