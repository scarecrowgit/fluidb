//! MySQL compressed-packet transport framing.

use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

/// Payloads at or below this size are sent without compression.
const MIN_COMPRESS_LENGTH: usize = 50;
/// The largest uncompressed payload represented by one compressed packet.
const MAX_COMPRESSED_PACKET_UNCOMPRESSED: usize = 16 * 1024 * 1024 - 1;

/// Compression algorithm negotiated for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    /// MySQL's zlib compressed-packet format.
    Zlib,
    /// MySQL's zstd compressed-packet format at the negotiated compression level.
    Zstd {
        /// Compression level requested by the client.
        level: i32,
    },
}

#[derive(Debug, Default)]
struct PendingReadFrame {
    header: [u8; 7],
    header_read: usize,
    header_complete: bool,
    payload: Vec<u8>,
    payload_read: usize,
}

#[derive(Debug, Default)]
struct SeqCounter {
    next: u8,
}

impl SeqCounter {
    fn reset(&mut self) {
        self.next = 0;
    }

    fn take(&mut self) -> u8 {
        let sequence = self.next;
        self.next = self.next.wrapping_add(1);
        sequence
    }

    fn expect(&mut self, sequence: u8) -> io::Result<()> {
        let expected = self.next;
        if sequence != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("compressed packet sequence mismatch: expected {expected}, got {sequence}"),
            ));
        }
        self.next = self.next.wrapping_add(1);
        Ok(())
    }
}

/// A `Read`/`Write` transport that applies MySQL compressed-packet framing.
pub struct CompressedStream<S: Read + Write> {
    inner: S,
    algorithm: Option<CompressionAlgorithm>,
    read_buf: VecDeque<u8>,
    write_buf: Vec<u8>,
    compressed_seq: SeqCounter,
    pending_read_frame: Option<PendingReadFrame>,
    failed: bool,
    max_uncompressed: usize,
}

impl<S: Read + Write> CompressedStream<S> {
    /// Creates a compressed transport over `inner`.
    pub fn new(inner: S, algorithm: Option<CompressionAlgorithm>, max_uncompressed: usize) -> Self {
        Self {
            inner,
            algorithm,
            read_buf: VecDeque::new(),
            write_buf: Vec::new(),
            compressed_seq: SeqCounter::default(),
            pending_read_frame: None,
            failed: false,
            max_uncompressed,
        }
    }

    /// Returns a shared reference to the wrapped transport.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Returns a mutable reference to the wrapped transport.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Resets the compressed-packet sequence counter for a new command exchange.
    pub fn reset_sequence(&mut self) {
        self.compressed_seq.reset();
    }

    fn write_frame(&mut self, chunk: &[u8]) -> io::Result<()> {
        let compressed = if self.algorithm.is_some() && chunk.len() > MIN_COMPRESS_LENGTH {
            let encoded = match self.algorithm {
                Some(CompressionAlgorithm::Zlib) => {
                    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                    encoder.write_all(chunk)?;
                    encoder.finish()?
                }
                Some(CompressionAlgorithm::Zstd { level }) => zstd::bulk::compress(chunk, level)?,
                None => Vec::new(),
            };
            if encoded.len() < chunk.len() {
                Some(encoded)
            } else {
                None
            }
        } else {
            None
        };

        let (payload, uncompressed_length) = match compressed {
            Some(payload) => (payload, chunk.len()),
            None => (chunk.to_vec(), 0),
        };
        debug_assert!(payload.len() <= MAX_COMPRESSED_PACKET_UNCOMPRESSED);

        let mut header = [0u8; 7];
        write_u24(&mut header[..3], payload.len())?;
        header[3] = self.compressed_seq.take();
        write_u24(&mut header[4..], uncompressed_length)?;
        self.inner.write_all(&header)?;
        self.inner.write_all(&payload)?;
        Ok(())
    }

    fn read_frame(&mut self) -> io::Result<()> {
        let frame = self.pending_read_frame.get_or_insert_with(Default::default);

        while frame.header_read < frame.header.len() {
            match self.inner.read(&mut frame.header[frame.header_read..]) {
                Ok(0) if frame.header_read == 0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "clean end of compressed stream",
                    ))
                }
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated compressed packet header",
                    ))
                }
                Ok(count) => frame.header_read += count,
                Err(error) => return Err(error),
            }
        }

        if !frame.header_complete {
            let compressed_length = read_u24(&frame.header[..3]);
            self.compressed_seq.expect(frame.header[3])?;
            frame.payload.resize(compressed_length, 0);
            frame.header_complete = true;
        }

        while frame.payload_read < frame.payload.len() {
            match self.inner.read(&mut frame.payload[frame.payload_read..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated compressed packet payload",
                    ))
                }
                Ok(count) => frame.payload_read += count,
                Err(error) => return Err(error),
            }
        }

        let frame = self
            .pending_read_frame
            .take()
            .expect("pending frame was initialized above");
        let uncompressed_length = read_u24(&frame.header[4..]);
        let payload = frame.payload;

        if uncompressed_length == 0 {
            if payload.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zero-length raw compressed packet",
                ));
            }
            self.read_buf.extend(payload);
            return Ok(());
        }
        if self.algorithm.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "received compressed packet without a negotiated compression algorithm",
            ));
        }
        if uncompressed_length > self.max_uncompressed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed packet exceeds configured uncompressed limit",
            ));
        }

        // Stop decoding immediately after the declared length is exceeded.
        let limit = uncompressed_length.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed packet uncompressed length is too large",
            )
        })?;
        let mut decoded = Vec::with_capacity(uncompressed_length.min(self.max_uncompressed));
        match self.algorithm {
            Some(CompressionAlgorithm::Zlib) => {
                let decoder = ZlibDecoder::new(Cursor::new(payload));
                decoder
                    .take(limit as u64)
                    .read_to_end(&mut decoded)
                    .map_err(codec_error)?;
            }
            Some(CompressionAlgorithm::Zstd { .. }) => {
                let mut decoder = zstd::stream::read::Decoder::with_buffer(Cursor::new(payload))
                    .map_err(codec_error)?;
                decoder.window_log_max(24).map_err(codec_error)?;
                decoder
                    .take(limit as u64)
                    .read_to_end(&mut decoded)
                    .map_err(codec_error)?;
            }
            None => unreachable!(),
        }
        if decoded.len() > self.max_uncompressed || decoded.len() != uncompressed_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed packet uncompressed length does not match payload",
            ));
        }

        self.read_buf.extend(decoded);
        Ok(())
    }
}

impl<S: Read + Write> Read for CompressedStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed stream is in a failed state",
            ));
        }

        if self.read_buf.is_empty() {
            match self.read_frame() {
                Ok(()) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::UnexpectedEof
                        && error.to_string() == "clean end of compressed stream" =>
                {
                    return Ok(0);
                }
                Err(error) => {
                    if matches!(
                        error.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                    ) {
                        self.failed = true;
                    }
                    return Err(error);
                }
            }
        }

        let count = buf.len().min(self.read_buf.len());
        for slot in &mut buf[..count] {
            *slot = self
                .read_buf
                .pop_front()
                .expect("count is bounded by buffer length");
        }
        Ok(count)
    }
}

impl<S: Read + Write> Write for CompressedStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let buffered = std::mem::take(&mut self.write_buf);
        for chunk in buffered.chunks(MAX_COMPRESSED_PACKET_UNCOMPRESSED) {
            self.write_frame(chunk)?;
        }
        self.inner.flush()
    }
}

fn read_u24(bytes: &[u8]) -> usize {
    usize::from(bytes[0]) | (usize::from(bytes[1]) << 8) | (usize::from(bytes[2]) << 16)
}

fn write_u24(bytes: &mut [u8], value: usize) -> io::Result<()> {
    if value > MAX_COMPRESSED_PACKET_UNCOMPRESSED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "compressed packet length exceeds 24-bit field",
        ));
    }
    bytes[0] = value as u8;
    bytes[1] = (value >> 8) as u8;
    bytes[2] = (value >> 16) as u8;
    Ok(())
}

fn codec_error(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(
        algorithm: Option<CompressionAlgorithm>,
        max_uncompressed: usize,
        input: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut writer =
            CompressedStream::new(Cursor::new(Vec::new()), algorithm, max_uncompressed);
        writer.write_all(input).unwrap();
        writer.flush().unwrap();
        let wire = writer.get_ref().get_ref().clone();

        let mut reader =
            CompressedStream::new(Cursor::new(wire.clone()), algorithm, max_uncompressed);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        (wire, output)
    }

    struct TimedCursor {
        inner: Cursor<Vec<u8>>,
        timeout_after: usize,
        timed_out: bool,
    }

    impl TimedCursor {
        fn new(bytes: Vec<u8>, timeout_after: usize) -> Self {
            Self {
                inner: Cursor::new(bytes),
                timeout_after,
                timed_out: false,
            }
        }
    }

    impl Read for TimedCursor {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let position = self.inner.position() as usize;
            if !self.timed_out && position >= self.timeout_after {
                self.timed_out = true;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "simulated timeout"));
            }

            let allowed = if self.timed_out {
                buf.len()
            } else {
                buf.len().min(self.timeout_after - position)
            };
            self.inner.read(&mut buf[..allowed])
        }
    }

    impl Write for TimedCursor {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn uncompressed_frame(sequence: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; 7];
        write_u24(&mut frame[..3], payload.len()).unwrap();
        frame[3] = sequence;
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn test_below_threshold_round_trip() {
        let input = vec![b'x'; 30];
        let (wire, output) = round_trip(Some(CompressionAlgorithm::Zlib), 1024, &input);
        assert_eq!(read_u24(&wire[4..]), 0);
        assert_eq!(output, input);
    }

    #[test]
    fn test_zlib_above_threshold_round_trip() {
        let input = (0..1000).map(|i| b'a' + (i % 4) as u8).collect::<Vec<_>>();
        let (wire, output) = round_trip(Some(CompressionAlgorithm::Zlib), 2048, &input);
        assert_ne!(read_u24(&wire[4..]), 0);
        assert_eq!(output, input);
    }

    #[test]
    fn test_zstd_above_threshold_round_trip() {
        let input = (0..1000).map(|i| b'a' + (i % 4) as u8).collect::<Vec<_>>();
        let (wire, output) =
            round_trip(Some(CompressionAlgorithm::Zstd { level: 3 }), 2048, &input);
        assert_ne!(read_u24(&wire[4..]), 0);
        assert_eq!(output, input);
    }

    #[test]
    fn test_large_message_spans_multiple_frames() {
        let input = vec![b'x'; 32 * 1024 * 1024];
        let (_, output) = round_trip(
            Some(CompressionAlgorithm::Zlib),
            MAX_COMPRESSED_PACKET_UNCOMPRESSED,
            &input,
        );
        assert_eq!(output, input);
    }

    #[test]
    fn test_multiple_packets_single_frame() {
        let mut writer = CompressedStream::new(
            Cursor::new(Vec::new()),
            Some(CompressionAlgorithm::Zlib),
            1024,
        );
        writer.write_all(&[b'a'; 100]).unwrap();
        writer.write_all(&[b'b'; 100]).unwrap();
        writer.flush().unwrap();
        let wire = writer.get_ref().get_ref();
        assert_eq!(wire[3], 0);
        assert_eq!(wire.len(), 7 + read_u24(&wire[..3]));
    }

    #[test]
    fn test_compression_bomb_rejected() {
        let input = vec![b'x'; 1024 * 1024 + 1];
        let (mut wire, _) = round_trip(Some(CompressionAlgorithm::Zlib), input.len(), &input);
        write_u24(&mut wire[4..], MAX_COMPRESSED_PACKET_UNCOMPRESSED).unwrap();

        let mut reader = CompressedStream::new(
            Cursor::new(wire),
            Some(CompressionAlgorithm::Zlib),
            1024 * 1024,
        );
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_declared_length_mismatch_rejected() {
        let input = vec![b'x'; 1000];
        let (mut wire, _) = round_trip(Some(CompressionAlgorithm::Zlib), 2048, &input);
        write_u24(&mut wire[4..], 999).unwrap();

        let mut reader =
            CompressedStream::new(Cursor::new(wire), Some(CompressionAlgorithm::Zlib), 2048);
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_truncated_frame_rejected() {
        let mut frame = uncompressed_frame(0, b"payload");
        frame.truncate(frame.len() - 1);
        let mut reader =
            CompressedStream::new(Cursor::new(frame), Some(CompressionAlgorithm::Zlib), 1024);
        assert!(reader.read(&mut [0; 1]).is_err());
    }

    #[test]
    fn test_sequence_id_mismatch_rejected() {
        let mut wire = uncompressed_frame(0, b"a");
        wire.extend(uncompressed_frame(2, b"b"));
        let mut reader =
            CompressedStream::new(Cursor::new(wire), Some(CompressionAlgorithm::Zlib), 1024);

        let mut output = [0; 1];
        reader.read_exact(&mut output).unwrap();
        let error = reader.read(&mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let retry = reader.read(&mut output).unwrap_err();
        assert_eq!(retry.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_empty_raw_frame_rejected() {
        let frame = uncompressed_frame(0, b"");
        let mut reader =
            CompressedStream::new(Cursor::new(frame), Some(CompressionAlgorithm::Zlib), 1024);

        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_read_resumes_after_timeout_in_header() {
        let input = vec![b'x'; 1000];
        let (wire, _) = round_trip(Some(CompressionAlgorithm::Zlib), 2048, &input);
        let mut reader = CompressedStream::new(
            TimedCursor::new(wire, 3),
            Some(CompressionAlgorithm::Zlib),
            2048,
        );

        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn test_read_resumes_after_timeout_in_payload() {
        let input = vec![b'x'; 1000];
        let (wire, _) = round_trip(Some(CompressionAlgorithm::Zlib), 2048, &input);
        let mut reader = CompressedStream::new(
            TimedCursor::new(wire, 8),
            Some(CompressionAlgorithm::Zlib),
            2048,
        );

        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn test_zstd_read_resumes_after_timeout_in_header() {
        let input = vec![b'x'; 1000];
        let (wire, _) = round_trip(Some(CompressionAlgorithm::Zstd { level: 3 }), 2048, &input);
        let mut reader = CompressedStream::new(
            TimedCursor::new(wire, 3),
            Some(CompressionAlgorithm::Zstd { level: 3 }),
            2048,
        );

        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn test_zstd_read_resumes_after_timeout_in_payload() {
        let input = vec![b'x'; 1000];
        let (wire, _) = round_trip(Some(CompressionAlgorithm::Zstd { level: 3 }), 2048, &input);
        let mut reader = CompressedStream::new(
            TimedCursor::new(wire, 8),
            Some(CompressionAlgorithm::Zstd { level: 3 }),
            2048,
        );

        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn test_compressed_payload_exceeding_declared_length_is_rejected_early() {
        let input = vec![b'x'; 4 * 1024 * 1024];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&input).unwrap();
        let payload = encoder.finish().unwrap();

        let mut frame = vec![0; 7];
        let declared_length = 10;
        write_u24(&mut frame[..3], payload.len()).unwrap();
        frame[3] = 0;
        write_u24(&mut frame[4..], declared_length).unwrap();
        frame.extend_from_slice(&payload);

        let mut decoded = Vec::new();
        ZlibDecoder::new(Cursor::new(&payload))
            .take((declared_length + 1) as u64)
            .read_to_end(&mut decoded)
            .unwrap();
        assert!(decoded.len() <= declared_length + 1);

        let mut reader = CompressedStream::new(
            Cursor::new(frame),
            Some(CompressionAlgorithm::Zlib),
            input.len(),
        );
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_zstd_window_larger_than_16mb_is_rejected() {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(25).unwrap();
        encoder.write_all(b"small payload").unwrap();
        let payload = encoder.finish().unwrap();

        let mut frame = vec![0; 7];
        write_u24(&mut frame[..3], payload.len()).unwrap();
        frame[3] = 0;
        write_u24(&mut frame[4..], 13).unwrap();
        frame.extend_from_slice(&payload);

        let mut reader = CompressedStream::new(
            Cursor::new(frame),
            Some(CompressionAlgorithm::Zstd { level: 3 }),
            1024,
        );
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_algorithm_none_does_not_compress() {
        let input = vec![b'x'; 1000];
        let (wire, output) = round_trip(None, 2048, &input);
        assert_eq!(read_u24(&wire[4..]), 0);
        assert_eq!(output, input);
    }
}
