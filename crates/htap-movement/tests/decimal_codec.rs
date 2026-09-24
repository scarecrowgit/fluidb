//! Integration tests for decimal handling in CSV and JSONLines movement codecs.

use htap_common::{ColumnDef, DataType, HtapError, Row, Schema, Value};
use htap_movement::{decode_record, encode_record, DataFormat};

fn decimal_schema(precision: u8, scale: u8, nullable: bool) -> Schema {
    Schema::new(vec![ColumnDef {
        name: "amount".into(),
        data_type: DataType::Decimal { precision, scale },
        nullable,
        primary_key: false,
    }])
    .unwrap()
}

fn assert_decimal(value: &Value, expected_value: i64, expected_precision: u8, expected_scale: u8) {
    match value {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, expected_value);
            assert_eq!(*precision, expected_precision);
            assert_eq!(*scale, expected_scale);
        }
        other => panic!("expected decimal value, found {other:?}"),
    }
}

#[test]
fn test_csv_decimal_roundtrip_cases() {
    let schema = decimal_schema(18, 4, false);
    let cases = [
        (12_345_i64, 12_345_i64),
        (-12_345_i64, -12_345_i64),
        (0_i64, 0_i64),
        (12_300_i64, 12_300_i64),
        (999_999_999_999_999_999_i64, 999_999_999_999_999_999_i64),
    ];

    for (input_value, expected_value) in cases {
        let row = Row::new(vec![Value::Decimal {
            value: input_value,
            precision: 18,
            scale: 4,
        }]);

        let encoded = encode_record(&schema, DataFormat::Csv, &row).unwrap();
        let decoded = decode_record(&schema, DataFormat::Csv, &encoded).unwrap();

        assert_decimal(&decoded.values()[0], expected_value, 18, 4);
    }
}

#[test]
fn test_jsonl_decimal_roundtrip_cases_emit_strings() {
    let schema = decimal_schema(18, 4, false);
    let cases = [
        (12_345_i64, "\"1.2345\"", 12_345_i64),
        (-12_345_i64, "\"-1.2345\"", -12_345_i64),
        (0_i64, "\"0.0000\"", 0_i64),
        (12_300_i64, "\"1.2300\"", 12_300_i64),
        (
            999_999_999_999_999_999_i64,
            "\"99999999999999.9999\"",
            999_999_999_999_999_999_i64,
        ),
    ];

    for (input_value, expected_json_value, expected_value) in cases {
        let row = Row::new(vec![Value::Decimal {
            value: input_value,
            precision: 18,
            scale: 4,
        }]);

        let encoded = encode_record(&schema, DataFormat::JsonLines, &row).unwrap();
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(
            text.contains(&format!("\"amount\":{expected_json_value}")),
            "decimal JSON value must be emitted as a string: {text}"
        );

        let decoded = decode_record(&schema, DataFormat::JsonLines, &encoded).unwrap();
        assert_decimal(&decoded.values()[0], expected_value, 18, 4);
    }
}

#[test]
fn test_jsonl_decimal_number_is_rejected_with_column_name() {
    let schema = decimal_schema(8, 2, false);
    let err = decode_record(&schema, DataFormat::JsonLines, br#"{"amount":12.34}"#).unwrap_err();

    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("amount"));
}

#[test]
fn test_null_decimal_roundtrip_through_csv_and_jsonl() {
    let schema = decimal_schema(8, 2, true);
    let row = Row::new(vec![Value::Null]);

    for format in [DataFormat::Csv, DataFormat::JsonLines] {
        assert!(matches!(row.values()[0], Value::Null));

        let encoded = encode_record(&schema, format, &row).unwrap();
        let decoded = decode_record(&schema, format, &encoded).unwrap();

        assert!(matches!(decoded.values()[0], Value::Null));
    }
}

#[test]
fn test_decimal_precision_overflow_is_rejected_by_csv_and_jsonl() {
    let schema = decimal_schema(5, 2, false);

    let csv_err = decode_record(&schema, DataFormat::Csv, b"1000.00").unwrap_err();
    assert!(matches!(csv_err, HtapError::InvalidArgument(_)));
    assert!(csv_err.to_string().contains("exceeds DECIMAL"));

    let json_err =
        decode_record(&schema, DataFormat::JsonLines, br#"{"amount":"1000.00"}"#).unwrap_err();
    assert!(matches!(json_err, HtapError::InvalidArgument(_)));
    assert!(json_err.to_string().contains("exceeds DECIMAL"));
}

#[test]
fn test_decimal_import_rounds_extra_fractional_digits_half_away_from_zero() {
    let schema = decimal_schema(8, 2, false);

    let csv = decode_record(&schema, DataFormat::Csv, b"1.235").unwrap();
    assert_decimal(&csv.values()[0], 124, 8, 2);

    let json = decode_record(&schema, DataFormat::JsonLines, br#"{"amount":"-1.235"}"#).unwrap();
    assert_decimal(&json.values()[0], -124, 8, 2);
}
