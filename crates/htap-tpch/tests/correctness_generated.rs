#[path = "fixtures/compare.rs"]
mod compare;
mod oracle;

use compare::{compare_ordered_with_ties, compare_results};
use htap_common::types::{ColumnDef, DataType, Value};
use htap_sql::result::StatementResult;
use htap_sql::QueryResult;
use oracle::decimal::Dec;
use oracle::load::load_at;

// Q20 needs a larger scale factor because its selective predicates need a wider data search.
const Q20_SCALE_FACTOR: &str = "0.1";
// Q17 is NULL at 0.01 but non-NULL at 0.03.
const Q17_SCALE_FACTOR: &str = "0.03";

fn execute(server: &htap_server::LocalServer, query_number: u8) -> htap_sql::QueryResult {
    execute_at(server, query_number, "0.01")
}

fn execute_at(
    server: &htap_server::LocalServer,
    query_number: u8,
    scale_factor: &str,
) -> htap_sql::QueryResult {
    let sql = htap_tpch::query(query_number, scale_factor).expect("TPC-H query exists");
    match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_dec_div_exact_half_positive() {
    assert_eq!(Dec::integer(1).div(Dec::integer(2)), Dec::new(5_000, 18, 4));
}

#[test]
fn test_dec_div_exact_half_negative() {
    assert_eq!(
        Dec::integer(-1).div(Dec::integer(2)),
        Dec::new(-5_000, 18, 4)
    );
}

#[test]
fn test_dec_avg_exact_half_positive() {
    assert_eq!(
        Dec::avg([Dec::integer(1), Dec::integer(2)]),
        Some(Dec::new(15_000, 18, 4))
    );
}

#[test]
fn test_dec_avg_exact_half_negative() {
    assert_eq!(
        Dec::avg([Dec::integer(-1), Dec::integer(-2)]),
        Some(Dec::new(-15_000, 18, 4))
    );
}

fn synthetic(rows: Vec<Vec<Value>>) -> QueryResult {
    let columns = rows
        .first()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, value)| ColumnDef {
                    name: format!("column_{index}"),
                    data_type: match value {
                        Value::Int64(_) => DataType::Int64,
                        Value::String(_) => DataType::String,
                        _ => DataType::String,
                    },
                    nullable: false,
                    primary_key: false,
                })
                .collect()
        })
        .unwrap_or_default();

    QueryResult::new(
        columns,
        rows.into_iter().map(htap_common::Row::new).collect(),
    )
}

#[test]
fn test_ties_no_ties() {
    let actual = synthetic(vec![vec![Value::Int64(1)], vec![Value::Int64(0)]]);
    let expected = vec![
        vec![Value::Int64(0)],
        vec![Value::Int64(1)],
        vec![Value::Int64(2)],
    ];

    assert!(
        compare_ordered_with_ties(&actual, &expected, &[0], 2).is_err(),
        "non-tied rows must retain their ORDER BY order"
    );
}

#[test]
fn test_ties_all_tied() {
    let actual = synthetic(vec![vec![Value::Int64(2)], vec![Value::Int64(0)]]);
    let expected = vec![
        vec![Value::Int64(0)],
        vec![Value::Int64(1)],
        vec![Value::Int64(2)],
    ];

    compare_ordered_with_ties(&actual, &expected, &[], 2)
        .expect("LIMIT may select any submultiset of one tie group");
}

#[test]
fn test_ties_straddling_duplicates() {
    let actual = synthetic(vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(1), Value::String("second".into())],
    ]);
    let expected = vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(1), Value::String("first".into())],
        vec![Value::Int64(1), Value::String("second".into())],
    ];

    compare_ordered_with_ties(&actual, &expected, &[0], 2)
        .expect("a boundary tie group may contribute any matching member");
}

#[test]
fn test_ties_wrong_multiplicity() {
    let actual = synthetic(vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(1), Value::String("only".into())],
        vec![Value::Int64(1), Value::String("only".into())],
    ]);
    let expected = vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(1), Value::String("only".into())],
        vec![Value::Int64(1), Value::String("other".into())],
    ];

    assert!(
        compare_ordered_with_ties(&actual, &expected, &[0], 2).is_err(),
        "a boundary tie group cannot contain a row more often than expected"
    );
}

#[test]
fn test_ties_reject_row_after_boundary() {
    let actual = synthetic(vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(2), Value::String("after".into())],
    ]);
    let expected = vec![
        vec![Value::Int64(0), Value::String("fixed".into())],
        vec![Value::Int64(1), Value::String("first".into())],
        vec![Value::Int64(1), Value::String("second".into())],
        vec![Value::Int64(2), Value::String("after".into())],
    ];

    assert!(
        compare_ordered_with_ties(&actual, &expected, &[0], 2).is_err(),
        "a row after the LIMIT boundary cannot replace a boundary-tied row"
    );
}

#[test]
#[ignore]
fn test_refresh_rf1_rf2_stream_one_updates_q1_and_q6() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    let mut session = server.open_session().expect("open session");

    htap_tpch::rf1_new_sales(&mut session, "0.01", 1, 42).expect("apply RF1 stream 1");
    htap_tpch::rf2_old_sales(&mut session, "0.01", 1).expect("apply RF2 stream 1");

    let rf2_order_keys = htap_tpch::generate_rf2_plan("0.01", 1).expect("generate RF2 plan");
    let rf1_rows = htap_tpch::generate_rf1_rows("0.01", 1, 42).expect("generate RF1 rows");
    let mut mutated_dataset = dataset.clone();

    mutated_dataset
        .orders
        .retain(|order| !rf2_order_keys.contains(&order.o_orderkey));
    mutated_dataset
        .lineitem
        .retain(|lineitem| !rf2_order_keys.contains(&lineitem.l_orderkey));
    mutated_dataset
        .orders
        .extend(rf1_rows.orders.iter().cloned());
    mutated_dataset
        .lineitem
        .extend(rf1_rows.lineitems.iter().cloned());

    assert_eq!(mutated_dataset.orders.len(), dataset.orders.len());
    assert!(rf1_rows.orders.iter().all(|rf1_order| {
        mutated_dataset
            .orders
            .iter()
            .any(|order| order.o_orderkey == rf1_order.o_orderkey)
    }));
    assert!(rf2_order_keys.iter().all(|rf2_order_key| {
        mutated_dataset
            .orders
            .iter()
            .all(|order| order.o_orderkey != *rf2_order_key)
    }));
    assert!(
        rf1_rows
            .lineitems
            .iter()
            .any(|lineitem| lineitem.l_shipdate.as_str() <= oracle::dates::Q1_SHIPDATE_CUTOFF),
        "RF1 stream 1 must include a Q1-eligible lineitem"
    );

    oracle::q1::verify_coverage(&mutated_dataset).expect("Q1 oracle coverage");
    let actual_q1 = execute(&server, 1);
    compare_results(&actual_q1, &oracle::q1::expected(&mutated_dataset), true)
        .expect("Q1 engine result matches refreshed independent oracle");

    oracle::q6::verify_coverage(&mutated_dataset).expect("Q6 oracle coverage");
    let actual_q6 = execute(&server, 6);
    compare_results(&actual_q6, &oracle::q6::expected(&mutated_dataset), true)
        .expect("Q6 engine result matches refreshed independent oracle");
}

#[test]
#[ignore]
fn test_refresh_rf1_rf2_stream_one_all_queries() {
    let scale_queries: [(&str, &[u8]); 3] = [
        (
            "0.01",
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 19, 21, 22,
            ],
        ),
        (Q17_SCALE_FACTOR, &[17, 18]),
        (Q20_SCALE_FACTOR, &[20]),
    ];
    let mut failures = Vec::new();

    macro_rules! check {
        ($server:expr, $dataset:expr, $scale:expr, $query:expr, $coverage:path, $compare:expr) => {
            match htap_tpch::query($query, $scale) {
                Ok(sql) => match $server.execute(&sql) {
                    Ok(StatementResult::Query(actual)) => {
                        if let Err(error) = $coverage(&$dataset) {
                            failures.push(format!("Q{}: oracle coverage failed: {error}", $query));
                        }
                        if let Err(error) = $compare(&actual, &$dataset) {
                            failures.push(format!("Q{}: {error}", $query));
                        }
                    }
                    Ok(other) => {
                        failures.push(format!("Q{}: expected query result, got {other:?}", $query))
                    }
                    Err(error) => failures.push(format!("Q{}: execute failed: {error}", $query)),
                },
                Err(error) => failures.push(format!("Q{}: query unavailable: {error}", $query)),
            }
        };
    }

    for (scale_factor, expected_queries) in scale_queries {
        let (_directory, server, dataset) = load_at(scale_factor, 42);
        let mut session = server.open_session().expect("open session");

        htap_tpch::rf1_new_sales(&mut session, scale_factor, 1, 42).expect("apply RF1 stream 1");
        session.commit().expect("commit RF1 stream 1");
        htap_tpch::rf2_old_sales(&mut session, scale_factor, 1).expect("apply RF2 stream 1");
        session.commit().expect("commit RF2 stream 1");

        let rf2_order_keys =
            htap_tpch::generate_rf2_plan(scale_factor, 1).expect("generate RF2 plan");
        let rf1_rows =
            htap_tpch::generate_rf1_rows(scale_factor, 1, 42).expect("generate RF1 rows");
        let mut mutated_dataset = dataset.clone();

        mutated_dataset
            .orders
            .retain(|order| !rf2_order_keys.contains(&order.o_orderkey));
        mutated_dataset
            .lineitem
            .retain(|lineitem| !rf2_order_keys.contains(&lineitem.l_orderkey));
        mutated_dataset
            .orders
            .extend(rf1_rows.orders.iter().cloned());
        mutated_dataset
            .lineitem
            .extend(rf1_rows.lineitems.iter().cloned());

        for &query_number in htap_tpch::query_order(0)
            .iter()
            .filter(|query_number| expected_queries.contains(query_number))
        {
            match query_number {
                1 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    1,
                    oracle::q1::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q1::expected(dataset), true)
                    }
                ),
                2 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    2,
                    oracle::q2::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q2::expected(dataset), true)
                    }
                ),
                3 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    3,
                    oracle::q3::verify_coverage,
                    |actual, dataset| {
                        compare_ordered_with_ties(
                            actual,
                            &oracle::q3::expected(dataset),
                            &[1, 2],
                            10,
                        )
                    }
                ),
                4 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    4,
                    oracle::q4::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q4::expected(dataset), true)
                    }
                ),
                5 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    5,
                    oracle::q5::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q5::expected(dataset), true)
                    }
                ),
                6 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    6,
                    oracle::q6::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q6::expected(dataset), true)
                    }
                ),
                7 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    7,
                    oracle::q7::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q7::expected(dataset), true)
                    }
                ),
                8 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    8,
                    oracle::q8::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q8::expected(dataset), true)
                    }
                ),
                9 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    9,
                    oracle::q9::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q9::expected(dataset), true)
                    }
                ),
                10 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    10,
                    oracle::q10::verify_coverage,
                    |actual, dataset| {
                        compare_ordered_with_ties(actual, &oracle::q10::expected(dataset), &[2], 20)
                    }
                ),
                11 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    11,
                    oracle::q11::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q11::expected(dataset), true)
                    }
                ),
                12 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    12,
                    oracle::q12::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q12::expected(dataset), true)
                    }
                ),
                13 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    13,
                    oracle::q13::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q13::expected(dataset), true)
                    }
                ),
                14 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    14,
                    oracle::q14::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q14::expected(dataset), true)
                    }
                ),
                15 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    15,
                    oracle::q15::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q15::expected(dataset), true)
                    }
                ),
                16 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    16,
                    oracle::q16::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q16::expected(dataset), true)
                    }
                ),
                17 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    17,
                    oracle::q17::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q17::expected(dataset), true)
                    }
                ),
                18 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    18,
                    oracle::q18::verify_coverage,
                    |actual, dataset| {
                        compare_ordered_with_ties(
                            actual,
                            &oracle::q18::expected(dataset),
                            &[4, 3],
                            100,
                        )
                    }
                ),
                19 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    19,
                    oracle::q19::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q19::expected(dataset), true)
                    }
                ),
                20 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    20,
                    oracle::q20::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q20::expected(dataset), true)
                    }
                ),
                21 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    21,
                    oracle::q21::verify_coverage,
                    |actual, dataset| {
                        compare_ordered_with_ties(
                            actual,
                            &oracle::q21::expected(dataset),
                            &[1, 0],
                            100,
                        )
                    }
                ),
                22 => check!(
                    server,
                    mutated_dataset,
                    scale_factor,
                    22,
                    oracle::q22::verify_coverage,
                    |actual, dataset| {
                        compare_results(actual, &oracle::q22::expected(dataset), true)
                    }
                ),
                _ => unreachable!("Appendix A contains only TPC-H queries"),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "TPC-H refreshed oracle failures:\n{}",
        failures.join("\n")
    );
}

#[test]
#[ignore]
fn test_q1_pilot() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q1::verify_coverage(&dataset).expect("Q1 oracle coverage");
    let actual = execute(&server, 1);
    compare_results(&actual, &oracle::q1::expected(&dataset), true)
        .expect("Q1 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q2_pilot() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q2::verify_coverage(&dataset).expect("Q2 oracle coverage");
    let actual = execute(&server, 2);
    compare_results(&actual, &oracle::q2::expected(&dataset), true)
        .expect("Q2 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q11_pilot() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q11::verify_coverage(&dataset).expect("Q11 oracle coverage");
    let actual = execute(&server, 11);
    compare_results(&actual, &oracle::q11::expected(&dataset), true)
        .expect("Q11 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q17_pilot() {
    let (_directory, server, dataset) = load_at(Q17_SCALE_FACTOR, 42);
    oracle::q17::verify_coverage(&dataset).expect("Q17 oracle coverage");
    let actual = execute_at(&server, 17, Q17_SCALE_FACTOR);
    compare_results(&actual, &oracle::q17::expected(&dataset), true)
        .expect("Q17 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q3_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q3::verify_coverage(&dataset).expect("Q3 oracle coverage");
    let actual = execute(&server, 3);
    compare_ordered_with_ties(&actual, &oracle::q3::expected(&dataset), &[1, 2], 10)
        .expect("Q3 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q4_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q4::verify_coverage(&dataset).expect("Q4 oracle coverage");
    let actual = execute(&server, 4);
    compare_results(&actual, &oracle::q4::expected(&dataset), true)
        .expect("Q4 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q5_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q5::verify_coverage(&dataset).expect("Q5 oracle coverage");
    let actual = execute(&server, 5);
    compare_results(&actual, &oracle::q5::expected(&dataset), true)
        .expect("Q5 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q6_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q6::verify_coverage(&dataset).expect("Q6 oracle coverage");
    let actual = execute(&server, 6);
    compare_results(&actual, &oracle::q6::expected(&dataset), true)
        .expect("Q6 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q7_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q7::verify_coverage(&dataset).expect("Q7 oracle coverage");
    let actual = execute(&server, 7);
    compare_results(&actual, &oracle::q7::expected(&dataset), true)
        .expect("Q7 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q8_batch2() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q8::verify_coverage(&dataset).expect("Q8 oracle coverage");
    let actual = execute(&server, 8);
    compare_results(&actual, &oracle::q8::expected(&dataset), true)
        .expect("Q8 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q9_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q9::verify_coverage(&dataset).expect("Q9 oracle coverage");
    let actual = execute(&server, 9);
    compare_results(&actual, &oracle::q9::expected(&dataset), true)
        .expect("Q9 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q10_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q10::verify_coverage(&dataset).expect("Q10 oracle coverage");
    let actual = execute(&server, 10);
    compare_ordered_with_ties(&actual, &oracle::q10::expected(&dataset), &[2], 20)
        .expect("Q10 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q12_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q12::verify_coverage(&dataset).expect("Q12 oracle coverage");
    let actual = execute(&server, 12);
    compare_results(&actual, &oracle::q12::expected(&dataset), true)
        .expect("Q12 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q13_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q13::verify_coverage(&dataset).expect("Q13 oracle coverage");
    let actual = execute(&server, 13);
    compare_results(&actual, &oracle::q13::expected(&dataset), true)
        .expect("Q13 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q14_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q14::verify_coverage(&dataset).expect("Q14 oracle coverage");
    let actual = execute(&server, 14);
    compare_results(&actual, &oracle::q14::expected(&dataset), true)
        .expect("Q14 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q15_batch3() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q15::verify_coverage(&dataset).expect("Q15 oracle coverage");
    let actual = execute(&server, 15);
    compare_results(&actual, &oracle::q15::expected(&dataset), true)
        .expect("Q15 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q16_batch4() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q16::verify_coverage(&dataset).expect("Q16 oracle coverage");
    let actual = execute(&server, 16);
    compare_results(&actual, &oracle::q16::expected(&dataset), true)
        .expect("Q16 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q18_batch4() {
    let (_directory, server, dataset) = load_at("0.03", 42);
    oracle::q18::verify_coverage(&dataset).expect("Q18 oracle coverage");
    let actual = execute_at(&server, 18, "0.03");
    compare_ordered_with_ties(&actual, &oracle::q18::expected(&dataset), &[4, 3], 100)
        .expect("Q18 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q19_batch4() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q19::verify_coverage(&dataset).expect("Q19 oracle coverage");
    let actual = execute(&server, 19);
    compare_results(&actual, &oracle::q19::expected(&dataset), true)
        .expect("Q19 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q20_batch4() {
    let (_directory, server, dataset) = load_at(Q20_SCALE_FACTOR, 42);
    oracle::q20::verify_coverage(&dataset).expect("Q20 oracle coverage");
    let actual = execute_at(&server, 20, Q20_SCALE_FACTOR);
    compare_results(&actual, &oracle::q20::expected(&dataset), true)
        .expect("Q20 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q21_batch4() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q21::verify_coverage(&dataset).expect("Q21 oracle coverage");
    let actual = execute(&server, 21);
    compare_ordered_with_ties(&actual, &oracle::q21::expected(&dataset), &[1, 0], 100)
        .expect("Q21 engine result matches independent oracle");
}

#[test]
#[ignore]
fn test_q22_batch4() {
    let (_directory, server, dataset) = load_at("0.01", 42);
    oracle::q22::verify_coverage(&dataset).expect("Q22 oracle coverage");
    let actual = execute(&server, 22);
    compare_results(&actual, &oracle::q22::expected(&dataset), true)
        .expect("Q22 engine result matches independent oracle");
}

/// Oracle result row counts: Q1 4, Q2 100, Q3 10, Q4 5, Q5 5, Q6 1, Q7 4, Q8 2,
/// Q9 175, Q10 20, Q11 1048, Q12 2, Q13 42, Q14 1, Q15 1, Q16 18314, Q17 1,
/// Q18 100, Q19 1, Q20 186, Q21 100, Q22 7.
#[test]
#[ignore]
fn tpch_oracle_all_22_queries() {
    let (_directory_001, server_001, dataset_001) = load_at("0.01", 42);
    let (_directory_003, server_003, dataset_003) = load_at("0.03", 42);
    let (_directory_q20, server_q20, dataset_q20) = load_at(Q20_SCALE_FACTOR, 42);
    let mut failures = Vec::new();

    macro_rules! check {
        ($server:expr, $dataset:expr, $scale:expr, $query:expr, skip_coverage, $compare:expr) => {
            match htap_tpch::query($query, $scale) {
                Ok(sql) => match $server.execute(&sql) {
                    Ok(StatementResult::Query(actual)) => {
                        if let Err(error) = $compare(&actual, &$dataset) {
                            failures.push(format!("Q{}: {error}", $query));
                        }
                    }
                    Ok(other) => {
                        failures.push(format!("Q{}: expected query result, got {other:?}", $query))
                    }
                    Err(error) => failures.push(format!("Q{}: execute failed: {error}", $query)),
                },
                Err(error) => failures.push(format!("Q{}: query unavailable: {error}", $query)),
            }
        };
        ($server:expr, $dataset:expr, $scale:expr, $query:expr, $coverage:path, $compare:expr) => {
            match htap_tpch::query($query, $scale) {
                Ok(sql) => match $server.execute(&sql) {
                    Ok(StatementResult::Query(actual)) => {
                        if let Err(error) = $coverage(&$dataset) {
                            failures.push(format!("Q{}: oracle coverage failed: {error}", $query));
                        }
                        if let Err(error) = $compare(&actual, &$dataset) {
                            failures.push(format!("Q{}: {error}", $query));
                        }
                    }
                    Ok(other) => {
                        failures.push(format!("Q{}: expected query result, got {other:?}", $query))
                    }
                    Err(error) => failures.push(format!("Q{}: execute failed: {error}", $query)),
                },
                Err(error) => failures.push(format!("Q{}: query unavailable: {error}", $query)),
            }
        };
    }

    check!(
        &server_001,
        dataset_001,
        "0.01",
        1,
        oracle::q1::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q1::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        2,
        oracle::q2::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q2::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        3,
        oracle::q3::verify_coverage,
        |actual, dataset| {
            compare_ordered_with_ties(actual, &oracle::q3::expected(dataset), &[1, 2], 10)
        }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        4,
        oracle::q4::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q4::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        5,
        oracle::q5::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q5::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        6,
        skip_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q6::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        7,
        oracle::q7::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q7::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        8,
        oracle::q8::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q8::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        9,
        oracle::q9::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q9::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        10,
        oracle::q10::verify_coverage,
        |actual, dataset| {
            compare_ordered_with_ties(actual, &oracle::q10::expected(dataset), &[2], 20)
        }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        11,
        oracle::q11::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q11::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        12,
        oracle::q12::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q12::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        13,
        oracle::q13::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q13::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        14,
        oracle::q14::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q14::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        15,
        oracle::q15::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q15::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        16,
        oracle::q16::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q16::expected(dataset), true) }
    );
    check!(
        &server_003,
        dataset_003,
        Q17_SCALE_FACTOR,
        17,
        oracle::q17::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q17::expected(dataset), true) }
    );
    check!(
        &server_003,
        dataset_003,
        "0.03",
        18,
        oracle::q18::verify_coverage,
        |actual, dataset| {
            compare_ordered_with_ties(actual, &oracle::q18::expected(dataset), &[4, 3], 100)
        }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        19,
        oracle::q19::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q19::expected(dataset), true) }
    );
    check!(
        &server_q20,
        dataset_q20,
        Q20_SCALE_FACTOR,
        20,
        oracle::q20::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q20::expected(dataset), true) }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        21,
        oracle::q21::verify_coverage,
        |actual, dataset| {
            compare_ordered_with_ties(actual, &oracle::q21::expected(dataset), &[1, 0], 100)
        }
    );
    check!(
        &server_001,
        dataset_001,
        "0.01",
        22,
        oracle::q22::verify_coverage,
        |actual, dataset| { compare_results(actual, &oracle::q22::expected(dataset), true) }
    );

    assert!(
        failures.is_empty(),
        "TPC-H oracle failures:\n{}",
        failures.join("\n")
    );
}
