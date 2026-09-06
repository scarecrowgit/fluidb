// Independent adversarial verification of keycodec order-preservation.
use htap_common::keycodec::{encode_key, encode_key_prefix};
use htap_common::types::Value;

fn check(a: Vec<Value>, b: Vec<Value>, label: &str) {
    let ea = encode_key(&a).unwrap();
    let eb = encode_key(&b).unwrap();
    let logical = a.cmp(&b);
    let bytewise = ea.cmp(&eb);
    assert_eq!(
        logical, bytewise,
        "MISMATCH [{}]: {:?} vs {:?} logical={:?} bytes={:?}",
        label, a, b, logical, bytewise
    );
}

#[test]
fn adversarial_embedded_nulls() {
    // The classic ambiguity: ["a\0b", x] vs ["a", "b", x] shapes.
    check(
        vec![Value::String("a\0b".into()), Value::Int64(1)],
        vec![Value::String("a".into()), Value::Int64(1)],
        "embedded null vs short",
    );
    check(
        vec![Value::String("a\0".into()), Value::Int64(1)],
        vec![Value::String("a".into()), Value::Int64(2)],
        "trailing null",
    );
    check(
        vec![Value::String("\0".into()), Value::Int64(1)],
        vec![Value::String("".into()), Value::Int64(1)],
        "null-only vs empty",
    );
    check(
        vec![Value::Bytes(vec![0, 1]), Value::Int64(1)],
        vec![Value::Bytes(vec![0, 0]), Value::Int64(1)],
        "escape-seq collision",
    );
    check(
        vec![Value::Bytes(vec![0, 0, 0]), Value::Int64(1)],
        vec![Value::Bytes(vec![0]), Value::Int64(1)],
        "multi null",
    );
}

#[test]
fn adversarial_numeric_boundaries() {
    let ints = [
        i64::MIN,
        i64::MIN + 1,
        -2,
        -1,
        0,
        1,
        2,
        i64::MAX - 1,
        i64::MAX,
    ];
    for w in ints.windows(2) {
        check(
            vec![Value::Int64(w[0])],
            vec![Value::Int64(w[1])],
            "i64 boundary",
        );
    }
    let i32s = [i32::MIN, -1, 0, 1, i32::MAX];
    for w in i32s.windows(2) {
        check(
            vec![Value::Int32(w[0])],
            vec![Value::Int32(w[1])],
            "i32 boundary",
        );
    }
    let fs = [
        f64::NEG_INFINITY,
        -1e308,
        -1.0,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.0,
        1e308,
        f64::INFINITY,
    ];
    for w in fs.windows(2) {
        check(
            vec![Value::Float64(w[0])],
            vec![Value::Float64(w[1])],
            "f64 boundary",
        );
    }
}

#[test]
fn adversarial_null_ordering() {
    for v in [
        Value::Bool(false),
        Value::Int32(i32::MIN),
        Value::Int64(i64::MIN),
        Value::Float64(f64::NEG_INFINITY),
        Value::String("".into()),
        Value::Bytes(vec![]),
        Value::Timestamp(i64::MIN),
    ] {
        check(vec![Value::Null], vec![v.clone()], "null first");
    }
}

#[test]
fn adversarial_full_sort_agreement() {
    // Cross-product of many keys; assert full sort orders agree.
    let mut keys: Vec<Vec<Value>> = Vec::new();
    for s in ["", "\0", "a", "a\0", "a\0b", "ab", "b", "\u{7f}", "\u{80}"] {
        for i in [i64::MIN, -1, 0, 1, i64::MAX] {
            keys.push(vec![Value::String(s.into()), Value::Int64(i)]);
        }
        keys.push(vec![Value::Null, Value::Int64(0)]);
        keys.push(vec![Value::String(s.into()), Value::Null]);
    }
    let mut by_logical = keys.clone();
    by_logical.sort();
    let mut by_bytes = keys.clone();
    by_bytes.sort_by_key(|k| encode_key(k).unwrap());
    assert_eq!(by_logical, by_bytes, "full sort order disagreement");
}

#[test]
fn adversarial_prefix_property() {
    // encode_key_prefix of first k components must be a bytewise prefix
    // of the full encode_key of any key starting with those components.
    let full = vec![
        Value::String("abc".into()),
        Value::Int64(42),
        Value::String("xyz".into()),
    ];
    for k in 1..full.len() {
        let pre = encode_key_prefix(&full[..k]).unwrap();
        let all = encode_key(&full).unwrap();
        assert!(
            all.starts_with(&pre),
            "prefix k={} not a bytewise prefix",
            k
        );
    }
    // And with embedded nulls in the prefix component.
    let full2 = vec![Value::String("a\0b".into()), Value::Int64(7)];
    let pre2 = encode_key_prefix(&full2[..1]).unwrap();
    assert!(
        encode_key(&full2).unwrap().starts_with(&pre2),
        "embedded-null prefix broken"
    );
}
