mod fixtures;

use fixtures::compare::{compare_results, date, decimal};
use htap_common::types::Value;
use htap_sql::result::StatementResult;

fn execute_query(query_number: u8) -> htap_sql::QueryResult {
    let (_directory, server) = fixtures::correctness::load_fixture();
    let sql = htap_tpch::query(query_number, "1").expect("TPC-H query exists");

    match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    }
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

    compare_results(&actual, expected, true).expect("Q1 result matches hand-derived result");
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

    compare_results(&actual, expected, true).expect("Q2 result matches hand-derived result");
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

    compare_results(&actual, expected, true).expect("Q3 result matches hand-derived result");
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

    compare_results(&actual, expected, true).expect("Q4 result matches hand-derived result");
}

#[test]
fn test_q13() {
    let (_directory, server) = fixtures::outer_join::load_fixture();
    let sql = htap_tpch::query(13, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Customer 1 has two orders, customer 2 has one, customer 3 has none,
    // customer 4 has one order excluded by the ON clause but is preserved by the
    // left join, and customer 5 has two orders with one excluded. The ON-clause
    // exclusion prevents matching "special ... requests" orders without removing
    // their customers. The tie at custdist 2 for c_count values 1 and 0 proves
    // the secondary sort orders the tied distributions by c_count descending.
    let expected: &[Vec<Value>] = &[
        vec![Value::Int64(1), Value::Int64(2)],
        vec![Value::Int64(0), Value::Int64(2)],
        vec![Value::Int64(2), Value::Int64(1)],
    ];

    compare_results(&actual, expected, true).expect("Q13 result matches outer-join fixture result");
}

#[test]
fn test_q11() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(11, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // The German parts have values 100000.00 (part 100), 80000.00 (part 101),
    // and 1.00 (part 102). Their total is 180001.00, so the scalar subquery's
    // 0.01% threshold is approximately 18.0001: parts 100 and 101 pass it,
    // while part 102 fails.
    //
    // The large French decoy, supplier 3's part 103, has value 800000000.00
    // (ps_supplycost 10000.00 * ps_availqty 80000). It proves that the nation
    // filter is applied both in the outer query and inside the scalar subquery.
    // If the outer filter alone were missing, part 103 would appear in the outer
    // output and change the result. If the subquery filter alone were missing,
    // the total would become 800180001.00 and the threshold approximately
    // 80018.00; part 101, with value 80000.00, would drop out, leaving only
    // part 100 rather than the correct output of parts 100 and 101.
    let expected: &[Vec<Value>] = &[
        vec![Value::Int64(100), decimal(10_000_000, 18, 2)],
        vec![Value::Int64(101), decimal(8_000_000, 18, 2)],
    ];

    compare_results(&actual, expected, true).expect("Q11 result matches join fixture result");
}

#[test]
fn test_q12() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(12, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("MAIL".into()),
            Value::Int64(2),
            Value::Int64(0),
        ],
        vec![
            Value::String("SHIP".into()),
            Value::Int64(0),
            Value::Int64(1),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q12 result matches join fixture result");
}

#[test]
fn test_q9() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(9, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived from green part 301 only. For FRANCE in 1995, the two lines
    // from supplier 3 contribute 100.00 * (1 - 0.00) - 10.00 * 1.00 = 90.0000
    // and 200.00 * (1 - 0.00) - 10.00 * 1.00 = 190.0000, totaling 280.0000.
    // FRANCE in 1994 contributes 50.00 - 10.00 = 40.0000; its order date
    // determines the extracted year even though its lineitem dates are in 1995
    // to keep it outside Q12's receipt-date range. ITALY in 1995 uses supplier
    // 4's distinct 20.00 cost, contributing 100.00 - 20.00 = 80.0000.
    // The 999.00 line for non-green part 302 is excluded by the part-name filter.
    // Within FRANCE, 1995 precedes 1994 because the query orders years descending.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("FRANCE".into()),
            Value::Int64(1995),
            decimal(2_800_000, 18, 4),
        ],
        vec![
            Value::String("FRANCE".into()),
            Value::Int64(1994),
            decimal(400_000, 18, 4),
        ],
        vec![
            Value::String("ITALY".into()),
            Value::Int64(1995),
            decimal(800_000, 18, 4),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q9 result matches join fixture result");
}

#[test]
fn test_q10() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(10, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[
        vec![
            Value::Int64(102),
            Value::String("Customer France".into()),
            decimal(3_800_000, 18, 4),
            decimal(200_000, 15, 2),
            Value::String("FRANCE".into()),
            Value::String("Address 102".into()),
            Value::String("31-102-000-0000".into()),
            Value::String("customer".into()),
        ],
        vec![
            Value::Int64(101),
            Value::String("Customer Germany".into()),
            decimal(3_300_000, 18, 4),
            decimal(100_000, 15, 2),
            Value::String("GERMANY".into()),
            Value::String("Address 101".into()),
            Value::String("13-101-000-0000".into()),
            Value::String("customer".into()),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q10 result matches join fixture result");
}

#[test]
fn test_q14() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(14, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[vec![decimal(500_000_000_000, 18, 10)]];

    compare_results(&actual, expected, true).expect("Q14 result matches join fixture result");
}

#[test]
fn test_q15() {
    let (_directory, server) = fixtures::join::load_fixture();
    let sql = htap_tpch::query(15, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived over shipments in [1996-01-01, 1996-04-01). Supplier 10 has
    // one line worth 1000.00 * (1 - 0.00) = 1000.0000. Supplier 11 has two
    // lines worth 600.00 * (1 - 0.00) = 600.0000 and 500.00 * (1 - 0.20) =
    // 400.0000, also totaling 1000.0000. Supplier 12 totals only 900.0000;
    // its 99999.00 line ships exactly on 1996-04-01 and is excluded. Both
    // suppliers tied at the maximum must survive, ordered by supplier key.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::Int64(10),
            Value::String("Q15 Single Line Supplier".into()),
            Value::String("Address 10".into()),
            Value::String("33-010-000-0000".into()),
            decimal(10_000_000, 18, 4),
        ],
        vec![
            Value::Int64(11),
            Value::String("Q15 Multi Line Supplier".into()),
            Value::String("Address 11".into()),
            Value::String("39-011-000-0000".into()),
            decimal(10_000_000, 18, 4),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q15 result matches join fixture result");
}

#[test]
fn test_q16() {
    let (_directory, server) = fixtures::anti_join::load_fixture();
    let sql = htap_tpch::query(16, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived from the two qualifying part groups. Parts 100 and 101 share
    // Brand#10, SMALL STEEL, and size 14. Their qualifying supplier keys are 1,
    // 2, 4, and 9: supplier 2 appears for both parts but counts once because the
    // query uses COUNT(DISTINCT ps_suppkey). Supplier 3 is excluded because its
    // comment contains "Customer" before "Complaints", matching the correlated
    // NOT IN subquery's '%Customer%Complaints%' predicate. Supplier 9 remains:
    // its reversed "Complaints Customer" text does not match that ordered pattern.
    //
    // Part 200 forms the other qualifying group, Brand#20, BIG WOOD, size 49,
    // with the two distinct suppliers 5 and 6. The output ordering is by brand,
    // type, and size, so the Brand#10 group precedes Brand#20.
    //
    // Every filter is load-bearing. Part 102 is excluded by p_brand <> 'Brand#45';
    // part 103 is excluded by p_type NOT LIKE 'MEDIUM POLISHED%'; and part 104 is
    // excluded because size 99 is outside the explicit size list. The supplier
    // anti-join is also load-bearing: admitting supplier 3 would raise the first
    // group's count from four to five, while matching the comment words in either
    // order would incorrectly exclude supplier 9 and reduce it to three. Finally,
    // using a non-distinct count would count supplier 2 twice and produce five
    // for the first group.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("Brand#10".into()),
            Value::String("SMALL STEEL".into()),
            Value::Int32(14),
            Value::Int64(4),
        ],
        vec![
            Value::String("Brand#20".into()),
            Value::String("BIG WOOD".into()),
            Value::Int32(49),
            Value::Int64(2),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q16 result matches anti-join fixture result");
}

#[test]
fn test_q17() {
    let (_directory, server) = fixtures::quantity_threshold::load_q17_fixture();
    let sql = htap_tpch::query(17, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // The full query returns 10.000000. Part 1701 matches both the Brand#23
    // and MED BOX predicates. Its correlated average quantity is (1.00 + 9.00 +
    // 20.00) / 3 = 10.000000, making the 0.2 threshold 2.000000; therefore,
    // only its quantity-1.00 line qualifies. Its 70.00 extended price divided
    // by 7.0 is exactly 10.000000.
    //
    // Each filter is load-bearing, as shown by reduced queries:
    // - Without the brand filter, parts 1701 and 1702 contribute, returning
    //   20.000000.
    // - Without the container filter, parts 1701 and 1703 contribute, also
    //   returning 20.000000.
    // - Without the quantity threshold, all three lines of part 1701 contribute,
    //   returning 51.43.
    //
    // Part 1704's quantity 1000.00 proves the average must remain correlated per
    // part: replacing it with a global average admits all three part-1701 lines
    // and returns 51.43, whereas the correlated per-part average returns
    // 10.000000.
    let expected: &[Vec<Value>] = &[vec![decimal(10_000_000, 18, 6)]];

    compare_results(&actual, expected, true)
        .expect("Q17 result matches quantity threshold fixture");
}

#[test]
fn test_q17_zero_qualifying_rows_is_null() {
    let (_directory, server) = fixtures::quantity_threshold::load_q17_empty_fixture();
    let sql = htap_tpch::query(17, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // The only line has quantity 10.00 and a correlated threshold of 2.000000,
    // so no line qualifies. The aggregate therefore remains NULL, not zero.
    // Without the quantity condition, the actual output row is 70.000000 from
    // 490.00 / 7.0.
    let expected: &[Vec<Value>] = &[vec![Value::Null]];

    compare_results(&actual, expected, true)
        .expect("Q17 empty qualifying set produces a NULL aggregate");
}

#[test]
fn test_q18() {
    let (_directory, server) = fixtures::quantity_threshold::load_fixture();
    let sql = htap_tpch::query(18, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived line-item totals are: order 1801 = 150.50 + 150.00 =
    // 300.50; order 1802 = 200.00 + 100.00 = 300.00; order 1803 = 150.00 +
    // 149.99 = 299.99; and order 1804 = 75.00 + 125.00 = 200.00. Thus the
    // strict HAVING predicate admits only 1801. Neither of its individual lines
    // exceeds 300, so evaluating the threshold per line would incorrectly return
    // no row. Its selected values are the customer and order declarations:
    // "Q18 Qualifying Customer", 101, 1801, 1995-03-15, and 900.50; its grouped
    // quantity is 300.50. The declared order price is DECIMAL(15,2), while the
    // SUM of DECIMAL(15,2) quantities is DECIMAL(18,2).
    //
    // Every fixture row was checked against each reduced query:
    // - Without HAVING sum(l_quantity) > 300, the grouped output rows are
    //   (101, 1801), (102, 1802), (103, 1803), and (104, 1804). Customer 201
    //   has no order, so it cannot join; all four orders have line items.
    // - Without c_custkey = o_custkey, the IN subquery still admits only 1801
    //   and its line join still supplies its two lines. The output rows are
    //   (101, 1801), (102, 1801), (103, 1801), (104, 1801), and (201, 1801).
    //   In particular, unmatched customer 201 appears despite not owning 1801.
    // - Without o_orderkey = l_orderkey, the IN subquery still admits only 1801
    //   and the customer-order equality still selects customer 101. The sole
    //   output row remains (101, 1801), but its sum is incorrectly the total of
    //   every fixture line: 300.50 + 300.00 + 299.99 + 200.00 = 1100.49.
    //   Order 1804's own independent subquery group is only 200.00, so it cannot
    //   be re-admitted by that scan; its lines affect this reduced query solely
    //   because the outer order-lineitem equality was removed.
    let expected: &[Vec<Value>] = &[vec![
        Value::String("Q18 Qualifying Customer".into()),
        Value::Int64(101),
        Value::Int64(1801),
        date("1995-03-15"),
        decimal(90_050, 15, 2),
        decimal(30_050, 18, 2),
    ]];

    compare_results(&actual, expected, true)
        .expect("Q18 result matches quantity threshold fixture");
}

#[test]
fn test_q19() {
    let (_directory, server) = fixtures::part_predicates::load_q19_fixture();
    let sql = htap_tpch::query(19, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // The three OR-joined brackets contribute independently: Brand#12 small
    // witness 3101 contributes 100.0000, Brand#23 medium witness 3102
    // contributes 200.0000, and Brand#34 large witness 3103 contributes
    // 300.0000, for total revenue 600.0000. Removing clauses one, two, or three
    // would respectively leave 500.0000, 400.0000, or 300.0000; each witness
    // fails the other two clauses on brand.
    //
    // The join predicate is also load-bearing. Removing it creates a 36-pair
    // Cartesian product for each bracket. The resulting full-query sums are
    // 1217.0000 for bracket one, 1987.0000 for bracket two, and 2757.0000 for
    // bracket three, rather than the correct 600.0000 total.
    let expected: &[Vec<Value>] = &[vec![decimal(6_000_000, 18, 4)]];

    compare_results(&actual, expected, true).expect("Q19 result matches part predicate fixture");
}

#[test]
fn test_q20() {
    let (_directory, server) = fixtures::part_predicates::load_fixture();
    let sql = htap_tpch::query(20, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // This is the first correctness test of the published Q20 text, including
    // its date() function syntax. The shipment window is [1994-01-01,
    // 1995-01-01), tested through qualifying part-supplier pairs. Supplier 2001
    // qualifies through parts 2001 and 2005; supplier 2002 qualifies through
    // part 2004; supplier 2003 qualifies only through lower-boundary part 2006;
    // and supplier 2005 qualifies only through upper-boundary part 2007.
    //
    // All seven clauses are load-bearing when removed alone. Without p_name LIKE
    // 'forest%', supplier 2006 is added because its part 2002 name decoy then
    // qualifies. Without the strict > comparison, supplier 2007 is added because
    // part 2003's available quantity 10 equals its half-shipment sum of 10.
    // Without n_name = 'CANADA', supplier 2004 is added. Without the
    // l_suppkey = ps_suppkey correlation, supplier 2002 is excluded. Without the
    // l_partkey = ps_partkey correlation, supplier 2001 is excluded. Relaxing the
    // lower date bound to include the preceding boundary excludes supplier 2003:
    // part 2006's shipment sum doubles to 20 while its available quantity 11
    // fails. Relaxing the upper date bound to include the following boundary
    // similarly excludes supplier 2005: part 2007's shipment sum doubles to 20
    // while its available quantity 11 fails.
    //
    // Both boundary decoys retain in-window lines, on 1994-01-01 for part 2006
    // and 1994-12-31 for part 2007, so neither shipment sum is ever NULL.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("Q20 Canadian Decoy Supplier".into()),
            Value::String("Address 2003".into()),
        ],
        vec![
            Value::String("Q20 Other Canadian Supplier".into()),
            Value::String("Address 2002".into()),
        ],
        vec![
            Value::String("Q20 Qualifying Supplier".into()),
            Value::String("Address 2001".into()),
        ],
        vec![
            Value::String("Q20 Upper Boundary Supplier".into()),
            Value::String("Address 2005".into()),
        ],
    ];

    compare_results(&actual, expected, true).expect("Q20 result matches part predicate fixture");
}

#[test]
fn test_q21() {
    let (_directory, server) = fixtures::anti_join::load_fixture();
    let sql = htap_tpch::query(21, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived from order 2101 only. Its two late lines both belong to
    // Supplier 1, so they each contribute one to that supplier's count. Supplier
    // 2's companion is on time, satisfying EXISTS because it is a different
    // supplier while allowing NOT EXISTS because no different supplier is late.
    // The second same-supplier late line must not satisfy either alias-inequality
    // predicate as another supplier.
    //
    // Without EXISTS: Supplier 1 (count 2), Supplier 12 (count 1).
    // Without NOT EXISTS: Supplier 1 (count 2), Supplier 4 (count 1), Supplier 5 (count 1).
    // Without o_orderstatus='F': Supplier 1 (count 2), Supplier 6 (count 1).
    // Without l_receiptdate > l_commitdate: Supplier 1 (count 2), Supplier 6 (count 1), Supplier 9 (count 1).
    // Without n_name='SAUDI ARABIA': Supplier 1 (count 2), Supplier 11 (count 1).
    //
    // Therefore Supplier 1 is the sole result with numwait 2.
    let expected: &[Vec<Value>] = &[vec![Value::String("Supplier 1".into()), Value::Int64(2)]];

    compare_results(&actual, expected, true).expect("Q21 result matches anti-join fixture result");
}

#[test]
fn test_q22() {
    let (_directory, server) = fixtures::customer_avg::load_fixture();
    let sql = htap_tpch::query(22, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // The five customers are: 101 with listed prefix 13, balance 500.00, and no
    // orders; 102 with listed prefix 13, balance 300.00, and no orders; 103 with
    // listed prefix 31, balance -400.00, and no orders; 104 with listed prefix
    // 23, balance 600.00, and order 1001; and 105 with unlisted prefix 14,
    // balance 700.00, and no orders.
    //
    // The averaging subquery includes only positive-balance customers with listed
    // prefixes: 101 (500.00), 102 (300.00), and 104 (600.00). Its exact
    // DECIMAL(18,6) average is 1400 / 3 = 466.666666. Only customer 101 has a
    // listed prefix, exceeds that average, and has no order, producing prefix 13,
    // count 1, and balance sum 500.00.
    //
    // Every clause is load-bearing: without the main balance comparison, rows 101,
    // 102, and 103 qualify; without NOT EXISTS, rows 101 and 104 qualify; without
    // the outer prefix filter, rows 101, 104, and 105 qualify. Without the
    // averaging subquery's positivity filter, -400.00 lowers the average to
    // 1000 / 4 = 250.000000, so rows 101 and 102 qualify. Without the averaging
    // subquery's prefix filter, 700.00 raises the average to 2100 / 4 =
    // 525.000000, so no rows qualify.
    let expected: &[Vec<Value>] = &[vec![
        Value::String("13".into()),
        Value::Int64(1),
        decimal(50_000, 18, 2),
    ]];

    compare_results(&actual, expected, true).expect("Q22 result matches customer average fixture");
}

#[test]
fn test_multiset_validation() {
    let (_directory, server) = fixtures::correctness::load_fixture();
    let sql = "SELECT r_regionkey FROM region WHERE r_regionkey IN (0, 1) ORDER BY r_regionkey";

    let actual = match server.execute(sql).expect("execute region query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[vec![Value::Int64(0)], vec![Value::Int64(0)]];

    assert!(
        compare_results(&actual, expected, false).is_err(),
        "unordered comparison must reject a wrong multiset"
    );
}

#[test]
fn test_q5() {
    let (_directory, server) = fixtures::nation_region::load_fixture();
    let sql = htap_tpch::query(5, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived revenue is grouped by supplier nation. INDIA has two lines:
    // 100.00 * 0.90 = 90.0000 and 200.00 * 0.95 = 190.0000, totaling
    // 280.0000. INDONESIA has one line: 500.00 * 0.85 = 425.0000.
    //
    // Decoy 1 tests c_nationkey = s_nationkey: its customer is in INDIA while
    // its supplier is in INDONESIA, though both nations are in ASIA. Decoy 2
    // tests the region filter because both customer and supplier are in
    // FRANCE/EUROPE. Decoy 3 tests the lower date boundary with an order before
    // 1994-01-01, and decoy 4 tests the upper date boundary with an order at the
    // exclusive 1995-01-01 boundary. No other fixture row can mask these effects.
    let expected: &[Vec<Value>] = &[
        vec![Value::String("INDONESIA".into()), decimal(4_250_000, 18, 4)],
        vec![Value::String("INDIA".into()), decimal(2_800_000, 18, 4)],
    ];

    compare_results(&actual, expected, true)
        .expect("Q5 result matches nation and region fixture result");
}

#[test]
fn test_q7() {
    let (_directory, server) = fixtures::nation_region::load_fixture();
    let sql = htap_tpch::query(7, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // Hand-derived revenue is grouped by supplier nation, customer nation, and
    // ship year. Order 1001 has a French supplier and German customer: 100.00 *
    // (1 - 0.10) = 100.00 * 0.90 = 90.00, and 200.00 * (1 - 0.05) = 200.00 *
    // 0.95 = 190.00, for 280.00 total. At DECIMAL(18,4), its value part is
    // 280.00 * 10000 = 2800000. Order 1002 reverses the pair, with a German
    // supplier and French customer: 300.00 * (1 - 0.20) = 300.00 * 0.80 =
    // 240.00, represented as 2400000. Order 1010 is on the inclusive
    // 1996-12-31 upper boundary: 200.00 * (1 - 0.20) = 200.00 * 0.80 =
    // 160.00, represented as 1600000.
    //
    // Every clause is load-bearing. Without the nation-pair condition, the
    // same-nation rows (FRANCE, FRANCE, 1996) and (GERMANY, GERMANY, 1995),
    // plus wrong-pair rows (FRANCE, PERU, 1996), (PERU, FRANCE, 1995), and
    // (GERMANY, PERU, 1996), appear. Removing the FRANCE-supplier,
    // GERMANY-customer OR direction leaves only the GERMANY-to-FRANCE row;
    // removing the reverse direction leaves only the two FRANCE-to-GERMANY
    // rows. Without the lower shipdate bound, the 1994-12-31 row adds
    // (FRANCE, GERMANY, 1994). Changing the upper bound to exclusive loses the
    // 1996-12-31 row, leaving only the two 1995 rows.
    //
    // Removing the n1 supplier-nation alias condition adds (PERU, FRANCE, 1995)
    // from the PERU supplier decoy. Removing the n2 customer-nation alias
    // condition adds (GERMANY, PERU, 1996) from the PERU customer decoy. All
    // decoys fail only their intended clause, so no other row masks these effects.
    let expected: &[Vec<Value>] = &[
        vec![
            Value::String("FRANCE".into()),
            Value::String("GERMANY".into()),
            Value::Int64(1995),
            decimal(2_800_000, 18, 4),
        ],
        vec![
            Value::String("FRANCE".into()),
            Value::String("GERMANY".into()),
            Value::Int64(1996),
            decimal(1_600_000, 18, 4),
        ],
        vec![
            Value::String("GERMANY".into()),
            Value::String("FRANCE".into()),
            Value::Int64(1995),
            decimal(2_400_000, 18, 4),
        ],
    ];

    compare_results(&actual, expected, true)
        .expect("Q7 result matches nation and region fixture result");
}

#[test]
fn test_q8() {
    let (_directory, server) = fixtures::nation_region::load_fixture();
    let sql = htap_tpch::query(8, "1").expect("TPC-H query exists");

    let actual = match server.execute(&sql).expect("execute TPC-H query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    // In 1995, O2001's two BRAZIL-supplier lines contribute 150.00 each, for
    // 300.0000 numerator volume. O2002's FRANCE-supplier line contributes
    // 300.0000 only to the denominator. Therefore the share is 300 / 600 =
    // 0.50000000. At DECIMAL(18,4), the numerator and denominator value parts
    // are 3000000 and 6000000; their division produces DECIMAL(18,8) value
    // 50000000. In 1996, O2007 contributes 300.0000 to both sums, so 300 / 300
    // = 1.00000000, represented by DECIMAL(18,8) value 100000000.
    //
    // The two nation aliases are load-bearing. n1 is the customer's nation and
    // supplies the AMERICA region filter, while n2 is the supplier's nation and
    // supplies the BRAZIL CASE condition. If they are swapped, O2002 is excluded
    // because its supplier FRANCE is not in AMERICA, and O2001 fails the CASE
    // because its customer PERU is not BRAZIL. The resulting 1995 share is
    // 0 / 300 = 0 instead of 300 / 600 = 0.5.
    //
    // Each decoy fails only its intended clause: O2003 has a FRANCE customer in
    // EUROPE and would add 300.0000 BRAZIL volume if the region filter were
    // removed. O2008 has the wrong part type and would add 300.0000 BRAZIL
    // volume if the part-type filter were removed. O2005 and O2006 are
    // respectively before and after the date window, and each would add
    // 300.0000 BRAZIL volume if the date filter were removed. O2002's FRANCE
    // supplier shows that non-BRAZIL supplier volume contributes only to the
    // denominator through the CASE expression.
    let expected: &[Vec<Value>] = &[
        vec![Value::Int64(1995), decimal(50_000_000, 18, 8)],
        vec![Value::Int64(1996), decimal(100_000_000, 18, 8)],
    ];

    compare_results(&actual, expected, true)
        .expect("Q8 result matches nation and region fixture result");
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

    compare_results(&actual, expected, true).expect("Q6 result matches hand-derived result");
}

#[test]
fn test_multiset_validation_accepts_permuted_rows() {
    let (_directory, server) = fixtures::correctness::load_fixture();
    let sql = "SELECT r_regionkey FROM region WHERE r_regionkey IN (0, 1) ORDER BY r_regionkey";

    let actual = match server.execute(sql).expect("execute region query") {
        StatementResult::Query(result) => result,
        other => panic!("expected query result, got {other:?}"),
    };

    let expected: &[Vec<Value>] = &[vec![Value::Int64(1)], vec![Value::Int64(0)]];

    compare_results(&actual, expected, false)
        .expect("unordered comparison accepts the same rows in a different order");
}

#[test]
fn test_multiset_validation_rejects_decimal_precision_mismatch() {
    let actual = execute_query(6);
    let expected: &[Vec<Value>] = &[vec![decimal(1_200_000, 17, 4)]];

    assert!(
        compare_results(&actual, expected, false).is_err(),
        "unordered comparison must reject a decimal with different precision"
    );
}
