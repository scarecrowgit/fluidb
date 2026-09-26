/// Table names in foreign-key dependency order.
pub const TABLE_NAMES: [&str; 8] = [
    "region", "nation", "part", "supplier", "partsupp", "customer", "orders", "lineitem",
];

const REGION_DDL: &str = "
CREATE TABLE region (
    r_regionkey BIGINT NOT NULL,
    r_name CHAR(25) NOT NULL,
    r_comment VARCHAR(152) NOT NULL,
    PRIMARY KEY (r_regionkey)
)";

const NATION_DDL: &str = "
CREATE TABLE nation (
    n_nationkey BIGINT NOT NULL,
    n_name CHAR(25) NOT NULL,
    n_regionkey BIGINT NOT NULL,
    n_comment VARCHAR(152) NOT NULL,
    PRIMARY KEY (n_nationkey)
)";

const PART_DDL: &str = "
CREATE TABLE part (
    p_partkey BIGINT NOT NULL,
    p_name VARCHAR(55) NOT NULL,
    p_mfgr CHAR(25) NOT NULL,
    p_brand CHAR(10) NOT NULL,
    p_type VARCHAR(25) NOT NULL,
    p_size INT NOT NULL,
    p_container CHAR(10) NOT NULL,
    p_retailprice DECIMAL(15,2) NOT NULL,
    p_comment VARCHAR(23) NOT NULL,
    PRIMARY KEY (p_partkey)
)";

const SUPPLIER_DDL: &str = "
CREATE TABLE supplier (
    s_suppkey BIGINT NOT NULL,
    s_name CHAR(25) NOT NULL,
    s_address VARCHAR(40) NOT NULL,
    s_nationkey BIGINT NOT NULL,
    s_phone CHAR(15) NOT NULL,
    s_acctbal DECIMAL(15,2) NOT NULL,
    s_comment VARCHAR(101) NOT NULL,
    PRIMARY KEY (s_suppkey)
)";

const PARTSUPP_DDL: &str = "
CREATE TABLE partsupp (
    ps_partkey BIGINT NOT NULL,
    ps_suppkey BIGINT NOT NULL,
    ps_availqty INT NOT NULL,
    ps_supplycost DECIMAL(15,2) NOT NULL,
    ps_comment VARCHAR(199) NOT NULL,
    PRIMARY KEY (ps_partkey, ps_suppkey)
)";

const CUSTOMER_DDL: &str = "
CREATE TABLE customer (
    c_custkey BIGINT NOT NULL,
    c_name VARCHAR(25) NOT NULL,
    c_address VARCHAR(40) NOT NULL,
    c_nationkey BIGINT NOT NULL,
    c_phone CHAR(15) NOT NULL,
    c_acctbal DECIMAL(15,2) NOT NULL,
    c_mktsegment CHAR(10) NOT NULL,
    c_comment VARCHAR(117) NOT NULL,
    PRIMARY KEY (c_custkey)
)";

const ORDERS_DDL: &str = "
CREATE TABLE orders (
    o_orderkey BIGINT NOT NULL,
    o_custkey BIGINT NOT NULL,
    o_orderstatus CHAR(1) NOT NULL,
    o_totalprice DECIMAL(15,2) NOT NULL,
    o_orderdate DATE NOT NULL,
    o_orderpriority CHAR(15) NOT NULL,
    o_clerk CHAR(15) NOT NULL,
    o_shippriority INT NOT NULL,
    o_comment VARCHAR(79) NOT NULL,
    PRIMARY KEY (o_orderkey)
)";

const LINEITEM_DDL: &str = "
CREATE TABLE lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INT NOT NULL,
    l_partkey BIGINT NOT NULL,
    l_suppkey BIGINT NOT NULL,
    l_quantity DECIMAL(15,2) NOT NULL,
    l_extendedprice DECIMAL(15,2) NOT NULL,
    l_discount DECIMAL(15,2) NOT NULL,
    l_tax DECIMAL(15,2) NOT NULL,
    l_returnflag CHAR(1) NOT NULL,
    l_linestatus CHAR(1) NOT NULL,
    l_shipdate DATE NOT NULL,
    l_commitdate DATE NOT NULL,
    l_receiptdate DATE NOT NULL,
    l_shipinstruct CHAR(25) NOT NULL,
    l_shipmode CHAR(10) NOT NULL,
    l_comment VARCHAR(44) NOT NULL,
    PRIMARY KEY (l_orderkey, l_linenumber)
)";

/// Returns the eight TPC-H table definitions in foreign-key dependency order.
///
/// Foreign-key relationships are represented by the schema's columns but are
/// intentionally not emitted as engine constraints; generated data maintains
/// the relationships.
pub fn ddl_statements() -> [&'static str; 8] {
    [
        REGION_DDL,
        NATION_DDL,
        PART_DDL,
        SUPPLIER_DDL,
        PARTSUPP_DDL,
        CUSTOMER_DDL,
        ORDERS_DDL,
        LINEITEM_DDL,
    ]
}
