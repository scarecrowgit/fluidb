pub mod rng;
pub mod text;

use htap_common::{HtapError, Result};

use crate::history::build_h_id;

use self::rng::RandomState;
use self::text::{a_string, c_last, n_string, nurand, permutation, zip_code};

const ITEMS_PER_WAREHOUSE: u32 = 100_000;
const DISTRICTS_PER_WAREHOUSE: u32 = 10;
const CUSTOMERS_PER_DISTRICT: u32 = 3_000;
/// Highest order ID that is initially delivered.
const LAST_DELIVERED_ORDER_ID: u32 = 2_100;
const NEW_ORDERS_PER_DISTRICT: u32 = 900;
const POPULATION_DATE: &str = "2000-01-01 00:00:00";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub i_id: i64,
    pub i_im_id: i64,
    pub i_name: String,
    pub i_price: i64,
    pub i_data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warehouse {
    pub w_id: i64,
    pub w_name: String,
    pub w_street_1: String,
    pub w_street_2: String,
    pub w_city: String,
    pub w_state: String,
    pub w_zip: String,
    /// Tax rate in ten-thousandths (0.0000 through 0.2000).
    pub w_tax: i64,
    pub w_ytd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct District {
    pub d_id: i64,
    pub d_w_id: i64,
    pub d_name: String,
    pub d_street_1: String,
    pub d_street_2: String,
    pub d_city: String,
    pub d_state: String,
    pub d_zip: String,
    /// Tax rate in ten-thousandths (0.0000 through 0.2000).
    pub d_tax: i64,
    pub d_ytd: i64,
    pub d_next_o_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stock {
    pub s_i_id: i64,
    pub s_w_id: i64,
    pub s_quantity: i64,
    pub s_dist_01: String,
    pub s_dist_02: String,
    pub s_dist_03: String,
    pub s_dist_04: String,
    pub s_dist_05: String,
    pub s_dist_06: String,
    pub s_dist_07: String,
    pub s_dist_08: String,
    pub s_dist_09: String,
    pub s_dist_10: String,
    pub s_ytd: i64,
    pub s_order_cnt: i64,
    pub s_remote_cnt: i64,
    pub s_data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Customer {
    pub c_id: i64,
    pub c_d_id: i64,
    pub c_w_id: i64,
    pub c_first: String,
    pub c_middle: String,
    pub c_last: String,
    pub c_street_1: String,
    pub c_street_2: String,
    pub c_city: String,
    pub c_state: String,
    pub c_zip: String,
    pub c_phone: String,
    pub c_since: String,
    pub c_credit: String,
    pub c_credit_lim: i64,
    /// Discount rate in ten-thousandths (0.0000 through 0.5000).
    pub c_discount: i64,
    pub c_balance: i64,
    pub c_ytd_payment: i64,
    pub c_payment_cnt: i64,
    pub c_delivery_cnt: i64,
    pub c_data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub h_id: i64,
    pub h_c_id: i64,
    pub h_c_d_id: i64,
    pub h_c_w_id: i64,
    pub h_d_id: i64,
    pub h_w_id: i64,
    pub h_date: String,
    pub h_amount: i64,
    pub h_data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orders {
    pub o_id: i64,
    pub o_d_id: i64,
    pub o_w_id: i64,
    pub o_c_id: i64,
    pub o_entry_d: String,
    pub o_carrier_id: Option<i64>,
    pub o_ol_cnt: i64,
    pub o_all_local: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderLine {
    pub ol_o_id: i64,
    pub ol_d_id: i64,
    pub ol_w_id: i64,
    pub ol_number: i64,
    pub ol_i_id: i64,
    pub ol_supply_w_id: i64,
    pub ol_delivery_d: Option<String>,
    pub ol_quantity: i64,
    pub ol_amount: i64,
    pub ol_dist_info: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub no_o_id: i64,
    pub no_d_id: i64,
    pub no_w_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    pub warehouse: Vec<Warehouse>,
    pub district: Vec<District>,
    pub item: Vec<Item>,
    pub stock: Vec<Stock>,
    pub customer: Vec<Customer>,
    pub history: Vec<History>,
    pub orders: Vec<Orders>,
    pub order_line: Vec<OrderLine>,
    pub new_order: Vec<NewOrder>,
}

fn stream(seed: u64, discriminator: u64) -> RandomState {
    RandomState::new(seed ^ discriminator.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn original_data(rng: &mut RandomState) -> String {
    let mut data = a_string(rng, 26..=50);
    if rng.structural_bounded(10) == 0 {
        let position = rng.structural_bounded((data.len() - 8 + 1) as u64) as usize;
        data.replace_range(position..position + 8, "ORIGINAL");
    }
    data
}

fn alpha_string(rng: &mut RandomState, length: usize) -> String {
    (0..length)
        .map(|_| (b'A' + rng.structural_bounded(26) as u8) as char)
        .collect()
}

fn address(rng: &mut RandomState) -> (String, String, String, String, String, String) {
    (
        a_string(rng, 10..=20),
        a_string(rng, 10..=20),
        a_string(rng, 10..=20),
        alpha_string(rng, 2),
        zip_code(rng),
        a_string(rng, 6..=10),
    )
}

/// Generates the fixed, warehouse-independent ITEM population.
pub fn generate_item(seed: u64) -> Vec<Item> {
    let mut rng = stream(seed, 1);
    (1..=ITEMS_PER_WAREHOUSE)
        .map(|id| Item {
            i_id: i64::from(id),
            i_im_id: (rng.structural_bounded(10_000) + 1) as i64,
            i_name: a_string(&mut rng, 14..=24),
            i_price: rng.two_decimal_range(100, 10_000),
            i_data: original_data(&mut rng),
        })
        .collect()
}

/// Generates one WAREHOUSE row for every requested warehouse.
pub fn generate_warehouse(warehouse_count: u32, seed: u64) -> Vec<Warehouse> {
    let mut rng = stream(seed, 2);
    (1..=warehouse_count)
        .map(|id| {
            let (street_1, street_2, city, state, zip, _) = address(&mut rng);
            Warehouse {
                w_id: i64::from(id),
                w_name: a_string(&mut rng, 6..=10),
                w_street_1: street_1,
                w_street_2: street_2,
                w_city: city,
                w_state: state,
                w_zip: zip,
                w_tax: rng.four_decimal_range(0, 2_000),
                w_ytd: 30_000_000,
            }
        })
        .collect()
}

/// Generates ten DISTRICT rows for every warehouse.
pub fn generate_district(warehouse_count: u32, seed: u64) -> Vec<District> {
    let mut rng = stream(seed, 3);
    let mut rows = Vec::with_capacity(warehouse_count as usize * DISTRICTS_PER_WAREHOUSE as usize);
    for warehouse_id in 1..=warehouse_count {
        for district_id in 1..=DISTRICTS_PER_WAREHOUSE {
            let (street_1, street_2, city, state, zip, _) = address(&mut rng);
            rows.push(District {
                d_id: i64::from(district_id),
                d_w_id: i64::from(warehouse_id),
                d_name: a_string(&mut rng, 6..=10),
                d_street_1: street_1,
                d_street_2: street_2,
                d_city: city,
                d_state: state,
                d_zip: zip,
                d_tax: rng.four_decimal_range(0, 2_000),
                d_ytd: 3_000_000,
                d_next_o_id: 3_001,
            });
        }
    }
    rows
}

/// Generates 100,000 STOCK rows for every warehouse.
pub fn generate_stock(warehouse_count: u32, seed: u64) -> Vec<Stock> {
    let mut rng = stream(seed, 4);
    let mut rows = Vec::with_capacity(warehouse_count as usize * ITEMS_PER_WAREHOUSE as usize);
    for warehouse_id in 1..=warehouse_count {
        for item_id in 1..=ITEMS_PER_WAREHOUSE {
            rows.push(Stock {
                s_i_id: i64::from(item_id),
                s_w_id: i64::from(warehouse_id),
                s_quantity: (rng.structural_bounded(91) + 10) as i64,
                s_dist_01: a_string(&mut rng, 24..=24),
                s_dist_02: a_string(&mut rng, 24..=24),
                s_dist_03: a_string(&mut rng, 24..=24),
                s_dist_04: a_string(&mut rng, 24..=24),
                s_dist_05: a_string(&mut rng, 24..=24),
                s_dist_06: a_string(&mut rng, 24..=24),
                s_dist_07: a_string(&mut rng, 24..=24),
                s_dist_08: a_string(&mut rng, 24..=24),
                s_dist_09: a_string(&mut rng, 24..=24),
                s_dist_10: a_string(&mut rng, 24..=24),
                s_ytd: 0,
                s_order_cnt: 0,
                s_remote_cnt: 0,
                s_data: original_data(&mut rng),
            });
        }
    }
    rows
}

/// Generates 3,000 CUSTOMER rows for every district.
pub fn generate_customer(warehouse_count: u32, seed: u64) -> Vec<Customer> {
    let mut rng = stream(seed, 5);
    let mut rows = Vec::with_capacity(
        warehouse_count as usize
            * DISTRICTS_PER_WAREHOUSE as usize
            * CUSTOMERS_PER_DISTRICT as usize,
    );
    for warehouse_id in 1..=warehouse_count {
        for district_id in 1..=DISTRICTS_PER_WAREHOUSE {
            for customer_id in 1..=CUSTOMERS_PER_DISTRICT {
                let last_number = if customer_id <= 1_000 {
                    u64::from(customer_id - 1)
                } else {
                    nurand(&mut rng, 255, 0, 999, 157)
                };
                let (street_1, street_2, city, state, zip, _) = address(&mut rng);
                rows.push(Customer {
                    c_id: i64::from(customer_id),
                    c_d_id: i64::from(district_id),
                    c_w_id: i64::from(warehouse_id),
                    c_first: a_string(&mut rng, 8..=16),
                    c_middle: "OE".to_owned(),
                    c_last: c_last(&mut rng, 0, last_number),
                    c_street_1: street_1,
                    c_street_2: street_2,
                    c_city: city,
                    c_state: state,
                    c_zip: zip,
                    c_phone: n_string(&mut rng, 16..=16),
                    c_since: POPULATION_DATE.to_owned(),
                    c_credit: if rng.structural_bounded(10) == 0 {
                        "BC".to_owned()
                    } else {
                        "GC".to_owned()
                    },
                    c_credit_lim: 5_000_000,
                    c_discount: rng.four_decimal_range(0, 5_000),
                    c_balance: -1_000,
                    c_ytd_payment: 1_000,
                    c_payment_cnt: 1,
                    c_delivery_cnt: 0,
                    c_data: a_string(&mut rng, 300..=500),
                });
            }
        }
    }
    rows
}

/// Generates one initial HISTORY row for every generated customer.
pub fn generate_history(warehouse_count: u32, seed: u64) -> Vec<History> {
    let mut rng = stream(seed, 6);
    let total = warehouse_count as u64
        * u64::from(DISTRICTS_PER_WAREHOUSE)
        * u64::from(CUSTOMERS_PER_DISTRICT);
    let mut rows = Vec::with_capacity(total as usize);
    let mut sequence = 0_u64;

    for warehouse_id in 1..=warehouse_count {
        for district_id in 1..=DISTRICTS_PER_WAREHOUSE {
            for customer_id in 1..=CUSTOMERS_PER_DISTRICT {
                rows.push(History {
                    h_id: build_h_id(0, sequence).expect("population HISTORY sequence fits"),
                    h_c_id: i64::from(customer_id),
                    h_c_d_id: i64::from(district_id),
                    h_c_w_id: i64::from(warehouse_id),
                    h_d_id: i64::from(district_id),
                    h_w_id: i64::from(warehouse_id),
                    h_date: POPULATION_DATE.to_owned(),
                    h_amount: 1_000,
                    h_data: a_string(&mut rng, 12..=24),
                });
                sequence += 1;
            }
        }
    }
    rows
}

/// Generates 3,000 ORDERS rows for every district.
pub fn generate_orders(warehouse_count: u32, seed: u64) -> Vec<Orders> {
    let mut rng = stream(seed, 7);
    let mut rows = Vec::with_capacity(
        warehouse_count as usize
            * DISTRICTS_PER_WAREHOUSE as usize
            * CUSTOMERS_PER_DISTRICT as usize,
    );
    for warehouse_id in 1..=warehouse_count {
        for district_id in 1..=DISTRICTS_PER_WAREHOUSE {
            let customer_order = permutation(&mut rng, CUSTOMERS_PER_DISTRICT as usize);
            for order_id in 1..=CUSTOMERS_PER_DISTRICT {
                let delivered = order_id <= LAST_DELIVERED_ORDER_ID;
                rows.push(Orders {
                    o_id: i64::from(order_id),
                    o_d_id: i64::from(district_id),
                    o_w_id: i64::from(warehouse_id),
                    o_c_id: (customer_order[(order_id - 1) as usize] + 1) as i64,
                    o_entry_d: POPULATION_DATE.to_owned(),
                    o_carrier_id: delivered.then(|| (rng.structural_bounded(10) + 1) as i64),
                    o_ol_cnt: (rng.structural_bounded(11) + 5) as i64,
                    o_all_local: 1,
                });
            }
        }
    }
    rows
}

/// Generates ORDER_LINE rows corresponding to every generated order.
pub fn generate_order_line(orders: &[Orders], seed: u64) -> Vec<OrderLine> {
    let mut rng = stream(seed, 8);
    let total = orders.iter().map(|order| order.o_ol_cnt as usize).sum();
    let mut rows = Vec::with_capacity(total);

    for order in orders {
        let delivered = order.o_id <= i64::from(LAST_DELIVERED_ORDER_ID);
        for number in 1..=order.o_ol_cnt {
            rows.push(OrderLine {
                ol_o_id: order.o_id,
                ol_d_id: order.o_d_id,
                ol_w_id: order.o_w_id,
                ol_number: number,
                ol_i_id: (rng.structural_bounded(u64::from(ITEMS_PER_WAREHOUSE)) + 1) as i64,
                ol_supply_w_id: order.o_w_id,
                ol_delivery_d: delivered.then(|| order.o_entry_d.clone()),
                ol_quantity: 5,
                ol_amount: if delivered {
                    0
                } else {
                    rng.two_decimal_range(1, 999_999)
                },
                ol_dist_info: a_string(&mut rng, 24..=24),
            });
        }
    }
    rows
}

/// Generates NEW_ORDER rows for orders 2101 through 3000 in every district.
pub fn generate_new_order(warehouse_count: u32) -> Vec<NewOrder> {
    let mut rows = Vec::with_capacity(
        warehouse_count as usize
            * DISTRICTS_PER_WAREHOUSE as usize
            * NEW_ORDERS_PER_DISTRICT as usize,
    );
    for warehouse_id in 1..=warehouse_count {
        for district_id in 1..=DISTRICTS_PER_WAREHOUSE {
            for order_id in (LAST_DELIVERED_ORDER_ID + 1)..=CUSTOMERS_PER_DISTRICT {
                rows.push(NewOrder {
                    no_o_id: i64::from(order_id),
                    no_d_id: i64::from(district_id),
                    no_w_id: i64::from(warehouse_id),
                });
            }
        }
    }
    rows
}

/// Generates a complete deterministic TPC-C initial population.
pub fn generate(warehouse_count: u32, seed: u64) -> Result<Dataset> {
    if warehouse_count == 0 {
        return Err(HtapError::InvalidArgument(
            "warehouse_count must be greater than 0".into(),
        ));
    }

    let orders = generate_orders(warehouse_count, seed);
    Ok(Dataset {
        warehouse: generate_warehouse(warehouse_count, seed),
        district: generate_district(warehouse_count, seed),
        item: generate_item(seed),
        stock: generate_stock(warehouse_count, seed),
        customer: generate_customer(warehouse_count, seed),
        history: generate_history(warehouse_count, seed),
        order_line: generate_order_line(&orders, seed),
        new_order: generate_new_order(warehouse_count),
        orders,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn item_and_stock_original_rates_and_cardinalities() {
        let items = generate_item(7);
        assert_eq!(items.len(), 100_000);
        let item_original_count = items
            .iter()
            .filter(|row| row.i_data.contains("ORIGINAL"))
            .count();
        assert!((9_500..=10_500).contains(&item_original_count));
        assert!(items
            .iter()
            .all(|row| (26..=50).contains(&row.i_data.len())));
        assert!(items.iter().any(|row| row.i_data.len() == 26));
        assert!(items.iter().any(|row| row.i_data.len() == 50));

        let stock = generate_stock(1, 7);
        assert_eq!(stock.len(), 100_000);
        let stock_original_count = stock
            .iter()
            .filter(|row| row.s_data.contains("ORIGINAL"))
            .count();
        assert!((9_500..=10_500).contains(&stock_original_count));
        assert!(stock
            .iter()
            .all(|row| (26..=50).contains(&row.s_data.len())));
        assert!(stock.iter().all(|row| {
            row.s_dist_01.len() == 24
                && row.s_dist_10.len() == 24
                && row.s_ytd == 0
                && row.s_order_cnt == 0
                && row.s_remote_cnt == 0
        }));
    }

    #[test]
    fn population_is_deterministic_and_referentially_closed() {
        let first = generate(1, 19).unwrap();
        let second = generate(1, 19).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.item.len(), 100_000);
        assert_eq!(first.warehouse.len(), 1);
        assert_eq!(first.stock.len(), 100_000);
        assert_eq!(first.district.len(), 10);
        assert_eq!(first.customer.len(), 30_000);
        assert_eq!(first.history.len(), 30_000);
        assert_eq!(first.orders.len(), 30_000);
        assert_eq!(first.new_order.len(), 9_000);

        let customers: HashSet<_> = first
            .customer
            .iter()
            .map(|row| (row.c_w_id, row.c_d_id, row.c_id))
            .collect();
        let orders: HashSet<_> = first
            .orders
            .iter()
            .map(|row| (row.o_w_id, row.o_d_id, row.o_id))
            .collect();
        let history_ids: HashSet<_> = first.history.iter().map(|row| row.h_id).collect();

        assert_eq!(history_ids.len(), first.history.len());
        assert!(first
            .orders
            .iter()
            .all(|row| { customers.contains(&(row.o_w_id, row.o_d_id, row.o_c_id)) }));
        assert!(first
            .order_line
            .iter()
            .all(|row| { orders.contains(&(row.ol_w_id, row.ol_d_id, row.ol_o_id)) }));
        assert!(first
            .new_order
            .iter()
            .all(|row| { orders.contains(&(row.no_w_id, row.no_d_id, row.no_o_id)) }));

        for district_id in 1..=DISTRICTS_PER_WAREHOUSE as i64 {
            let delivered_orders: Vec<_> = first
                .orders
                .iter()
                .filter(|row| row.o_w_id == 1 && row.o_d_id == district_id && row.o_id <= 2_100)
                .collect();
            let undelivered_orders: Vec<_> = first
                .orders
                .iter()
                .filter(|row| row.o_w_id == 1 && row.o_d_id == district_id && row.o_id > 2_100)
                .collect();

            assert_eq!(delivered_orders.len(), 2_100);
            assert_eq!(undelivered_orders.len(), 900);

            let order_2100 = first
                .orders
                .iter()
                .find(|row| row.o_w_id == 1 && row.o_d_id == district_id && row.o_id == 2_100)
                .unwrap();
            assert!(order_2100.o_carrier_id.is_some());
            assert!(first
                .order_line
                .iter()
                .filter(|row| {
                    row.ol_w_id == 1 && row.ol_d_id == district_id && row.ol_o_id == 2_100
                })
                .all(|row| row.ol_delivery_d.is_some() && row.ol_amount == 0));
            assert!(!first.new_order.iter().any(|row| {
                row.no_w_id == 1 && row.no_d_id == district_id && row.no_o_id == 2_100
            }));

            let order_2101 = first
                .orders
                .iter()
                .find(|row| row.o_w_id == 1 && row.o_d_id == district_id && row.o_id == 2_101)
                .unwrap();
            assert!(order_2101.o_carrier_id.is_none());
            assert!(first
                .order_line
                .iter()
                .filter(|row| {
                    row.ol_w_id == 1 && row.ol_d_id == district_id && row.ol_o_id == 2_101
                })
                .all(|row| row.ol_delivery_d.is_none()));
            assert!(first.new_order.iter().any(|row| {
                row.no_w_id == 1 && row.no_d_id == district_id && row.no_o_id == 2_101
            }));
        }

        assert!(first
            .customer
            .iter()
            .all(|row| row.c_phone.len() == 16
                && row.c_phone.bytes().all(|byte| byte.is_ascii_digit())));
    }

    #[test]
    fn taxes_and_discounts_use_four_decimal_precision() {
        let warehouses = generate_warehouse(100, 29);
        assert!(warehouses
            .iter()
            .all(|row| (0..=2_000).contains(&row.w_tax)));

        let districts = generate_district(100, 29);
        assert!(districts.iter().all(|row| (0..=2_000).contains(&row.d_tax)));

        let customers = generate_customer(1, 29);
        assert!(customers
            .iter()
            .all(|row| (0..=5_000).contains(&row.c_discount)));
    }

    #[test]
    fn customer_bad_credit_rate_is_approximately_ten_percent() {
        let customers = generate_customer(1, 31);
        let bad_credit_count = customers.iter().filter(|row| row.c_credit == "BC").count();

        assert!((2_850..=3_150).contains(&bad_credit_count));
    }

    #[test]
    fn order_customer_ids_are_permutations() {
        let orders = generate_orders(1, 23);
        for district_id in 1..=10 {
            let ids: HashSet<_> = orders
                .iter()
                .filter(|row| row.o_d_id == district_id)
                .map(|row| row.o_c_id)
                .collect();
            assert_eq!(ids.len(), 3_000);
            assert!(ids.iter().all(|id| (1..=3_000).contains(id)));
        }
    }
}
