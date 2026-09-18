//! Minimal SHA-1 helpers for `mysql_native_password` authentication.
//!
//! The implementation now lives in `htap_common::password`; this module
//! re-exports the canonical functions and preserves all pre-existing public
//! signatures so that the rest of `htap-wire` compiles unchanged.

/// Uses the canonical SHA-1 implementation from `htap_common`.
use htap_common::password::sha1 as common_sha1;

/// Computes the SHA-1 digest of `data`.
///
/// Delegates to [`htap_common::password::sha1`].
pub fn sha1(data: &[u8]) -> [u8; 20] {
    common_sha1(data)
}

/// Computes the SHA-1 digest of `data`.
pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
    sha1(data)
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

    #[test]
    fn wire_sha1_agrees_with_htap_common_password() {
        // Confirm the local sha1() delegates correctly to htap_common::password::sha1.
        let data = b"hello world";
        assert_eq!(sha1(data), htap_common::password::sha1(data));

        // Confirm verify_native_password is consistent with htap_common::verify_native_password_hash.
        let scramble: [u8; 20] = *b"01234567890123456789";
        let password = "password";
        let stored = htap_common::hash_native_password(password);
        let resp = scramble_native_password(&scramble, password.as_bytes());
        assert!(verify_native_password(&scramble, password, &resp));
        assert!(htap_common::verify_native_password_hash(
            &scramble, &stored, &resp
        ));
    }
}
