#![doc = r#"
TPC-C workload driver support.

The driver runs fixed-deck transaction-mix selection (Clause 5.2.4.2) over N terminals, each with its own session and fixed home warehouse. Transaction inputs follow each transaction's input clause (2.4-2.8), with NURand parameters as specified (A and range). A fixed C constant is used for all NURand calls and all terminals; this is a disclosed simplification of Clause 2.1.6.1's C-delta rule.

The default scale is the TPC-C specification scale: 10 districts per warehouse, 3,000 customers per district, and 100,000 items. Non-default scales exist only to support small functional tests; they are not TPC-C compliant benchmark configurations.

The underlying engine is optimistic and uses first-committer-wins conflict detection. The driver implements a liveness mechanism, not the engine: after several conflicts, a transaction escalates to exclusive driver execution. This prevents long transactions from starving when there is no think time. The escalation will be disclosed in the TPC-C report. The underlying server still holds a process-wide execution lock for complete execute and commit calls, so driver-level concurrent submissions and sessions do not make statements execute in parallel.

No keying, think-time pacing, randomized conflict backoff, or response-time percentile gating is implemented (analogous to ADR-030). The minimum transaction mix percentages (Clause 5.2.3) and per-transaction NURand input ranges are implemented. Fixed decks guarantee the mix only for completed deck passes; a transaction limit may stop a run partway through a deck. Observed mix percentages include committed transactions only and exclude expected New-Order rollbacks. This module reports transaction counts, conflict retries, escalation attempts, and observed mix percentages but computes no official TPC metric (no tpmC, throughput rate, or price/performance).
"#]

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

use crate::generate::rng::RandomState;
use crate::generate::text::{c_last, nurand, permutation};
use crate::history::build_h_id;
use crate::transactions::{
    delivery_one_district, new_order, order_status, payment, stock_level, CustomerSelector,
    DeliveryRequest, NewOrderItem, NewOrderRequest, OrderStatusRequest, PaymentRequest,
    StockLevelRequest, TransactionError,
};

const DECK_SIZE: usize = 23;
const MAX_CONFLICT_RETRIES: u32 = 10;
const ESCALATION_CONFLICT_THRESHOLD: u32 = 3;
const FIXED_C: u64 = 157;

pub const COMPLETED_LABEL: &str = "completed transaction count";
pub const EXPECTED_ROLLBACKS_LABEL: &str = "expected rollback count";
pub const CONFLICT_RETRIES_LABEL: &str = "conflict retry count";
pub const ESCALATIONS_LABEL: &str = "exclusive execution escalation count";
pub const OBSERVED_MIX_LABEL: &str = "observed transaction mix percentage";
pub const ELAPSED_LABEL: &str = "elapsed duration";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionKind {
    NewOrder,
    Payment,
    OrderStatus,
    Delivery,
    StockLevel,
}

impl TransactionKind {
    const ALL: [Self; 5] = [
        Self::NewOrder,
        Self::Payment,
        Self::OrderStatus,
        Self::Delivery,
        Self::StockLevel,
    ];

    fn deck_card(self) -> u8 {
        match self {
            Self::NewOrder => 0,
            Self::Payment => 1,
            Self::OrderStatus => 2,
            Self::Delivery => 3,
            Self::StockLevel => 4,
        }
    }

    fn from_deck_card(card: u8) -> Self {
        match card {
            0 => Self::NewOrder,
            1 => Self::Payment,
            2 => Self::OrderStatus,
            3 => Self::Delivery,
            4 => Self::StockLevel,
            _ => unreachable!("TPC-C deck only contains known cards"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionCounts {
    pub completed: u64,
    pub expected_rollbacks: u64,
    pub conflict_retries: u64,
    pub escalations: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransactionMix {
    pub kind: TransactionKind,
    pub counts: TransactionCounts,
    pub observed_percentage: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadReport {
    pub transactions: Vec<TransactionMix>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum DriverError {
    Htap(HtapError),
    Transaction(TransactionError),
    InvalidWarehouseCount,
    InvalidTerminalCount,
    InvalidRunLimit,
    InvalidScale,
    DriverLockPoisoned,
    WorkerPanicked,
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Htap(error) => write!(f, "TPC-C driver error: {error}"),
            Self::Transaction(error) => write!(f, "TPC-C transaction error: {error}"),
            Self::InvalidWarehouseCount => f.write_str("warehouse count must be positive"),
            Self::InvalidTerminalCount => f.write_str("terminal count must be positive"),
            Self::InvalidRunLimit => f.write_str("run limit must be nonzero"),
            Self::InvalidScale => f.write_str("all driver scale dimensions must be positive"),
            Self::DriverLockPoisoned => f.write_str("driver execution lock was poisoned"),
            Self::WorkerPanicked => f.write_str("driver worker panicked"),
        }
    }
}

impl Error for DriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Htap(error) => Some(error),
            Self::Transaction(error) => Some(error),
            Self::InvalidWarehouseCount
            | Self::InvalidTerminalCount
            | Self::InvalidRunLimit
            | Self::InvalidScale
            | Self::DriverLockPoisoned
            | Self::WorkerPanicked => None,
        }
    }
}

impl From<HtapError> for DriverError {
    fn from(error: HtapError) -> Self {
        Self::Htap(error)
    }
}

impl From<TransactionError> for DriverError {
    fn from(error: TransactionError) -> Self {
        Self::Transaction(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverScale {
    pub districts_per_warehouse: u32,
    pub customers_per_district: u32,
    pub item_count: u32,
}

impl Default for DriverScale {
    fn default() -> Self {
        Self {
            districts_per_warehouse: 10,
            customers_per_district: 3_000,
            item_count: 100_000,
        }
    }
}

impl DriverScale {
    fn valid(self) -> bool {
        self.districts_per_warehouse > 0 && self.customers_per_district > 0 && self.item_count > 0
    }

    fn last_name_count(self) -> u64 {
        u64::from(self.customers_per_district.min(1_000))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionLimitOrDuration {
    TransactionLimit(u64),
    Duration(Duration),
}

impl TransactionLimitOrDuration {
    fn valid(self) -> bool {
        match self {
            Self::TransactionLimit(limit) => limit > 0,
            Self::Duration(duration) => !duration.is_zero(),
        }
    }
}

fn timestamp_micros() -> Result<i64, DriverError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DriverError::Htap(HtapError::InvalidArgument(error.to_string())))?;
    i64::try_from(duration.as_micros())
        .map_err(|_| DriverError::Htap(HtapError::InvalidArgument("timestamp overflowed".into())))
}

fn random_bool(rng: &mut RandomState, numerator: u64, denominator: u64) -> bool {
    rng.structural_bounded(denominator) < numerator
}

fn random_warehouse(rng: &mut RandomState, warehouse_count: u32, excluding: i64) -> i64 {
    if warehouse_count == 1 {
        return excluding;
    }
    loop {
        let warehouse_id = (rng.structural_bounded(u64::from(warehouse_count)) + 1) as i64;
        if warehouse_id != excluding {
            return warehouse_id;
        }
    }
}

fn customer_selector(rng: &mut RandomState, scale: DriverScale) -> CustomerSelector {
    if random_bool(rng, 60, 100) {
        let last_name_number = nurand(rng, 255, 0, scale.last_name_count() - 1, FIXED_C);
        CustomerSelector::LastName(c_last(rng, 0, last_name_number))
    } else {
        CustomerSelector::Id(nurand(
            rng,
            1023,
            1,
            u64::from(scale.customers_per_district),
            FIXED_C,
        ) as i64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GeneratedInput {
    NewOrder {
        d_id: i64,
        c_id: i64,
        items: Vec<NewOrderItem>,
    },
    Payment {
        d_id: i64,
        customer_w_id: i64,
        customer_d_id: i64,
        customer: CustomerSelector,
        h_amount: i64,
    },
    OrderStatus {
        d_id: i64,
        customer: CustomerSelector,
    },
    Delivery {
        carrier_id: i64,
    },
    StockLevel {
        d_id: i64,
        threshold: i64,
    },
}

fn generate_input(
    rng: &mut RandomState,
    kind: TransactionKind,
    home_w_id: i64,
    warehouse_count: u32,
    scale: DriverScale,
    stock_level_d_id: i64,
) -> GeneratedInput {
    match kind {
        TransactionKind::NewOrder => {
            let line_count = (rng.structural_bounded(11) + 5) as usize;
            let invalid_line = random_bool(rng, 1, 100).then_some(line_count - 1);
            let mut items = Vec::with_capacity(line_count);
            for line in 0..line_count {
                let remote = warehouse_count > 1 && random_bool(rng, 1, 100);
                items.push(NewOrderItem {
                    item_id: if Some(line) == invalid_line {
                        i64::from(scale.item_count) + 1
                    } else {
                        nurand(rng, 8191, 1, u64::from(scale.item_count), FIXED_C) as i64
                    },
                    supply_w_id: if remote {
                        random_warehouse(rng, warehouse_count, home_w_id)
                    } else {
                        home_w_id
                    },
                    quantity: (rng.structural_bounded(10) + 1) as i64,
                });
            }
            GeneratedInput::NewOrder {
                d_id: (rng.structural_bounded(u64::from(scale.districts_per_warehouse)) + 1) as i64,
                c_id: nurand(
                    rng,
                    1023,
                    1,
                    u64::from(scale.customers_per_district),
                    FIXED_C,
                ) as i64,
                items,
            }
        }
        TransactionKind::Payment => {
            let d_id =
                (rng.structural_bounded(u64::from(scale.districts_per_warehouse)) + 1) as i64;
            let remote = warehouse_count > 1 && random_bool(rng, 15, 100);
            GeneratedInput::Payment {
                d_id,
                customer_w_id: if remote {
                    random_warehouse(rng, warehouse_count, home_w_id)
                } else {
                    home_w_id
                },
                customer_d_id: if remote {
                    (rng.structural_bounded(u64::from(scale.districts_per_warehouse)) + 1) as i64
                } else {
                    d_id
                },
                customer: customer_selector(rng, scale),
                h_amount: (rng.structural_bounded(499_901) + 100) as i64,
            }
        }
        TransactionKind::OrderStatus => GeneratedInput::OrderStatus {
            d_id: (rng.structural_bounded(u64::from(scale.districts_per_warehouse)) + 1) as i64,
            customer: customer_selector(rng, scale),
        },
        TransactionKind::Delivery => GeneratedInput::Delivery {
            carrier_id: (rng.structural_bounded(10) + 1) as i64,
        },
        TransactionKind::StockLevel => GeneratedInput::StockLevel {
            d_id: stock_level_d_id,
            threshold: (rng.structural_bounded(11) + 10) as i64,
        },
    }
}

fn deck(rng: &mut RandomState) -> Vec<TransactionKind> {
    let mut cards = Vec::with_capacity(DECK_SIZE);
    cards.extend(std::iter::repeat_n(
        TransactionKind::NewOrder.deck_card(),
        10,
    ));
    cards.extend(std::iter::repeat_n(
        TransactionKind::Payment.deck_card(),
        10,
    ));
    cards.push(TransactionKind::OrderStatus.deck_card());
    cards.push(TransactionKind::Delivery.deck_card());
    cards.push(TransactionKind::StockLevel.deck_card());

    let positions = permutation(rng, DECK_SIZE);
    positions
        .into_iter()
        .map(|position| TransactionKind::from_deck_card(cards[position]))
        .collect()
}

fn execute_input(
    session: &mut htap_server::Session,
    input: &GeneratedInput,
    home_w_id: i64,
    terminal_source: u16,
    history_sequence: &mut u64,
) -> Result<(), DriverError> {
    match input {
        GeneratedInput::NewOrder { d_id, c_id, items } => {
            new_order(
                session,
                &NewOrderRequest {
                    w_id: home_w_id,
                    d_id: *d_id,
                    c_id: *c_id,
                    entry_timestamp_micros: timestamp_micros()?,
                    items: items.clone(),
                },
            )?;
        }
        GeneratedInput::Payment {
            d_id,
            customer_w_id,
            customer_d_id,
            customer,
            h_amount,
        } => {
            payment(
                session,
                &PaymentRequest {
                    w_id: home_w_id,
                    d_id: *d_id,
                    customer_w_id: *customer_w_id,
                    customer_d_id: *customer_d_id,
                    customer: customer.clone(),
                    entry_timestamp_micros: timestamp_micros()?,
                    h_amount: *h_amount,
                    h_terminal: terminal_source,
                    h_sequence: *history_sequence,
                },
            )?;
            *history_sequence = history_sequence.checked_add(1).ok_or_else(|| {
                DriverError::Htap(HtapError::InvalidArgument(
                    "history sequence overflowed".into(),
                ))
            })?;
        }
        GeneratedInput::OrderStatus { d_id, customer } => {
            order_status(
                session,
                &OrderStatusRequest {
                    w_id: home_w_id,
                    d_id: *d_id,
                    customer: customer.clone(),
                    entry_timestamp_micros: timestamp_micros()?,
                },
            )?;
        }
        GeneratedInput::Delivery { .. } => {
            return Err(DriverError::Htap(HtapError::InvalidArgument(
                "Delivery input must be executed district-by-district".into(),
            )));
        }
        GeneratedInput::StockLevel { d_id, threshold } => {
            stock_level(
                session,
                &StockLevelRequest {
                    w_id: home_w_id,
                    d_id: *d_id,
                    threshold: *threshold,
                },
            )?;
        }
    }
    Ok(())
}

fn execute_with_driver_lock(
    execution_lock: &RwLock<()>,
    exclusive: bool,
    session: &mut htap_server::Session,
    input: &GeneratedInput,
    home_w_id: i64,
    terminal_source: u16,
    history_sequence: &mut u64,
) -> Result<(), DriverError> {
    let mut execute = || {
        let result = execute_input(session, input, home_w_id, terminal_source, history_sequence);
        if matches!(
            result,
            Err(DriverError::Transaction(TransactionError::Conflict))
        ) {
            let _ = session.rollback();
        }
        result
    };

    if exclusive {
        let _guard = execution_lock
            .write()
            .map_err(|_| DriverError::DriverLockPoisoned)?;
        execute()
    } else {
        let _guard = execution_lock
            .read()
            .map_err(|_| DriverError::DriverLockPoisoned)?;
        execute()
    }
}

fn history_sequence(
    session: &mut htap_server::Session,
    terminal_source: u16,
) -> Result<u64, DriverError> {
    let first_id = build_h_id(terminal_source, 0)
        .map_err(|error| DriverError::Htap(HtapError::InvalidArgument(error.into())))?;
    let last_id = build_h_id(terminal_source, (1_u64 << 48) - 1)
        .map_err(|error| DriverError::Htap(HtapError::InvalidArgument(error.into())))?;
    let result = match session.execute(&format!(
        "SELECT h_id FROM history WHERE h_id >= {first_id} AND h_id <= {last_id}"
    ))? {
        StatementResult::Query(result) => result,
        result => {
            return Err(DriverError::Htap(HtapError::InvalidArgument(format!(
                "history sequence query returned a non-query result: {result:?}"
            ))));
        }
    };

    let mut next_sequence = 0_u64;
    for row in result.rows() {
        let Some(Value::Int64(history_id)) = row.get(0) else {
            return Err(DriverError::Htap(HtapError::InvalidArgument(
                "history id query returned a non-BIGINT value".into(),
            )));
        };
        let sequence = u64::try_from(*history_id - first_id).map_err(|_| {
            DriverError::Htap(HtapError::InvalidArgument(
                "history id query returned an invalid value".into(),
            ))
        })?;
        next_sequence = next_sequence.max(sequence.checked_add(1).ok_or_else(|| {
            DriverError::Htap(HtapError::InvalidArgument(
                "history sequence overflowed".into(),
            ))
        })?);
    }
    Ok(next_sequence)
}

fn reserve_submission(submitted: &AtomicU64, limit: u64) -> bool {
    let mut current = submitted.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match submitted.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

struct TerminalRun {
    server: Arc<LocalServer>,
    warehouse_count: u32,
    terminal_id: u32,
    limit: TransactionLimitOrDuration,
    scale: DriverScale,
    seed: u64,
    barrier: Arc<Barrier>,
    cancel: Arc<AtomicBool>,
    submitted: Arc<AtomicU64>,
    execution_lock: Arc<RwLock<()>>,
}

fn run_terminal(config: TerminalRun) -> Result<[TransactionCounts; 5], DriverError> {
    let TerminalRun {
        server,
        warehouse_count,
        terminal_id,
        limit,
        scale,
        seed,
        barrier,
        cancel,
        submitted,
        execution_lock,
    } = config;

    let mut rng =
        RandomState::new(seed ^ u64::from(terminal_id).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let home_w_id = (terminal_id % warehouse_count) as i64 + 1;
    // Terminals reuse districts cyclically when there are more terminals than
    // warehouse-district pairs; each terminal itself keeps this district fixed.
    let stock_level_d_id =
        ((terminal_id / warehouse_count) % scale.districts_per_warehouse) as i64 + 1;
    let setup = (|| {
        let terminal_source =
            u16::try_from(terminal_id + 1).map_err(|_| DriverError::InvalidTerminalCount)?;
        let mut session = server.open_session()?;
        let history_sequence = history_sequence(&mut session, terminal_source)?;
        Ok::<_, DriverError>((session, history_sequence, terminal_source))
    })();

    // Every spawned worker reaches this barrier exactly once, including setup failures.
    barrier.wait();

    let (mut session, mut history_sequence, terminal_source) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            cancel.store(true, Ordering::Release);
            return Err(error);
        }
    };
    let start = Instant::now();
    let mut counts: [TransactionCounts; 5] = std::array::from_fn(|_| TransactionCounts::default());

    while !cancel.load(Ordering::Acquire) {
        if matches!(limit, TransactionLimitOrDuration::Duration(duration) if start.elapsed() >= duration)
        {
            break;
        }

        for kind in deck(&mut rng) {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            if matches!(limit, TransactionLimitOrDuration::Duration(duration) if start.elapsed() >= duration)
            {
                break;
            }
            if matches!(limit, TransactionLimitOrDuration::TransactionLimit(limit) if !reserve_submission(&submitted, limit))
            {
                return Ok(counts);
            }

            let input = generate_input(
                &mut rng,
                kind,
                home_w_id,
                warehouse_count,
                scale,
                stock_level_d_id,
            );
            let count = &mut counts[kind as usize];

            if let GeneratedInput::Delivery { carrier_id } = input {
                let request = DeliveryRequest {
                    w_id: home_w_id,
                    carrier_id,
                    delivery_timestamp_micros: timestamp_micros()?,
                };
                for district_id in 1..=i64::from(scale.districts_per_warehouse) {
                    let mut conflict_retries = 0_u32;
                    let mut escalated = false;

                    loop {
                        if !escalated && conflict_retries >= ESCALATION_CONFLICT_THRESHOLD {
                            escalated = true;
                            count.escalations += 1;
                        }

                        let result = if escalated {
                            let _guard = execution_lock
                                .write()
                                .map_err(|_| DriverError::DriverLockPoisoned)?;
                            delivery_one_district(&mut session, &request, district_id)
                        } else {
                            let _guard = execution_lock
                                .read()
                                .map_err(|_| DriverError::DriverLockPoisoned)?;
                            delivery_one_district(&mut session, &request, district_id)
                        };

                        match result {
                            Ok(_) => break,
                            Err(TransactionError::Conflict)
                                if conflict_retries < MAX_CONFLICT_RETRIES =>
                            {
                                let _ = session.rollback();
                                conflict_retries += 1;
                                count.conflict_retries += 1;
                            }
                            Err(error) => {
                                cancel.store(true, Ordering::Release);
                                return Err(DriverError::Transaction(error));
                            }
                        }
                    }
                }
                count.completed += 1;
                continue;
            }

            let mut conflict_retries = 0_u32;
            let mut escalated = false;
            loop {
                if !escalated && conflict_retries >= ESCALATION_CONFLICT_THRESHOLD {
                    escalated = true;
                    count.escalations += 1;
                }

                match execute_with_driver_lock(
                    &execution_lock,
                    escalated,
                    &mut session,
                    &input,
                    home_w_id,
                    terminal_source,
                    &mut history_sequence,
                ) {
                    Ok(()) => {
                        count.completed += 1;
                        break;
                    }
                    Err(DriverError::Transaction(TransactionError::ExpectedRollback)) => {
                        count.expected_rollbacks += 1;
                        break;
                    }
                    Err(DriverError::Transaction(TransactionError::Conflict))
                        if conflict_retries < MAX_CONFLICT_RETRIES =>
                    {
                        conflict_retries += 1;
                        count.conflict_retries += 1;
                    }
                    Err(error) => {
                        cancel.store(true, Ordering::Release);
                        return Err(error);
                    }
                }
            }
        }
    }

    Ok(counts)
}

/// Runs a fixed-deck TPC-C transaction workload.
///
/// A transaction limit applies collectively across all terminals. Duration limits
/// begin once each terminal passes the common start barrier. Fixed-deck mix
/// guarantees apply only to completed deck passes, so a transaction limit may
/// leave a partial final deck. Observed percentages exclude expected rollbacks.
pub fn run(
    server: &Arc<LocalServer>,
    warehouse_count: u32,
    terminal_count: u32,
    limit: TransactionLimitOrDuration,
    scale: DriverScale,
    seed: u64,
) -> Result<WorkloadReport, DriverError> {
    if warehouse_count == 0 {
        return Err(DriverError::InvalidWarehouseCount);
    }
    if terminal_count == 0 || terminal_count > 32_767 {
        return Err(DriverError::InvalidTerminalCount);
    }
    if !limit.valid() {
        return Err(DriverError::InvalidRunLimit);
    }
    if !scale.valid() {
        return Err(DriverError::InvalidScale);
    }

    let barrier = Arc::new(Barrier::new(terminal_count as usize + 1));
    let cancel = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicU64::new(0));
    let execution_lock = Arc::new(RwLock::new(()));
    let mut handles = Vec::with_capacity(terminal_count as usize);

    for terminal_id in 0..terminal_count {
        let config = TerminalRun {
            server: Arc::clone(server),
            warehouse_count,
            terminal_id,
            limit,
            scale,
            seed,
            barrier: Arc::clone(&barrier),
            cancel: Arc::clone(&cancel),
            submitted: Arc::clone(&submitted),
            execution_lock: Arc::clone(&execution_lock),
        };
        handles.push(thread::spawn(move || run_terminal(config)));
    }

    barrier.wait();
    let start = Instant::now();

    let mut totals: [TransactionCounts; 5] = std::array::from_fn(|_| TransactionCounts::default());
    let mut error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(counts)) => {
                for (total, count) in totals.iter_mut().zip(counts) {
                    total.completed += count.completed;
                    total.expected_rollbacks += count.expected_rollbacks;
                    total.conflict_retries += count.conflict_retries;
                    total.escalations += count.escalations;
                }
            }
            Ok(Err(worker_error)) => {
                cancel.store(true, Ordering::Release);
                error.get_or_insert(worker_error);
            }
            Err(_) => {
                cancel.store(true, Ordering::Release);
                error.get_or_insert(DriverError::WorkerPanicked);
            }
        }
    }
    if let Some(error) = error {
        return Err(error);
    }

    let total_completed: u64 = totals.iter().map(|count| count.completed).sum();
    Ok(WorkloadReport {
        transactions: TransactionKind::ALL
            .into_iter()
            .zip(totals)
            .map(|(kind, counts)| TransactionMix {
                kind,
                observed_percentage: if total_completed == 0 {
                    0.0
                } else {
                    counts.completed as f64 * 100.0 / total_completed as f64
                },
                counts,
            })
            .collect(),
        elapsed: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_labels_do_not_contain_official_metrics() {
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
                assert!(!label.contains(forbidden), "{label} contains {forbidden}");
            }
        }
    }

    #[test]
    fn deck_has_required_composition_and_minimum_mix() {
        let mut rng = RandomState::new(42);
        let cards = deck(&mut rng);

        assert_eq!(cards.len(), DECK_SIZE);
        assert_eq!(
            cards
                .iter()
                .filter(|&&kind| kind == TransactionKind::NewOrder)
                .count(),
            10
        );
        assert_eq!(
            cards
                .iter()
                .filter(|&&kind| kind == TransactionKind::Payment)
                .count(),
            10
        );
        assert_eq!(
            cards
                .iter()
                .filter(|&&kind| kind == TransactionKind::OrderStatus)
                .count(),
            1
        );
        assert_eq!(
            cards
                .iter()
                .filter(|&&kind| kind == TransactionKind::Delivery)
                .count(),
            1
        );
        assert_eq!(
            cards
                .iter()
                .filter(|&&kind| kind == TransactionKind::StockLevel)
                .count(),
            1
        );
    }

    #[test]
    fn generated_default_inputs_obey_ranges_and_rates() {
        let scale = DriverScale::default();
        let warehouse_count = 10;
        let home_w_id = 1;
        let samples = 100_000;
        let mut rng = RandomState::new(7);
        let mut new_order_invalid = 0;
        let mut new_order_remote = 0;
        let mut new_order_lines = 0;
        let mut payment_remote = 0;
        let mut payment_last_name = 0;
        let mut order_status_last_name = 0;

        for _ in 0..samples {
            match generate_input(
                &mut rng,
                TransactionKind::NewOrder,
                home_w_id,
                warehouse_count,
                scale,
                1,
            ) {
                GeneratedInput::NewOrder { d_id, c_id, items } => {
                    assert!((1..=i64::from(scale.districts_per_warehouse)).contains(&d_id));
                    assert!((1..=i64::from(scale.customers_per_district)).contains(&c_id));
                    assert!((5..=15).contains(&items.len()));

                    let invalid_items = items
                        .iter()
                        .filter(|item| item.item_id == i64::from(scale.item_count) + 1)
                        .count();
                    assert!(invalid_items <= 1);
                    new_order_invalid += invalid_items;
                    new_order_lines += items.len();

                    for item in items {
                        assert!(
                            (1..=i64::from(scale.item_count)).contains(&item.item_id)
                                || item.item_id == i64::from(scale.item_count) + 1
                        );
                        assert!((1..=10).contains(&item.quantity));
                        assert!((1..=i64::from(warehouse_count)).contains(&item.supply_w_id));
                        if item.supply_w_id != home_w_id {
                            new_order_remote += 1;
                        }
                    }
                }
                _ => unreachable!(),
            }

            match generate_input(
                &mut rng,
                TransactionKind::Payment,
                home_w_id,
                warehouse_count,
                scale,
                1,
            ) {
                GeneratedInput::Payment {
                    d_id,
                    customer_w_id,
                    customer_d_id,
                    customer,
                    h_amount,
                } => {
                    assert!((1..=i64::from(scale.districts_per_warehouse)).contains(&d_id));
                    assert!((1..=i64::from(scale.districts_per_warehouse)).contains(&customer_d_id));
                    assert!((1..=i64::from(warehouse_count)).contains(&customer_w_id));
                    assert!((100..=500_000).contains(&h_amount));
                    if customer_w_id != home_w_id {
                        payment_remote += 1;
                    }
                    match customer {
                        CustomerSelector::Id(customer_id) => {
                            assert!((1..=i64::from(scale.customers_per_district))
                                .contains(&customer_id));
                        }
                        CustomerSelector::LastName(_) => payment_last_name += 1,
                    }
                }
                _ => unreachable!(),
            }

            match generate_input(
                &mut rng,
                TransactionKind::OrderStatus,
                home_w_id,
                warehouse_count,
                scale,
                1,
            ) {
                GeneratedInput::OrderStatus { d_id, customer } => {
                    assert!((1..=i64::from(scale.districts_per_warehouse)).contains(&d_id));
                    if matches!(customer, CustomerSelector::LastName(_)) {
                        order_status_last_name += 1;
                    }
                }
                _ => unreachable!(),
            }

            match generate_input(
                &mut rng,
                TransactionKind::Delivery,
                home_w_id,
                warehouse_count,
                scale,
                1,
            ) {
                GeneratedInput::Delivery { carrier_id } => {
                    assert!((1..=10).contains(&carrier_id));
                }
                _ => unreachable!(),
            }

            match generate_input(
                &mut rng,
                TransactionKind::StockLevel,
                home_w_id,
                warehouse_count,
                scale,
                2,
            ) {
                GeneratedInput::StockLevel { d_id, threshold } => {
                    assert_eq!(d_id, 2);
                    assert!((10..=20).contains(&threshold));
                }
                _ => unreachable!(),
            }
        }

        let invalid_rate = new_order_invalid as f64 / samples as f64;
        let new_order_remote_rate = new_order_remote as f64 / new_order_lines as f64;
        let payment_remote_rate = payment_remote as f64 / samples as f64;
        let payment_last_name_rate = payment_last_name as f64 / samples as f64;
        let order_status_last_name_rate = order_status_last_name as f64 / samples as f64;

        assert!((0.008..=0.012).contains(&invalid_rate));
        assert!((0.008..=0.012).contains(&new_order_remote_rate));
        assert!((0.13..=0.17).contains(&payment_remote_rate));
        assert!((0.58..=0.62).contains(&payment_last_name_rate));
        assert!((0.58..=0.62).contains(&order_status_last_name_rate));
        assert_eq!(scale.last_name_count(), 1_000);
    }
}
