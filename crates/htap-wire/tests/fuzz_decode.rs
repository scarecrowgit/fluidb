//! Fuzz-style (deterministic-seed) tests for finding 7 of the Phase 11 fix pass: every decoder
//! reachable straight from network bytes, fed random/adversarial byte strings in a loop, must
//! never panic — only ever return `Err`, since a malformed or hostile payload is exactly the
//! input these functions exist to reject cleanly.
//!
//! Uses a tiny inline deterministic PRNG (splitmix64) rather than a `rand` dependency: the
//! workspace's no-new-dependencies rule applies, and a fixed seed is all a regression test like
//! this needs (byte-for-byte reproducible on failure).

use htap_common::types::{ColumnDef, DataType};
use htap_wire::binary_codec::{decode_binary_row, decode_execute};
use htap_wire::handshake::{ChangeUserRequest, HandshakeResponse41};

/// Deterministic splitmix64 PRNG: fast, dependency-free, and reproducible across runs/platforms.
struct Splitmix64(u64);

impl Splitmix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// A random byte buffer of a random length in `0..max_len`, biased toward short lengths
    /// (which most effectively probe truncated/boundary decoding, exactly like a real
    /// adversarial or simply too-short packet).
    fn random_bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = (self.next_u64() as usize) % max_len;
        let mut buf = vec![0u8; len];
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        buf
    }
}

const ITERATIONS: usize = 20_000;
const SEED: u64 = 0xC0FF_EE15_5EED_0001;

fn columns() -> Vec<ColumnDef> {
    let mk = |name: &str, dt: DataType| ColumnDef {
        name: name.into(),
        data_type: dt,
        nullable: true,
        primary_key: false,
    };
    vec![
        mk("a", DataType::Bool),
        mk("b", DataType::Int32),
        mk("c", DataType::Int64),
        mk("d", DataType::Float64),
        mk("e", DataType::String),
        mk("f", DataType::Bytes),
        mk("g", DataType::Timestamp),
    ]
}

#[test]
fn fuzz_decode_execute_never_panics() {
    let mut rng = Splitmix64(SEED);
    for _ in 0..ITERATIONS {
        let payload = rng.random_bytes(256);
        let num_params = (rng.next_u64() % 8) as u16;
        let long_data_pending = vec![false; num_params as usize];
        let _ = decode_execute(&payload, num_params, None, &long_data_pending);
    }
}

#[test]
fn fuzz_change_user_request_decode_never_panics() {
    let mut rng = Splitmix64(SEED.wrapping_add(1));
    let capability_flag_choices: [u32; 4] = [
        0,
        htap_wire::proto::CLIENT_SECURE_CONNECTION,
        htap_wire::proto::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
        htap_wire::proto::CLIENT_SECURE_CONNECTION | htap_wire::proto::CLIENT_PLUGIN_AUTH,
    ];
    for _ in 0..ITERATIONS {
        let payload = rng.random_bytes(256);
        let caps =
            capability_flag_choices[(rng.next_u64() as usize) % capability_flag_choices.len()];
        let _ = ChangeUserRequest::decode(&payload, caps);
    }
}

#[test]
fn fuzz_handshake_response41_decode_never_panics() {
    let mut rng = Splitmix64(SEED.wrapping_add(2));
    for _ in 0..ITERATIONS {
        let payload = rng.random_bytes(256);
        let _ = HandshakeResponse41::decode(&payload);
    }
}

#[test]
fn fuzz_decode_binary_row_never_panics() {
    let mut rng = Splitmix64(SEED.wrapping_add(3));
    let cols = columns();
    for _ in 0..ITERATIONS {
        let payload = rng.random_bytes(256);
        let _ = decode_binary_row(&payload, &cols);
    }
}
