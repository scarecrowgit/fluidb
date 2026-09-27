/*
 * Copyright 1993 - 2024 Transaction Processing Performance Council
 *
 * Permission is hereby granted to copy without fee all or part of this
 * material including this copyright notice provided that the copies are not
 * made or distributed for direct commercial advantage.
 */

/// Fixed REGION row values transcribed from TPC-H specification Clause 4.2.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceRegion {
    pub r_regionkey: i64,
    pub r_name: &'static str,
}

/// Fixed NATION row values transcribed from TPC-H specification Clause 4.2.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceNation {
    pub n_nationkey: i64,
    pub n_name: &'static str,
    pub n_regionkey: i64,
}

/// The five REGION keys and names from Clause 4.2.3 lines 4041-4045.
pub const REGIONS: [ReferenceRegion; 5] = [
    ReferenceRegion {
        r_regionkey: 0,
        r_name: "AFRICA",
    },
    ReferenceRegion {
        r_regionkey: 1,
        r_name: "AMERICA",
    },
    ReferenceRegion {
        r_regionkey: 2,
        r_name: "ASIA",
    },
    ReferenceRegion {
        r_regionkey: 3,
        r_name: "EUROPE",
    },
    ReferenceRegion {
        r_regionkey: 4,
        r_name: "MIDDLE EAST",
    },
];

/// The 25 NATION keys, names, and REGION keys from Clause 4.2.3 lines 4047-4059.
pub const NATIONS: [ReferenceNation; 25] = [
    ReferenceNation {
        n_nationkey: 0,
        n_name: "ALGERIA",
        n_regionkey: 0,
    },
    ReferenceNation {
        n_nationkey: 1,
        n_name: "ARGENTINA",
        n_regionkey: 1,
    },
    ReferenceNation {
        n_nationkey: 2,
        n_name: "BRAZIL",
        n_regionkey: 1,
    },
    ReferenceNation {
        n_nationkey: 3,
        n_name: "CANADA",
        n_regionkey: 1,
    },
    ReferenceNation {
        n_nationkey: 4,
        n_name: "EGYPT",
        n_regionkey: 4,
    },
    ReferenceNation {
        n_nationkey: 5,
        n_name: "ETHIOPIA",
        n_regionkey: 0,
    },
    ReferenceNation {
        n_nationkey: 6,
        n_name: "FRANCE",
        n_regionkey: 3,
    },
    ReferenceNation {
        n_nationkey: 7,
        n_name: "GERMANY",
        n_regionkey: 3,
    },
    ReferenceNation {
        n_nationkey: 8,
        n_name: "INDIA",
        n_regionkey: 2,
    },
    ReferenceNation {
        n_nationkey: 9,
        n_name: "INDONESIA",
        n_regionkey: 2,
    },
    ReferenceNation {
        n_nationkey: 10,
        n_name: "IRAN",
        n_regionkey: 4,
    },
    ReferenceNation {
        n_nationkey: 11,
        n_name: "IRAQ",
        n_regionkey: 4,
    },
    ReferenceNation {
        n_nationkey: 12,
        n_name: "JAPAN",
        n_regionkey: 2,
    },
    ReferenceNation {
        n_nationkey: 13,
        n_name: "JORDAN",
        n_regionkey: 4,
    },
    ReferenceNation {
        n_nationkey: 14,
        n_name: "KENYA",
        n_regionkey: 0,
    },
    ReferenceNation {
        n_nationkey: 15,
        n_name: "MOROCCO",
        n_regionkey: 0,
    },
    ReferenceNation {
        n_nationkey: 16,
        n_name: "MOZAMBIQUE",
        n_regionkey: 0,
    },
    ReferenceNation {
        n_nationkey: 17,
        n_name: "PERU",
        n_regionkey: 1,
    },
    ReferenceNation {
        n_nationkey: 18,
        n_name: "CHINA",
        n_regionkey: 2,
    },
    ReferenceNation {
        n_nationkey: 19,
        n_name: "ROMANIA",
        n_regionkey: 3,
    },
    ReferenceNation {
        n_nationkey: 20,
        n_name: "SAUDI ARABIA",
        n_regionkey: 4,
    },
    ReferenceNation {
        n_nationkey: 21,
        n_name: "VIETNAM",
        n_regionkey: 2,
    },
    ReferenceNation {
        n_nationkey: 22,
        n_name: "RUSSIA",
        n_regionkey: 3,
    },
    ReferenceNation {
        n_nationkey: 23,
        n_name: "UNITED KINGDOM",
        n_regionkey: 3,
    },
    ReferenceNation {
        n_nationkey: 24,
        n_name: "UNITED STATES",
        n_regionkey: 1,
    },
];

#[cfg(test)]
mod tests {
    use super::NATIONS;

    #[test]
    fn nation_transcription_matches_specified_pairings() {
        assert_eq!(NATIONS[2].n_name, "BRAZIL");
        assert_eq!(NATIONS[2].n_regionkey, 1);
        assert_eq!(NATIONS[4].n_name, "EGYPT");
        assert_eq!(NATIONS[4].n_regionkey, 4);
        assert_eq!(NATIONS[6].n_name, "FRANCE");
        assert_eq!(NATIONS[6].n_regionkey, 3);
    }
}
