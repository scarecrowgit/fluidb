//! TPC-H dataset generation framework.

pub mod customer;
pub mod lineitem;
pub mod orders;
pub mod part;
pub mod partsupp;
pub mod reference;
pub mod rng;
pub mod supplier;
pub mod text;

use std::collections::BTreeMap;

use crate::scale_factor::{scale_factor, ScaleFactorError};
use reference::{NATIONS, REGIONS};
use rng::RandomState;
use text::generate_text;

/// A REGION row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub r_regionkey: i64,
    pub r_name: String,
    pub r_comment: String,
}

/// A NATION row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nation {
    pub n_nationkey: i64,
    pub n_name: String,
    pub n_regionkey: i64,
    pub n_comment: String,
}

/// A PART row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub p_partkey: i64,
    pub p_name: String,
    pub p_mfgr: String,
    pub p_brand: String,
    pub p_type: String,
    pub p_size: i32,
    pub p_container: String,
    pub p_retailprice: i64,
    pub p_comment: String,
}

/// A SUPPLIER row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supplier {
    pub s_suppkey: i64,
    pub s_name: String,
    pub s_address: String,
    pub s_nationkey: i64,
    pub s_phone: String,
    pub s_acctbal: i64,
    pub s_comment: String,
}

/// A PARTSUPP row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partsupp {
    pub ps_partkey: i64,
    pub ps_suppkey: i64,
    pub ps_availqty: i32,
    pub ps_supplycost: i64,
    pub ps_comment: String,
}

/// A CUSTOMER row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Customer {
    pub c_custkey: i64,
    pub c_name: String,
    pub c_address: String,
    pub c_nationkey: i64,
    pub c_phone: String,
    pub c_acctbal: i64,
    pub c_mktsegment: String,
    pub c_comment: String,
}

/// An ORDERS row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orders {
    pub o_orderkey: i64,
    pub o_custkey: i64,
    pub o_orderstatus: String,
    pub o_totalprice: i64,
    pub o_orderdate: String,
    pub o_orderpriority: String,
    pub o_clerk: String,
    pub o_shippriority: i32,
    pub o_comment: String,
}

/// A LINEITEM row, with fields in schema DDL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lineitem {
    pub l_orderkey: i64,
    pub l_linenumber: i32,
    pub l_partkey: i64,
    pub l_suppkey: i64,
    pub l_quantity: i64,
    pub l_extendedprice: i64,
    pub l_discount: i64,
    pub l_tax: i64,
    pub l_returnflag: String,
    pub l_linestatus: String,
    pub l_shipdate: String,
    pub l_commitdate: String,
    pub l_receiptdate: String,
    pub l_shipinstruct: String,
    pub l_shipmode: String,
    pub l_comment: String,
}

/// All generated TPC-H tables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dataset {
    pub region: Vec<Region>,
    pub nation: Vec<Nation>,
    pub part: Vec<Part>,
    pub supplier: Vec<Supplier>,
    pub partsupp: Vec<Partsupp>,
    pub customer: Vec<Customer>,
    pub orders_shells: Vec<orders::OrderShell>,
    pub orders: Vec<Orders>,
    pub lineitem: Vec<Lineitem>,
}

pub(super) fn supplier_key(partkey: u64, supplier_offset: u64, supplier_count: u64) -> u64 {
    (partkey + supplier_offset * (supplier_count / 4 + (partkey - 1) / supplier_count))
        % supplier_count
        + 1
}

fn partsupp_supplier_keys_are_distinct(part_count: u64, supplier_count: u64) -> bool {
    for partkey in 1..=part_count {
        let mut supplier_keys = [0; 4];

        for supplier_offset in 0..4 {
            let supplier_key = supplier_key(partkey, supplier_offset, supplier_count);
            if supplier_keys[..supplier_offset as usize].contains(&supplier_key) {
                return false;
            }
            supplier_keys[supplier_offset as usize] = supplier_key;
        }
    }

    true
}

/// Generates a deterministic TPC-H dataset for `scale_factor` and `seed`.
pub fn generate(scale_factor_text: &str, seed: u64) -> Result<Dataset, ScaleFactorError> {
    // This order follows foreign-key dependencies. It matters because later
    // table generators select valid keys from tables generated before them.
    let part_count = scale_factor(scale_factor_text, 200_000)?;
    let supplier_count = scale_factor(scale_factor_text, 10_000)?;
    let _partsupp_count = scale_factor(scale_factor_text, 800_000)?;
    let _customer_count = scale_factor(scale_factor_text, 150_000)?;
    let _orders_count = scale_factor(scale_factor_text, 1_500_000)?;
    let _lineitem_count = scale_factor(scale_factor_text, 6_000_000)?;

    if !partsupp_supplier_keys_are_distinct(part_count, supplier_count) {
        return Err(ScaleFactorError::DuplicatePartsuppSupplierKeys);
    }

    let mut rng = RandomState::new(seed);

    // REGION and NATION are specification-fixed reference tables, not scaled.
    let region = REGIONS
        .iter()
        .map(|row| Region {
            r_regionkey: row.r_regionkey,
            r_name: row.r_name.to_owned(),
            r_comment: generate_text(&mut rng, 31, 115),
        })
        .collect();

    let nation = NATIONS
        .iter()
        .map(|row| Nation {
            n_nationkey: row.n_nationkey,
            n_name: row.n_name.to_owned(),
            n_regionkey: row.n_regionkey,
            n_comment: generate_text(&mut rng, 31, 114),
        })
        .collect();

    let part = part::generate(&mut rng, scale_factor_text)?;
    let supplier = supplier::generate(&mut rng, scale_factor_text)?;
    let partsupp = partsupp::generate(&mut rng, scale_factor_text, part_count, supplier_count)?;
    let customer = customer::generate(&mut rng, scale_factor_text)?;
    let orders_shells = orders::generate(&mut rng, scale_factor_text)?;
    let lineitem = lineitem::generate(
        &mut rng,
        scale_factor_text,
        &orders_shells,
        &part,
        &partsupp,
    )?;

    let mut lineitems_by_order = BTreeMap::<i64, Vec<&Lineitem>>::new();
    for row in &lineitem {
        lineitems_by_order
            .entry(row.l_orderkey)
            .or_default()
            .push(row);
    }

    let orders = orders_shells
        .iter()
        .map(|shell| {
            let lines = lineitems_by_order
                .get(&shell.o_orderkey)
                .expect("every generated order must have lineitems");

            // Tax and discount are hundredths; round the final fixed-point total once.
            let total_numerator: i128 = lines
                .iter()
                .map(|line| {
                    i128::from(line.l_extendedprice)
                        * (100 + i128::from(line.l_tax))
                        * (100 - i128::from(line.l_discount))
                })
                .sum();
            let o_totalprice = ((total_numerator + 5_000) / 10_000) as i64;

            let o_orderstatus = if lines.iter().all(|line| line.l_linestatus == "F") {
                "F"
            } else if lines.iter().all(|line| line.l_linestatus == "O") {
                "O"
            } else {
                "P"
            };

            Orders {
                o_orderkey: shell.o_orderkey,
                o_custkey: shell.o_custkey,
                o_orderstatus: o_orderstatus.to_owned(),
                o_totalprice,
                o_orderdate: shell.o_orderdate.clone(),
                o_orderpriority: shell.o_orderpriority.clone(),
                o_clerk: shell.o_clerk.clone(),
                o_shippriority: shell.o_shippriority,
                o_comment: shell.o_comment.clone(),
            }
        })
        .collect();

    Ok(Dataset {
        region,
        nation,
        part,
        supplier,
        partsupp,
        customer,
        orders_shells,
        orders,
        lineitem,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::OnceLock,
    };

    use super::{generate, rng::RandomState, supplier, Dataset};
    use crate::scale_factor::ScaleFactorError;

    pub(super) fn shared_dataset_at_0_01() -> &'static Dataset {
        static DATASET: OnceLock<Dataset> = OnceLock::new();
        DATASET.get_or_init(|| generate("0.01", 42).unwrap())
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(generate("0.01", 42), generate("0.01", 42));
    }

    #[test]
    fn rejects_scale_factors_with_duplicate_partsupp_supplier_keys() {
        assert_eq!(
            generate("0.001", 42),
            Err(ScaleFactorError::DuplicatePartsuppSupplierKeys)
        );
    }

    #[test]
    fn fixed_reference_tables_match_specified_content() {
        let dataset = shared_dataset_at_0_01();

        assert_eq!(dataset.region.len(), 5);
        assert_eq!(dataset.nation.len(), 25);

        assert!(dataset
            .nation
            .iter()
            .any(|row| row.n_name == "EGYPT" && row.n_regionkey == 4));
        assert!(dataset
            .nation
            .iter()
            .any(|row| row.n_name == "FRANCE" && row.n_regionkey == 3));
        assert!(dataset
            .nation
            .iter()
            .any(|row| row.n_name == "BRAZIL" && row.n_regionkey == 1));
    }

    #[test]
    fn fixed_reference_tables_comprehensively_match_clause_4_2_3() {
        let dataset = shared_dataset_at_0_01();

        let expected_regions = [
            (0, "AFRICA"),
            (1, "AMERICA"),
            (2, "ASIA"),
            (3, "EUROPE"),
            (4, "MIDDLE EAST"),
        ];
        assert_eq!(dataset.region.len(), expected_regions.len());
        for (row, (expected_key, expected_name)) in dataset.region.iter().zip(expected_regions) {
            assert_eq!(row.r_regionkey, expected_key);
            assert_eq!(row.r_name, expected_name);
            assert!((31..=115).contains(&row.r_comment.len()));
        }

        let expected_nations = [
            (0, "ALGERIA", 0),
            (1, "ARGENTINA", 1),
            (2, "BRAZIL", 1),
            (3, "CANADA", 1),
            (4, "EGYPT", 4),
            (5, "ETHIOPIA", 0),
            (6, "FRANCE", 3),
            (7, "GERMANY", 3),
            (8, "INDIA", 2),
            (9, "INDONESIA", 2),
            (10, "IRAN", 4),
            (11, "IRAQ", 4),
            (12, "JAPAN", 2),
            (13, "JORDAN", 4),
            (14, "KENYA", 0),
            (15, "MOROCCO", 0),
            (16, "MOZAMBIQUE", 0),
            (17, "PERU", 1),
            (18, "CHINA", 2),
            (19, "ROMANIA", 3),
            (20, "SAUDI ARABIA", 4),
            (21, "VIETNAM", 2),
            (22, "RUSSIA", 3),
            (23, "UNITED KINGDOM", 3),
            (24, "UNITED STATES", 1),
        ];
        assert_eq!(dataset.nation.len(), expected_nations.len());
        for (row, (expected_key, expected_name, expected_regionkey)) in
            dataset.nation.iter().zip(expected_nations)
        {
            assert_eq!(row.n_nationkey, expected_key);
            assert_eq!(row.n_name, expected_name);
            assert_eq!(row.n_regionkey, expected_regionkey);
            assert!((31..=114).contains(&row.n_comment.len()));
        }
    }

    #[test]
    fn part_supplier_and_partsupp_counts_match_scale_factor() {
        let dataset = shared_dataset_at_0_01();

        assert_eq!(dataset.part.len(), 2_000);
        assert_eq!(dataset.supplier.len(), 100);
        assert_eq!(dataset.partsupp.len(), 8_000);
    }

    #[test]
    fn partsupp_references_and_supplier_sets_are_valid() {
        let dataset = shared_dataset_at_0_01();
        let part_keys: BTreeSet<i64> = dataset.part.iter().map(|row| row.p_partkey).collect();
        let supplier_keys: BTreeSet<i64> =
            dataset.supplier.iter().map(|row| row.s_suppkey).collect();
        let mut suppliers_by_part = BTreeMap::<i64, BTreeSet<i64>>::new();
        let mut partsupp_pairs = BTreeSet::new();

        for row in &dataset.partsupp {
            assert!(part_keys.contains(&row.ps_partkey));
            assert!(supplier_keys.contains(&row.ps_suppkey));
            assert!(partsupp_pairs.insert((row.ps_partkey, row.ps_suppkey)));
            suppliers_by_part
                .entry(row.ps_partkey)
                .or_default()
                .insert(row.ps_suppkey);
        }

        assert_eq!(suppliers_by_part.len(), dataset.part.len());
        assert!(suppliers_by_part.values().all(|keys| keys.len() == 4));
    }

    #[test]
    fn part_domains_and_color_requirements_are_valid() {
        let dataset = shared_dataset_at_0_01();
        let type_first = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
        let type_second = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
        let type_third = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];
        let container_first = ["SM", "LG", "MED", "JUMBO", "WRAP"];
        let container_second = ["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"];

        for row in &dataset.part {
            let type_words: Vec<_> = row.p_type.split(' ').collect();
            assert_eq!(type_words.len(), 3);
            assert!(type_first.contains(&type_words[0]));
            assert!(type_second.contains(&type_words[1]));
            assert!(type_third.contains(&type_words[2]));

            let container_words: Vec<_> = row.p_container.split(' ').collect();
            assert_eq!(container_words.len(), 2);
            assert!(container_first.contains(&container_words[0]));
            assert!(container_second.contains(&container_words[1]));
            assert!((1..=50).contains(&row.p_size));

            let brand = row.p_brand.as_bytes();
            assert_eq!(brand.len(), 8);
            assert_eq!(&brand[..6], b"Brand#");
            assert!((b'1'..=b'5').contains(&brand[6]));
            assert!((b'1'..=b'5').contains(&brand[7]));
        }

        assert!(dataset
            .part
            .iter()
            .any(|row| row.p_name.split(' ').any(|color| color == "green")));
        assert!(dataset
            .part
            .iter()
            .any(|row| row.p_name.starts_with("forest ")));
    }

    #[test]
    fn supplier_phone_prefixes_and_comment_cohorts_are_valid() {
        let mut rng = RandomState::new(42);
        let suppliers = supplier::generate(&mut rng, "1.0").unwrap();
        let complaints: BTreeSet<_> = suppliers
            .iter()
            .filter(|row| row.s_comment.contains("Customer Complaints"))
            .map(|row| row.s_suppkey)
            .collect();
        let recommendations: BTreeSet<_> = suppliers
            .iter()
            .filter(|row| row.s_comment.contains("Customer Recommends"))
            .map(|row| row.s_suppkey)
            .collect();

        assert_eq!(complaints.len(), 5);
        assert_eq!(recommendations.len(), 5);
        assert!(complaints.is_disjoint(&recommendations));

        for row in &suppliers {
            let expected_prefix = format!("{:02}-", row.s_nationkey + 10);
            assert!(row.s_phone.starts_with(&expected_prefix));
        }
    }
}
