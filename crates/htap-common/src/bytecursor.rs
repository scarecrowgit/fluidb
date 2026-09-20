//! Bounds-checked little-endian byte reader.

/// Errors returned by [`ByteReader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteReaderError {
    /// The requested value extends beyond the available input.
    UnexpectedEof {
        position: usize,
        needed: usize,
        remaining: usize,
    },
    /// Computing the end position overflowed `usize`.
    LengthOverflow { position: usize, length: usize },
    /// Bytes remain after a decoder expected to consume the entire input.
    TrailingBytes { position: usize, remaining: usize },
}

/// A cursor over an immutable byte slice with transactional failed reads.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ByteReader<'a> {
    /// Construct a reader positioned at the beginning of `bytes`.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Return the current byte position.
    pub const fn position(&self) -> usize {
        self.position
    }

    /// Return the number of unread bytes.
    pub const fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// Read exactly `length` bytes.
    pub fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], ByteReaderError> {
        let start = self.position;
        let end = start
            .checked_add(length)
            .ok_or(ByteReaderError::LengthOverflow {
                position: start,
                length,
            })?;

        if end > self.bytes.len() {
            return Err(ByteReaderError::UnexpectedEof {
                position: start,
                needed: length,
                remaining: self.remaining(),
            });
        }

        self.position = end;
        Ok(&self.bytes[start..end])
    }

    /// Read one byte.
    pub fn read_u8(&mut self) -> Result<u8, ByteReaderError> {
        Ok(self.read_bytes(1)?[0])
    }

    /// Read a little-endian `u16`.
    pub fn read_u16_le(&mut self) -> Result<u16, ByteReaderError> {
        let bytes: [u8; 2] = self.read_bytes(2)?.try_into().expect("fixed-width read");
        Ok(u16::from_le_bytes(bytes))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32_le(&mut self) -> Result<u32, ByteReaderError> {
        let bytes: [u8; 4] = self.read_bytes(4)?.try_into().expect("fixed-width read");
        Ok(u32::from_le_bytes(bytes))
    }

    /// Read a little-endian `u64`.
    pub fn read_u64_le(&mut self) -> Result<u64, ByteReaderError> {
        let bytes: [u8; 8] = self.read_bytes(8)?.try_into().expect("fixed-width read");
        Ok(u64::from_le_bytes(bytes))
    }

    /// Read a little-endian `i32`.
    pub fn read_i32_le(&mut self) -> Result<i32, ByteReaderError> {
        let bytes: [u8; 4] = self.read_bytes(4)?.try_into().expect("fixed-width read");
        Ok(i32::from_le_bytes(bytes))
    }

    /// Read a little-endian `i64`.
    pub fn read_i64_le(&mut self) -> Result<i64, ByteReaderError> {
        let bytes: [u8; 8] = self.read_bytes(8)?.try_into().expect("fixed-width read");
        Ok(i64::from_le_bytes(bytes))
    }

    /// Read a little-endian IEEE-754 `f64`.
    pub fn read_f64_le(&mut self) -> Result<f64, ByteReaderError> {
        let bytes: [u8; 8] = self.read_bytes(8)?.try_into().expect("fixed-width read");
        Ok(f64::from_le_bytes(bytes))
    }

    /// Read a `u32` byte length followed by that many bytes.
    ///
    /// If either read fails, the reader is restored to its original position.
    pub fn read_len_prefixed_bytes_u32(&mut self) -> Result<&'a [u8], ByteReaderError> {
        let start = self.position;
        let result = (|| {
            let length = self.read_u32_le()? as usize;
            self.read_bytes(length)
        })();

        if result.is_err() {
            self.position = start;
        }
        result
    }

    /// Require that the input has been consumed exactly.
    pub fn expect_exhausted(&self) -> Result<(), ByteReaderError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ByteReaderError::TrailingBytes {
                position: self.position,
                remaining: self.remaining(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_reads_decode_little_endian_values() {
        let mut bytes = Vec::new();
        bytes.push(7);
        bytes.extend_from_slice(&0x1234u16.to_le_bytes());
        bytes.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        bytes.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        bytes.extend_from_slice(&(-1234i32).to_le_bytes());
        bytes.extend_from_slice(&(-5678i64).to_le_bytes());
        bytes.extend_from_slice(&1.25f64.to_le_bytes());

        let mut reader = ByteReader::new(&bytes);
        assert_eq!(reader.read_u8().unwrap(), 7);
        assert_eq!(reader.read_u16_le().unwrap(), 0x1234);
        assert_eq!(reader.read_u32_le().unwrap(), 0x1234_5678);
        assert_eq!(reader.read_u64_le().unwrap(), 0x0123_4567_89ab_cdef);
        assert_eq!(reader.read_i32_le().unwrap(), -1234);
        assert_eq!(reader.read_i64_le().unwrap(), -5678);
        assert_eq!(reader.read_f64_le().unwrap(), 1.25);
        assert_eq!(reader.remaining(), 0);
        reader.expect_exhausted().unwrap();
    }

    #[test]
    fn failed_fixed_width_read_does_not_advance() {
        let mut reader = ByteReader::new(&[1, 2, 3]);
        assert!(matches!(
            reader.read_u32_le(),
            Err(ByteReaderError::UnexpectedEof { .. })
        ));
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), 3);
    }

    #[test]
    fn failed_length_prefixed_read_does_not_advance() {
        let bytes = [5, 0, 0, 0, 1, 2];
        let mut reader = ByteReader::new(&bytes);

        assert!(matches!(
            reader.read_len_prefixed_bytes_u32(),
            Err(ByteReaderError::UnexpectedEof { .. })
        ));
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn length_arithmetic_is_overflow_safe() {
        let mut reader = ByteReader::new(&[1]);
        reader.read_u8().unwrap();

        assert_eq!(
            reader.read_bytes(usize::MAX),
            Err(ByteReaderError::LengthOverflow {
                position: 1,
                length: usize::MAX
            })
        );
        assert_eq!(reader.position(), 1);
    }

    #[test]
    fn empty_and_exact_boundary_reads_succeed() {
        let bytes = [1, 2, 3];
        let mut reader = ByteReader::new(&bytes);

        assert_eq!(reader.read_bytes(0).unwrap(), &[] as &[u8]);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.read_bytes(3).unwrap(), &bytes);
        assert_eq!(reader.position(), 3);
        assert_eq!(reader.read_bytes(0).unwrap(), &[] as &[u8]);
        reader.expect_exhausted().unwrap();
    }

    #[test]
    fn expect_exhausted_reports_remaining_bytes() {
        let reader = ByteReader::new(&[1, 2]);

        assert_eq!(
            reader.expect_exhausted(),
            Err(ByteReaderError::TrailingBytes {
                position: 0,
                remaining: 2
            })
        );
    }
}
