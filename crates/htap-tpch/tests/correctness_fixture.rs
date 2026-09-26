mod fixture;

use htap_common::types::{parse_date_to_timestamp_micros, Value};
use htap_sql::result::StatementResult;

fn execute_query(query_number: u8) -> htap_sql::QueryResult {
    let server = fixture::load_fixture();
    let sql = htap_tpch::query(query_number, "1").expect("TPC-H query exists");

    match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    }
}

fn decimal(value: i64, precision: u8, scale: u8) -> Value {
    Value::Decimal {
        value,
        precision,
        scale,
    }
}

fn date(value: &str) -> Value {
    Value::Timestamp(parse_date_to_timestamp_micros(value).expect("valid fixture date"))
}

#[test]
fn test_q1() {
    let actual = execute_query(1);

    // Hand-derived by retaining the 19 line items shipped on or before 1998-09-02,
    // partitioning them by return flag and line status, and calculating every sum,
    // average, and count from those partitions. Order 5 line 3, with quantity
    // 20.00 and extended price 2000.00, forms the (A, F) group and is the only row
    // with that return-flag and line-status combination. This exercises the date
    // boundary: the 1998-09-02 rows are inside while the 1998-09-03 rows are
    // outside. It has no ties, outer-join-preserved rows, NULL aggregates, or
    // empty groups.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("A".into()),
            Value::String("F".into()),
            decimal(2_000, 18, 2),
            decimal(200_000, 18, 2),
            decimal(18_800_000, 18, 4),
            decimal(1_917_600_000, 18, 6),
            decimal(20_000_000, 18, 6),
            decimal(2_000_000_000, 18, 6),
            decimal(60_000, 18, 6),
            Value::Int64(1),
        ],
        vec![
            Value::String("N".into()),
            Value::String("F".into()),
            decimal(4_000, 18, 2),
            decimal(40_000, 18, 2),
            decimal(4_000_000, 18, 4),
            decimal(400_000_000, 18, 6),
            decimal(40_000_000, 18, 6),
            decimal(400_000_000, 18, 6),
            decimal(0, 18, 6),
            Value::Int64(1),
        ],
        vec![
            Value::String("N".into()),
            Value::String("O".into()),
            decimal(70_000, 18, 2),
            decimal(700_000, 18, 2),
            decimal(68_240_000, 18, 4),
            decimal(6_824_000_000, 18, 6),
            decimal(87_500_000, 18, 6),
            decimal(875_000_000, 18, 6),
            decimal(18_750, 18, 6),
            Value::Int64(8),
        ],
        vec![
            Value::String("R".into()),
            Value::String("F".into()),
            decimal(92_000, 18, 2),
            decimal(920_000, 18, 2),
            decimal(87_870_000, 18, 4),
            decimal(8_808_420_000, 18, 6),
            decimal(115_000_000, 18, 6),
            decimal(1_150_000_000, 18, 6),
            decimal(58_750, 18, 6),
            Value::Int64(8),
        ],
        vec![
            Value::String("R".into()),
            Value::String("O".into()),
            decimal(3_000, 18, 2),
            decimal(30_000, 18, 2),
            decimal(2_850_000, 18, 4),
            decimal(313_500_000, 18, 6),
            decimal(30_000_000, 18, 6),
            decimal(300_000_000, 18, 6),
            decimal(50_000, 18, 6),
            Value::Int64(1),
        ],
    ];

    fixture::compare_results(&actual, expected, true)
        .expect("Q1 result matches hand-derived result");
}

#[test]
fn test_q2() {
    let actual = execute_query(2);

    // Hand-derived by restricting parts to size-15 BRASS parts and suppliers to
    // EUROPE, then taking each part's minimum European supply cost. Part 1 has
    // two suppliers tied at 10.00, so both survive; part 2 selects supplier 2 at
    // 20.00. This exercises minimum-cost ties, but not outer joins, NULL
    // aggregates, boundary values, or empty groups.
    let expected: &[Vec<Value>] = &[
        vec![
            decimal(700_000, 15, 2),
            Value::String("Supp Germany A".into()),
            Value::String("GERMANY".into()),
            Value::Int64(1),
            Value::String("MFGR#1".into()),
            Value::String("Address 1".into()),
            Value::String("13-000-000-0000".into()),
            Value::String("reliable".into()),
        ],
        vec![
            decimal(700_000, 15, 2),
            Value::String("Supp Germany B".into()),
            Value::String("GERMANY".into()),
            Value::Int64(1),
            Value::String("MFGR#1".into()),
            Value::String("Address 2".into()),
            Value::String("31-000-000-0000".into()),
            Value::String("reliable".into()),
        ],
        vec![
            decimal(700_000, 15, 2),
            Value::String("Supp Germany B".into()),
            Value::String("GERMANY".into()),
            Value::Int64(2),
            Value::String("MFGR#1".into()),
            Value::String("Address 2".into()),
            Value::String("31-000-000-0000".into()),
            Value::String("reliable".into()),
        ],
    ];

    fixture::compare_results(&actual, expected, true)
        .expect("Q2 result matches hand-derived result");
}

#[test]
fn test_q3() {
    let actual = execute_query(3);

    // Hand-derived by selecting BUILDING customers, orders strictly before
    // 1995-03-15, and line items strictly after that date. Order 3 contributes
    // 500.00 * 0.90 = 450.0000 and order 2 contributes 400.00 * 1.00 =
    // 400.0000. This exercises one-inside/one-outside boundary rows on both
    // date predicates; it has no ties, outer joins, NULL aggregates, or empty groups.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::Int64(3),
            decimal(4_500_000, 18, 4),
            date("1995-03-14"),
            Value::Int32(0),
        ],
        vec![
            Value::Int64(2),
            decimal(4_000_000, 18, 4),
            date("1994-06-01"),
            Value::Int32(0),
        ],
    ];

    fixture::compare_results(&actual, expected, true)
        .expect("Q3 result matches hand-derived result");
}

#[test]
fn test_q4() {
    let actual = execute_query(4);

    // Hand-derived by considering orders from 1993-07-01 through 1993-09-30:
    // only order 1 is in that interval, and its first line has commit date before
    // receipt date, so priority 1-URGENT has count one. This exercises the
    // half-open date boundary and confirms priorities with no qualifying orders
    // remain empty groups; it has no ties, outer joins, or NULL aggregates.
    let expected: &[Vec<Value>] = &[vec![Value::String("1-URGENT".into()), Value::Int64(1)]];

    fixture::compare_results(&actual, expected, true)
        .expect("Q4 result matches hand-derived result");
}

#[test]
fn test_multiset_validation() {
    let server = fixture::load_fixture();
    let sql = "SELECT r_regionkey FROM region WHERE r_regionkey IN (0, 1) ORDER BY r_regionkey";

    let actual = match server.execute(sql).expect("execute region query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[vec![Value::Int64(0)], vec![Value::Int64(0)]];

    assert!(
        fixture::compare_results(&actual, expected, false).is_err(),
        "unordered comparison must reject a wrong multiset"
    );
}

#[test]
fn test_q6() {
    let actual = execute_query(6);

    // Hand-derived by checking every 1994 shipment against discount 0.05 through
    // 0.07 inclusive and quantity below 24. Order 5 line 3 is the single
    // qualifying row: its return flag is A, line status is F, quantity is 20.00,
    // extended price is 2000.00, and discount is 0.06. Its discount is within
    // the inclusive 0.05-0.07 range, its quantity is less than 24, and its
    // shipdate is in 1994.
    //
    // The expected decimal's value part is 1200000. The contribution is derived
    // from extended_price * discount = 2000.00 * 0.06 = 120.00. Multiplying
    // the scale-2 extended price by the scale-2 discount yields a result with
    // scale 2 + 2 = 4, so 120.00 is represented by the value part 1200000.
    // The other 1994 line items remain present and excluded from the
    // sum: two fail on quantity and one fails on discount, so the exclusion side
    // of each boundary is still exercised. A boundary that wrongly admitted one
    // of them would change the sum and fail the test.
    //
    // Note: The empty-aggregate case (NULL sum for an empty group) that this test
    // used to cover is no longer covered here, so nobody assumes it is.
    let expected: &[Vec<Value>] = &[vec![decimal(1_200_000, 18, 4)]];

    fixture::compare_results(&actual, expected, true)
        .expect("Q6 result matches hand-derived result");
}

#[test]
fn test_multiset_validation_accepts_permuted_rows() {
    let server = fixture::load_fixture();
    let sql = "SELECT r_regionkey FROM region WHERE r_regionkey IN (0, 1) ORDER BY r_regionkey";

    let actual = match server.execute(sql).expect("execute region query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[vec![Value::Int64(1)], vec![Value::Int64(0)]];

    fixture::compare_results(&actual, expected, false)
        .expect("unordered comparison accepts the same rows in a different order");
}

#[test]
fn test_multiset_validation_rejects_decimal_precision_mismatch() {
    let actual = execute_query(6);
    let expected: &[Vec<Value>] = &[vec![decimal(1_200_000, 17, 4)]];

    assert!(
        fixture::compare_results(&actual, expected, false).is_err(),
        "unordered comparison must reject a decimal with different precision"
    );
}
