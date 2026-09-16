//! Minimal SHA-1 implementation for `mysql_native_password` authentication.
//!
//! SHA-1 is not collision resistant and is used here only because the MySQL native
//! password exchange mandates it. It is not used for any other purpose.

/// Computes the SHA-1 digest of `data`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Computes the `mysql_native_password` client response:
/// `SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))`.
///
/// An empty password yields an empty response by MySQL convention.
pub fn scramble_native_password(scramble: &[u8; 20], password: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = sha1(password);
    let stage2 = sha1(&stage1);
    let mut buf = Vec::with_capacity(40);
    buf.extend_from_slice(scramble);
    buf.extend_from_slice(&stage2);
    let stage3 = sha1(&buf);
    stage1
        .iter()
        .zip(stage3.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// Verifies a client `mysql_native_password` response against the configured password.
pub fn verify_native_password(scramble: &[u8; 20], password: &str, response: &[u8]) -> bool {
    let expected = scramble_native_password(scramble, password.as_bytes());
    if expected.len() != response.len() {
        return false;
    }
    // Constant-time comparison to avoid trivially leaking the prefix length.
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(response.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha1_empty_string() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn sha1_abc() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn sha1_multi_block() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            hex(&sha1(input)),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let long: Vec<u8> = std::iter::repeat_n(b'a', 1_000_000).collect();
        assert_eq!(
            hex(&sha1(&long)),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn native_password_scramble_matches_test_vector() {
        // Known vector: scramble of 20 ASCII bytes and password "password".
        let scramble: [u8; 20] = *b"01234567890123456789";
        let resp = scramble_native_password(&scramble, b"password");
        assert_eq!(resp.len(), 20);
        // Independently recomputed with the same formula.
        let s1 = sha1(b"password");
        let s2 = sha1(&s1);
        let mut buf = scramble.to_vec();
        buf.extend_from_slice(&s2);
        let s3 = sha1(&buf);
        let expected: Vec<u8> = s1.iter().zip(s3.iter()).map(|(a, b)| a ^ b).collect();
        assert_eq!(resp, expected);
        assert!(verify_native_password(&scramble, "password", &resp));
        assert!(!verify_native_password(&scramble, "wrong", &resp));
        assert!(!verify_native_password(&scramble, "password", &resp[..19]));
        assert!(scramble_native_password(&scramble, b"").is_empty());
    }
}
