pub mod dates;
pub mod decimal;
pub mod load;
pub mod q1;
pub mod q10;
pub mod q11;
pub mod q12;
pub mod q13;
pub mod q14;
pub mod q15;
pub mod q16;
pub mod q17;
pub mod q18;
pub mod q19;
pub mod q2;
pub mod q20;
pub mod q21;
pub mod q22;
pub mod q3;
pub mod q4;
pub mod q5;
pub mod q6;
pub mod q7;
pub mod q8;
pub mod q9;

use decimal::Dec;

macro_rules! verify_non_empty {
    () => {
        pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
            if expected(dataset).is_empty() {
                Err("expected result must be non-empty for coverage".to_string())
            } else {
                Ok(())
            }
        }
    };
}

pub(crate) use verify_non_empty;

/// Converts the generator's raw lineitem quantity into DECIMAL(15,2).
pub fn lineitem_quantity(raw_quantity: i64) -> Dec {
    Dec::new(i128::from(raw_quantity) * 100, 15, 2)
}
