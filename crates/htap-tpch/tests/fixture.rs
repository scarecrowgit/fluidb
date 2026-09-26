use std::sync::Arc;

use anyhow::bail;
use htap_common::types::Value;
use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_fixture() -> Arc<LocalServer> {
    let directory = TempDir::new().expect("create temporary fixture directory");
    let path = directory.keep();
    let server = Arc::new(LocalServer::open(&path).expect("open fixture server"));

    for ddl in htap_tpch::schema::ddl_statements() {
        server.execute(ddl).expect("create fixture table");
    }

    server
        .execute(
            "INSERT INTO region (r_regionkey, r_name, r_comment) VALUES
             (0, 'AFRICA', 'region'),
             (1, 'AMERICA', 'region'),
             (2, 'ASIA', 'region'),
             (3, 'EUROPE', 'region'),
             (4, 'MIDDLE EAST', 'region')",
        )
        .expect("insert regions");

    server
        .execute(
            "INSERT INTO nation (n_nationkey, n_name, n_regionkey, n_comment) VALUES
             (0, 'ALGERIA', 0, 'nation'),
             (1, 'ARGENTINA', 1, 'nation'),
             (2, 'BRAZIL', 1, 'nation'),
             (3, 'CANADA', 1, 'nation'),
             (4, 'EGYPT', 4, 'nation'),
             (5, 'ETHIOPIA', 0, 'nation'),
             (6, 'FRANCE', 3, 'nation'),
             (7, 'GERMANY', 3, 'nation'),
             (8, 'INDIA', 2, 'nation'),
             (9, 'INDONESIA', 2, 'nation'),
             (10, 'IRAN', 4, 'nation'),
             (11, 'IRAQ', 4, 'nation'),
             (12, 'JAPAN', 2, 'nation'),
             (13, 'JORDAN', 4, 'nation'),
             (14, 'KENYA', 0, 'nation'),
             (15, 'MOROCCO', 0, 'nation'),
             (16, 'MOZAMBIQUE', 0, 'nation'),
             (17, 'PERU', 1, 'nation'),
             (18, 'CHINA', 2, 'nation'),
             (19, 'ROMANIA', 3, 'nation'),
             (20, 'SAUDI ARABIA', 4, 'nation'),
             (21, 'VIETNAM', 2, 'nation'),
             (22, 'RUSSIA', 3, 'nation'),
             (23, 'UNITED KINGDOM', 3, 'nation'),
             (24, 'UNITED STATES', 1, 'nation')",
        )
        .expect("insert nations");

    server
        .execute(
            "INSERT INTO part (p_partkey, p_name, p_mfgr, p_brand, p_type, p_size, p_container, p_retailprice, p_comment) VALUES
             (1, 'brass one', 'MFGR#1', 'Brand#12', 'SMALL BRASS', 15, 'SM CASE', 100.00, 'part'),
             (2, 'brass two', 'MFGR#1', 'Brand#23', 'LARGE BRASS', 15, 'MED BOX', 200.00, 'part'),
             (3, 'promo steel', 'MFGR#2', 'Brand#34', 'PROMO ANODIZED STEEL', 5, 'LG PACK', 300.00, 'part'),
             (4, 'green widget', 'MFGR#2', 'Brand#45', 'MEDIUM POLISHED TIN', 49, 'SM BOX', 400.00, 'part'),
             (5, 'forest tool', 'MFGR#3', 'Brand#12', 'STANDARD STEEL', 14, 'SM PACK', 500.00, 'part'),
             (6, 'plain item', 'MFGR#3', 'Brand#23', 'STANDARD BRASS', 23, 'MED BOX', 600.00, 'part'),
             (7, 'promo bolt', 'MFGR#4', 'Brand#34', 'PROMO BRASS', 45, 'LG CASE', 700.00, 'part'),
             (8, 'green part', 'MFGR#4', 'Brand#12', 'ECONOMY ANODIZED STEEL', 19, 'SM PKG', 800.00, 'part'),
             (9, 'forest screw', 'MFGR#5', 'Brand#23', 'STANDARD STEEL', 3, 'MED PACK', 900.00, 'part'),
             (10, 'other part', 'MFGR#5', 'Brand#34', 'STANDARD TIN', 36, 'LG BOX', 1000.00, 'part'),
             (11, 'small item', 'MFGR#6', 'Brand#12', 'STANDARD COPPER', 9, 'SM CASE', 1100.00, 'part'),
             (12, 'large item', 'MFGR#6', 'Brand#23', 'STANDARD STEEL', 49, 'LG PACK', 1200.00, 'part')",
        )
        .expect("insert parts");

    server
        .execute(
            "INSERT INTO supplier (s_suppkey, s_name, s_address, s_nationkey, s_phone, s_acctbal, s_comment) VALUES
             (1, 'Supp Germany A', 'Address 1', 7, '13-000-000-0000', 7000.00, 'reliable'),
             (2, 'Supp Germany B', 'Address 2', 7, '31-000-000-0000', 7000.00, 'reliable'),
             (3, 'Supp France', 'Address 3', 6, '23-000-000-0000', 6000.00, 'reliable'),
             (4, 'Supp Brazil', 'Address 4', 2, '29-000-000-0000', 4000.00, 'reliable'),
             (5, 'Supp Canada', 'Address 5', 3, '30-000-000-0000', 5500.00, 'reliable'),
             (6, 'Supp Saudi', 'Address 6', 20, '18-000-000-0000', 3000.00, 'reliable'),
             (7, 'Supp India', 'Address 7', 8, '17-000-000-0000', 2000.00, 'reliable'),
             (8, 'Supp Excluded', 'Address 8', 1, '13-111-111-1111', 8000.00, 'Customer Complaints'),
             (9, 'Supp Europe', 'Address 9', 19, '31-111-111-1111', 6500.00, 'reliable')",
        )
        .expect("insert suppliers");

    server
        .execute(
            "INSERT INTO partsupp (ps_partkey, ps_suppkey, ps_availqty, ps_supplycost, ps_comment) VALUES
             (1, 1, 100, 10.00, 'supply'), (1, 2, 100, 10.00, 'supply'),
             (1, 3, 100, 12.00, 'supply'), (1, 4, 100, 8.00, 'supply'),
             (2, 1, 100, 30.00, 'supply'), (2, 2, 100, 20.00, 'supply'),
             (2, 3, 100, 25.00, 'supply'), (2, 4, 100, 15.00, 'supply'),
             (3, 1, 100, 40.00, 'supply'), (3, 3, 100, 30.00, 'supply'),
             (3, 4, 100, 20.00, 'supply'), (3, 5, 100, 25.00, 'supply'),
             (4, 1, 100, 12.00, 'supply'), (4, 2, 100, 13.00, 'supply'),
             (4, 3, 100, 14.00, 'supply'), (4, 8, 100, 11.00, 'supply'),
             (5, 1, 100, 11.00, 'supply'), (5, 2, 100, 12.00, 'supply'),
             (5, 3, 100, 13.00, 'supply'), (5, 5, 50, 9.00, 'supply'),
             (6, 1, 100, 15.00, 'supply'), (6, 2, 100, 14.00, 'supply'),
             (6, 3, 100, 16.00, 'supply'), (6, 4, 100, 13.00, 'supply'),
             (7, 1, 100, 18.00, 'supply'), (7, 2, 100, 17.00, 'supply'),
             (7, 3, 100, 19.00, 'supply'), (7, 4, 100, 16.00, 'supply'),
             (8, 1, 100, 21.00, 'supply'), (8, 2, 100, 22.00, 'supply'),
             (8, 3, 100, 23.00, 'supply'), (8, 4, 100, 20.00, 'supply'),
             (9, 1, 100, 24.00, 'supply'), (9, 2, 100, 25.00, 'supply'),
             (9, 3, 100, 26.00, 'supply'), (9, 5, 100, 10.00, 'supply'),
             (10, 1, 100, 27.00, 'supply'), (10, 2, 100, 28.00, 'supply'),
             (10, 3, 100, 29.00, 'supply'), (10, 4, 100, 26.00, 'supply'),
             (11, 1, 100, 31.00, 'supply'), (11, 2, 100, 32.00, 'supply'),
             (11, 3, 100, 33.00, 'supply'), (11, 4, 100, 30.00, 'supply'),
             (12, 1, 100, 34.00, 'supply'), (12, 2, 100, 35.00, 'supply'),
             (12, 3, 100, 36.00, 'supply'), (12, 4, 100, 33.00, 'supply')",
        )
        .expect("insert part supplies");

    server
        .execute(
            "INSERT INTO customer (c_custkey, c_name, c_address, c_nationkey, c_phone, c_acctbal, c_mktsegment, c_comment) VALUES
             (1, 'Building France', 'Address', 6, '13-000-000-0000', 7000.00, 'BUILDING', 'customer'),
             (2, 'Building Germany', 'Address', 7, '31-000-000-0000', 3000.00, 'BUILDING', 'customer'),
             (3, 'Brazil Customer', 'Address', 2, '23-000-000-0000', 6000.00, 'AUTOMOBILE', 'customer'),
             (4, 'Canada Customer', 'Address', 3, '29-000-000-0000', 1000.00, 'HOUSEHOLD', 'customer'),
             (5, 'India Customer', 'Address', 8, '30-000-000-0000', 8000.00, 'MACHINERY', 'customer'),
             (6, 'Saudi Customer', 'Address', 20, '18-000-000-0000', 4000.00, 'FURNITURE', 'customer'),
             (7, 'No Orders', 'Address', 1, '17-000-000-0000', 9000.00, 'BUILDING', 'customer')",
        )
        .expect("insert customers");

    server
        .execute(
            "INSERT INTO orders (o_orderkey, o_custkey, o_orderstatus, o_totalprice, o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) VALUES
             (1, 1, 'F', 1000.00, DATE '1993-08-01', '1-URGENT', 'Clerk#1', 0, 'regular'),
             (2, 1, 'O', 2000.00, DATE '1994-06-01', '2-HIGH', 'Clerk#2', 0, 'regular'),
             (3, 2, 'F', 3000.00, DATE '1995-03-14', '3-MEDIUM', 'Clerk#3', 0, 'regular'),
             (4, 3, 'O', 4000.00, DATE '1995-07-01', '4-LOW', 'Clerk#4', 0, 'regular'),
             (5, 4, 'P', 5000.00, DATE '1996-01-01', '5-VERY LOW', 'Clerk#5', 0, 'regular'),
             (6, 5, 'F', 6000.00, DATE '1996-06-01', '1-URGENT', 'Clerk#6', 0, 'regular'),
             (7, 6, 'F', 7000.00, DATE '1997-01-01', '2-HIGH', 'Clerk#7', 0, 'regular'),
             (8, 1, 'O', 8000.00, DATE '1998-09-02', '3-MEDIUM', 'Clerk#8', 0, 'regular'),
             (9, 2, 'O', 9000.00, DATE '1998-09-03', '4-LOW', 'Clerk#9', 0, 'regular'),
             (10, 3, 'F', 10000.00, DATE '1994-02-01', '5-VERY LOW', 'Clerk#10', 0, 'regular')",
        )
        .expect("insert orders");

    server
        .execute(
            "INSERT INTO lineitem (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, l_shipmode, l_comment) VALUES
             (1, 1, 1, 1, 10.00, 100.00, 0.10, 0.05, 'R', 'F', DATE '1993-08-10', DATE '1993-08-08', DATE '1993-08-12', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (1, 2, 2, 2, 20.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1993-08-11', DATE '1993-08-12', DATE '1993-08-12', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (2, 1, 3, 3, 30.00, 300.00, 0.05, 0.10, 'R', 'O', DATE '1994-06-10', DATE '1994-06-08', DATE '1994-06-12', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (2, 2, 4, 1, 40.00, 400.00, 0.00, 0.00, 'N', 'F', DATE '1995-04-01', DATE '1995-03-30', DATE '1995-04-02', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (3, 1, 5, 5, 50.00, 500.00, 0.10, 0.00, 'R', 'F', DATE '1995-03-16', DATE '1995-03-14', DATE '1995-03-18', 'DELIVER IN PERSON', 'AIR', 'line'),
             (3, 2, 6, 2, 60.00, 600.00, 0.00, 0.00, 'N', 'O', DATE '1995-03-14', DATE '1995-03-13', DATE '1995-03-15', 'DELIVER IN PERSON', 'AIR REG', 'line'),
             (4, 1, 8, 4, 70.00, 700.00, 0.10, 0.00, 'R', 'F', DATE '1995-09-15', DATE '1995-09-14', DATE '1995-09-16', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (4, 2, 7, 3, 80.00, 800.00, 0.00, 0.00, 'N', 'O', DATE '1995-10-01', DATE '1995-09-30', DATE '1995-10-02', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (5, 1, 9, 5, 90.00, 900.00, 0.06, 0.02, 'R', 'F', DATE '1996-02-01', DATE '1996-01-30', DATE '1996-02-03', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (5, 2, 10, 4, 100.00, 1000.00, 0.07, 0.00, 'N', 'O', DATE '1996-03-01', DATE '1996-02-28', DATE '1996-03-03', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (6, 1, 1, 6, 110.00, 1100.00, 0.02, 0.00, 'R', 'F', DATE '1996-06-10', DATE '1996-06-09', DATE '1996-06-11', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (6, 2, 2, 7, 120.00, 1200.00, 0.03, 0.00, 'N', 'O', DATE '1996-06-11', DATE '1996-06-10', DATE '1996-06-12', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (7, 1, 3, 6, 130.00, 1300.00, 0.04, 0.00, 'R', 'F', DATE '1997-01-10', DATE '1997-01-08', DATE '1997-01-12', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (7, 2, 4, 7, 140.00, 1400.00, 0.05, 0.00, 'N', 'O', DATE '1997-01-11', DATE '1997-01-10', DATE '1997-01-12', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (8, 1, 5, 1, 150.00, 1500.00, 0.00, 0.00, 'R', 'F', DATE '1998-09-02', DATE '1998-09-01', DATE '1998-09-03', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (8, 2, 6, 2, 160.00, 1600.00, 0.00, 0.00, 'N', 'O', DATE '1998-09-01', DATE '1998-08-31', DATE '1998-09-02', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (9, 1, 7, 3, 170.00, 1700.00, 0.00, 0.00, 'R', 'F', DATE '1998-09-03', DATE '1998-09-02', DATE '1998-09-04', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (9, 2, 8, 4, 180.00, 1800.00, 0.00, 0.00, 'N', 'O', DATE '1998-09-04', DATE '1998-09-03', DATE '1998-09-05', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (10, 1, 1, 4, 310.00, 3100.00, 0.05, 0.00, 'R', 'F', DATE '1994-03-01', DATE '1994-02-28', DATE '1994-03-02', 'DELIVER IN PERSON', 'MAIL', 'line'),
             (10, 2, 2, 4, 20.00, 200.00, 0.00, 0.00, 'N', 'O', DATE '1994-03-02', DATE '1994-03-01', DATE '1994-03-03', 'DELIVER IN PERSON', 'SHIP', 'line'),
             (5, 3, 1, 7, 20.00, 2000.00, 0.06, 0.02, 'A', 'F', DATE '1994-06-15', DATE '1994-06-14', DATE '1994-06-17', 'DELIVER IN PERSON', 'MAIL', 'line')",
        )
        .expect("insert line items");

    server
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

fn rows_match(actual: &htap_common::types::Row, expected: &[Value]) -> bool {
    actual.values().len() == expected.len()
        && actual
            .values()
            .iter()
            .zip(expected)
            .all(|(actual_value, expected_value)| values_match(actual_value, expected_value))
}

#[allow(dead_code)]
pub fn compare_results(
    actual: &htap_sql::QueryResult,
    expected: &[Vec<Value>],
    ordered: bool,
) -> anyhow::Result<()> {
    if actual.rows.len() != expected.len() {
        bail!(
            "row count mismatch: actual {}, expected {}",
            actual.rows.len(),
            expected.len()
        );
    }

    if let Some(expected_row) = expected.first() {
        if actual.columns.len() != expected_row.len() {
            bail!(
                "column count mismatch: actual {}, expected {}",
                actual.columns.len(),
                expected_row.len()
            );
        }
    }

    if !ordered {
        let mut matched_actual_rows = vec![false; actual.rows.len()];

        for (expected_row_index, expected_row) in expected.iter().enumerate() {
            let matching_actual_row =
                actual
                    .rows
                    .iter()
                    .enumerate()
                    .find_map(|(actual_row_index, actual_row)| {
                        (!matched_actual_rows[actual_row_index]
                            && rows_match(actual_row, expected_row))
                        .then_some(actual_row_index)
                    });

            let Some(actual_row_index) = matching_actual_row else {
                bail!("no matching actual row found for expected row {expected_row_index}");
            };

            matched_actual_rows[actual_row_index] = true;
        }

        return Ok(());
    }

    for (row_index, expected_row) in expected.iter().enumerate() {
        let actual_row = actual
            .rows
            .get(row_index)
            .ok_or_else(|| anyhow::anyhow!("missing row {row_index}"))?;

        if rows_match(actual_row, expected_row) {
            continue;
        }

        for (column_index, expected_value) in expected_row.iter().enumerate() {
            let actual_value = actual_row
                .get(column_index)
                .ok_or_else(|| anyhow::anyhow!("missing row {row_index}, column {column_index}"))?;

            match (actual_value, expected_value) {
                (
                    Value::Decimal {
                        value: actual_decimal_value,
                        precision: actual_precision,
                        scale: actual_scale,
                    },
                    Value::Decimal {
                        value: expected_decimal_value,
                        precision: expected_precision,
                        scale: expected_scale,
                    },
                ) if !values_match(actual_value, expected_value) => {
                    bail!(
                        "decimal mismatch at row {row_index}, column {column_index}: \
                         actual value={}, precision={}, scale={}; \
                         expected value={}, precision={}, scale={}",
                        actual_decimal_value,
                        actual_precision,
                        actual_scale,
                        expected_decimal_value,
                        expected_precision,
                        expected_scale
                    );
                }
                _ if !values_match(actual_value, expected_value) => {
                    bail!(
                        "value mismatch at row {row_index}, column {column_index}: \
                         actual {actual_value:?}, expected {expected_value:?}"
                    );
                }
                _ => {}
            }
        }

        bail!("row mismatch at row {row_index}");
    }

    Ok(())
}
