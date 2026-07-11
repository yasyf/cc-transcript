use std::cmp::Ordering;
use std::collections::HashMap;

use once_cell::sync::Lazy;

use crate::generated::unicode::ALPHA_RANGES;

// The override + AFINN scoring data is the canonical vendored snapshot in
// cc_transcript/sentiment/data/, embedded here at compile time; the Python Lexicon
// reads the same two TSVs at runtime. A parity test guards against drift.
const AFINN_DATA: &str = include_str!("../../cc_transcript/sentiment/data/afinn-en-165.tsv");
const OVERRIDES_DATA: &str =
    include_str!("../../cc_transcript/sentiment/data/domain_overrides.tsv");
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

// Membership in Python's str.isalpha() set via binary search over the generated,
// version-pinned ranges — NOT char::is_alphabetic(), a ~10k-codepoint superset.
fn is_alpha(c: char) -> bool {
    let cp = c as u32;
    ALPHA_RANGES
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                Ordering::Greater
            } else if cp > hi {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// Split `text` into lowercased maximal runs of alphabetic characters — the shared,
/// deterministic tokenizer that mirrors the Python `tokenize`. Each whole run is
/// lowercased with `str::to_lowercase`.
///
/// Documented limitation: Greek final-sigma lowercasing can diverge from Python next to
/// exotic Cased-property characters (Unicode 15.1 vs rustc's tables, e.g. U+0295); this
/// is out of corpus scope, and the parity fixture pins the common cases (ΟΣ → ος agrees).
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut run = String::new();
    for c in text.chars() {
        if is_alpha(c) {
            run.push(c);
        } else if !run.is_empty() {
            tokens.push(run.to_lowercase());
            run.clear();
        }
    }
    if !run.is_empty() {
        tokens.push(run.to_lowercase());
    }
    tokens
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

/// Whether any token surface's polarity crosses the fixed `FLOOR` (`<= -FLOOR` when
/// `want_negative`, else `>= FLOOR`). Tokenizes with `tokenize` and scores each surface.
pub(crate) fn has_hit(text: &str, want_negative: bool) -> bool {
    tokenize(text).iter().any(|token| {
        let p = polarity(token);
        if want_negative {
            p <= -FLOOR
        } else {
            p >= FLOOR
        }
    })
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
    fn tokenize_lowercases_alpha_runs() {
        assert_eq!(tokenize("LOST losing"), vec!["lost", "losing"]);
        assert_eq!(tokenize("can't"), vec!["can", "t"]);
        assert!(tokenize("...").is_empty());
    }

    #[test]
    fn has_hit_pins_the_two_bugs() {
        assert!(has_hit("this is broken", true)); // override -3, deterministic both backends
        assert!(has_hit("we lost the data", true)); // AFINN surface -3, was lost to the lemma path
    }
}
