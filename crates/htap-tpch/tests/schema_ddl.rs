use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::DataType;
use htap_server::LocalServer;
use htap_tpch::{ddl_statements, TABLE_NAMES};
use tempfile::TempDir;

struct ExpectedTable {
    name: &'static str,
    columns: &'static [(&'static str, DataType)],
    primary_key: &'static [usize],
}

const DECIMAL_15_2: DataType = DataType::Decimal {
    precision: 15,
    scale: 2,
};

const EXPECTED_TABLES: &[ExpectedTable] = &[
    ExpectedTable {
        name: "region",
        columns: &[
            ("r_regionkey", DataType::Int64),
            ("r_name", DataType::String),
            ("r_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "nation",
        columns: &[
            ("n_nationkey", DataType::Int64),
            ("n_name", DataType::String),
            ("n_regionkey", DataType::Int64),
            ("n_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "part",
        columns: &[
            ("p_partkey", DataType::Int64),
            ("p_name", DataType::String),
            ("p_mfgr", DataType::String),
            ("p_brand", DataType::String),
            ("p_type", DataType::String),
            ("p_size", DataType::Int32),
            ("p_container", DataType::String),
            ("p_retailprice", DECIMAL_15_2),
            ("p_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "supplier",
        columns: &[
            ("s_suppkey", DataType::Int64),
            ("s_name", DataType::String),
            ("s_address", DataType::String),
            ("s_nationkey", DataType::Int64),
            ("s_phone", DataType::String),
            ("s_acctbal", DECIMAL_15_2),
            ("s_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "partsupp",
        columns: &[
            ("ps_partkey", DataType::Int64),
            ("ps_suppkey", DataType::Int64),
            ("ps_availqty", DataType::Int32),
            ("ps_supplycost", DECIMAL_15_2),
            ("ps_comment", DataType::String),
        ],
        primary_key: &[0, 1],
    },
    ExpectedTable {
        name: "customer",
        columns: &[
            ("c_custkey", DataType::Int64),
            ("c_name", DataType::String),
            ("c_address", DataType::String),
            ("c_nationkey", DataType::Int64),
            ("c_phone", DataType::String),
            ("c_acctbal", DECIMAL_15_2),
            ("c_mktsegment", DataType::String),
            ("c_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "orders",
        columns: &[
            ("o_orderkey", DataType::Int64),
            ("o_custkey", DataType::Int64),
            ("o_orderstatus", DataType::String),
            ("o_totalprice", DECIMAL_15_2),
            ("o_orderdate", DataType::Timestamp),
            ("o_orderpriority", DataType::String),
            ("o_clerk", DataType::String),
            ("o_shippriority", DataType::Int32),
            ("o_comment", DataType::String),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "lineitem",
        columns: &[
            ("l_orderkey", DataType::Int64),
            ("l_linenumber", DataType::Int32),
            ("l_partkey", DataType::Int64),
            ("l_suppkey", DataType::Int64),
            ("l_quantity", DECIMAL_15_2),
            ("l_extendedprice", DECIMAL_15_2),
            ("l_discount", DECIMAL_15_2),
            ("l_tax", DECIMAL_15_2),
            ("l_returnflag", DataType::String),
            ("l_linestatus", DataType::String),
            ("l_shipdate", DataType::Timestamp),
            ("l_commitdate", DataType::Timestamp),
            ("l_receiptdate", DataType::Timestamp),
            ("l_shipinstruct", DataType::String),
            ("l_shipmode", DataType::String),
            ("l_comment", DataType::String),
        ],
        primary_key: &[0, 1],
    },
];

#[test]
fn test_tpch_schema_ddl_creates_expected_catalog_schema() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server.bootstrap_root_account(Some("root")).unwrap();

    for ddl in ddl_statements() {
        server.execute(ddl).unwrap();
    }

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();

    assert_eq!(EXPECTED_TABLES.len(), TABLE_NAMES.len());

    for (expected, table_name) in EXPECTED_TABLES.iter().zip(TABLE_NAMES) {
        assert_eq!(expected.name, table_name);

        let table = snapshot
            .table_by_name(expected.name)
            .unwrap_or_else(|| panic!("TPC-H table '{}' was not created", expected.name));
        let columns = table.schema.columns();

        assert_eq!(
            columns.len(),
            expected.columns.len(),
            "unexpected column count for table '{}'",
            expected.name
        );

        for (index, (column, (expected_name, expected_type))) in
            columns.iter().zip(expected.columns).enumerate()
        {
            assert_eq!(
                column.name, *expected_name,
                "unexpected name for column {index} in table '{}'",
                expected.name
            );
            assert_eq!(
                column.data_type, *expected_type,
                "unexpected type for column '{}' in table '{}'",
                expected_name, expected.name
            );
            assert!(
                !column.nullable,
                "column '{}' in table '{}' should be NOT NULL",
                expected_name, expected.name
            );
        }

        assert_eq!(
            table.primary_key.as_slice(),
            expected.primary_key,
            "unexpected primary key for table '{}'",
            expected.name
        );
    }
}
