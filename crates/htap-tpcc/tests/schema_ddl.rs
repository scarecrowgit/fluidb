use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::types::DataType;
use htap_server::LocalServer;
use htap_tpcc::schema::{ddl_statements, TABLE_NAMES};
use tempfile::TempDir;

struct ExpectedTable {
    name: &'static str,
    columns: &'static [(&'static str, DataType, bool)],
    primary_key: &'static [usize],
}

const DECIMAL_4_4: DataType = DataType::Decimal {
    precision: 4,
    scale: 4,
};
const DECIMAL_5_2: DataType = DataType::Decimal {
    precision: 5,
    scale: 2,
};
const DECIMAL_6_2: DataType = DataType::Decimal {
    precision: 6,
    scale: 2,
};
const DECIMAL_12_2: DataType = DataType::Decimal {
    precision: 12,
    scale: 2,
};

const EXPECTED_TABLES: &[ExpectedTable] = &[
    ExpectedTable {
        name: "warehouse",
        columns: &[
            ("w_id", DataType::Int32, false),
            ("w_name", DataType::String, false),
            ("w_street_1", DataType::String, false),
            ("w_street_2", DataType::String, false),
            ("w_city", DataType::String, false),
            ("w_state", DataType::String, false),
            ("w_zip", DataType::String, false),
            ("w_tax", DECIMAL_4_4, false),
            ("w_ytd", DECIMAL_12_2, false),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "district",
        columns: &[
            ("d_id", DataType::Int32, false),
            ("d_w_id", DataType::Int32, false),
            ("d_name", DataType::String, false),
            ("d_street_1", DataType::String, false),
            ("d_street_2", DataType::String, false),
            ("d_city", DataType::String, false),
            ("d_state", DataType::String, false),
            ("d_zip", DataType::String, false),
            ("d_tax", DECIMAL_4_4, false),
            ("d_ytd", DECIMAL_12_2, false),
            ("d_next_o_id", DataType::Int32, false),
        ],
        primary_key: &[1, 0],
    },
    ExpectedTable {
        name: "item",
        columns: &[
            ("i_id", DataType::Int32, false),
            ("i_im_id", DataType::Int32, false),
            ("i_name", DataType::String, false),
            ("i_price", DECIMAL_5_2, false),
            ("i_data", DataType::String, false),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "stock",
        columns: &[
            ("s_i_id", DataType::Int32, false),
            ("s_w_id", DataType::Int32, false),
            ("s_quantity", DataType::Int32, false),
            ("s_dist_01", DataType::String, false),
            ("s_dist_02", DataType::String, false),
            ("s_dist_03", DataType::String, false),
            ("s_dist_04", DataType::String, false),
            ("s_dist_05", DataType::String, false),
            ("s_dist_06", DataType::String, false),
            ("s_dist_07", DataType::String, false),
            ("s_dist_08", DataType::String, false),
            ("s_dist_09", DataType::String, false),
            ("s_dist_10", DataType::String, false),
            ("s_ytd", DataType::Int32, false),
            ("s_order_cnt", DataType::Int32, false),
            ("s_remote_cnt", DataType::Int32, false),
            ("s_data", DataType::String, false),
        ],
        primary_key: &[1, 0],
    },
    ExpectedTable {
        name: "customer",
        columns: &[
            ("c_id", DataType::Int32, false),
            ("c_d_id", DataType::Int32, false),
            ("c_w_id", DataType::Int32, false),
            ("c_first", DataType::String, false),
            ("c_middle", DataType::String, false),
            ("c_last", DataType::String, false),
            ("c_street_1", DataType::String, false),
            ("c_street_2", DataType::String, false),
            ("c_city", DataType::String, false),
            ("c_state", DataType::String, false),
            ("c_zip", DataType::String, false),
            ("c_phone", DataType::String, false),
            ("c_since", DataType::Timestamp, false),
            ("c_credit", DataType::String, false),
            ("c_credit_lim", DECIMAL_12_2, false),
            ("c_discount", DECIMAL_4_4, false),
            ("c_balance", DECIMAL_12_2, false),
            ("c_ytd_payment", DECIMAL_12_2, false),
            ("c_payment_cnt", DataType::Int32, false),
            ("c_delivery_cnt", DataType::Int32, false),
            ("c_data", DataType::String, false),
        ],
        primary_key: &[2, 1, 0],
    },
    ExpectedTable {
        name: "history",
        columns: &[
            ("h_id", DataType::Int64, false),
            ("h_c_id", DataType::Int32, false),
            ("h_c_d_id", DataType::Int32, false),
            ("h_c_w_id", DataType::Int32, false),
            ("h_d_id", DataType::Int32, false),
            ("h_w_id", DataType::Int32, false),
            ("h_date", DataType::Timestamp, false),
            ("h_amount", DECIMAL_6_2, false),
            ("h_data", DataType::String, false),
        ],
        primary_key: &[0],
    },
    ExpectedTable {
        name: "orders",
        columns: &[
            ("o_id", DataType::Int32, false),
            ("o_d_id", DataType::Int32, false),
            ("o_w_id", DataType::Int32, false),
            ("o_c_id", DataType::Int32, false),
            ("o_entry_d", DataType::Timestamp, false),
            ("o_carrier_id", DataType::Int32, true),
            ("o_ol_cnt", DataType::Int32, false),
            ("o_all_local", DataType::Int32, false),
        ],
        primary_key: &[2, 1, 0],
    },
    ExpectedTable {
        name: "order_line",
        columns: &[
            ("ol_o_id", DataType::Int32, false),
            ("ol_d_id", DataType::Int32, false),
            ("ol_w_id", DataType::Int32, false),
            ("ol_number", DataType::Int32, false),
            ("ol_i_id", DataType::Int32, false),
            ("ol_supply_w_id", DataType::Int32, false),
            ("ol_delivery_d", DataType::Timestamp, true),
            ("ol_quantity", DataType::Int32, false),
            ("ol_amount", DECIMAL_6_2, false),
            ("ol_dist_info", DataType::String, false),
        ],
        primary_key: &[2, 1, 0, 3],
    },
    ExpectedTable {
        name: "new_order",
        columns: &[
            ("no_o_id", DataType::Int32, false),
            ("no_d_id", DataType::Int32, false),
            ("no_w_id", DataType::Int32, false),
        ],
        primary_key: &[2, 1, 0],
    },
];

#[test]
fn ddl_creates_expected_catalog_schema() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server.bootstrap_root_account(Some("root")).unwrap();

    let statements = ddl_statements();
    assert_eq!(TABLE_NAMES.len(), EXPECTED_TABLES.len());
    assert_eq!(statements.len(), EXPECTED_TABLES.len());

    for statement in statements {
        server.execute(&statement).unwrap();
    }

    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog.load().unwrap().unwrap();

    for (expected, table_name) in EXPECTED_TABLES.iter().zip(TABLE_NAMES) {
        assert_eq!(expected.name, *table_name);

        let table = snapshot
            .table_by_name(expected.name)
            .unwrap_or_else(|| panic!("TPC-C table '{}' was not created", expected.name));
        let actual_columns = table.schema.columns();

        assert_eq!(
            actual_columns.len(),
            expected.columns.len(),
            "unexpected column count for table '{}'",
            expected.name
        );

        for (actual, &(expected_name, expected_type, expected_nullable)) in
            actual_columns.iter().zip(expected.columns)
        {
            assert_eq!(
                actual.name.as_str(),
                expected_name,
                "unexpected column name for table '{}'",
                expected.name
            );
            assert_eq!(
                actual.data_type, expected_type,
                "unexpected type for column '{}.{}'",
                expected.name, expected_name
            );
            assert_eq!(
                actual.nullable, expected_nullable,
                "unexpected nullability for column '{}.{}'",
                expected.name, expected_name
            );
        }

        assert_eq!(
            table.primary_key, expected.primary_key,
            "unexpected primary key for table '{}'",
            expected.name
        );
    }
}
