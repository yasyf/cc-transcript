use std::collections::HashMap;

use once_cell::sync::Lazy;

use crate::nlp;

// The override + AFINN scoring data is the canonical vendored snapshot in
// cc_transcript/sentiment/data/, embedded here at compile time; the Python Lexicon
// reads the same two TSVs at runtime. A parity test guards against drift.
const AFINN_DATA: &str = include_str!("../../../../cc_transcript/sentiment/data/afinn-en-165.tsv");
const OVERRIDES_DATA: &str =
    include_str!("../../../../cc_transcript/sentiment/data/domain_overrides.tsv");
const MIN_MAGNITUDE: i32 = 2;
const FLOOR: i32 = 3;

static AFINN: Lazy<HashMap<String, i32>> = Lazy::new(|| parse_tsv(AFINN_DATA));
static OVERRIDES: Lazy<HashMap<String, i32>> = Lazy::new(|| parse_tsv(OVERRIDES_DATA));

fn parse_tsv(data: &str) -> HashMap<String, i32> {
    data.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (word, score) = line.split_once('\t')?;
            Some((word.to_string(), score.trim().parse::<i32>().ok()?))
        })
        .collect()
}

/// The lowercased surface of every UDPipe token in `text`, in order (punctuation
/// included). The shared tokenizer over the embedded UD-EWT model.
pub(crate) fn tokenize(text: &str) -> Result<Vec<String>, String> {
    Ok(nlp::analyze(text)?.into_iter().map(|t| t.lower).collect())
}

/// Polarity of a single token surface — mirrors `Lexicon.polarity`: override first, then
/// AFINN zeroed below `MIN_MAGNITUDE`. `token` is a tokenizer surface (already lowercased);
/// no lemmatization.
pub(crate) fn polarity(token: &str) -> i32 {
    if let Some(&score) = OVERRIDES.get(token) {
        return score;
    }
    match AFINN.get(token) {
        Some(&score) if score.abs() >= MIN_MAGNITUDE => score,
        _ => 0,
    }
}

/// Whether any token's effective polarity crosses the fixed `FLOOR` (`<= -FLOOR` when
/// `want_negative`, else `>= FLOOR`). Surface polarity with negation sign-flip and no
/// POS gate: every token's surface polarity counts, and a negated token's polarity is
/// sign-flipped, so a negated positive ("isn't great") registers on the negative axis.
/// POS-based suppression is a highlighter concern, not a scoring one.
pub(crate) fn has_hit(text: &str, want_negative: bool) -> Result<bool, String> {
    Ok(nlp::analyze(text)?.iter().any(|token| {
        let p = polarity(&token.lower);
        let effective = if token.negated { -p } else { p };
        if want_negative {
            effective <= -FLOOR
        } else {
            effective >= FLOOR
        }
    }))
}

pub(crate) fn overrides_entries() -> Vec<(String, i32)> {
    OVERRIDES.iter().map(|(k, v)| (k.clone(), *v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_beats_afinn_and_min_magnitude_zeroes() {
        assert_eq!(polarity("stop"), -3); // override
        assert_eq!(polarity("the"), 0); // not present
        assert_eq!(polarity("cool"), 0); // AFINN +1, below MIN_MAGNITUDE
        assert_eq!(polarity("nightmare"), -3); // override
        assert_eq!(polarity("broken"), -3); // override beats AFINN -1
        assert_eq!(polarity("lost"), -3); // AFINN surface -3 — the recovered signal
    }

    #[test]
    fn tokenize_lowercases_and_splits_contractions() {
        assert_eq!(tokenize("LOST losing").unwrap(), vec!["lost", "losing"]);
        assert_eq!(tokenize("can't").unwrap(), vec!["ca", "n't"]); // MWT split
    }

    #[test]
    fn has_hit_pins_the_two_bugs() {
        assert!(has_hit("this is broken", true).unwrap()); // override -3, un-negated
        assert!(has_hit("we lost the data", true).unwrap()); // AFINN surface -3, off the lemma path
    }

    #[test]
    fn negation_flips_the_axis() {
        assert!(has_hit("this is amazing", false).unwrap()); // ADJ +4 positive hit
        assert!(!has_hit("this isn't amazing", false).unwrap()); // negated positive is not a positive hit
        assert!(has_hit("this isn't amazing", true).unwrap()); // ...it registers on the negative axis
    }

    #[test]
    fn has_hit_counts_surface_polarity_regardless_of_pos() {
        // UDPipe mistags "splendid" to a non-content POS; the removed POS gate must not
        // drop its AFINN +3 — surface polarity alone decides the hit.
        assert!(has_hit("this is splendid", false).unwrap());
    }
}
