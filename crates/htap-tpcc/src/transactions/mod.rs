//! TPC-C transactional workload operations.

use std::fmt;

use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::Session;
use htap_sql::result::{CommandResult, StatementResult};

use crate::history::build_h_id;
use crate::load::format_decimal;

#[derive(Debug)]
pub enum TransactionError {
    /// A write-write conflict. Retry the failed atomic transaction from `BEGIN`.
    Conflict,
    /// The TPC-C New-Order transaction's expected invalid-item rollback.
    ExpectedRollback,
    /// Caller-provided input does not satisfy TPC-C transaction constraints.
    InvalidInput(String),
    /// An engine failure other than a write-write conflict.
    Htap(HtapError),
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => write!(f, "transaction conflict; retry the complete transaction"),
            Self::ExpectedRollback => write!(f, "expected TPC-C rollback"),
            Self::InvalidInput(message) => write!(f, "invalid transaction input: {message}"),
            Self::Htap(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TransactionError {}

impl From<HtapError> for TransactionError {
    fn from(error: HtapError) -> Self {
        match error {
            HtapError::Conflict(_) => Self::Conflict,
            error => Self::Htap(error),
        }
    }
}

pub type Result<T> = std::result::Result<T, TransactionError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrderItem {
    pub item_id: i64,
    pub supply_w_id: i64,
    pub quantity: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrderRequest {
    pub w_id: i64,
    pub d_id: i64,
    pub c_id: i64,
    /// Entry timestamp in microseconds since the Unix epoch.
    pub entry_timestamp_micros: i64,
    pub items: Vec<NewOrderItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrderLineResult {
    pub item_id: i64,
    pub item_name: String,
    pub supply_w_id: i64,
    pub quantity: i64,
    pub amount: i64,
    pub brand_generic: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrderResult {
    pub order_id: i64,
    /// Entry timestamp in microseconds since the Unix epoch.
    pub entry_timestamp_micros: i64,
    pub customer_last: String,
    pub customer_credit: String,
    pub customer_discount: i64,
    pub warehouse_tax: i64,
    pub district_tax: i64,
    pub total_amount: i64,
    pub all_local: bool,
    pub order_lines: Vec<NewOrderLineResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomerSelector {
    Id(i64),
    LastName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRequest {
    pub w_id: i64,
    pub d_id: i64,
    pub customer_w_id: i64,
    pub customer_d_id: i64,
    pub customer: CustomerSelector,
    /// Entry timestamp in microseconds since the Unix epoch.
    pub entry_timestamp_micros: i64,
    /// Payment amount in cents.
    pub h_amount: i64,
    /// Nonzero runtime HISTORY source, typically a terminal or session number.
    pub h_terminal: u16,
    /// Monotonically increasing sequence within `h_terminal`.
    pub h_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentResult {
    pub customer_id: i64,
    pub customer_first: String,
    pub customer_middle: String,
    pub customer_last: String,
    pub customer_street_1: String,
    pub customer_street_2: String,
    pub customer_city: String,
    pub customer_state: String,
    pub customer_zip: String,
    pub customer_phone: String,
    /// Customer timestamp in microseconds since the Unix epoch.
    pub customer_since_micros: i64,
    pub customer_credit: String,
    pub customer_credit_limit: i64,
    pub customer_discount: i64,
    pub customer_balance: i64,
    pub warehouse_name: String,
    pub warehouse_street_1: String,
    pub warehouse_street_2: String,
    pub warehouse_city: String,
    pub warehouse_state: String,
    pub warehouse_zip: String,
    pub district_name: String,
    pub district_street_1: String,
    pub district_street_2: String,
    pub district_city: String,
    pub district_state: String,
    pub district_zip: String,
    pub history_id: i64,
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn query(session: &mut Session, sql: &str) -> Result<htap_sql::result::QueryResult> {
    match session.execute(sql) {
        Ok(StatementResult::Query(result)) => Ok(result),
        Ok(result) => Err(TransactionError::InvalidInput(format!(
            "expected query result for '{sql}', got {result:?}"
        ))),
        Err(error) => Err(TransactionError::from(error)),
    }
}

fn execute(session: &mut Session, sql: &str) -> Result<()> {
    match session.execute(sql) {
        Ok(_) => Ok(()),
        Err(error) => Err(TransactionError::from(error)),
    }
}

fn int(row: &htap_common::types::Row, index: usize) -> Result<i64> {
    match row.get(index) {
        Some(&Value::Int32(value)) => Ok(i64::from(value)),
        Some(&Value::Int64(value)) => Ok(value),
        Some(&Value::Decimal { value, .. }) => Ok(value),
        value => Err(TransactionError::InvalidInput(format!(
            "expected integer-compatible result column {index}, got {value:?}"
        ))),
    }
}

fn string(row: &htap_common::types::Row, index: usize) -> Result<String> {
    match row.get(index) {
        Some(Value::String(value)) => Ok(value.clone()),
        value => Err(TransactionError::InvalidInput(format!(
            "expected string result column {index}, got {value:?}"
        ))),
    }
}

fn one_row(
    result: htap_sql::result::QueryResult,
    description: &str,
) -> Result<htap_common::types::Row> {
    if result.num_rows() != 1 {
        return Err(TransactionError::InvalidInput(format!(
            "{description} was not found"
        )));
    }
    Ok(result.rows()[0].clone())
}

fn rollback_after<T>(session: &mut Session, error: TransactionError) -> Result<T> {
    match session.rollback() {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(TransactionError::from(rollback_error)),
    }
}

fn multiply_scaled_round_half_away_from_zero(
    left: i64,
    middle: i64,
    right: i64,
    scale: i64,
) -> Result<i64> {
    let value = left
        .checked_mul(middle)
        .and_then(|value| value.checked_mul(right))
        .ok_or_else(|| TransactionError::InvalidInput("decimal calculation overflowed".into()))?;
    let quotient = value / scale;
    let remainder = value % scale;

    // TPC-C's total is rounded once, using the engine's half-away-from-zero decimal convention.
    if remainder.abs() * 2 >= scale {
        quotient
            .checked_add(if value.is_negative() { -1 } else { 1 })
            .ok_or_else(|| TransactionError::InvalidInput("decimal calculation overflowed".into()))
    } else {
        Ok(quotient)
    }
}

fn new_order_inner(session: &mut Session, request: &NewOrderRequest) -> Result<NewOrderResult> {
    if request.w_id <= 0 || request.d_id <= 0 || request.c_id <= 0 {
        return Err(TransactionError::InvalidInput(
            "warehouse, district, and customer IDs must be positive".into(),
        ));
    }
    if !(5..=15).contains(&request.items.len()) {
        return Err(TransactionError::InvalidInput(
            "New-Order requires from 5 through 15 order lines".into(),
        ));
    }
    if request.items.iter().any(|item| {
        item.item_id <= 0 || item.supply_w_id <= 0 || !(1..=10).contains(&item.quantity)
    }) {
        return Err(TransactionError::InvalidInput(
            "item IDs and supply warehouse IDs must be positive and quantities must be 1 through 10"
                .into(),
        ));
    }

    let warehouse = one_row(
        query(
            session,
            &format!("SELECT w_tax FROM warehouse WHERE w_id = {}", request.w_id),
        )?,
        "warehouse",
    )?;
    let w_tax = int(&warehouse, 0)?;

    let district = one_row(
        query(
            session,
            &format!(
                "SELECT d_tax, d_next_o_id FROM district WHERE d_w_id = {} AND d_id = {}",
                request.w_id, request.d_id
            ),
        )?,
        "district",
    )?;
    let d_tax = int(&district, 0)?;
    let order_id = int(&district, 1)?;
    execute(
        session,
        &format!(
            "UPDATE district SET d_next_o_id = d_next_o_id + 1 \
             WHERE d_w_id = {} AND d_id = {}",
            request.w_id, request.d_id
        ),
    )?;

    let customer = one_row(
        query(
            session,
            &format!(
                "SELECT c_discount, c_last, c_credit FROM customer \
                 WHERE c_w_id = {} AND c_d_id = {} AND c_id = {}",
                request.w_id, request.d_id, request.c_id
            ),
        )?,
        "customer",
    )?;
    let c_discount = int(&customer, 0)?;
    let c_last = string(&customer, 1)?;
    let c_credit = string(&customer, 2)?;
    let all_local = request
        .items
        .iter()
        .all(|item| item.supply_w_id == request.w_id);

    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({order_id}, {}, {}, {}, {}, NULL, {}, {})",
            request.d_id,
            request.w_id,
            request.c_id,
            request.entry_timestamp_micros,
            request.items.len(),
            i64::from(all_local),
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO new_order (no_o_id, no_d_id, no_w_id) \
             VALUES ({order_id}, {}, {})",
            request.d_id, request.w_id
        ),
    )?;

    let mut line_results = Vec::with_capacity(request.items.len());
    let mut line_total = 0_i64;
    for (line_number, item) in request.items.iter().enumerate() {
        let item_row = query(
            session,
            &format!(
                "SELECT i_price, i_name, i_data FROM item WHERE i_id = {}",
                item.item_id
            ),
        )?;
        if item_row.num_rows() == 0 {
            return Err(TransactionError::ExpectedRollback);
        }
        let item_row = one_row(item_row, "item")?;
        let i_price = int(&item_row, 0)?;
        let i_name = string(&item_row, 1)?;
        let i_data = string(&item_row, 2)?;

        let stock = one_row(
            query(
                session,
                &format!(
                    "SELECT s_quantity, s_data, s_dist_{:02} FROM stock \
                     WHERE s_w_id = {} AND s_i_id = {}",
                    request.d_id, item.supply_w_id, item.item_id
                ),
            )?,
            "stock",
        )?;
        let s_quantity = int(&stock, 0)?;
        let s_data = string(&stock, 1)?;
        let dist_info = string(&stock, 2)?;
        let adjusted_quantity = if s_quantity - item.quantity >= 10 {
            s_quantity - item.quantity
        } else {
            s_quantity - item.quantity + 91
        };
        let remote_increment = i64::from(item.supply_w_id != request.w_id);
        execute(
            session,
            &format!(
                "UPDATE stock SET s_quantity = {adjusted_quantity}, \
                 s_ytd = s_ytd + {}, s_order_cnt = s_order_cnt + 1, \
                 s_remote_cnt = s_remote_cnt + {remote_increment} \
                 WHERE s_w_id = {} AND s_i_id = {}",
                item.quantity, item.supply_w_id, item.item_id
            ),
        )?;

        let amount = item
            .quantity
            .checked_mul(i_price)
            .ok_or_else(|| TransactionError::InvalidInput("order-line amount overflowed".into()))?;
        line_total = line_total
            .checked_add(amount)
            .ok_or_else(|| TransactionError::InvalidInput("order total overflowed".into()))?;
        let brand_generic = if i_data.contains("ORIGINAL") && s_data.contains("ORIGINAL") {
            'B'
        } else {
            'G'
        };
        execute(
            session,
            &format!(
                "INSERT INTO order_line \
                 (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
                  ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
                 VALUES ({order_id}, {}, {}, {}, {}, {}, NULL, {}, {}, {})",
                request.d_id,
                request.w_id,
                line_number + 1,
                item.item_id,
                item.supply_w_id,
                item.quantity,
                format_decimal(amount),
                sql_literal(&dist_info),
            ),
        )?;
        line_results.push(NewOrderLineResult {
            item_id: item.item_id,
            item_name: i_name,
            supply_w_id: item.supply_w_id,
            quantity: item.quantity,
            amount,
            brand_generic,
        });
    }

    let total_amount = multiply_scaled_round_half_away_from_zero(
        line_total,
        10_000 - c_discount,
        10_000 + w_tax + d_tax,
        100_000_000,
    )?;
    Ok(NewOrderResult {
        order_id,
        entry_timestamp_micros: request.entry_timestamp_micros,
        customer_last: c_last,
        customer_credit: c_credit,
        customer_discount: c_discount,
        warehouse_tax: w_tax,
        district_tax: d_tax,
        total_amount,
        all_local,
        order_lines: line_results,
    })
}

/// Runs a TPC-C New-Order transaction atomically.
pub fn new_order(session: &mut Session, request: &NewOrderRequest) -> Result<NewOrderResult> {
    session.begin().map_err(TransactionError::from)?;
    match new_order_inner(session, request) {
        Ok(result) => match session.commit() {
            Ok(()) => Ok(result),
            Err(error) => rollback_after(session, TransactionError::from(error)),
        },
        Err(error) => rollback_after(session, error),
    }
}

fn payment_inner(session: &mut Session, request: &PaymentRequest) -> Result<PaymentResult> {
    if request.w_id <= 0
        || request.d_id <= 0
        || request.customer_w_id <= 0
        || request.customer_d_id <= 0
        || request.h_amount <= 0
    {
        return Err(TransactionError::InvalidInput(
            "IDs and payment amount must be positive".into(),
        ));
    }
    let history_id = build_h_id(request.h_terminal, request.h_sequence)
        .map_err(|message| TransactionError::InvalidInput(message.into()))?;

    let warehouse = one_row(
        query(
            session,
            &format!(
                "SELECT w_name, w_street_1, w_street_2, w_city, w_state, w_zip \
                 FROM warehouse WHERE w_id = {}",
                request.w_id
            ),
        )?,
        "warehouse",
    )?;
    let warehouse_name = string(&warehouse, 0)?;
    let warehouse_street_1 = string(&warehouse, 1)?;
    let warehouse_street_2 = string(&warehouse, 2)?;
    let warehouse_city = string(&warehouse, 3)?;
    let warehouse_state = string(&warehouse, 4)?;
    let warehouse_zip = string(&warehouse, 5)?;
    execute(
        session,
        &format!(
            "UPDATE warehouse SET w_ytd = w_ytd + {} WHERE w_id = {}",
            format_decimal(request.h_amount),
            request.w_id
        ),
    )?;

    let district = one_row(
        query(
            session,
            &format!(
                "SELECT d_name, d_street_1, d_street_2, d_city, d_state, d_zip \
                 FROM district WHERE d_w_id = {} AND d_id = {}",
                request.w_id, request.d_id
            ),
        )?,
        "district",
    )?;
    let district_name = string(&district, 0)?;
    let district_street_1 = string(&district, 1)?;
    let district_street_2 = string(&district, 2)?;
    let district_city = string(&district, 3)?;
    let district_state = string(&district, 4)?;
    let district_zip = string(&district, 5)?;
    execute(
        session,
        &format!(
            "UPDATE district SET d_ytd = d_ytd + {} \
             WHERE d_w_id = {} AND d_id = {}",
            format_decimal(request.h_amount),
            request.w_id,
            request.d_id
        ),
    )?;

    let customer_sql = match &request.customer {
        CustomerSelector::Id(customer_id) if *customer_id > 0 => format!(
            "SELECT c_id, c_first, c_middle, c_last, c_street_1, c_street_2, c_city, \
             c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
             c_balance, c_data FROM customer \
             WHERE c_w_id = {} AND c_d_id = {} AND c_id = {customer_id}",
            request.customer_w_id, request.customer_d_id
        ),
        CustomerSelector::Id(_) => {
            return Err(TransactionError::InvalidInput(
                "customer ID must be positive".into(),
            ))
        }
        CustomerSelector::LastName(last_name) if !last_name.is_empty() => format!(
            "SELECT c_id, c_first, c_middle, c_last, c_street_1, c_street_2, c_city, \
             c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
             c_balance, c_data FROM customer \
             WHERE c_w_id = {} AND c_d_id = {} AND c_last = {} ORDER BY c_first",
            request.customer_w_id,
            request.customer_d_id,
            sql_literal(last_name)
        ),
        CustomerSelector::LastName(_) => {
            return Err(TransactionError::InvalidInput(
                "customer last name must not be empty".into(),
            ))
        }
    };
    let customers = query(session, &customer_sql)?;
    if customers.num_rows() == 0 {
        return Err(TransactionError::InvalidInput(
            "customer was not found".into(),
        ));
    }
    let customer_index = match request.customer {
        CustomerSelector::Id(_) => 0,
        CustomerSelector::LastName(_) => (customers.num_rows() - 1) / 2,
    };
    let customer = customers.rows()[customer_index].clone();
    let customer_id = int(&customer, 0)?;
    let customer_first = string(&customer, 1)?;
    let customer_middle = string(&customer, 2)?;
    let customer_last = string(&customer, 3)?;
    let customer_street_1 = string(&customer, 4)?;
    let customer_street_2 = string(&customer, 5)?;
    let customer_city = string(&customer, 6)?;
    let customer_state = string(&customer, 7)?;
    let customer_zip = string(&customer, 8)?;
    let customer_phone = string(&customer, 9)?;
    let customer_since_micros = optional_int(&customer, 10)?.ok_or_else(|| {
        TransactionError::InvalidInput("customer since timestamp must not be null".into())
    })?;
    let customer_credit = string(&customer, 11)?;
    let customer_credit_limit = int(&customer, 12)?;
    let customer_discount = int(&customer, 13)?;
    let customer_balance = int(&customer, 14)?
        .checked_sub(request.h_amount)
        .ok_or_else(|| TransactionError::InvalidInput("customer balance overflowed".into()))?;
    let customer_data = string(&customer, 15)?;

    let new_data = if customer_credit == "BC" {
        let prefix = format!(
            "{} {} {} {} {} {}|",
            customer_id,
            request.customer_d_id,
            request.customer_w_id,
            request.d_id,
            request.w_id,
            format_decimal(request.h_amount),
        );
        format!("{prefix}{customer_data}")
            .chars()
            .take(500)
            .collect()
    } else {
        customer_data
    };
    execute(
        session,
        &format!(
            "UPDATE customer SET c_balance = {}, \
             c_ytd_payment = c_ytd_payment + {}, c_payment_cnt = c_payment_cnt + 1, \
             c_data = {} WHERE c_w_id = {} AND c_d_id = {} AND c_id = {customer_id}",
            format_decimal(customer_balance),
            format_decimal(request.h_amount),
            sql_literal(&new_data),
            request.customer_w_id,
            request.customer_d_id,
        ),
    )?;

    let history_data = format!("{warehouse_name}    {district_name}");
    execute(
        session,
        &format!(
            "INSERT INTO history \
             (h_id, h_c_id, h_c_d_id, h_c_w_id, h_d_id, h_w_id, h_date, h_amount, h_data) \
             VALUES ({history_id}, {customer_id}, {}, {}, {}, {}, \
             {}, {}, {})",
            request.customer_d_id,
            request.customer_w_id,
            request.d_id,
            request.w_id,
            request.entry_timestamp_micros,
            format_decimal(request.h_amount),
            sql_literal(&history_data),
        ),
    )?;

    Ok(PaymentResult {
        customer_id,
        customer_first,
        customer_middle,
        customer_last,
        customer_street_1,
        customer_street_2,
        customer_city,
        customer_state,
        customer_zip,
        customer_phone,
        customer_since_micros,
        customer_credit,
        customer_credit_limit,
        customer_discount,
        customer_balance,
        warehouse_name,
        warehouse_street_1,
        warehouse_street_2,
        warehouse_city,
        warehouse_state,
        warehouse_zip,
        district_name,
        district_street_1,
        district_street_2,
        district_city,
        district_state,
        district_zip,
        history_id,
    })
}

/// Runs a TPC-C Payment transaction atomically.
///
/// Callers must keep `(h_terminal, h_sequence)` unique for every HISTORY insert.
pub fn payment(session: &mut Session, request: &PaymentRequest) -> Result<PaymentResult> {
    session.begin().map_err(TransactionError::from)?;
    match payment_inner(session, request) {
        Ok(result) => match session.commit() {
            Ok(()) => Ok(result),
            Err(error) => rollback_after(session, TransactionError::from(error)),
        },
        Err(error) => rollback_after(session, error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusRequest {
    pub w_id: i64,
    pub d_id: i64,
    pub customer: CustomerSelector,
    /// Entry timestamp in microseconds since the Unix epoch.
    pub entry_timestamp_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusLineResult {
    pub item_id: i64,
    pub supply_w_id: i64,
    pub quantity: i64,
    pub amount: i64,
    /// Delivery timestamp in microseconds since the Unix epoch.
    pub delivery_d: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusResult {
    pub customer_id: i64,
    pub customer_first: String,
    pub customer_middle: String,
    pub customer_last: String,
    pub customer_balance: i64,
    pub order_id: i64,
    /// Entry timestamp in microseconds since the Unix epoch.
    pub order_entry_timestamp_micros: i64,
    pub order_carrier_id: Option<i64>,
    pub order_lines: Vec<OrderStatusLineResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRequest {
    pub w_id: i64,
    pub carrier_id: i64,
    /// Delivery timestamp in microseconds since the Unix epoch.
    pub delivery_timestamp_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryDistrictResult {
    Skipped,
    Delivered {
        order_id: i64,
        customer_id: i64,
        sum_amount: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryResult {
    pub per_district: Vec<DeliveryDistrictResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockLevelRequest {
    pub w_id: i64,
    pub d_id: i64,
    pub threshold: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockLevelResult {
    pub low_stock: i64,
}

fn optional_int(row: &htap_common::types::Row, index: usize) -> Result<Option<i64>> {
    match row.get(index) {
        Some(Value::Null) => Ok(None),
        Some(&Value::Int32(value)) => Ok(Some(i64::from(value))),
        Some(&Value::Int64(value)) => Ok(Some(value)),
        Some(&Value::Decimal { value, .. }) => Ok(Some(value)),
        Some(&Value::Timestamp(value)) => Ok(Some(value)),
        value => Err(TransactionError::InvalidInput(format!(
            "expected nullable integer-compatible result column {index}, got {value:?}"
        ))),
    }
}

fn execute_target(session: &mut Session, sql: &str, description: &str) -> Result<()> {
    match session.execute(sql).map_err(TransactionError::from)? {
        StatementResult::Command(CommandResult::Dml { affected, .. }) if affected > 0 => Ok(()),
        StatementResult::Command(CommandResult::Dml { .. }) => Err(TransactionError::InvalidInput(
            format!("{description} was not found"),
        )),
        result => Err(TransactionError::InvalidInput(format!(
            "expected DML result for '{sql}', got {result:?}"
        ))),
    }
}

fn select_order_status_customer(
    session: &mut Session,
    request: &OrderStatusRequest,
) -> Result<htap_common::types::Row> {
    let customer_sql = match &request.customer {
        CustomerSelector::Id(customer_id) if *customer_id > 0 => format!(
            "SELECT c_id, c_first, c_middle, c_last, c_balance \
             FROM customer WHERE c_w_id = {} AND c_d_id = {} AND c_id = {customer_id}",
            request.w_id, request.d_id
        ),
        CustomerSelector::Id(_) => {
            return Err(TransactionError::InvalidInput(
                "customer ID must be positive".into(),
            ))
        }
        CustomerSelector::LastName(last_name) if !last_name.is_empty() => format!(
            "SELECT c_id, c_first, c_middle, c_last, c_balance \
             FROM customer WHERE c_w_id = {} AND c_d_id = {} AND c_last = {} \
             ORDER BY c_first",
            request.w_id,
            request.d_id,
            sql_literal(last_name)
        ),
        CustomerSelector::LastName(_) => {
            return Err(TransactionError::InvalidInput(
                "customer last name must not be empty".into(),
            ))
        }
    };
    let customers = query(session, &customer_sql)?;
    if customers.num_rows() == 0 {
        return Err(TransactionError::InvalidInput(
            "customer was not found".into(),
        ));
    }
    let customer_index = match request.customer {
        CustomerSelector::Id(_) => 0,
        CustomerSelector::LastName(_) => (customers.num_rows() - 1) / 2,
    };
    Ok(customers.rows()[customer_index].clone())
}

fn order_status_inner(
    session: &mut Session,
    request: &OrderStatusRequest,
) -> Result<OrderStatusResult> {
    if request.w_id <= 0 || request.d_id <= 0 {
        return Err(TransactionError::InvalidInput(
            "warehouse and district IDs must be positive".into(),
        ));
    }

    let customer = select_order_status_customer(session, request)?;
    let customer_id = int(&customer, 0)?;
    let customer_first = string(&customer, 1)?;
    let customer_middle = string(&customer, 2)?;
    let customer_last = string(&customer, 3)?;
    let customer_balance = int(&customer, 4)?;

    let order = one_row(
        query(
            session,
            &format!(
                "SELECT o_id, o_entry_d, o_carrier_id FROM orders \
                 WHERE o_w_id = {} AND o_d_id = {} AND o_c_id = {customer_id} \
                 ORDER BY o_id DESC LIMIT 1",
                request.w_id, request.d_id
            ),
        )?,
        "customer order",
    )?;
    let order_id = int(&order, 0)?;
    let order_entry_timestamp_micros = optional_int(&order, 1)?.ok_or_else(|| {
        TransactionError::InvalidInput("order entry timestamp must not be null".into())
    })?;
    let order_carrier_id = optional_int(&order, 2)?;

    let lines = query(
        session,
        &format!(
            "SELECT ol_i_id, ol_supply_w_id, ol_quantity, ol_amount, ol_delivery_d \
             FROM order_line WHERE ol_w_id = {} AND ol_d_id = {} AND ol_o_id = {order_id} \
             ORDER BY ol_number",
            request.w_id, request.d_id
        ),
    )?;
    let mut order_lines = Vec::with_capacity(lines.num_rows());
    for line in lines.rows() {
        order_lines.push(OrderStatusLineResult {
            item_id: int(line, 0)?,
            supply_w_id: int(line, 1)?,
            quantity: int(line, 2)?,
            amount: int(line, 3)?,
            delivery_d: optional_int(line, 4)?,
        });
    }

    Ok(OrderStatusResult {
        customer_id,
        customer_first,
        customer_middle,
        customer_last,
        customer_balance,
        order_id,
        order_entry_timestamp_micros,
        order_carrier_id,
        order_lines,
    })
}

/// Runs a TPC-C Order-Status transaction atomically.
pub fn order_status(
    session: &mut Session,
    request: &OrderStatusRequest,
) -> Result<OrderStatusResult> {
    session.begin().map_err(TransactionError::from)?;
    match order_status_inner(session, request) {
        Ok(result) => match session.commit() {
            Ok(()) => Ok(result),
            Err(error) => rollback_after(session, TransactionError::from(error)),
        },
        Err(error) => rollback_after(session, error),
    }
}

fn delivery_district(
    session: &mut Session,
    request: &DeliveryRequest,
    district_id: i64,
) -> Result<DeliveryDistrictResult> {
    let outstanding = query(
        session,
        &format!(
            "SELECT no_o_id FROM new_order \
             WHERE no_w_id = {} AND no_d_id = {district_id} ORDER BY no_o_id",
            request.w_id
        ),
    )?;
    if outstanding.num_rows() == 0 {
        return Ok(DeliveryDistrictResult::Skipped);
    }
    let order_id = int(&outstanding.rows()[0], 0)?;

    execute_target(
        session,
        &format!(
            "DELETE FROM new_order WHERE no_w_id = {} AND no_d_id = {district_id} \
             AND no_o_id = {order_id}",
            request.w_id
        ),
        "new-order row",
    )?;

    let order = one_row(
        query(
            session,
            &format!(
                "SELECT o_c_id FROM orders WHERE o_w_id = {} AND o_d_id = {district_id} \
                 AND o_id = {order_id}",
                request.w_id
            ),
        )?,
        "order",
    )?;
    let customer_id = int(&order, 0)?;
    execute_target(
        session,
        &format!(
            "UPDATE orders SET o_carrier_id = {} WHERE o_w_id = {} \
             AND o_d_id = {district_id} AND o_id = {order_id}",
            request.carrier_id, request.w_id
        ),
        "order",
    )?;

    let lines = query(
        session,
        &format!(
            "SELECT ol_amount FROM order_line WHERE ol_w_id = {} AND ol_d_id = {district_id} \
             AND ol_o_id = {order_id}",
            request.w_id
        ),
    )?;
    if lines.num_rows() == 0 {
        return Err(TransactionError::InvalidInput(
            "order lines were not found".into(),
        ));
    }
    let mut sum_amount = 0_i64;
    for line in lines.rows() {
        sum_amount = sum_amount
            .checked_add(int(line, 0)?)
            .ok_or_else(|| TransactionError::InvalidInput("order-line total overflowed".into()))?;
    }
    execute_target(
        session,
        &format!(
            "UPDATE order_line SET ol_delivery_d = {} WHERE ol_w_id = {} \
             AND ol_d_id = {district_id} AND ol_o_id = {order_id}",
            request.delivery_timestamp_micros, request.w_id
        ),
        "order lines",
    )?;
    execute_target(
        session,
        &format!(
            "UPDATE customer SET c_balance = c_balance + {}, \
             c_delivery_cnt = c_delivery_cnt + 1 WHERE c_w_id = {} \
             AND c_d_id = {district_id} AND c_id = {customer_id}",
            format_decimal(sum_amount),
            request.w_id
        ),
        "customer",
    )?;

    Ok(DeliveryDistrictResult::Delivered {
        order_id,
        customer_id,
        sum_amount,
    })
}

/// Runs one TPC-C Delivery district atomically.
///
/// A conflict retries only this district's transaction, not districts that have
/// already committed for the same Delivery input.
pub fn delivery_one_district(
    session: &mut Session,
    request: &DeliveryRequest,
    district_id: i64,
) -> Result<DeliveryDistrictResult> {
    if request.w_id <= 0 || request.carrier_id <= 0 {
        return Err(TransactionError::InvalidInput(
            "warehouse and carrier IDs must be positive".into(),
        ));
    }
    if !(1..=10).contains(&district_id) {
        return Err(TransactionError::InvalidInput(
            "district ID must be from 1 through 10".into(),
        ));
    }

    session.begin().map_err(TransactionError::from)?;
    match delivery_district(session, request, district_id) {
        Ok(result) => match session.commit() {
            Ok(()) => Ok(result),
            Err(error) => rollback_after(session, TransactionError::from(error)),
        },
        Err(error) => rollback_after(session, error),
    }
}

/// Runs the TPC-C Delivery transaction, using one transaction for each district.
pub fn delivery(session: &mut Session, request: &DeliveryRequest) -> Result<DeliveryResult> {
    let mut per_district = Vec::with_capacity(10);
    for district_id in 1..=10 {
        per_district.push(delivery_one_district(session, request, district_id)?);
    }
    Ok(DeliveryResult { per_district })
}

fn stock_level_inner(
    session: &mut Session,
    request: &StockLevelRequest,
) -> Result<StockLevelResult> {
    if request.w_id <= 0 || request.d_id <= 0 || request.threshold <= 0 {
        return Err(TransactionError::InvalidInput(
            "warehouse, district, and threshold must be positive".into(),
        ));
    }

    let district = one_row(
        query(
            session,
            &format!(
                "SELECT d_next_o_id FROM district WHERE d_w_id = {} AND d_id = {}",
                request.w_id, request.d_id
            ),
        )?,
        "district",
    )?;
    let next_order_id = int(&district, 0)?;
    let lower_order_id = next_order_id.saturating_sub(20);
    let lines = query(
        session,
        &format!(
            "SELECT ol_i_id FROM order_line WHERE ol_w_id = {} AND ol_d_id = {} \
             AND ol_o_id >= {lower_order_id} AND ol_o_id < {next_order_id}",
            request.w_id, request.d_id
        ),
    )?;

    let mut item_ids = Vec::new();
    for line in lines.rows() {
        let item_id = int(line, 0)?;
        if !item_ids.contains(&item_id) {
            item_ids.push(item_id);
        }
    }

    let mut low_stock = 0_i64;
    for item_id in item_ids {
        let stock = one_row(
            query(
                session,
                &format!(
                    "SELECT s_quantity FROM stock WHERE s_w_id = {} AND s_i_id = {item_id}",
                    request.w_id
                ),
            )?,
            "stock",
        )?;
        if int(&stock, 0)? < request.threshold {
            low_stock += 1;
        }
    }
    Ok(StockLevelResult { low_stock })
}

/// Runs a TPC-C Stock-Level transaction atomically.
pub fn stock_level(session: &mut Session, request: &StockLevelRequest) -> Result<StockLevelResult> {
    session.begin().map_err(TransactionError::from)?;
    match stock_level_inner(session, request) {
        Ok(result) => match session.commit() {
            Ok(()) => Ok(result),
            Err(error) => rollback_after(session, TransactionError::from(error)),
        },
        Err(error) => rollback_after(session, error),
    }
}
