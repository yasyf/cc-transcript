use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use udpipe_rs::{download_model_from_url, Model};

// The override + AFINN scoring data is generated from the canonical Python sources
// (cc_transcript DOMAIN_OVERRIDES + the afinn package) by scripts/build_lexicon_data.py
// and embedded here at compile time. A parity test guards against drift.
const AFINN_DATA: &str = include_str!("../../cc_transcript/sentiment/data/afinn-en-165.tsv");
const OVERRIDES_DATA: &str =
    include_str!("../../cc_transcript/sentiment/data/domain_overrides.tsv");
const MIN_MAGNITUDE: i32 = 2;
const MODEL_FILE: &str = "english-ewt-ud-2.5-191206.udpipe";
// udpipe-rs's bundled download_model hits a LINDAT DSpace landing page (HTML), so
// we fetch the binary from the stable jwijffels UD-2.5 mirror instead.
const MODEL_URL: &str =
    "https://raw.githubusercontent.com/jwijffels/udpipe.models.ud.2.5/master/inst/udpipe-ud-2.5-191206/english-ewt-ud-2.5-191206.udpipe";

static AFINN: Lazy<HashMap<String, i32>> = Lazy::new(|| parse_tsv(AFINN_DATA));
static OVERRIDES: Lazy<HashMap<String, i32>> = Lazy::new(|| parse_tsv(OVERRIDES_DATA));
// The UDPipe model is downloaded once and cached, mirroring the spaCy-model download
// on the Python path. None on any failure (offline, download/load error) so the
// Python spaCy path takes over — never panics. UDPipe's Model is not Sync, so a
// Mutex serializes parses (low-volume, lexicon-only use).
static MODEL: Lazy<Option<Mutex<Model>>> = Lazy::new(|| load_model().map(Mutex::new));

fn parse_tsv(data: &str) -> HashMap<String, i32> {
    data.lines()
        .filter_map(|line| {
            let (word, score) = line.split_once('\t')?;
            Some((word.to_string(), score.trim().parse::<i32>().ok()?))
        })
        .collect()
}

fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("cc-transcript").join("udpipe"))
}

fn load_model() -> Option<Model> {
    let dir = cache_dir()?;
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(MODEL_FILE);
    if path.exists() {
        if let Ok(model) = Model::load(path.to_str()?) {
            return Some(model);
        }
        let _ = fs::remove_file(&path); // corrupt/partial download — re-fetch
    }
    download_model_from_url(MODEL_URL, path.to_str()?).ok()?;
    Model::load(path.to_str()?).ok()
}

/// Polarity of a single lemma — mirrors ``Lexicon.polarity``: override first, then
/// AFINN zeroed below ``MIN_MAGNITUDE``. (Lemmatization happens in ``has_hit``.)
pub(crate) fn polarity(lemma: &str) -> i32 {
    let lower = lemma.to_lowercase();
    if let Some(&score) = OVERRIDES.get(lower.as_str()) {
        return score;
    }
    match AFINN.get(lower.as_str()) {
        Some(&score) if score.abs() >= MIN_MAGNITUDE => score,
        _ => 0,
    }
}

pub(crate) fn available() -> bool {
    MODEL.is_some()
}

/// Whether any alphabetic token's lemma polarity crosses ``floor`` (``<= -floor``
/// when ``want_negative``). Lemmatizes with UDPipe — the spaCy-equivalent path.
pub(crate) fn has_hit(text: &str, floor: i32, want_negative: bool) -> bool {
    let Some(lock) = MODEL.as_ref() else {
        return false;
    };
    let model = lock.lock().expect("lexicon model mutex poisoned");
    let Ok(words) = model.parse(text) else {
        return false;
    };
    words
        .iter()
        .filter(|word| !word.form.is_empty() && word.form.chars().all(char::is_alphabetic))
        .any(|word| {
            let p = polarity(&word.lemma);
            if want_negative {
                p <= -floor
            } else {
                p >= floor
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
        assert_eq!(polarity("STOP"), -3); // case-insensitive
        assert_eq!(polarity("the"), 0); // not present
        assert_eq!(polarity("cool"), 0); // AFINN +1, below MIN_MAGNITUDE
        assert_eq!(polarity("nightmare"), -3); // override
    }
}
