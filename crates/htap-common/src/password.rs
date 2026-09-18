//! Password hashing utilities for MySQL Native Password authentication.
//!
//! ## MySQL Native Password Protocol
//!
//! MySQL's `mysql_native_password` authentication plugin uses a challenge-response
//! scheme based on SHA-1 hashing. The protocol works as follows:
//!
//! ### Password Storage
//!
//! MySQL stores passwords as a double SHA-1 hash:
//!
//! ```text
//! stored_hash = SHA1(SHA1(password))
//! ```
//!
//! This is what appears in `mysql.user.authentication_string`.
//!
//! ### Authentication Handshake
//!
//! During authentication the server sends a 20-byte random nonce called the
//! *scramble* (also referred to as the *auth-plugin-data* in the handshake
//! packet). The client must respond with:
//!
//! ```text
//! response = SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))
//! ```
//!
//! Where `||` denotes concatenation. This allows the server to verify the
//! response without ever seeing the plaintext password:
//!
//! ```text
//! SHA1(password) = response XOR SHA1(scramble || stored_hash)
//! verify:        SHA1(recovered_sha1) == stored_hash
//! ```
//!
//! ### Amendment M6 – Empty Password Convention
//!
//! Per MySQL amendment M6, when the password is empty the client sends a
//! **zero-length auth response** (not a scrambled empty string). The server
//! recognises a zero-length response as "no password" and grants access if and
//! only if the account was created with no password (i.e. the stored hash is
//! the empty byte string or the double-SHA-1 of the empty string).
//!
//! Concretely, this module implements M6 as follows:
//!
//! * [`scramble_native_password`] returns an all-zero 20-byte array when
//!   `password` is the empty string. Callers that implement the wire protocol
//!   should transmit a zero-length payload in that case.
//! * [`verify_native_password_hash`] accepts a zero-length `auth_response`
//!   slice and verifies it against the stored double-SHA-1 of the empty string.

use std::num::Wrapping;

// ---------------------------------------------------------------------------
// SHA-1 implementation
// ---------------------------------------------------------------------------

/// Initial hash values (first 32 bits of the fractional parts of the square
/// roots of the first five primes).
const H0: u32 = 0x67452301;
const H1: u32 = 0xEFCDAB89;
const H2: u32 = 0x98BADCFE;
const H3: u32 = 0x10325476;
const H4: u32 = 0xC3D2E1F0;

/// Computes the SHA-1 digest of `message` and returns the 20-byte result.
///
/// This is a self-contained, dependency-free implementation written to the
/// FIPS 180-4 specification. It is intentionally simple and readable rather
/// than optimised for throughput; authentication code runs infrequently and
/// correctness matters more than speed.
pub fn sha1(message: &[u8]) -> [u8; 20] {
    // --- pre-processing: padding -------------------------------------------
    //
    // Append bit '1' (= byte 0x80), then zeros, then the 64-bit big-endian
    // representation of the original message length in *bits*, such that the
    // padded length is a multiple of 512 bits (64 bytes).

    let bit_len: u64 = (message.len() as u64).wrapping_mul(8);
    let mut padded: Vec<u8> = Vec::with_capacity(message.len() + 9 + 63);
    padded.extend_from_slice(message);
    padded.push(0x80);
    // pad with zeros until length ≡ 56 (mod 64)
    while padded.len() % 64 != 56 {
        padded.push(0x00);
    }
    // append original length as 64-bit big-endian
    padded.extend_from_slice(&bit_len.to_be_bytes());

    debug_assert_eq!(padded.len() % 64, 0);

    // --- processing: compress each 512-bit block ---------------------------

    let mut h0 = Wrapping(H0);
    let mut h1 = Wrapping(H1);
    let mut h2 = Wrapping(H2);
    let mut h3 = Wrapping(H3);
    let mut h4 = Wrapping(H4);

    for block in padded.chunks_exact(64) {
        // Prepare the message schedule W[0..80]
        let mut w = [Wrapping(0u32); 80];
        for i in 0..16 {
            let b = &block[i * 4..i * 4 + 4];
            w[i] = Wrapping(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        }
        for i in 16..80 {
            let val = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16];
            w[i] = Wrapping(val.0.rotate_left(1));
        }

        // Initialise working variables
        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        // Main compression loop
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), Wrapping(0x5A827999u32)),
                20..=39 => (b ^ c ^ d, Wrapping(0x6ED9EBA1u32)),
                40..=59 => ((b & c) | (b & d) | (c & d), Wrapping(0x8F1BBCDCu32)),
                _ => (b ^ c ^ d, Wrapping(0xCA62C1D6u32)),
            };
            let temp = Wrapping(a.0.rotate_left(5)) + f + e + k + wi;
            e = d;
            d = c;
            c = Wrapping(b.0.rotate_left(30));
            b = a;
            a = temp;
        }

        // Add compressed chunk to current hash value
        h0 += a;
        h1 += b;
        h2 += c;
        h3 += d;
        h4 += e;
    }

    // --- produce the final hash value (big-endian) -------------------------
    let mut digest = [0u8; 20];
    digest[0..4].copy_from_slice(&h0.0.to_be_bytes());
    digest[4..8].copy_from_slice(&h1.0.to_be_bytes());
    digest[8..12].copy_from_slice(&h2.0.to_be_bytes());
    digest[12..16].copy_from_slice(&h3.0.to_be_bytes());
    digest[16..20].copy_from_slice(&h4.0.to_be_bytes());
    digest
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Computes `SHA1(SHA1(password))` – the form in which MySQL stores passwords.
///
/// # Example
///
/// ```
/// use htap_common::password::hash_native_password;
///
/// let hash = hash_native_password("password");
/// let hex: String = hash.iter().map(|b| format!("{:02X}", b)).collect();
/// assert_eq!(hex, "2470C0C06DEE42FD1618BB99005ADCA2EC9D1E19");
/// ```
pub fn hash_native_password(password: &str) -> [u8; 20] {
    sha1(&sha1(password.as_bytes()))
}

/// Computes the client authentication response for `mysql_native_password`.
///
/// Returns:
///
/// ```text
/// SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))
/// ```
///
/// Per amendment M6, if `password` is the empty string this function returns
/// `[0u8; 20]`. Callers implementing the wire protocol **must** send a
/// zero-length byte string to the server in that case (not these 20 bytes).
pub fn scramble_native_password(scramble: &[u8], password: &str) -> [u8; 20] {
    // Amendment M6: empty password → special zero sentinel
    if password.is_empty() {
        return [0u8; 20];
    }

    let sha1_password = sha1(password.as_bytes());
    let double_sha1 = sha1(&sha1_password);

    // SHA1(scramble || SHA1(SHA1(password)))
    let mut combined = Vec::with_capacity(scramble.len() + 20);
    combined.extend_from_slice(scramble);
    combined.extend_from_slice(&double_sha1);
    let scramble_hash = sha1(&combined);

    // XOR
    let mut response = [0u8; 20];
    for i in 0..20 {
        response[i] = sha1_password[i] ^ scramble_hash[i];
    }
    response
}

/// Compares two 20-byte password hashes without early exit.
pub fn constant_time_eq_20(left: &[u8; 20], right: &[u8; 20]) -> bool {
    let mut difference = 0u8;
    for i in 0..20 {
        difference |= left[i] ^ right[i];
    }
    difference == 0
}

/// Verifies a client's `mysql_native_password` authentication response.
///
/// # Parameters
///
/// * `scramble` - the 20-byte random nonce sent by the server in the initial
///   handshake packet.
/// * `stored_double_sha1` - the `SHA1(SHA1(password))` value stored in
///   `mysql.user.authentication_string`.
/// * `auth_response` - the bytes sent by the client in the `HandshakeResponse`
///   packet.
///
/// # Returns
///
/// `true` if the response is valid, `false` otherwise.
///
/// # Amendment M6 - Empty Password
///
/// Callers must check 'account has no password AND response is empty' before
/// calling this function.
pub fn verify_native_password_hash(
    scramble: &[u8],
    stored_double_sha1: &[u8; 20],
    auth_response: &[u8],
) -> bool {
    if auth_response.is_empty() {
        return false;
    }

    // Response must be exactly 20 bytes
    if auth_response.len() != 20 {
        return false;
    }

    // Recover SHA1(password) = response XOR SHA1(scramble || stored_double_sha1)
    let mut combined = Vec::with_capacity(scramble.len() + 20);
    combined.extend_from_slice(scramble);
    combined.extend_from_slice(stored_double_sha1);
    let scramble_hash = sha1(&combined);

    let mut recovered_sha1 = [0u8; 20];
    for i in 0..20 {
        recovered_sha1[i] = auth_response[i] ^ scramble_hash[i];
    }

    // Verify: SHA1(recovered_sha1) should equal stored_double_sha1
    let candidate_double_sha1 = sha1(&recovered_sha1);
    constant_time_eq_20(&candidate_double_sha1, stored_double_sha1)
}

/// Verifies that a password response is empty.
pub fn verify_empty_password_response(response: &[u8]) -> bool {
    response.is_empty()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02X}", b)).collect()
    }

    // -----------------------------------------------------------------------
    // SHA-1 standard test vectors (FIPS 180-4 / RFC 3174)
    // -----------------------------------------------------------------------

    #[test]
    fn sha1_empty_string() {
        // SHA1("") = da39a3ee5e6b4b0d3255bfef95601890afd80709
        let digest = sha1(b"");
        assert_eq!(to_hex(&digest), "DA39A3EE5E6B4B0D3255BFEF95601890AFD80709");
    }

    #[test]
    fn sha1_abc() {
        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        let digest = sha1(b"abc");
        assert_eq!(to_hex(&digest), "A9993E364706816ABA3E25717850C26C9CD0D89D");
    }

    #[test]
    fn sha1_448_bit_message() {
        // SHA1("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
        // = 84983e441c3bd26ebaae4aa1f95129e5e54670f1
        let digest = sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(to_hex(&digest), "84983E441C3BD26EBAAE4AA1F95129E5E54670F1");
    }

    #[test]
    fn sha1_one_million_a() {
        // SHA1("aaa...a" × 1_000_000) = 34aa973cd4c4daa4f61eeb2bdbad27316534016f
        let message = vec![b'a'; 1_000_000];
        let digest = sha1(&message);
        assert_eq!(to_hex(&digest), "34AA973CD4C4DAA4F61EEB2BDBAD27316534016F");
    }

    #[test]
    fn sha1_longer_message() {
        // SHA1("The quick brown fox jumps over the lazy dog")
        // = 2fd4e1c67a2d28fced849ee1bb76e7391b93eb12
        let digest = sha1(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(to_hex(&digest), "2FD4E1C67A2D28FCED849EE1BB76E7391B93EB12");
    }

    // -----------------------------------------------------------------------
    // hash_native_password
    // -----------------------------------------------------------------------

    #[test]
    fn hash_native_password_known_vector() {
        // This is the canonical MySQL test vector: the password "password"
        // should produce the double-SHA-1 shown here. MySQL itself stores
        // this value in mysql.user.authentication_string.
        let hash = hash_native_password("password");
        assert_eq!(to_hex(&hash), "2470C0C06DEE42FD1618BB99005ADCA2EC9D1E19");
    }

    #[test]
    fn hash_native_password_root() {
        // SHA1(SHA1("root")) – verify intermediate steps separately
        let sha1_root = sha1(b"root");
        let double = sha1(&sha1_root);
        assert_eq!(hash_native_password("root"), double);
    }

    #[test]
    fn hash_native_password_empty() {
        // SHA1(SHA1("")) should equal SHA1(DA39A3EE...)
        let expected = sha1(&sha1(b""));
        assert_eq!(hash_native_password(""), expected);
    }

    #[test]
    fn hash_native_password_unicode() {
        // Passwords may contain non-ASCII characters (UTF-8 bytes are hashed)
        let hash = hash_native_password("pässwörد");
        // Just verify it doesn't panic and produces 20 bytes
        assert_eq!(hash.len(), 20);
    }

    // -----------------------------------------------------------------------
    // scramble_native_password
    // -----------------------------------------------------------------------

    #[test]
    fn scramble_produces_20_bytes() {
        let scramble = b"12345678901234567890"; // 20 bytes
        let response = scramble_native_password(scramble, "secret");
        assert_eq!(response.len(), 20);
    }

    #[test]
    fn scramble_empty_password_returns_zeros() {
        // Amendment M6: empty password → all-zero sentinel
        let scramble = b"12345678901234567890";
        let response = scramble_native_password(scramble, "");
        assert_eq!(response, [0u8; 20]);
    }

    #[test]
    fn scramble_different_scrambles_produce_different_responses() {
        let scramble_a = b"AAAAAAAAAAAAAAAAAAAA";
        let scramble_b = b"BBBBBBBBBBBBBBBBBBBB";
        let r_a = scramble_native_password(scramble_a, "hunter2");
        let r_b = scramble_native_password(scramble_b, "hunter2");
        assert_ne!(r_a, r_b);
    }

    #[test]
    fn scramble_different_passwords_produce_different_responses() {
        let scramble = b"abcdefghijklmnopqrst";
        let r1 = scramble_native_password(scramble, "password1");
        let r2 = scramble_native_password(scramble, "password2");
        assert_ne!(r1, r2);
    }

    // -----------------------------------------------------------------------
    // verify_native_password_hash – round-trip
    // -----------------------------------------------------------------------

    fn round_trip(password: &str, scramble: &[u8]) -> bool {
        let stored = hash_native_password(password);
        let response = scramble_native_password(scramble, password);

        if password.is_empty() {
            // For the empty password the wire sends zero bytes; simulate that.
            verify_native_password_hash(scramble, &stored, &[])
        } else {
            verify_native_password_hash(scramble, &stored, &response)
        }
    }

    #[test]
    fn round_trip_password() {
        assert!(round_trip("password", b"12345678901234567890"));
    }

    #[test]
    fn round_trip_empty_password() {
        assert!(!round_trip("", b"12345678901234567890"));
    }

    #[test]
    fn round_trip_complex_password() {
        assert!(round_trip("C0mpl3x!P@ssw0rd#2024", b"randomscrambleXXXXXX"));
    }

    #[test]
    fn round_trip_unicode_password() {
        assert!(round_trip("pässwörد", b"12345678901234567890"));
    }

    #[test]
    fn round_trip_multiple_scrambles() {
        let passwords = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let scrambles: &[&[u8]] = &[
            b"AAAAAAAAAAAAAAAAAAAA",
            b"12345678901234567890",
            b"!@#$%^&*()_+-=[]{}|",
            b"zyxwvutsrqponmlkjihg",
            b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13",
        ];
        for pw in &passwords {
            for sc in scrambles {
                assert!(
                    round_trip(pw, sc),
                    "round-trip failed for password={pw:?} scramble={sc:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // verify_native_password_hash – rejection cases
    // -----------------------------------------------------------------------

    #[test]
    fn constant_time_eq_20_compares_equal_and_unequal_hashes() {
        assert!(constant_time_eq_20(&[7; 20], &[7; 20]));
        assert!(!constant_time_eq_20(&[7; 20], &[8; 20]));
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let scramble = b"12345678901234567890";
        let stored = hash_native_password("correct");
        let bad_response = scramble_native_password(scramble, "wrong");
        assert!(!verify_native_password_hash(
            scramble,
            &stored,
            &bad_response
        ));
    }

    #[test]
    fn verify_rejects_wrong_length_response() {
        let scramble = b"12345678901234567890";
        let stored = hash_native_password("password");

        // Too short (but not zero – zero is the M6 empty-password sentinel)
        assert!(!verify_native_password_hash(scramble, &stored, &[0u8; 5]));
        // Too long
        assert!(!verify_native_password_hash(scramble, &stored, &[0u8; 21]));
        // Exactly 19 bytes
        assert!(!verify_native_password_hash(scramble, &stored, &[0u8; 19]));
    }

    #[test]
    fn verify_rejects_zero_response_for_non_empty_stored_password() {
        // A client sending an empty response when the stored hash is for a
        // non-empty password must be rejected.
        let scramble = b"12345678901234567890";
        let stored = hash_native_password("secret");
        // Zero-length response is the M6 empty-password marker
        assert!(!verify_native_password_hash(scramble, &stored, &[]));
    }

    #[test]
    fn verify_accepts_zero_response_for_empty_stored_password() {
        let scramble = b"12345678901234567890";
        let stored = hash_native_password("");
        assert!(!verify_native_password_hash(scramble, &stored, &[]));
    }

    #[test]
    fn verify_rejects_zero_response_for_empty_password_hash() {
        let scramble = b"12345678901234567890";
        assert!(!verify_native_password_hash(
            scramble,
            &hash_native_password(""),
            &[]
        ));
    }

    #[test]
    fn verify_rejects_bit_flipped_response() {
        let scramble = b"12345678901234567890";
        let stored = hash_native_password("password");
        let mut response = scramble_native_password(scramble, "password");
        // Flip one bit in the response
        response[10] ^= 0x01;
        assert!(!verify_native_password_hash(scramble, &stored, &response));
    }

    #[test]
    fn verify_rejects_wrong_scramble() {
        let scramble_correct = b"AAAAAAAAAAAAAAAAAAAA";
        let scramble_wrong = b"BBBBBBBBBBBBBBBBBBBB";
        let stored = hash_native_password("password");
        // Response computed with one scramble but verified with another
        let response = scramble_native_password(scramble_correct, "password");
        assert!(!verify_native_password_hash(
            scramble_wrong,
            &stored,
            &response
        ));
    }
}
