use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::consistency::{check_consistency, ConsistencyViolation};
use htap_tpcc::drivers::{
    run_with_isolation, DriverIsolation, DriverScale, TransactionKind, TransactionLimitOrDuration,
    WorkloadReport,
};
use htap_tpcc::generate::rng::RandomState;
use htap_tpcc::generate::text::c_last;
use htap_tpcc::generate::{
    Customer, Dataset, District, History, Item, NewOrder, OrderLine, Orders, Stock, Warehouse,
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
        "condition 11 must be reported if and only if the district's delivered-order count \
         differs from 2100"
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

fn execute(session: &mut Session, sql: &str) -> Result<(), Box<dyn Error>> {
    session.execute(sql)?;
    Ok(())
}

fn insert_warehouse(session: &mut Session, w_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO warehouse \
             (w_id, w_name, w_street_1, w_street_2, w_city, w_state, w_zip, w_tax, w_ytd) \
             VALUES ({w_id}, 'W{w_id}', 'street', 'suite', 'city', 'ST', '123456789', \
                     0.0000, 0.00)"
        ),
    )
}

fn insert_district(session: &mut Session, w_id: i64, d_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO district \
             (d_id, d_w_id, d_name, d_street_1, d_street_2, d_city, d_state, d_zip, \
              d_tax, d_ytd, d_next_o_id) \
             VALUES ({d_id}, {w_id}, 'D{d_id}', 'street', 'suite', 'city', 'ST', \
                     '123456789', 0.0000, 0.00, 3001)"
        ),
    )
}

fn insert_customer(
    session: &mut Session,
    w_id: i64,
    d_id: i64,
    c_id: i64,
    balance: &str,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO customer \
             (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
              c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
              c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
             VALUES ({c_id}, {d_id}, {w_id}, 'First{c_id}', 'OE', 'Last{c_id}', \
                     'street', 'suite', 'city', 'ST', '123456789', '1234567890123456', \
                     DATE '2000-01-01', 'GC', 50000.00, 0.0000, {balance}, 0.00, 0, 0, 'data')"
        ),
    )
}

fn start_transaction(session: &mut Session, isolation_level: &str) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!("START TRANSACTION ISOLATION LEVEL {isolation_level}"),
    )
}

fn assert_serialization_failure(result: Result<(), HtapError>) {
    match result {
        Err(HtapError::Conflict(error)) => assert!(
            error.to_string().contains("serialization failure"),
            "expected serialization failure, got conflict: {error}"
        ),
        Err(error) => panic!("expected serialization failure, got: {error}"),
        Ok(()) => panic!("expected serialization failure, but commit succeeded"),
    }
}

fn seed_balance_pair(session: &mut Session) -> Result<(), Box<dyn Error>> {
    insert_warehouse(session, 1)?;
    insert_district(session, 1, 1)?;
    insert_customer(session, 1, 1, 1, "100.00")?;
    insert_customer(session, 1, 1, 2, "100.00")
}

fn balance_pair_total(session: &mut Session) -> Result<i64, Box<dyn Error>> {
    query_count(
        session,
        "SELECT SUM(c_balance) FROM customer \
         WHERE c_w_id = 1 AND c_d_id = 1 AND c_id IN (1, 2)",
    )
}

fn run_balance_pair_write_skew(
    isolation_level: &str,
    expect_serialization_failure: bool,
) -> Result<i64, Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_balance_pair(&mut seed)?;

    start_transaction(&mut t1, isolation_level)?;
    start_transaction(&mut t2, isolation_level)?;

    assert_eq!(balance_pair_total(&mut t1)?, 20_000);
    assert_eq!(balance_pair_total(&mut t2)?, 20_000);

    execute(
        &mut t1,
        "UPDATE customer SET c_balance = c_balance - 150.00 \
         WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1",
    )?;
    execute(
        &mut t2,
        "UPDATE customer SET c_balance = c_balance - 150.00 \
         WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 2",
    )?;

    t1.commit()?;
    if expect_serialization_failure {
        assert_serialization_failure(t2.commit());
    } else {
        t2.commit()?;
    }

    balance_pair_total(&mut seed)
}

fn insert_order_and_new_order(session: &mut Session, order_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({order_id}, 1, 1, 1, 1715000000000000, NULL, 0, 1)"
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO new_order (no_o_id, no_d_id, no_w_id) \
             VALUES ({order_id}, 1, 1)"
        ),
    )
}

fn seed_new_order_queue(session: &mut Session) -> Result<(), Box<dyn Error>> {
    insert_warehouse(session, 1)?;
    insert_district(session, 1, 1)?;
    insert_customer(session, 1, 1, 1, "0.00")?;
    insert_order_and_new_order(session, 3001)
}

fn new_order_queue_count(session: &mut Session) -> Result<i64, Box<dyn Error>> {
    query_count(
        session,
        "SELECT COUNT(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1",
    )
}

fn run_new_order_queue_phantom_skew(
    isolation_level: &str,
    expect_serialization_failure: bool,
) -> Result<i64, Box<dyn Error>> {
    const QUEUE_CAP: i64 = 2;

    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_queue(&mut seed)?;

    start_transaction(&mut t1, isolation_level)?;
    start_transaction(&mut t2, isolation_level)?;

    let t1_count = new_order_queue_count(&mut t1)?;
    let t2_count = new_order_queue_count(&mut t2)?;
    assert!(t1_count < QUEUE_CAP);
    assert!(t2_count < QUEUE_CAP);

    insert_order_and_new_order(&mut t1, 3002)?;
    insert_order_and_new_order(&mut t2, 3003)?;

    t1.commit()?;
    if expect_serialization_failure {
        assert_serialization_failure(t2.commit());
    } else {
        t2.commit()?;
    }

    new_order_queue_count(&mut seed)
}

#[test]
fn customer_balance_pair_write_skew_prevented() -> Result<(), Box<dyn Error>> {
    let repeatable_read_total = run_balance_pair_write_skew("REPEATABLE READ", false)?;
    // Snapshot isolation permits both individually safe debits and breaks the pair invariant.
    assert!(
        repeatable_read_total < 0,
        "repeatable read should demonstrate the write-skew anomaly"
    );

    let serializable_total = run_balance_pair_write_skew("SERIALIZABLE", true)?;
    // Serializable isolation rejects one debit, preserving the non-negative combined balance.
    assert!(
        serializable_total >= 0,
        "serializable execution must preserve the balance-pair invariant"
    );
    Ok(())
}

#[test]
fn new_order_queue_cap_phantom_skew_prevented() -> Result<(), Box<dyn Error>> {
    const QUEUE_CAP: i64 = 2;

    let repeatable_read_count = run_new_order_queue_phantom_skew("REPEATABLE READ", false)?;
    // Snapshot isolation permits both inserts based on the same stale predicate result.
    assert!(
        repeatable_read_count > QUEUE_CAP,
        "repeatable read should demonstrate the queue-cap anomaly"
    );

    let serializable_count = run_new_order_queue_phantom_skew("SERIALIZABLE", true)?;
    // Serializable isolation rejects one insert, preserving the queue-size cap.
    assert!(
        serializable_count <= QUEUE_CAP,
        "serializable execution must preserve the queue cap"
    );
    Ok(())
}

#[test]
fn five_transactions_under_serializable_keep_consistency_conditions() -> Result<(), Box<dyn Error>>
{
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    let mut session = server.open_session()?;
    let dataset = small_consistent_dataset();

    load_dataset(&server, directory.path(), &dataset, &LoadOptions::default())?;

    let initial_violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &initial_violations, 2)?;

    let delivered_before = delivered_orders_by_district(&mut session, 2)?;
    assert_eq!(delivered_before, vec![DELIVERED_ORDER_COUNT; 2]);

    let report = run_with_isolation(
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
        DriverIsolation::Serializable,
    )?;

    assert_report(&report, 230);

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

    // Two terminals contend on shared district and warehouse rows under whole-partition
    // read footprints, which in practice always produces serialization aborts (50 consecutive
    // runs passed), but this is not a deterministic guarantee. The two anomaly tests above
    // provide deterministic evidence that SERIALIZABLE was active.
    let validation_aborts = server
        .txn_manager()
        .expect("local server transaction manager")
        .serializable_stats()
        .validation_aborts;
    assert!(
        validation_aborts > 0,
        "serializable driver run should produce validation aborts"
    );

    let violations = check_consistency(&mut session, &[1])?;
    assert_condition_11_and_delivery_invariant(&mut session, &violations, 2)?;
    Ok(())
}
