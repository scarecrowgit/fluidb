use crate::scale_factor::{scale_factor, ScaleFactorError};

use super::rng::RandomState;
use super::text::generate_text;
use super::Part;

/// TPC-H Clause 4.2.3, lines 3954-3965.
const COLORS: [&str; 92] = [
    "almond",
    "antique",
    "aquamarine",
    "azure",
    "beige",
    "bisque",
    "black",
    "blanched",
    "blue",
    "blush",
    "brown",
    "burlywood",
    "burnished",
    "chartreuse",
    "chiffon",
    "chocolate",
    "coral",
    "cornflower",
    "cornsilk",
    "cream",
    "cyan",
    "dark",
    "deep",
    "dim",
    "dodger",
    "drab",
    "firebrick",
    "floral",
    "forest",
    "frosted",
    "gainsboro",
    "ghost",
    "goldenrod",
    "green",
    "grey",
    "honeydew",
    "hot",
    "indian",
    "ivory",
    "khaki",
    "lace",
    "lavender",
    "lawn",
    "lemon",
    "light",
    "lime",
    "linen",
    "magenta",
    "maroon",
    "medium",
    "metallic",
    "midnight",
    "mint",
    "misty",
    "moccasin",
    "navajo",
    "navy",
    "olive",
    "orange",
    "orchid",
    "pale",
    "papaya",
    "peach",
    "peru",
    "pink",
    "plum",
    "powder",
    "puff",
    "purple",
    "red",
    "rose",
    "rosy",
    "royal",
    "saddle",
    "salmon",
    "sandy",
    "seashell",
    "sienna",
    "sky",
    "slate",
    "smoke",
    "snow",
    "spring",
    "steel",
    "tan",
    "thistle",
    "tomato",
    "turquoise",
    "violet",
    "wheat",
    "white",
    "yellow",
];

/// TPC-H Clause 4.2.2.13.
const TYPE_FIRST: [&str; 6] = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
const TYPE_SECOND: [&str; 5] = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
const TYPE_THIRD: [&str; 5] = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];

/// TPC-H Clause 4.2.2.13.
const CONTAINER_FIRST: [&str; 5] = ["SM", "LG", "MED", "JUMBO", "WRAP"];
const CONTAINER_SECOND: [&str; 8] = ["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"];

/// Generates all PART rows for the requested scale factor.
pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
) -> Result<Vec<Part>, ScaleFactorError> {
    let count = scale_factor(scale_factor_text, 200_000)?;
    let mut parts = Vec::with_capacity(count as usize);

    for partkey in 1..=count {
        let manufacturer = rng.structural_bounded(5) + 1;
        let brand_digit = rng.structural_bounded(5) + 1;
        let retail_price =
            90_000 + ((partkey / 10) % 20_001) as i64 + 100 * (partkey % 1_000) as i64;

        parts.push(Part {
            p_partkey: partkey as i64,
            p_name: part_name(rng),
            p_mfgr: format!("Manufacturer#{manufacturer}"),
            p_brand: format!("Brand#{manufacturer}{brand_digit}"),
            p_type: format!(
                "{} {} {}",
                TYPE_FIRST[rng.structural_bounded(TYPE_FIRST.len() as u64) as usize],
                TYPE_SECOND[rng.structural_bounded(TYPE_SECOND.len() as u64) as usize],
                TYPE_THIRD[rng.structural_bounded(TYPE_THIRD.len() as u64) as usize],
            ),
            p_size: (rng.structural_bounded(50) + 1) as i32,
            p_container: format!(
                "{} {}",
                CONTAINER_FIRST[rng.structural_bounded(CONTAINER_FIRST.len() as u64) as usize],
                CONTAINER_SECOND[rng.structural_bounded(CONTAINER_SECOND.len() as u64) as usize],
            ),
            // The specification formula is expressed in dollars; retain cents.
            p_retailprice: retail_price,
            p_comment: generate_text(rng, 5, 22),
        });
    }

    Ok(parts)
}

fn part_name(rng: &mut RandomState) -> String {
    let mut color_indices: Vec<usize> = (0..COLORS.len()).collect();

    // Select the first five elements of a partial Fisher-Yates shuffle.
    for position in 0..5 {
        let selected = position + rng.structural_bounded((COLORS.len() - position) as u64) as usize;
        color_indices.swap(position, selected);
    }

    color_indices[..5]
        .iter()
        .map(|&index| COLORS[index])
        .collect::<Vec<_>>()
        .join(" ")
}
