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

/// Error kind used by [`read_message_with_stop`] to signal that a message's total length would
/// exceed the caller's `max_allowed_packet`, distinct from an ordinary framing/protocol error
/// ([`io::ErrorKind::InvalidData`]): callers can match on this to answer with
/// `ER_NET_PACKET_TOO_LARGE` (1153) before closing the connection, exactly as MySQL does, rather
/// than the generic "desynchronized, just close" handling every other framing error gets.
pub const PACKET_TOO_LARGE_ERROR_KIND: io::ErrorKind = io::ErrorKind::FileTooLarge;

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

/// Writes one physical packet with the given sequence id, with no size guard: `payload.len()`
/// must fit in the header's 3-byte length field (`<= 0xFFFFFF`), which every caller in this crate
/// already guarantees ([`write_packet`]'s own guard, or [`write_message`]'s chunking). Shared by
/// both: [`write_packet`] additionally rejects a payload of exactly [`MAX_PACKET_PAYLOAD`] bytes
/// (ambiguous as a stand-alone packet, since that length is the wire's "more chunks follow"
/// marker), while [`write_message`] deliberately writes exactly that length for a full chunk.
fn write_raw_packet<W: Write>(writer: &mut W, seq: u8, payload: &[u8]) -> io::Result<()> {
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

/// Writes one packet with the given sequence id.
pub fn write_packet<W: Write>(writer: &mut W, seq: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() >= MAX_PACKET_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large for a single packet (>= 16MB)",
        ));
    }
    write_raw_packet(writer, seq, payload)
}

/// Reads one logical message, reassembling it from consecutive physical packets while a chunk's
/// payload length is exactly [`MAX_PACKET_PAYLOAD`] (`0xFFFFFF`), the MySQL protocol's marker that
/// more chunks follow. Returns the *last* physical packet's sequence id and the concatenated
/// payload: sequence ids increment by one (wrapping) for every physical packet exchanged while
/// processing one command, request chunks and response chunks alike, so a caller that must write
/// a reply continuing this exchange (`htap_wire::server`'s command loop; a `WireClient` mid
/// handshake/auth-switch) needs the *last* chunk's id to pass to
/// [`SeqCounter::continue_after`] — not the first, which is only equal to it when the message
/// happened to fit in a single packet.
///
/// Sequence ids must increment by one (wrapping) across every chunk of the same message;
/// otherwise this returns an [`io::ErrorKind::InvalidData`] error (the stream is desynchronized,
/// so the caller should close the connection, exactly like any other framing error).
///
/// Before each chunk's payload is read (indeed, before it is even allocated), the running total
/// of bytes read so far for this message plus this chunk's declared length is checked against
/// `max_allowed_packet`; exceeding it returns [`PACKET_TOO_LARGE_ERROR_KIND`] immediately, without
/// reading the oversize chunk's payload bytes off the wire (a well-behaved client is expected to
/// stop sending once it sees the connection close, but this function itself never blocks trying
/// to read data it has already decided to reject).
///
/// See [`read_fully_or_stop`] for the meaning of `stop`.
pub fn read_message_with_stop<R: Read>(
    reader: &mut R,
    max_allowed_packet: usize,
    stop: Option<&AtomicBool>,
) -> io::Result<(u8, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut last_seq: Option<u8> = None;
    let mut expected_seq: u8 = 0;
    loop {
        let mut header = [0u8; 4];
        // Only the very first chunk's header read observes `stop`: once any byte of the message
        // has arrived, `read_fully_or_stop` no longer offers a shutdown opportunity anyway (see
        // its doc comment), so passing `stop` for every chunk is harmless but only matters here.
        read_fully_or_stop(reader, &mut header, stop)?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        let seq = header[3];
        match last_seq {
            None => {
                expected_seq = seq.wrapping_add(1);
            }
            Some(_) if seq == expected_seq => {
                expected_seq = expected_seq.wrapping_add(1);
            }
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "packet sequence id mismatch while reassembling a multi-packet message: \
                         expected {expected_seq}, got {seq}"
                    ),
                ));
            }
        }
        last_seq = Some(seq);

        let new_total = buf
            .len()
            .checked_add(len)
            .filter(|&t| t <= max_allowed_packet);
        let Some(new_total) = new_total else {
            return Err(io::Error::new(
                PACKET_TOO_LARGE_ERROR_KIND,
                format!(
                    "message exceeds max_allowed_packet ({max_allowed_packet} bytes) while \
                     reassembling a multi-packet message"
                ),
            ));
        };
        let start = buf.len();
        buf.resize(new_total, 0);
        read_fully_or_stop(reader, &mut buf[start..], None)?;

        if len < MAX_PACKET_PAYLOAD {
            break;
        }
    }
    Ok((
        last_seq.expect("loop always sets last_seq before breaking"),
        buf,
    ))
}

/// [`read_message_with_stop`] without shutdown observation.
pub fn read_message<R: Read>(
    reader: &mut R,
    max_allowed_packet: usize,
) -> io::Result<(u8, Vec<u8>)> {
    read_message_with_stop(reader, max_allowed_packet, None)
}

/// Writes one logical message, splitting `payload` into consecutive physical packets of at most
/// [`MAX_PACKET_PAYLOAD`] bytes each. A payload whose length is an exact multiple of
/// [`MAX_PACKET_PAYLOAD`] (including zero) is followed by one trailing empty packet, so the
/// reader (which continues reassembling only while a chunk's length is exactly
/// [`MAX_PACKET_PAYLOAD`]) can tell the message is complete. Sequence ids are assigned from `seq`
/// at write time, one per physical packet, so they continue correctly across chunks regardless of
/// how many packets a caller has already sent for this same message.
pub fn write_message<W: Write>(
    writer: &mut W,
    seq: &mut SeqCounter,
    payload: &[u8],
) -> io::Result<()> {
    let mut offset = 0usize;
    loop {
        let remaining = payload.len() - offset;
        let chunk_len = remaining.min(MAX_PACKET_PAYLOAD);
        write_raw_packet(writer, seq.advance(), &payload[offset..offset + chunk_len])?;
        offset += chunk_len;
        if chunk_len < MAX_PACKET_PAYLOAD {
            break;
        }
    }
    Ok(())
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
///
/// Every multi-byte arm reads through [`read_fixed`] (rather than indexing `*pos..*pos + n`
/// directly) so its checked-arithmetic guard against offset overflow applies here too (finding 7
/// of the Phase 11 fix pass).
pub fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let first = *buf.get(*pos).ok_or_else(|| short("lenenc int"))?;
    *pos += 1;
    match first {
        0..=250 => Ok(first as u64),
        0xfc => {
            let b = read_fixed(buf, pos, 2)?;
            Ok(u16::from_le_bytes([b[0], b[1]]) as u64)
        }
        0xfd => {
            let b = read_fixed(buf, pos, 3)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64)
        }
        0xfe => {
            let b = read_fixed(buf, pos, 8)?;
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
///
/// Uses checked arithmetic throughout (finding 7 of the Phase 11 fix pass): a length-encoded
/// integer is adversary-controlled and can be as large as `u64::MAX`, which would overflow
/// `usize` on a 32-bit target and could overflow `*pos + len` even on a 64-bit one; either
/// overflow must become a clean truncated-packet error, never a panic (debug builds) or a
/// silently wrapped, wrong-but-in-bounds slice (release builds).
pub fn read_lenenc_str<'a>(buf: &'a [u8], pos: &mut usize) -> io::Result<&'a [u8]> {
    let len = usize::try_from(read_lenenc_int(buf, pos)?)
        .map_err(|_| short("lenenc string (length overflows usize)"))?;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| short("lenenc string (offset overflow)"))?;
    let s = buf.get(*pos..end).ok_or_else(|| short("lenenc string"))?;
    *pos = end;
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
///
/// Checked arithmetic (finding 7 of the Phase 11 fix pass): every caller's `n` is a compile-time
/// constant, but `*pos` accumulates from adversary-controlled lengths read earlier in the same
/// payload, so `*pos + n` must never be allowed to overflow silently.
pub fn read_fixed<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> io::Result<&'a [u8]> {
    let end = pos
        .checked_add(n)
        .ok_or_else(|| short("fixed bytes (offset overflow)"))?;
    let s = buf.get(*pos..end).ok_or_else(|| short("fixed bytes"))?;
    *pos = end;
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

    /// Parses a raw byte stream into the declared length of each physical packet it contains
    /// (header-only parsing; does not validate the payload bytes), for asserting exactly how
    /// [`write_message`] split a payload.
    fn packet_lens(mut bytes: &[u8]) -> Vec<usize> {
        let mut lens = Vec::new();
        while !bytes.is_empty() {
            let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as usize;
            lens.push(len);
            bytes = &bytes[4 + len..];
        }
        lens
    }

    #[test]
    fn test_message_reassembly_at_exact_boundary_with_trailing_empty_packet() {
        let payload = vec![0x5au8; MAX_PACKET_PAYLOAD];
        let mut out = Vec::new();
        let mut seq = SeqCounter::new();
        write_message(&mut out, &mut seq, &payload).unwrap();

        // Exactly two physical packets: one full-size chunk, then a trailing empty packet.
        assert_eq!(packet_lens(&out), vec![MAX_PACKET_PAYLOAD, 0]);
        assert_eq!(out.len(), 4 + MAX_PACKET_PAYLOAD + 4);

        let mut cur = Cursor::new(out);
        let (last_seq, reassembled) =
            read_message_with_stop(&mut cur, MAX_PACKET_PAYLOAD, None).unwrap();
        // The *last* physical packet (the trailing empty one) used sequence id 1, not the first
        // chunk's 0 (see `read_message_with_stop`'s doc comment on why the last id is returned).
        assert_eq!(last_seq, 1);
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn test_write_message_splits_exact_multiple_and_non_multiple() {
        // Exact multiple of MAX_PACKET_PAYLOAD (here, exactly one chunk): a trailing empty packet
        // is appended.
        let mut out = Vec::new();
        write_message(
            &mut out,
            &mut SeqCounter::new(),
            &vec![0u8; MAX_PACKET_PAYLOAD],
        )
        .unwrap();
        assert_eq!(packet_lens(&out), vec![MAX_PACKET_PAYLOAD, 0]);

        // Not an exact multiple (one full chunk plus a smaller remainder): no trailing empty
        // packet after the final, shorter chunk.
        let mut out = Vec::new();
        write_message(
            &mut out,
            &mut SeqCounter::new(),
            &vec![0u8; MAX_PACKET_PAYLOAD + 100],
        )
        .unwrap();
        assert_eq!(packet_lens(&out), vec![MAX_PACKET_PAYLOAD, 100]);

        // A payload that fits in a single packet is never split.
        let mut out = Vec::new();
        write_message(&mut out, &mut SeqCounter::new(), b"hello").unwrap();
        assert_eq!(packet_lens(&out), vec![5]);
        let mut cur = Cursor::new(out);
        let (seq, payload) = read_message_with_stop(&mut cur, 1024, None).unwrap();
        assert_eq!((seq, payload), (0, b"hello".to_vec()));
    }

    /// A reader that hands out bytes from a backing buffer while counting exactly how many bytes
    /// it has ever returned, so a test can assert that an oversize message's payload bytes were
    /// never actually read off the wire once the length was known to exceed the limit.
    struct CountingReader {
        data: Vec<u8>,
        pos: usize,
        bytes_read: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            self.bytes_read += n;
            Ok(n)
        }
    }

    #[test]
    fn test_message_reassembly_rejects_over_max_allowed_packet_before_full_read() {
        // A single (non-continuation) packet declaring 2,000,000 bytes of payload, with that much
        // real data actually available to read, but a `max_allowed_packet` of only 1,000: the
        // function must reject this from the 4-byte header alone, never reading any of the
        // payload bytes that are genuinely sitting in the stream behind it.
        let declared_len: u32 = 2_000_000;
        let mut data = Vec::new();
        data.extend_from_slice(&declared_len.to_le_bytes()[..3]);
        data.push(0); // seq
        data.extend(std::iter::repeat_n(0u8, declared_len as usize));

        let mut reader = CountingReader {
            data,
            pos: 0,
            bytes_read: 0,
        };
        let err = read_message_with_stop(&mut reader, 1_000, None).unwrap_err();
        assert_eq!(err.kind(), PACKET_TOO_LARGE_ERROR_KIND);
        assert_eq!(
            reader.bytes_read, 4,
            "only the 4-byte packet header should have been read, not the oversize payload"
        );
    }

    #[test]
    fn test_message_reassembly_rejects_sequence_id_mismatch_across_chunks() {
        // Two chunks of a would-be continuation message (first chunk's length equals
        // MAX_PACKET_PAYLOAD, so a second chunk is expected), but the second chunk's sequence id
        // does not continue from the first.
        let mut data = Vec::new();
        data.extend_from_slice(&(MAX_PACKET_PAYLOAD as u32).to_le_bytes()[..3]);
        data.push(5); // first chunk seq = 5
        data.extend(std::iter::repeat_n(0u8, MAX_PACKET_PAYLOAD));
        data.extend_from_slice(&0u32.to_le_bytes()[..3]); // second chunk: zero-length
        data.push(7); // wrong: should be 6
        let mut cur = Cursor::new(data);
        let err = read_message_with_stop(&mut cur, usize::MAX, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_message_and_read_message_sequence_ids_wrap_at_256() {
        let mut out = Vec::new();
        let mut seq = SeqCounter::new();
        seq.continue_after(253); // next advance() returns 254
        let messages: Vec<Vec<u8>> = (0..6).map(|i| format!("msg{i}").into_bytes()).collect();
        for m in &messages {
            write_message(&mut out, &mut seq, m).unwrap();
        }

        let mut cur = Cursor::new(out);
        let expected_first_seqs: Vec<u8> = vec![254, 255, 0, 1, 2, 3];
        for (m, expected_seq) in messages.iter().zip(expected_first_seqs) {
            let (first_seq, payload) = read_message_with_stop(&mut cur, 1024, None).unwrap();
            assert_eq!(first_seq, expected_seq);
            assert_eq!(&payload, m);
        }
    }
}
