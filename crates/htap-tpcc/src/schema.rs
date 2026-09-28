//! TPC-C table layouts expressed as engine DDL.

/// TPC-C table names in foreign-key dependency order.
///
/// The TPC-C spec names these ORDER, ORDER-LINE, and NEW-ORDER. The engine
/// uses `orders`, `order_line`, and `new_order` because ORDER is reserved and
/// MySQL dialects do not treat double quotes as identifier delimiters.
pub const TABLE_NAMES: &[&str] = &[
    "warehouse",
    "district",
    "item",
    "stock",
    "customer",
    "history",
    "orders",
    "order_line",
    "new_order",
];

/// Returns the DDL statements for all TPC-C tables.
pub fn ddl_statements() -> Vec<String> {
    [
        r#"
CREATE TABLE warehouse (
    w_id INTEGER NOT NULL,
    w_name CHAR(10) NOT NULL,
    w_street_1 CHAR(20) NOT NULL,
    w_street_2 CHAR(20) NOT NULL,
    w_city CHAR(20) NOT NULL,
    w_state CHAR(2) NOT NULL,
    w_zip CHAR(9) NOT NULL,
    w_tax DECIMAL(4, 4) NOT NULL,
    w_ytd DECIMAL(12, 2) NOT NULL,
    PRIMARY KEY (w_id)
)
"#,
        r#"
CREATE TABLE district (
    d_id INTEGER NOT NULL,
    d_w_id INTEGER NOT NULL,
    d_name CHAR(10) NOT NULL,
    d_street_1 CHAR(20) NOT NULL,
    d_street_2 CHAR(20) NOT NULL,
    d_city CHAR(20) NOT NULL,
    d_state CHAR(2) NOT NULL,
    d_zip CHAR(9) NOT NULL,
    d_tax DECIMAL(4, 4) NOT NULL,
    d_ytd DECIMAL(12, 2) NOT NULL,
    d_next_o_id INTEGER NOT NULL,
    PRIMARY KEY (d_w_id, d_id)
)
"#,
        r#"
CREATE TABLE item (
    i_id INTEGER NOT NULL,
    i_im_id INTEGER NOT NULL,
    i_name VARCHAR(24) NOT NULL,
    i_price DECIMAL(5, 2) NOT NULL,
    i_data VARCHAR(50) NOT NULL,
    PRIMARY KEY (i_id)
)
"#,
        r#"
CREATE TABLE stock (
    s_i_id INTEGER NOT NULL,
    s_w_id INTEGER NOT NULL,
    s_quantity INTEGER NOT NULL,
    s_dist_01 CHAR(24) NOT NULL,
    s_dist_02 CHAR(24) NOT NULL,
    s_dist_03 CHAR(24) NOT NULL,
    s_dist_04 CHAR(24) NOT NULL,
    s_dist_05 CHAR(24) NOT NULL,
    s_dist_06 CHAR(24) NOT NULL,
    s_dist_07 CHAR(24) NOT NULL,
    s_dist_08 CHAR(24) NOT NULL,
    s_dist_09 CHAR(24) NOT NULL,
    s_dist_10 CHAR(24) NOT NULL,
    s_ytd INTEGER NOT NULL,
    s_order_cnt INTEGER NOT NULL,
    s_remote_cnt INTEGER NOT NULL,
    s_data VARCHAR(50) NOT NULL,
    PRIMARY KEY (s_w_id, s_i_id)
)
"#,
        r#"
CREATE TABLE customer (
    c_id INTEGER NOT NULL,
    c_d_id INTEGER NOT NULL,
    c_w_id INTEGER NOT NULL,
    c_first VARCHAR(16) NOT NULL,
    c_middle CHAR(2) NOT NULL,
    c_last VARCHAR(16) NOT NULL,
    c_street_1 CHAR(20) NOT NULL,
    c_street_2 CHAR(20) NOT NULL,
    c_city CHAR(20) NOT NULL,
    c_state CHAR(2) NOT NULL,
    c_zip CHAR(9) NOT NULL,
    c_phone CHAR(16) NOT NULL,
    c_since TIMESTAMP NOT NULL,
    c_credit CHAR(2) NOT NULL,
    c_credit_lim DECIMAL(12, 2) NOT NULL,
    c_discount DECIMAL(4, 4) NOT NULL,
    c_balance DECIMAL(12, 2) NOT NULL,
    c_ytd_payment DECIMAL(12, 2) NOT NULL,
    c_payment_cnt INTEGER NOT NULL,
    c_delivery_cnt INTEGER NOT NULL,
    c_data VARCHAR(500) NOT NULL,
    PRIMARY KEY (c_w_id, c_d_id, c_id)
)
"#,
        // h_id is permitted by TPC-C Clauses 1.4.7 and 1.4.10. HISTORY is
        // insert-only and no transaction reads its rows, so it is never used
        // for row access. Its high 15 bits identify the source (0 for initial
        // population; terminal/session number at runtime) and its low 48 bits
        // are a per-source monotonically increasing sequence. This supports
        // 32,768 sources and 281,474,976,710,656 entries per source while
        // keeping every key within the positive signed BIGINT range.
        r#"
CREATE TABLE history (
    h_id BIGINT NOT NULL,
    h_c_id INTEGER NOT NULL,
    h_c_d_id INTEGER NOT NULL,
    h_c_w_id INTEGER NOT NULL,
    h_d_id INTEGER NOT NULL,
    h_w_id INTEGER NOT NULL,
    h_date TIMESTAMP NOT NULL,
    h_amount DECIMAL(6, 2) NOT NULL,
    h_data VARCHAR(24) NOT NULL,
    PRIMARY KEY (h_id)
)
"#,
        r#"
CREATE TABLE orders (
    o_id INTEGER NOT NULL,
    o_d_id INTEGER NOT NULL,
    o_w_id INTEGER NOT NULL,
    o_c_id INTEGER NOT NULL,
    o_entry_d TIMESTAMP NOT NULL,
    o_carrier_id INTEGER,
    o_ol_cnt INTEGER NOT NULL,
    o_all_local INTEGER NOT NULL,
    PRIMARY KEY (o_w_id, o_d_id, o_id)
)
"#,
        r#"
CREATE TABLE order_line (
    ol_o_id INTEGER NOT NULL,
    ol_d_id INTEGER NOT NULL,
    ol_w_id INTEGER NOT NULL,
    ol_number INTEGER NOT NULL,
    ol_i_id INTEGER NOT NULL,
    ol_supply_w_id INTEGER NOT NULL,
    ol_delivery_d TIMESTAMP,
    ol_quantity INTEGER NOT NULL,
    ol_amount DECIMAL(6, 2) NOT NULL,
    ol_dist_info CHAR(24) NOT NULL,
    PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number)
)
"#,
        r#"
CREATE TABLE new_order (
    no_o_id INTEGER NOT NULL,
    no_d_id INTEGER NOT NULL,
    no_w_id INTEGER NOT NULL,
    PRIMARY KEY (no_w_id, no_d_id, no_o_id)
)
"#,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
