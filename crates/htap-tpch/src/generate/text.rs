//! Deterministic synthetic prose for TPC-H text fields.

use super::rng::RandomState;

// This is intentionally original vocabulary rather than the specification's
// comment-grammar word lists.
const VOCABULARY: &[&str] = &[
    "amber", "anchor", "apricot", "brisk", "cedar", "cobalt", "coral", "distant", "ember", "fable",
    "garden", "harbor", "ivory", "juniper", "lantern", "marble", "meadow", "north", "orchard",
    "paddle", "quiet", "river", "saffron", "timber", "velvet", "willow", "wind",
];

/// Produces deterministic plain text with a length in `min_len..=max_len`.
pub fn generate_text(rng: &mut RandomState, min_len: usize, max_len: usize) -> String {
    assert!(
        min_len <= max_len,
        "minimum text length must not exceed maximum"
    );
    let target = choose_length(rng, min_len, max_len);
    compose(rng, "", target)
}

/// Produces deterministic text containing `phrase_word1` then `phrase_word2`.
pub fn generate_text_with_phrase(
    rng: &mut RandomState,
    phrase_word1: &str,
    phrase_word2: &str,
    min_len: usize,
    max_len: usize,
) -> String {
    assert!(
        min_len <= max_len,
        "minimum text length must not exceed maximum"
    );
    assert!(!phrase_word1.is_empty() && !phrase_word2.is_empty());
    let phrase = format!("{phrase_word1} {phrase_word2}");
    assert!(
        phrase.len() <= max_len,
        "phrase must fit within the requested maximum length"
    );

    let target = choose_length(rng, min_len.max(phrase.len()), max_len);
    compose_with_phrase(rng, &phrase, target)
}

fn compose_with_phrase(rng: &mut RandomState, phrase: &str, target: usize) -> String {
    let max_prefix_words = (target - phrase.len()) / 5;
    let prefix_words = rng.text_bounded((max_prefix_words + 1) as u64) as usize;
    let mut text = String::new();

    for remaining_words in (0..prefix_words).rev() {
        let word = VOCABULARY[rng.text_bounded(VOCABULARY.len() as u64) as usize];
        let minimum_remaining = phrase.len() + 1 + remaining_words * 5;
        let word = if text.len() + word.len() + minimum_remaining <= target {
            word
        } else {
            "wind"
        };

        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(word);
    }

    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(phrase);

    while text.len() < target {
        let word = VOCABULARY[rng.text_bounded(VOCABULARY.len() as u64) as usize];
        if text.len() + 1 + word.len() <= target {
            text.push(' ');
            text.push_str(word);
        } else {
            break;
        }
    }

    while text.len() < target {
        text.push('.');
    }

    text
}

fn choose_length(rng: &mut RandomState, min_len: usize, max_len: usize) -> usize {
    min_len + rng.text_bounded((max_len - min_len + 1) as u64) as usize
}

fn compose(rng: &mut RandomState, prefix: &str, target: usize) -> String {
    let mut text = String::from(prefix);

    while text.len() < target {
        let word = VOCABULARY[rng.text_bounded(VOCABULARY.len() as u64) as usize];
        let separator = if text.is_empty() { "" } else { " " };
        if text.len() + separator.len() + word.len() <= target {
            text.push_str(separator);
            text.push_str(word);
        } else {
            break;
        }
    }

    // A one-byte punctuation suffix makes every requested byte length reachable.
    while text.len() < target {
        text.push('.');
    }

    text
}

#[cfg(test)]
mod tests {
    use super::{generate_text, generate_text_with_phrase};
    use crate::generate::rng::RandomState;

    #[test]
    fn generated_text_respects_requested_lengths() {
        let mut rng = RandomState::new(17);
        for min_len in 0..20 {
            for max_len in min_len..40 {
                let text = generate_text(&mut rng, min_len, max_len);
                assert!((min_len..=max_len).contains(&text.len()));
            }
        }
    }

    #[test]
    fn phrase_text_respects_requested_lengths_and_order() {
        let mut rng = RandomState::new(23);
        for min_len in 0..20 {
            let text = generate_text_with_phrase(&mut rng, "blue", "harbor", min_len, 40);
            assert!((min_len..=40).contains(&text.len()));
            assert!(text.contains("blue harbor"));
        }
    }

    #[test]
    fn phrase_text_places_phrase_at_varying_offsets() {
        let mut rng = RandomState::new(29);
        let phrase = "Customer Complaints";
        let mut offsets = Vec::new();

        // Match at least SF 1.0's supplier count to make varied placement reliable.
        for _ in 0..100 {
            let text = generate_text_with_phrase(&mut rng, "Customer", "Complaints", 50, 120);
            offsets.push(
                text.find(phrase)
                    .expect("generated text must contain phrase"),
            );
        }

        assert!(
            offsets.iter().any(|&offset| offset != offsets[0]),
            "phrase offsets should vary across generated comments"
        );
    }
}
