//! Magic, version, length, and CRC32-C envelope framing.

use std::ops::RangeInclusive;

/// Fixed envelope header size: magic, version, payload length, and CRC32-C.
pub const ENVELOPE_HEADER_LEN: usize = 18;

/// Policy for comparing the encoded payload length with the available bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeCheckMode {
    /// Report short and trailing inputs as distinct errors.
    TruncatedThenTrailing,
    /// Report either discrepancy as a single size mismatch.
    ExactMatch,
}

/// Structural errors detected while decoding an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The input cannot contain a complete envelope header.
    TooSmall { found: usize, min: usize },
    /// The envelope magic does not match the expected format.
    BadMagic,
    /// The envelope version is not accepted by this decoder.
    UnsupportedVersion(u16),
    /// The declared payload exceeds the caller's allocation bound.
    PayloadTooLarge { len: u32, max: u32 },
    /// The input ends before the declared payload does.
    Truncated { expected: usize, found: usize },
    /// The input contains bytes after the declared payload.
    TrailingBytes { extra: usize },
    /// The input size differs from the exact declared envelope size.
    SizeMismatch { expected: usize, found: usize },
    /// The payload CRC32-C does not match the envelope header.
    ChecksumMismatch { expected: u32, actual: u32 },
}

/// Encode a payload using the shared whole-file envelope format.
///
/// The payload length cast intentionally matches the pre-migration encoders;
/// callers retain responsibility for enforcing their format-specific bounds.
pub fn encode_envelope(magic: &[u8; 8], format_version: u16, payload: &[u8]) -> Vec<u8> {
    let payload_len = payload.len() as u32;
    let checksum = crc32c::crc32c(payload);

    let mut bytes = Vec::with_capacity(ENVELOPE_HEADER_LEN + payload.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&format_version.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// Decode and validate a shared whole-file envelope.
///
/// Validation order is stable: header size, magic, version, payload bound,
/// total size according to `size_check`, and finally payload checksum.
pub fn decode_envelope<'a>(
    bytes: &'a [u8],
    expected_magic: &[u8; 8],
    accepted_versions: RangeInclusive<u16>,
    max_payload_len: u32,
    size_check: SizeCheckMode,
) -> Result<(u16, &'a [u8]), EnvelopeError> {
    if bytes.len() < ENVELOPE_HEADER_LEN {
        return Err(EnvelopeError::TooSmall {
            found: bytes.len(),
            min: ENVELOPE_HEADER_LEN,
        });
    }

    if &bytes[..8] != expected_magic {
        return Err(EnvelopeError::BadMagic);
    }

    let version = u16::from_le_bytes(bytes[8..10].try_into().expect("fixed header slice"));
    if !accepted_versions.contains(&version) {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }

    let payload_len = u32::from_le_bytes(bytes[10..14].try_into().expect("fixed header slice"));
    let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().expect("fixed header slice"));

    if payload_len > max_payload_len {
        return Err(EnvelopeError::PayloadTooLarge {
            len: payload_len,
            max: max_payload_len,
        });
    }

    let expected_total = ENVELOPE_HEADER_LEN + payload_len as usize;
    match size_check {
        SizeCheckMode::TruncatedThenTrailing if bytes.len() < expected_total => {
            return Err(EnvelopeError::Truncated {
                expected: expected_total,
                found: bytes.len(),
            });
        }
        SizeCheckMode::TruncatedThenTrailing if bytes.len() > expected_total => {
            return Err(EnvelopeError::TrailingBytes {
                extra: bytes.len() - expected_total,
            });
        }
        SizeCheckMode::ExactMatch if bytes.len() != expected_total => {
            return Err(EnvelopeError::SizeMismatch {
                expected: expected_total,
                found: bytes.len(),
            });
        }
        _ => {}
    }

    let payload = &bytes[ENVELOPE_HEADER_LEN..expected_total];
    let actual_crc = crc32c::crc32c(payload);
    if actual_crc != expected_crc {
        return Err(EnvelopeError::ChecksumMismatch {
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    Ok((version, payload))
}

/// Encode a length-and-CRC-prefixed frame with no magic or version.
pub fn encode_bare_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: &[u8; 8] = b"TESTENV1";

    fn encoded() -> Vec<u8> {
        encode_envelope(MAGIC, 2, b"payload")
    }

    #[test]
    fn round_trip() {
        let bytes = encoded();
        let (version, payload) = decode_envelope(
            &bytes,
            MAGIC,
            1..=3,
            1024,
            SizeCheckMode::TruncatedThenTrailing,
        )
        .unwrap();

        assert_eq!(version, 2);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn reports_too_small() {
        assert_eq!(
            decode_envelope(&[0; 17], MAGIC, 1..=3, 1024, SizeCheckMode::ExactMatch),
            Err(EnvelopeError::TooSmall {
                found: 17,
                min: ENVELOPE_HEADER_LEN
            })
        );
    }

    #[test]
    fn reports_bad_magic_before_other_header_faults() {
        let mut bytes = encoded();
        bytes[0] ^= 0xff;
        bytes[10..14].copy_from_slice(&u32::MAX.to_le_bytes());

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                1,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::BadMagic)
        );
    }

    #[test]
    fn reports_unsupported_version_before_bad_crc() {
        let mut bytes = encoded();
        bytes[8..10].copy_from_slice(&9u16.to_le_bytes());
        bytes[14..18].copy_from_slice(&0u32.to_le_bytes());

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                1024,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::UnsupportedVersion(9))
        );
    }

    #[test]
    fn reports_payload_too_large() {
        let mut bytes = encoded();
        bytes[10..14].copy_from_slice(&100u32.to_le_bytes());

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                99,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::PayloadTooLarge { len: 100, max: 99 })
        );
    }

    #[test]
    fn size_modes_distinguish_short_input() {
        let mut bytes = encoded();
        bytes.pop();

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                1024,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::Truncated {
                expected: 25,
                found: 24
            })
        );
        assert_eq!(
            decode_envelope(&bytes, MAGIC, 1..=3, 1024, SizeCheckMode::ExactMatch),
            Err(EnvelopeError::SizeMismatch {
                expected: 25,
                found: 24
            })
        );
    }

    #[test]
    fn size_modes_distinguish_trailing_input() {
        let mut bytes = encoded();
        bytes.push(0xaa);

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                1024,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::TrailingBytes { extra: 1 })
        );
        assert_eq!(
            decode_envelope(&bytes, MAGIC, 1..=3, 1024, SizeCheckMode::ExactMatch),
            Err(EnvelopeError::SizeMismatch {
                expected: 25,
                found: 26
            })
        );
    }

    #[test]
    fn reports_checksum_mismatch() {
        let mut bytes = encoded();
        let expected = u32::from_le_bytes(bytes[14..18].try_into().unwrap());
        *bytes.last_mut().unwrap() ^= 0xff;
        let actual = crc32c::crc32c(&bytes[ENVELOPE_HEADER_LEN..]);

        assert_eq!(
            decode_envelope(
                &bytes,
                MAGIC,
                1..=3,
                1024,
                SizeCheckMode::TruncatedThenTrailing
            ),
            Err(EnvelopeError::ChecksumMismatch { expected, actual })
        );
    }

    #[test]
    fn bare_frame_layout_is_len_crc_payload() {
        let frame = encode_bare_frame(b"abc");
        assert_eq!(&frame[..4], &3u32.to_le_bytes());
        assert_eq!(&frame[4..8], &crc32c::crc32c(b"abc").to_le_bytes());
        assert_eq!(&frame[8..], b"abc");
    }
}
