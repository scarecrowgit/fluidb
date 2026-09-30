use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::consistency::{check_consistency, ConsistencyViolation};
use htap_tpcc::drivers::{
    run, DriverError, DriverScale, TransactionKind, TransactionLimitOrDuration, WorkloadReport,
    COMPLETED_LABEL, CONFLICT_RETRIES_LABEL, ELAPSED_LABEL, ESCALATIONS_LABEL,
    EXPECTED_ROLLBACKS_LABEL, OBSERVED_MIX_LABEL,
};
use htap_tpcc::generate::rng::RandomState;
use htap_tpcc::generate::text::c_last;
use htap_tpcc::generate::{
    generate, Customer, Dataset, District, History, Item, NewOrder, OrderLine, Orders, Stock,
    Warehouse,
};
use htap_tpcc::history::build_h_id;
use htap_tpcc::load::{load_dataset, LoadOptions};
use htap_tpcc::schema::ddl_statements;
use tempfile::TempDir;

const DELIVERED_ORDER_COUNT: i64 = 2_100;
const NEW_ORDER_COUNT: i64 = 3;
const HISTORY_AMOUNT: i64 = 1_000;
const DELIVERED_LINE_AMOUNT: i64 = 100;

fn setup() -> Result<(TempDir, Arc<LocalServer>), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    for ddl in ddl_statements() {
        server.execute(&ddl)?;
    }
    Ok((directory, server))
}

fn small_consistent_dataset() -> Dataset {
    const DISTRICT_COUNT: i64 = 2;
    const CUSTOMER_COUNT: i64 = 50;
    const ITEM_COUNT: i64 = 20;

    let delivered_per_customer = DELIVERED_ORDER_COUNT / CUSTOMER_COUNT;
    let delivered_amount = delivered_per_customer * DELIVERED_LINE_AMOUNT;

    let warehouse = Warehouse {
        w_id: 1,
        w_name: "Warehouse 'Main\\".into(),
        w_street_1: "Street \\ One's".into(),
        w_street_2: "Street 2".into(),
        w_city: "City \\ Central's".into(),
        w_state: "ST".into(),
        w_zip: "123456789".into(),
        w_tax: 0,
        w_ytd: DISTRICT_COUNT * CUSTOMER_COUNT * HISTORY_AMOUNT,
    };

    let district = (1..=DISTRICT_COUNT)
        .map(|d_id| District {
            d_id,
            d_w_id: 1,
            d_name: format!("District '{d_id}\\"),
            d_street_1: "Street \\ One's".into(),
            d_street_2: "Street 2".into(),
            d_city: "City \\ Central's".into(),
            d_state: "ST".into(),
            d_zip: "123456789".into(),
            d_tax: 0,
            d_ytd: CUSTOMER_COUNT * HISTORY_AMOUNT,
            d_next_o_id: DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT + 1,
        })
        .collect();

    let item = (1..=ITEM_COUNT)
        .map(|i_id| Item {
            i_id,
            i_im_id: i_id,
            i_name: format!("Item {i_id}"),
            i_price: 100,
            i_data: "data".into(),
        })
        .collect();

    let stock = (1..=ITEM_COUNT)
        .map(|s_i_id| Stock {
            s_i_id,
            s_w_id: 1,
            s_quantity: 100,
            s_dist_01: "distribution information".into(),
            s_dist_02: "distribution information".into(),
            s_dist_03: "distribution information".into(),
            s_dist_04: "distribution information".into(),
            s_dist_05: "distribution information".into(),
            s_dist_06: "distribution information".into(),
            s_dist_07: "distribution information".into(),
            s_dist_08: "distribution information".into(),
            s_dist_09: "distribution information".into(),
            s_dist_10: "distribution information".into(),
            s_ytd: 0,
            s_order_cnt: 0,
            s_remote_cnt: 0,
            s_data: "data".into(),
        })
        .collect();

    let customer = (1..=DISTRICT_COUNT)
        .flat_map(|c_d_id| {
            let mut last_name_rng = RandomState::new(c_d_id as u64);
            (1..=CUSTOMER_COUNT)
                .map(move |c_id| Customer {
                    c_id,
                    c_d_id,
                    c_w_id: 1,
                    c_first: format!("Customer\\'{c_d_id}_{c_id}"),
                    c_middle: "OE".into(),
                    c_last: c_last(&mut last_name_rng, 0, (c_id - 1) as u64),
                    c_street_1: "Street \\ One's".into(),
                    c_street_2: "Street 2".into(),
                    c_city: "City \\ Central's".into(),
                    c_state: "ST".into(),
                    c_zip: "123456789".into(),
                    c_phone: "1234567890123456".into(),
                    c_since: "2000-01-01 00:00:00".into(),
                    c_credit: "BC".into(),
                    c_credit_lim: 5_000_000,
                    c_discount: 0,
                    c_balance: delivered_amount - HISTORY_AMOUNT,
                    c_ytd_payment: HISTORY_AMOUNT,
                    c_payment_cnt: 1,
                    c_delivery_cnt: delivered_per_customer,
                    c_data: format!("data '{c_d_id}_{c_id}\\"),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let history = (1..=DISTRICT_COUNT)
        .flat_map(|h_d_id| {
            (1..=CUSTOMER_COUNT).map(move |h_c_id| {
                let sequence = ((h_d_id - 1) * CUSTOMER_COUNT + h_c_id - 1) as u64;
                History {
                    h_id: build_h_id(0, sequence).unwrap(),
                    h_c_id,
                    h_c_d_id: h_d_id,
                    h_c_w_id: 1,
                    h_d_id,
                    h_w_id: 1,
                    h_date: "2000-01-01 00:00:00".into(),
                    h_amount: HISTORY_AMOUNT,
                    h_data: "initial payment".into(),
                }
            })
        })
        .collect();

    let orders = (1..=DISTRICT_COUNT)
        .flat_map(|o_d_id| {
            (1..=(DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT)).map(move |o_id| {
                let delivered = o_id <= DELIVERED_ORDER_COUNT;
                Orders {
                    o_id,
                    o_d_id,
                    o_w_id: 1,
                    o_c_id: (o_id - 1) % CUSTOMER_COUNT + 1,
                    o_entry_d: "2000-01-01 00:00:00".into(),
                    o_carrier_id: delivered.then_some(1),
                    o_ol_cnt: 1,
                    o_all_local: 1,
                }
            })
        })
        .collect::<Vec<_>>();

    let order_line = orders
        .iter()
        .map(|order| {
            let delivered = order.o_id <= DELIVERED_ORDER_COUNT;
            OrderLine {
                ol_o_id: order.o_id,
                ol_d_id: order.o_d_id,
                ol_w_id: 1,
                ol_number: 1,
                ol_i_id: (order.o_id - 1) % ITEM_COUNT + 1,
                ol_supply_w_id: 1,
                ol_delivery_d: delivered.then_some("2000-01-01 00:00:00".into()),
                ol_quantity: 5,
                ol_amount: if delivered {
                    DELIVERED_LINE_AMOUNT
                } else {
                    100
                },
                ol_dist_info: "distribution information".into(),
            }
        })
        .collect();

    let new_order = (1..=DISTRICT_COUNT)
        .flat_map(|no_d_id| {
            ((DELIVERED_ORDER_COUNT + 1)..=(DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT)).map(
                move |no_o_id| NewOrder {
                    no_o_id,
                    no_d_id,
                    no_w_id: 1,
                },
            )
        })
        .collect();

    Dataset {
        warehouse: vec![warehouse],
        district,
        item,
        stock,
        customer,
        history,
        orders,
        order_line,
        new_order,
    }
}

fn query_count(session: &mut Session, sql: &str) -> Result<i64, Box<dyn Error>> {
    let StatementResult::Query(result) = session.execute(sql)? else {
        panic!("expected query result for {sql}");
    };

    match result.rows()[0].get(0) {
        Some(Value::Int32(value)) => Ok(i64::from(*value)),
        Some(Value::Int64(value)) => Ok(*value),
        Some(Value::Decimal { value, .. }) => Ok(*value),
        value => panic!("expected count result, got {value:?}"),
    }
}

fn delivered_orders_by_district(
    session: &mut Session,
    district_count: i64,
) -> Result<Vec<i64>, Box<dyn Error>> {
    (1..=district_count)
        .map(|d_id| {
            let orders = query_count(
                session,
                &format!("SELECT count(*) FROM orders WHERE o_w_id = 1 AND o_d_id = {d_id}"),
            )?;
            let new_orders = query_count(
                session,
                &format!("SELECT count(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = {d_id}"),
            )?;
            Ok(orders - new_orders)
        })
        .collect()
}

fn condition_numbers(violations: &[ConsistencyViolation]) -> Vec<i64> {
    let mut conditions = violations
        .iter()
        .map(|violation| violation.condition_number)
        .collect::<Vec<_>>();
    conditions.sort_unstable();
    conditions
}

fn assert_condition_11_and_delivery_invariant(
    session: &mut Session,
    violations: &[ConsistencyViolation],
    district_count: i64,
) -> Result<(), Box<dyn Error>> {
    assert!(
        condition_numbers(violations)
            .into_iter()
            .all(|condition_number| condition_number == 11),
        "only condition 11 may be violated: {violations:?}"
    );

    let mut expected_condition_11_districts = BTreeSet::new();
    for district_id in 1..=district_count {
        let order_count = query_count(
            session,
            &format!("SELECT count(*) FROM orders WHERE o_w_id = 1 AND o_d_id = {district_id}"),
        )?;
        let new_order_count = query_count(
            session,
            &format!(
                "SELECT count(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = {district_id}"
            ),
        )?;
        let carrier_order_count = query_count(
            session,
            &format!(
                "SELECT count(*) FROM orders \
                 WHERE o_w_id = 1 AND o_d_id = {district_id} AND o_carrier_id IS NOT NULL"
            ),
        )?;

        let delivered_order_count = order_count - new_order_count;
        assert_eq!(
            delivered_order_count, carrier_order_count,
            "district {district_id}: count(ORDER) - count(NEW-ORDER) must equal \
             count(ORDER with O_CARRIER_ID)"
        );
        if delivered_order_count != DELIVERED_ORDER_COUNT {
            expected_condition_11_districts.insert(district_id);
        }
    }

    let condition_11_violations = violations
        .iter()
        .filter(|violation| violation.condition_number == 11)
        .collect::<Vec<_>>();
    let reported_condition_11_districts = condition_11_violations
        .iter()
        .map(|violation| {
            assert_eq!(violation.warehouse_id, Some(1));
            violation
                .district_id
                .expect("condition 11 violation must identify its district")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        condition_11_violations.len(),
        reported_condition_11_districts.len(),
        "condition 11 must be reported at most once per district"
    );
    assert_eq!(
        reported_condition_11_districts, expected_condition_11_districts,
        "condition 11 must be reported if and only if the district's delivered-order count differs from 2100"
    );

    Ok(())
}

fn assert_report(report: &WorkloadReport, transaction_count: u64) {
    assert_eq!(
        report
            .transactions
            .iter()
            .map(|transaction| {
                transaction.counts.completed + transaction.counts.expected_rollbacks
            })
            .sum::<u64>(),
        transaction_count
    );

    for transaction in &report.transactions {
        let minimum_percentage = match transaction.kind {
            TransactionKind::NewOrder | TransactionKind::Payment => 10.0 / 23.0 * 100.0,
            TransactionKind::OrderStatus
            | TransactionKind::Delivery
            | TransactionKind::StockLevel => 1.0 / 23.0 * 100.0,
        };
        // Runs ending mid-deck need slack because their final partial deck may be imbalanced.
        assert!(
            transaction.observed_percentage >= minimum_percentage - 1.0,
            "{:?} observed {}%, expected at least {}%",
            transaction.kind,
            transaction.observed_percentage,
            minimum_percentage - 1.0
        );
    }
}

#[test]
fn rejects_zero_warehouse_count() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        0,
        1,
        TransactionLimitOrDuration::TransactionLimit(1),
        DriverScale::default(),
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidWarehouseCount));
    Ok(())
}

#[test]
fn rejects_zero_terminal_count() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        1,
        0,
        TransactionLimitOrDuration::TransactionLimit(1),
        DriverScale::default(),
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidTerminalCount));
    Ok(())
}

#[test]
fn rejects_terminal_count_larger_than_history_terminal_range() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        1,
        u32::from(u16::MAX) + 1,
        TransactionLimitOrDuration::TransactionLimit(1),
        DriverScale::default(),
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidTerminalCount));
    Ok(())
}

#[test]
fn rejects_zero_transaction_limit() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        1,
        1,
        TransactionLimitOrDuration::TransactionLimit(0),
        DriverScale::default(),
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidRunLimit));
    Ok(())
}

#[test]
fn rejects_zero_duration_limit() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        1,
        1,
        TransactionLimitOrDuration::Duration(Duration::ZERO),
        DriverScale::default(),
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidRunLimit));
    Ok(())
}

#[test]
fn rejects_zero_scale_dimension() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;

    let error = run(
        &server,
        1,
        1,
        TransactionLimitOrDuration::TransactionLimit(1),
        DriverScale {
            districts_per_warehouse: 0,
            ..DriverScale::default()
        },
        1,
    )
    .unwrap_err();

    assert!(matches!(error, DriverError::InvalidScale));
    Ok(())
}

#[test]
fn report_labels_do_not_claim_official_tpcc_metrics() {
    for label in [
        COMPLETED_LABEL,
        EXPECTED_ROLLBACKS_LABEL,
        CONFLICT_RETRIES_LABEL,
        ESCALATIONS_LABEL,
        OBSERVED_MIX_LABEL,
        ELAPSED_LABEL,
    ] {
        let label = label.to_ascii_lowercase();
        for forbidden in [
            "tpmc",
            "tpm-c",
            "transactions per minute",
            "performance",
            "price",
        ] {
            assert!(
                !label.contains(forbidden),
                "{label:?} must not contain {forbidden:?}"
            );
        }
    }
}

#[test]
fn transaction_kinds_have_distinct_debug_names() {
    let kinds = [
        TransactionKind::NewOrder,
        TransactionKind::Payment,
        TransactionKind::OrderStatus,
        TransactionKind::Delivery,
        TransactionKind::StockLevel,
    ];

    let names = kinds.map(|kind| format!("{kind:?}"));
    assert_eq!(
        names,
        [
            "NewOrder",
            "Payment",
            "OrderStatus",
            "Delivery",
            "StockLevel",
        ]
    );
}

#[test]
fn transaction_kinds_have_expected_count() {
    let kinds = [
        TransactionKind::NewOrder,
        TransactionKind::Payment,
        TransactionKind::OrderStatus,
        TransactionKind::Delivery,
        TransactionKind::StockLevel,
    ];

    assert_eq!(kinds.len(), 5);
}

#[test]
fn duration_limit_accepts_nonzero_duration() {
    assert_ne!(
        TransactionLimitOrDuration::Duration(Duration::from_nanos(1)),
        TransactionLimitOrDuration::Duration(Duration::ZERO)
    );
}

#[test]
fn run_small_consistent_dataset() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    let mut session = server.open_session()?;
    let dataset = small_consistent_dataset();

    load_dataset(&server, directory.path(), &dataset, &LoadOptions::default())?;

    let initial_violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &initial_violations, 2)?;

    let delivered_before = delivered_orders_by_district(&mut session, 2)?;
    assert_eq!(delivered_before, vec![DELIVERED_ORDER_COUNT; 2]);

    let report = run(
        &server,
        1,
        2,
        TransactionLimitOrDuration::TransactionLimit(230),
        DriverScale {
            districts_per_warehouse: 2,
            customers_per_district: 50,
            item_count: 20,
        },
        42,
    )?;

    assert_report(&report, 230);

    let conflict_retries = report
        .transactions
        .iter()
        .map(|transaction| transaction.counts.conflict_retries)
        .sum::<u64>();
    let escalations = report
        .transactions
        .iter()
        .map(|transaction| transaction.counts.escalations)
        .sum::<u64>();
    assert!(
        conflict_retries + escalations > 0,
        "expected contention recovery; observed {conflict_retries} conflict retries and {escalations} escalations"
    );

    let delivery_count = report
        .transactions
        .iter()
        .find(|transaction| transaction.kind == TransactionKind::Delivery)
        .expect("delivery transaction count")
        .counts
        .completed;
    let delivered_after = delivered_orders_by_district(&mut session, 2)?;
    for (before, after) in delivered_before.iter().zip(delivered_after) {
        assert_eq!(
            after,
            before + i64::try_from(delivery_count)?,
            "condition 11 must reflect every completed delivery"
        );
    }

    let violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &violations, 2)?;
    Ok(())
}

#[test]
#[ignore]
fn consecutive_runs_preserve_history_rows() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    let mut session = server.open_session()?;
    let dataset = small_consistent_dataset();
    load_dataset(&server, directory.path(), &dataset, &LoadOptions::default())?;

    let scale = DriverScale {
        districts_per_warehouse: 2,
        customers_per_district: 50,
        item_count: 20,
    };
    let initial_history_count = query_count(&mut session, "SELECT count(*) FROM history")?;
    let first = run(
        &server,
        1,
        2,
        TransactionLimitOrDuration::TransactionLimit(230),
        scale,
        42,
    )?;
    let after_first = query_count(&mut session, "SELECT count(*) FROM history")?;
    let first_payments = first
        .transactions
        .iter()
        .find(|transaction| transaction.kind == TransactionKind::Payment)
        .expect("payment transaction count")
        .counts
        .completed;
    assert_eq!(
        after_first - initial_history_count,
        i64::try_from(first_payments)?
    );

    let second = run(
        &server,
        1,
        2,
        TransactionLimitOrDuration::TransactionLimit(230),
        scale,
        43,
    )?;
    let after_second = query_count(&mut session, "SELECT count(*) FROM history")?;
    let second_payments = second
        .transactions
        .iter()
        .find(|transaction| transaction.kind == TransactionKind::Payment)
        .expect("payment transaction count")
        .counts
        .completed;
    assert_eq!(after_second - after_first, i64::try_from(second_payments)?);

    let violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &violations, 2)?;
    Ok(())
}

#[test]
#[ignore]
fn run_full_warehouse_load_ignored() -> Result<(), Box<dyn Error>> {
    let overall_start = Instant::now();
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    let mut session = server.open_session()?;

    let generation_start = Instant::now();
    let dataset = generate(1, 42)?;
    println!(
        "generated full warehouse dataset in {:?}",
        generation_start.elapsed()
    );

    let load_start = Instant::now();
    load_dataset(&server, directory.path(), &dataset, &LoadOptions::default())?;
    println!(
        "loaded full warehouse dataset in {:?}",
        load_start.elapsed()
    );

    let driver_start = Instant::now();
    let report = run(
        &server,
        1,
        2,
        TransactionLimitOrDuration::TransactionLimit(400),
        DriverScale::default(),
        42,
    )?;
    println!(
        "ran full warehouse driver in {:?} (reported {:?})",
        driver_start.elapsed(),
        report.elapsed
    );

    assert_report(&report, 400);

    let violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &violations, 10)?;
    println!(
        "full warehouse integration test completed in {:?}",
        overall_start.elapsed()
    );
    Ok(())
}
