//! Order-preserving binary encoding for composite index keys.
//!
//! The single property this module exists to provide is:
//!
//! ```text
//! encode_key(a).cmp(&encode_key(b)) == a.cmp(&b)
//! ```
//!
//! where the right-hand side is the lexicographic ordering of the [`Value`]
//! slices. Because the byte encoding sorts exactly like the logical key, the
//! primary index can be a plain byte-ordered map (an ordinary `BTreeMap` in
//! memory, sorted blocks on disk) that serves both point lookups and range
//! scans without ever decoding a key.
//!
//! # Encoding
//!
//! Every component is prefixed with a one-byte NULL marker (`0x00` NULL,
//! `0x01` non-null) so NULL sorts before every non-null value, matching
//! [`Value`]'s ordering. Integers are encoded big-endian with the sign bit
//! flipped, which makes signed numeric order identical to unsigned byte order.
//! Floats are transformed so that [`f64::total_cmp`] order equals byte order.
//! Variable-length components (`String`, `Bytes`) are written raw when they are
//! the final component, and otherwise `0x00`-escaped and terminated.
//!
//! Decimal components encode their signed unscaled `i64` value. Callers must
//! pass decimal values already coerced to the column's declared scale: the
//! scale itself is not encoded. This invariant is enforced by the SQL binder
//! when values are bound, the columnar segment's row validation, and the
//! server's type-equality checks on `INSERT ... SELECT` and `UPDATE`.
//!
//! Conversion and overlay re-encode primary-key values decoded using the
//! segment footer's schema, then compare them with delta keys encoded using
//! the catalog's schema. If those schemas disagree on a decimal scale, an
//! updated row's delta key no longer suppresses its base row, so the row is
//! returned twice. Both artifacts are durable, making this look like a
//! snapshot-isolation failure rather than a schema failure and allowing it to
//! survive restart.
//!
//! # Attribution
//!
//! This design — sign-bit flipping for integers and `0x00` escaping with a
//! terminator for non-final string components — is re-derived from the
//! composite key encoding used by Apache Kudu and adopted by StarRocks, both
//! Apache-2.0 licensed. No code was copied.

use crate::error::{HtapError, Result};
use crate::types::Value;

/// Marker byte written before a NULL component.
const NULL_MARKER: u8 = 0x00;
/// Marker byte written before a non-null component.
const NOT_NULL_MARKER: u8 = 0x01;

/// Escape byte for embedded `0x00` inside non-final variable-length components.
const ESCAPE: u8 = 0x00;
/// Second byte of the `0x00` escape sequence (`0x00 0x01`).
const ESCAPED_ZERO: u8 = 0x01;
/// Second byte of the component terminator (`0x00 0x00`).
const TERMINATOR: u8 = 0x00;

/// Encode a composite key.
///
/// The final element is encoded as the last component, so a trailing
/// `String`/`Bytes` is appended raw (no escaping, no terminator). This keeps
/// keys short and is safe because nothing can follow it.
///
/// # Errors
///
/// Returns [`HtapError::InvalidArgument`] if `values` is empty; a key must have
/// at least one component.
pub fn encode_key(values: &[Value]) -> Result<Vec<u8>> {
    if values.is_empty() {
        return Err(HtapError::InvalidArgument(
            "cannot encode an empty key".into(),
        ));
    }
    let last = values.len() - 1;
    let mut out = Vec::with_capacity(values.len() * 9);
    for (i, v) in values.iter().enumerate() {
        encode_component(&mut out, v, i == last)?;
    }
    Ok(out)
}

/// Encode a key prefix for prefix range scans.
///
/// Every component is treated as non-last, so the result of encoding the first
/// `k` components is always a bytewise prefix of the full [`encode_key`] of any
/// key that starts with those components.
///
/// An empty slice encodes to an empty prefix, which matches every key.
///
/// # Errors
///
/// Currently infallible; returns [`Result`] to mirror [`encode_key`] and to
/// keep the signature stable if validation is added later.
pub fn encode_key_prefix(values: &[Value]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(values.len() * 9);
    for v in values {
        encode_component(&mut out, v, false)?;
    }
    Ok(out)
}

/// Encode one component into `out`.
///
/// `is_last` selects the raw (unterminated) form for variable-length values.
fn encode_component(out: &mut Vec<u8>, value: &Value, is_last: bool) -> Result<()> {
    match value {
        Value::Null => {
            // Nothing follows the marker: NULL is the smallest encoding.
            out.push(NULL_MARKER);
        }
        Value::Bool(v) => {
            out.push(NOT_NULL_MARKER);
            out.push(u8::from(*v));
        }
        Value::Int32(v) => {
            out.push(NOT_NULL_MARKER);
            // Flip the 32-bit sign bit so i32 order == big-endian byte order.
            let biased = (*v as u32) ^ (1u32 << 31);
            out.extend_from_slice(&biased.to_be_bytes());
        }
        Value::Int64(v) => {
            out.push(NOT_NULL_MARKER);
            let biased = (*v as u64) ^ (1u64 << 63);
            out.extend_from_slice(&biased.to_be_bytes());
        }
        Value::Timestamp(v) => {
            out.push(NOT_NULL_MARKER);
            // Microseconds since the epoch: an i64, encoded identically.
            let biased = (*v as u64) ^ (1u64 << 63);
            out.extend_from_slice(&biased.to_be_bytes());
        }
        Value::Float64(v) => {
            out.push(NOT_NULL_MARKER);
            out.extend_from_slice(&encode_f64(*v).to_be_bytes());
        }
        Value::String(v) => {
            out.push(NOT_NULL_MARKER);
            encode_var_len(out, v.as_bytes(), is_last);
        }
        Value::Bytes(v) => {
            out.push(NOT_NULL_MARKER);
            encode_var_len(out, v, is_last);
        }
        Value::Decimal { value, .. } => {
            out.push(NOT_NULL_MARKER);
            let biased = (*value as u64) ^ (1u64 << 63);
            out.extend_from_slice(&biased.to_be_bytes());
        }
    }
    Ok(())
}

/// Map an `f64` onto a `u64` whose unsigned order equals [`f64::total_cmp`].
///
/// Negative floats (sign bit set) get all bits inverted, which both clears the
/// top bit and reverses their magnitude ordering; non-negative floats just get
/// the sign bit set so they sort above every negative one. The result orders
/// `-NaN < -inf < -1.0 < -0.0 < 0.0 < 1.0 < +inf < +NaN`.
#[inline]
fn encode_f64(v: f64) -> u64 {
    let bits = v.to_bits();
    if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits | (1u64 << 63)
    }
}

/// Encode a variable-length component.
///
/// When it is the final component the bytes are written raw: nothing follows,
/// so bytewise comparison of the remainder is already correct.
///
/// Otherwise every `0x00` is escaped as `0x00 0x01` and the component is
/// terminated with `0x00 0x00`. Without this, distinct composite keys could
/// encode identically — `["a\0", "b"]` and `["a", "\0b"]` would both flatten to
/// `a\0b`. Escaping also keeps the order correct: the terminator's second byte
/// (`0x00`) is smaller than an escaped zero's (`0x01`) and smaller than any
/// real byte, so a shorter component sorts before any extension of it.
fn encode_var_len(out: &mut Vec<u8>, bytes: &[u8], is_last: bool) {
    if is_last {
        out.extend_from_slice(bytes);
        return;
    }
    for &b in bytes {
        if b == 0x00 {
            out.push(ESCAPE);
            out.push(ESCAPED_ZERO);
        } else {
            out.push(b);
        }
    }
    out.push(ESCAPE);
    out.push(TERMINATOR);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Encode each key and assert the encodings are strictly ascending.
    fn assert_encodings_ascending(keys: &[Vec<Value>]) {
        let encoded: Vec<Vec<u8>> = keys.iter().map(|k| encode_key(k).unwrap()).collect();
        for w in encoded.windows(2) {
            assert!(
                w[0] < w[1],
                "encodings not ascending:\n  {:02x?}\n  {:02x?}",
                w[0],
                w[1]
            );
        }
    }

    fn single(values: &[Value]) -> Vec<Vec<u8>> {
        values
            .iter()
            .map(|v| encode_key(std::slice::from_ref(v)).unwrap())
            .collect()
    }

    #[test]
    fn test_empty_key_is_rejected() {
        let err = encode_key(&[]).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(encode_key_prefix(&[]).unwrap().is_empty());
    }

    #[test]
    fn test_per_type_ordering_matches_value_ordering() {
        let groups: Vec<Vec<Value>> = vec![
            vec![Value::Bool(false), Value::Bool(true)],
            vec![
                Value::Int32(i32::MIN),
                Value::Int32(-1),
                Value::Int32(0),
                Value::Int32(1),
                Value::Int32(i32::MAX),
            ],
            vec![
                Value::Int64(i64::MIN),
                Value::Int64(-1),
                Value::Int64(0),
                Value::Int64(1),
                Value::Int64(i64::MAX),
            ],
            vec![
                Value::Float64(f64::NEG_INFINITY),
                Value::Float64(-1.0),
                Value::Float64(0.0),
                Value::Float64(1.0),
                Value::Float64(f64::INFINITY),
            ],
            vec![
                Value::String(String::new()),
                Value::String("a".into()),
                Value::String("ab".into()),
                Value::String("b".into()),
                Value::String("ba".into()),
            ],
            vec![
                Value::Bytes(vec![]),
                Value::Bytes(vec![0x00]),
                Value::Bytes(vec![0x01]),
                Value::Bytes(vec![0x01, 0x00]),
                Value::Bytes(vec![0xff]),
            ],
            vec![
                Value::Timestamp(i64::MIN),
                Value::Timestamp(-1),
                Value::Timestamp(0),
                Value::Timestamp(1_700_000_000_000_000),
                Value::Timestamp(i64::MAX),
            ],
        ];

        for group in &groups {
            // Sanity: the input list is sorted per Value ordering.
            let mut sorted = group.clone();
            sorted.sort();
            assert_eq!(&sorted, group, "test input not sorted: {group:?}");

            let keys: Vec<Vec<Value>> = group.iter().cloned().map(|v| vec![v]).collect();
            assert_encodings_ascending(&keys);
        }
    }

    #[test]
    fn test_decimal_equal_values_at_different_scales_encode_differently() {
        let scale_one = Value::Decimal {
            value: 55,
            precision: 2,
            scale: 1,
        };
        let scale_two = Value::Decimal {
            value: 550,
            precision: 3,
            scale: 2,
        };

        // Decimal equality is numeric and deliberately ignores declared scale.
        assert_eq!(scale_one, scale_two);
        assert_ne!(
            encode_key(&[scale_one]).unwrap(),
            encode_key(&[scale_two]).unwrap()
        );
    }

    #[test]
    fn test_decimal_non_final_component_precedes_next_component() {
        let decimal = Value::Decimal {
            value: 550,
            precision: 3,
            scale: 2,
        };
        let key = vec![decimal.clone(), Value::String("next".into())];
        let encoded = encode_key(&key).unwrap();
        let decimal_encoding = encode_key(&[decimal]).unwrap();

        // A decimal is fixed-width, so the next component starts after 9 bytes.
        assert_eq!(&encoded[..decimal_encoding.len()], decimal_encoding);
        assert_eq!(encoded[decimal_encoding.len()], NOT_NULL_MARKER);
        assert_eq!(
            encoded.cmp(&encode_key(&[key[0].clone(), Value::String("later".into())]).unwrap()),
            key.cmp(&vec![key[0].clone(), Value::String("later".into())])
        );
    }

    #[test]
    fn test_decimal_primary_key_spellings_collide() {
        let from_short_spelling = crate::types::parse_decimal_text("5.5", 2).unwrap();
        let from_long_spelling = crate::types::parse_decimal_text("5.50", 2).unwrap();

        assert_eq!(from_short_spelling, 550);
        assert_eq!(from_long_spelling, 550);

        let short = Value::Decimal {
            value: i64::try_from(from_short_spelling).unwrap(),
            precision: 3,
            scale: 2,
        };
        let long = Value::Decimal {
            value: i64::try_from(from_long_spelling).unwrap(),
            precision: 3,
            scale: 2,
        };

        let mut primary_index = std::collections::BTreeMap::new();
        primary_index.insert(encode_key(&[short]).unwrap(), "5.5");
        primary_index.insert(encode_key(&[long]).unwrap(), "5.50");

        assert_eq!(primary_index.len(), 1);
        assert_eq!(primary_index.values().next(), Some(&"5.50"));
    }

    #[test]
    fn test_decimal_encoding_preserves_unscaled_order() {
        let values = single(&[
            Value::Decimal {
                value: -101,
                precision: 5,
                scale: 2,
            },
            Value::Decimal {
                value: -100,
                precision: 5,
                scale: 2,
            },
            Value::Decimal {
                value: 0,
                precision: 5,
                scale: 2,
            },
            Value::Decimal {
                value: 100,
                precision: 5,
                scale: 2,
            },
            Value::Decimal {
                value: 101,
                precision: 5,
                scale: 2,
            },
        ]);

        for pair in values.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn test_integer_sign_boundary() {
        let i64s = single(&[
            Value::Int64(i64::MIN),
            Value::Int64(-1),
            Value::Int64(0),
            Value::Int64(1),
            Value::Int64(i64::MAX),
        ]);
        for w in i64s.windows(2) {
            assert!(w[0] < w[1], "{:02x?} !< {:02x?}", w[0], w[1]);
        }
        // marker + 8 bytes, sign bit flipped.
        assert_eq!(i64s[0], vec![0x01, 0x00, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(i64s[2], vec![0x01, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            i64s[4],
            vec![0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );

        let i32s = single(&[
            Value::Int32(i32::MIN),
            Value::Int32(-1),
            Value::Int32(0),
            Value::Int32(1),
            Value::Int32(i32::MAX),
        ]);
        for w in i32s.windows(2) {
            assert!(w[0] < w[1], "{:02x?} !< {:02x?}", w[0], w[1]);
        }
        // Int32 uses its own 32-bit sign flip and occupies exactly 4 bytes.
        for e in &i32s {
            assert_eq!(e.len(), 5);
        }
        assert_eq!(i32s[0], vec![0x01, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(i32s[2], vec![0x01, 0x80, 0x00, 0x00, 0x00]);
        assert_eq!(i32s[4], vec![0x01, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_float_ordering_and_nan_last() {
        let floats = single(&[
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(-1.0),
            Value::Float64(-0.0),
            Value::Float64(0.0),
            Value::Float64(1.0),
            Value::Float64(f64::INFINITY),
            Value::Float64(f64::NAN),
        ]);
        for w in floats.windows(2) {
            assert!(w[0] < w[1], "{:02x?} !< {:02x?}", w[0], w[1]);
        }
        // Consistent with total_cmp: positive NaN sorts above +inf.
        assert!(f64::INFINITY.total_cmp(&f64::NAN) == std::cmp::Ordering::Less);
    }

    #[test]
    fn test_null_sorts_before_every_non_null() {
        let null = encode_key(&[Value::Null]).unwrap();
        let non_null = [
            Value::Bool(false),
            Value::Int32(i32::MIN),
            Value::Int64(i64::MIN),
            Value::Float64(f64::NEG_INFINITY),
            Value::String(String::new()),
            Value::Bytes(Vec::new()),
            Value::Timestamp(i64::MIN),
        ];
        for v in &non_null {
            let enc = encode_key(std::slice::from_ref(v)).unwrap();
            assert!(
                null < enc,
                "NULL encoding should precede {v:?} -> {enc:02x?}"
            );
            assert!(Value::Null < *v);
        }
        // Also inside a composite, in a non-final position.
        let a = encode_key(&[Value::Null, Value::Int64(0)]).unwrap();
        let b = encode_key(&[Value::String(String::new()), Value::Int64(0)]).unwrap();
        assert!(a < b);
    }

    #[test]
    fn test_embedded_null_disambiguation() {
        // Without escaping + termination both would flatten to `a\0b`.
        let k1 = vec![Value::String("a\0".into()), Value::String("b".into())];
        let k2 = vec![Value::String("a".into()), Value::String("\0b".into())];
        assert_ne!(k1, k2);

        let e1 = encode_key(&k1).unwrap();
        let e2 = encode_key(&k2).unwrap();
        assert_ne!(e1, e2, "encodings collided: {e1:02x?}");
        // Order still agrees with the logical order.
        assert_eq!(e1.cmp(&e2), k1.cmp(&k2));

        // Same for Bytes.
        let b1 = vec![Value::Bytes(vec![0x61, 0x00]), Value::Bytes(vec![0x62])];
        let b2 = vec![Value::Bytes(vec![0x61]), Value::Bytes(vec![0x00, 0x62])];
        assert_ne!(encode_key(&b1).unwrap(), encode_key(&b2).unwrap());
    }

    #[test]
    fn test_composite_ordering_matches_value_slice_ordering() {
        let mut keys: Vec<Vec<Value>> = vec![
            vec![Value::Int64(1), Value::String("b".into())],
            vec![Value::Int64(1), Value::String("a".into())],
            vec![Value::Int64(-1), Value::String("z".into())],
            vec![Value::Null, Value::String("a".into())],
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(i64::MAX), Value::String(String::new())],
            vec![Value::Int64(0), Value::String("a\0b".into())],
            vec![Value::Int64(0), Value::String("ab".into())],
            vec![Value::Null, Value::Null],
        ];

        let mut by_encoding = keys.clone();
        by_encoding.sort_by_cached_key(|k| encode_key(k).unwrap());
        keys.sort();
        assert_eq!(keys, by_encoding);

        // Three columns, mixed types per position.
        let mut triples: Vec<Vec<Value>> = vec![
            vec![
                Value::Bool(true),
                Value::Float64(1.0),
                Value::Bytes(vec![1]),
            ],
            vec![
                Value::Bool(false),
                Value::Float64(f64::NAN),
                Value::Bytes(vec![0]),
            ],
            vec![
                Value::Bool(true),
                Value::Float64(-0.0),
                Value::Bytes(vec![]),
            ],
            vec![
                Value::Bool(true),
                Value::Float64(0.0),
                Value::Bytes(vec![0, 0]),
            ],
            vec![Value::Bool(false), Value::Null, Value::Bytes(vec![0xff])],
        ];
        let mut triples_by_encoding = triples.clone();
        triples_by_encoding.sort_by_cached_key(|k| encode_key(k).unwrap());
        triples.sort();
        assert_eq!(triples, triples_by_encoding);
    }

    #[test]
    fn test_prefix_is_bytewise_prefix_of_full_key() {
        let full = vec![
            Value::Int64(42),
            Value::String("tenant".into()),
            Value::String("trailing string".into()),
        ];
        let encoded = encode_key(&full).unwrap();

        for k in 0..full.len() {
            let prefix = encode_key_prefix(&full[..k]).unwrap();
            assert!(
                encoded.starts_with(&prefix),
                "prefix of {k} components is not a bytewise prefix: {prefix:02x?} vs {encoded:02x?}"
            );
        }

        // A prefix scan bound matches every key sharing the prefix.
        let prefix = encode_key_prefix(&full[..2]).unwrap();
        let sibling = encode_key(&[
            Value::Int64(42),
            Value::String("tenant".into()),
            Value::String("other".into()),
        ])
        .unwrap();
        assert!(sibling.starts_with(&prefix));

        let other_tenant = encode_key(&[
            Value::Int64(42),
            Value::String("tenant2".into()),
            Value::String("x".into()),
        ])
        .unwrap();
        assert!(!other_tenant.starts_with(&prefix));
    }

    // ---- property tests ----

    /// Strings that frequently contain embedded NUL bytes.
    fn any_string() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                3 => Just('\0'),
                7 => prop_oneof![Just('a'), Just('b'), Just('z'), Just('\u{7f}'), Just('é')],
            ],
            0..6usize,
        )
        .prop_map(|cs| cs.into_iter().collect())
    }

    /// Floats including NaN, infinities and signed zeros.
    fn any_float() -> impl Strategy<Value = f64> {
        prop_oneof![
            6 => any::<f64>(),
            1 => Just(f64::NAN),
            1 => Just(-f64::NAN),
            1 => Just(f64::INFINITY),
            1 => Just(f64::NEG_INFINITY),
            1 => Just(0.0f64),
            1 => Just(-0.0f64),
        ]
    }

    /// A pair of values of the same logical type (one column of two keys).
    fn typed_pair() -> impl Strategy<Value = (Value, Value)> {
        prop_oneof![
            (any::<bool>(), any::<bool>()).prop_map(|(a, b)| (Value::Bool(a), Value::Bool(b))),
            (any::<i32>(), any::<i32>()).prop_map(|(a, b)| (Value::Int32(a), Value::Int32(b))),
            (any::<i64>(), any::<i64>()).prop_map(|(a, b)| (Value::Int64(a), Value::Int64(b))),
            (any_float(), any_float()).prop_map(|(a, b)| (Value::Float64(a), Value::Float64(b))),
            (any_string(), any_string()).prop_map(|(a, b)| (Value::String(a), Value::String(b))),
            (
                prop::collection::vec(any::<u8>(), 0..6),
                prop::collection::vec(any::<u8>(), 0..6)
            )
                .prop_map(|(a, b)| (Value::Bytes(a), Value::Bytes(b))),
            (any::<i64>(), any::<i64>())
                .prop_map(|(a, b)| (Value::Timestamp(a), Value::Timestamp(b))),
            (any::<i64>(), any::<i64>()).prop_map(|(a, b)| {
                (
                    Value::Decimal {
                        value: a,
                        precision: 18,
                        scale: 4,
                    },
                    Value::Decimal {
                        value: b,
                        precision: 18,
                        scale: 4,
                    },
                )
            }),
        ]
    }

    /// A nullable column: either value may independently be NULL.
    fn nullable_pair() -> impl Strategy<Value = (Value, Value)> {
        let null_flag = prop_oneof![4 => Just(false), 1 => Just(true)];
        (typed_pair(), null_flag.clone(), null_flag).prop_map(|((a, b), na, nb)| {
            (
                if na { Value::Null } else { a },
                if nb { Value::Null } else { b },
            )
        })
    }

    /// Two equal-length, positionally type-matched keys.
    fn key_pair() -> impl Strategy<Value = (Vec<Value>, Vec<Value>)> {
        prop::collection::vec(nullable_pair(), 1..5usize).prop_map(|cols| cols.into_iter().unzip())
    }

    proptest! {
        // Keep the suite fast; 512 cases is plenty to catch ordering bugs.
        #![proptest_config(ProptestConfig { cases: 512, ..Default::default() })]

        /// The whole point of the module: byte order == logical order.
        #[test]
        fn prop_encoding_order_matches_value_order((a, b) in key_pair()) {
            let ea = encode_key(&a).unwrap();
            let eb = encode_key(&b).unwrap();
            prop_assert_eq!(ea.cmp(&eb), a.cmp(&b), "a={:?} b={:?}", a, b);
        }

        /// Equal keys encode identically, and distinct keys encode distinctly.
        #[test]
        fn prop_encoding_is_injective((a, b) in key_pair()) {
            let ea = encode_key(&a).unwrap();
            let eb = encode_key(&b).unwrap();
            prop_assert_eq!(ea == eb, a == b);
        }

        /// Every proper prefix encodes to a bytewise prefix of the full key.
        #[test]
        fn prop_prefix_is_bytewise_prefix((a, _b) in key_pair()) {
            let full = encode_key(&a).unwrap();
            for k in 0..a.len() {
                let prefix = encode_key_prefix(&a[..k]).unwrap();
                prop_assert!(full.starts_with(&prefix));
            }
        }
    }
}
