/// Fixed TPC-H query-validation substitution parameters.
///
/// These values document the published validation defaults. They are not the
/// official TPC-H parameter-generation and validation algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryParameters {
    /// The TPC-H query number.
    pub query_number: u8,
    /// The parameter names and their published validation-default values.
    pub values: &'static [(&'static str, &'static str)],
}

/// Returns the published TPC-H query-validation default parameters.
pub const fn fixed_parameters() -> [QueryParameters; 22] {
    [
        QueryParameters {
            query_number: 1,
            values: &[("DELTA", "90")],
        },
        QueryParameters {
            query_number: 2,
            values: &[("SIZE", "15"), ("TYPE", "BRASS"), ("REGION", "EUROPE")],
        },
        QueryParameters {
            query_number: 3,
            values: &[("SEGMENT", "BUILDING"), ("DATE", "1995-03-15")],
        },
        QueryParameters {
            query_number: 4,
            values: &[("DATE", "1993-07-01")],
        },
        QueryParameters {
            query_number: 5,
            values: &[("REGION", "ASIA"), ("DATE", "1994-01-01")],
        },
        QueryParameters {
            query_number: 6,
            values: &[
                ("DATE", "1994-01-01"),
                ("DISCOUNT", "0.06"),
                ("QUANTITY", "24"),
            ],
        },
        QueryParameters {
            query_number: 7,
            values: &[("NATION1", "FRANCE"), ("NATION2", "GERMANY")],
        },
        QueryParameters {
            query_number: 8,
            values: &[
                ("NATION", "BRAZIL"),
                ("REGION", "AMERICA"),
                ("TYPE", "ECONOMY ANODIZED STEEL"),
            ],
        },
        QueryParameters {
            query_number: 9,
            values: &[("COLOR", "green")],
        },
        QueryParameters {
            query_number: 10,
            values: &[("DATE", "1993-10-01")],
        },
        QueryParameters {
            query_number: 11,
            values: &[("NATION", "GERMANY"), ("FRACTION", "0.0001 / SF")],
        },
        QueryParameters {
            query_number: 12,
            values: &[
                ("SHIPMODE1", "MAIL"),
                ("SHIPMODE2", "SHIP"),
                ("DATE", "1994-01-01"),
            ],
        },
        QueryParameters {
            query_number: 13,
            values: &[("WORD1", "special"), ("WORD2", "requests")],
        },
        QueryParameters {
            query_number: 14,
            values: &[("DATE", "1995-09-01")],
        },
        QueryParameters {
            query_number: 15,
            values: &[("DATE", "1996-01-01")],
        },
        QueryParameters {
            query_number: 16,
            values: &[
                ("BRAND", "Brand#45"),
                ("TYPE", "MEDIUM POLISHED"),
                ("SIZE1", "49"),
                ("SIZE2", "14"),
                ("SIZE3", "23"),
                ("SIZE4", "45"),
                ("SIZE5", "19"),
                ("SIZE6", "3"),
                ("SIZE7", "36"),
                ("SIZE8", "9"),
            ],
        },
        QueryParameters {
            query_number: 17,
            values: &[("BRAND", "Brand#23"), ("CONTAINER", "MED BOX")],
        },
        QueryParameters {
            query_number: 18,
            values: &[("QUANTITY", "300")],
        },
        QueryParameters {
            query_number: 19,
            values: &[
                ("QUANTITY1", "1"),
                ("QUANTITY2", "10"),
                ("QUANTITY3", "20"),
                ("BRAND1", "Brand#12"),
                ("BRAND2", "Brand#23"),
                ("BRAND3", "Brand#34"),
            ],
        },
        QueryParameters {
            query_number: 20,
            values: &[
                ("COLOR", "forest"),
                ("DATE", "1994-01-01"),
                ("NATION", "CANADA"),
            ],
        },
        QueryParameters {
            query_number: 21,
            values: &[("NATION", "SAUDI ARABIA")],
        },
        QueryParameters {
            query_number: 22,
            values: &[
                ("I1", "13"),
                ("I2", "31"),
                ("I3", "23"),
                ("I4", "29"),
                ("I5", "30"),
                ("I6", "18"),
                ("I7", "17"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::fixed_parameters;
    use crate::queries;

    #[test]
    fn test_fixed_parameters_appear_in_query_texts() {
        fn contains_numeric_token(sql: &str, value: &str) -> bool {
            sql.match_indices(value).any(|(start, _)| {
                let end = start + value.len();
                let before_is_digit = start > 0 && sql.as_bytes()[start - 1].is_ascii_digit();
                let after_is_digit = end < sql.len() && sql.as_bytes()[end].is_ascii_digit();

                !before_is_digit && !after_is_digit
            })
        }

        fn contains_size_in_list(sql: &str, value: &str) -> bool {
            sql.split("p_size in (")
                .filter_map(|remainder| remainder.split_once(')'))
                .any(|(list, _)| list.split(',').any(|size| size.trim() == value))
        }

        fn contains_quantity_lower_bound(sql: &str, value: &str) -> bool {
            let prefix = "l_quantity >= ";

            sql.match_indices(prefix).any(|(start, _)| {
                let value_start = start + prefix.len();
                let value_end = value_start + value.len();

                sql.get(value_start..value_end) == Some(value)
                    && sql
                        .as_bytes()
                        .get(value_end)
                        .is_none_or(|byte| !byte.is_ascii_digit())
            })
        }

        for parameters in fixed_parameters() {
            let sql = queries::query(parameters.query_number, "1")
                .expect("published TPC-H query definition");

            for &(name, value) in parameters.values {
                let found = match (parameters.query_number, name) {
                    (1, "DELTA") => sql.contains(&format!("interval '{value}' day")),
                    (11, "FRACTION") => sql.contains(&value.replace("SF", "1")),
                    // Match the exact comma-delimited p_size list entry.
                    (
                        16,
                        "SIZE1" | "SIZE2" | "SIZE3" | "SIZE4" | "SIZE5" | "SIZE6" | "SIZE7"
                        | "SIZE8",
                    ) => contains_size_in_list(&sql, value),
                    // Do not allow a p_size value or a prefix of another quantity
                    // value to satisfy the l_quantity lower-bound check.
                    (19, "QUANTITY1" | "QUANTITY2" | "QUANTITY3") => {
                        contains_quantity_lower_bound(&sql, value)
                    }
                    _ if value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.') =>
                    {
                        contains_numeric_token(&sql, value)
                    }
                    _ => sql.contains(value),
                };

                assert!(
                    found,
                    "Query {} parameter {name}={value:?} was not found in expected context",
                    parameters.query_number
                );
            }
        }
    }
}
