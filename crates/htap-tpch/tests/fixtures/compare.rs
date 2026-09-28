#![allow(dead_code)] // Shared helper module; each test binary uses a different subset.

use htap_common::types::{parse_date_to_timestamp_micros, Row, Value};
use htap_sql::QueryResult;

pub fn decimal(value: i64, precision: u8, scale: u8) -> Value {
    Value::Decimal {
        value,
        precision,
        scale,
    }
}

pub fn date(value: &str) -> Value {
    Value::Timestamp(parse_date_to_timestamp_micros(value).expect("valid fixture date"))
}

fn values_match(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (
            Value::Decimal {
                value: actual_value,
                precision: actual_precision,
                scale: actual_scale,
            },
            Value::Decimal {
                value: expected_value,
                precision: expected_precision,
                scale: expected_scale,
            },
        ) => {
            actual_value == expected_value
                && actual_precision == expected_precision
                && actual_scale == expected_scale
        }
        _ => actual == expected,
    }
}

fn rows_match(actual: &Row, expected: &[Value]) -> anyhow::Result<()> {
    if actual.len() != expected.len() {
        anyhow::bail!(
            "column count mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        );
    }

    for (column_index, (actual_value, expected_value)) in
        actual.values().iter().zip(expected).enumerate()
    {
        match (actual_value, expected_value) {
            (
                Value::Decimal {
                    value: actual_value,
                    precision: actual_precision,
                    scale: actual_scale,
                },
                Value::Decimal {
                    value: expected_value,
                    precision: expected_precision,
                    scale: expected_scale,
                },
            ) if actual_value != expected_value
                || actual_precision != expected_precision
                || actual_scale != expected_scale =>
            {
                anyhow::bail!(
                    "column {column_index} decimal mismatch: expected value {expected_value} \
                     with DECIMAL({expected_precision},{expected_scale}), got value \
                     {actual_value} with DECIMAL({actual_precision},{actual_scale})"
                );
            }
            _ if !values_match(actual_value, expected_value) => {
                anyhow::bail!(
                    "column {column_index} mismatch: expected {expected_value:?}, got {actual_value:?}"
                );
            }
            _ => {}
        }
    }

    Ok(())
}

fn rows_match_multiset(actual: &[&Row], expected: &[Vec<Value>]) -> anyhow::Result<()> {
    if actual.len() != expected.len() {
        anyhow::bail!(
            "row count mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        );
    }

    let mut matched = vec![false; expected.len()];
    for (actual_index, actual_row) in actual.iter().enumerate() {
        let matching_index =
            expected
                .iter()
                .enumerate()
                .find_map(|(expected_index, expected_row)| {
                    (!matched[expected_index] && rows_match(actual_row, expected_row).is_ok())
                        .then_some(expected_index)
                });

        let Some(expected_index) = matching_index else {
            anyhow::bail!(
                "actual row {actual_index} does not match any remaining expected row: {:?}",
                actual_row.values()
            );
        };
        matched[expected_index] = true;
    }

    Ok(())
}

pub fn compare_results(
    actual: &QueryResult,
    expected: &[Vec<Value>],
    ordered: bool,
) -> anyhow::Result<()> {
    let expected_column_count = expected
        .first()
        .map(Vec::len)
        .unwrap_or_else(|| actual.columns.len());

    if actual.columns.len() != expected_column_count {
        anyhow::bail!(
            "result column count mismatch: expected {}, got {}",
            expected_column_count,
            actual.columns.len()
        );
    }

    for (row_index, expected_row) in expected.iter().enumerate() {
        if expected_row.len() != expected_column_count {
            anyhow::bail!(
                "expected row {row_index} has {} columns; expected {expected_column_count}",
                expected_row.len()
            );
        }
    }

    for (row_index, actual_row) in actual.rows.iter().enumerate() {
        if actual_row.len() != actual.columns.len() {
            anyhow::bail!(
                "actual row {row_index} has {} columns; result declares {}",
                actual_row.len(),
                actual.columns.len()
            );
        }
    }

    if actual.rows.len() != expected.len() {
        anyhow::bail!(
            "row count mismatch: expected {}, got {}",
            expected.len(),
            actual.rows.len()
        );
    }

    if ordered {
        for (row_index, (actual_row, expected_row)) in actual.rows.iter().zip(expected).enumerate()
        {
            rows_match(actual_row, expected_row)
                .map_err(|error| anyhow::anyhow!("ordered row {row_index} mismatch: {error}"))?;
        }
    } else {
        let actual_rows: Vec<_> = actual.rows.iter().collect();
        rows_match_multiset(&actual_rows, expected)?;
    }

    Ok(())
}

pub fn compare_ordered_with_ties(
    actual: &QueryResult,
    expected_full: &[Vec<Value>],
    order_key_columns: &[usize],
    limit: usize,
) -> anyhow::Result<()> {
    let expected_column_count = expected_full
        .first()
        .map(Vec::len)
        .unwrap_or_else(|| actual.columns.len());

    if actual.columns.len() != expected_column_count {
        anyhow::bail!(
            "result column count mismatch: expected {}, got {}",
            expected_column_count,
            actual.columns.len()
        );
    }
    if order_key_columns
        .iter()
        .any(|column_index| *column_index >= expected_column_count)
    {
        anyhow::bail!("ORDER BY key column is outside the result");
    }

    let expected_rows = expected_full.len().min(limit);
    if actual.rows.len() != expected_rows {
        anyhow::bail!(
            "row count mismatch: expected {}, got {}",
            expected_rows,
            actual.rows.len()
        );
    }
    if expected_rows == 0 {
        return Ok(());
    }

    let same_key = |left: &[Value], right: &[Value]| {
        order_key_columns
            .iter()
            .all(|column_index| values_match(&left[*column_index], &right[*column_index]))
    };

    let mut groups = Vec::new();
    let mut start = 0;
    while start < expected_full.len() {
        let mut end = start + 1;
        while end < expected_full.len() && same_key(&expected_full[start], &expected_full[end]) {
            end += 1;
        }
        groups.push((start, end));
        start = end;
    }

    let mut actual_offset = 0;
    let mut accumulated = 0;
    for (group_start, group_end) in groups {
        let group_length = group_end - group_start;
        if accumulated + group_length <= expected_rows {
            let actual_group = &actual.rows[actual_offset..actual_offset + group_length];
            let actual_group: Vec<_> = actual_group.iter().collect();
            rows_match_multiset(&actual_group, &expected_full[group_start..group_end])?;
            actual_offset += group_length;
            accumulated += group_length;
            if actual_offset == expected_rows {
                return Ok(());
            }
            continue;
        }

        let actual_group = &actual.rows[actual_offset..];
        for row in actual_group {
            let values = row.values();
            if !same_key(values, &expected_full[group_start]) {
                anyhow::bail!("result contains a row from a later ORDER BY tie group");
            }
        }
        let actual_group: Vec<_> = actual_group.iter().collect();
        let expected_group = &expected_full[group_start..group_end];

        let mut matched = vec![false; expected_group.len()];
        for row in actual_group {
            let matching_index = expected_group
                .iter()
                .enumerate()
                .find_map(|(index, expected)| {
                    (!matched[index] && rows_match(row, expected).is_ok()).then_some(index)
                });
            let Some(index) = matching_index else {
                anyhow::bail!(
                    "result contains a row not present in the straddling ORDER BY tie group: {:?}",
                    row.values()
                );
            };
            matched[index] = true;
        }
        return Ok(());
    }

    Ok(())
}
