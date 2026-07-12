use once_cell::sync::Lazy;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::sync::Mutex;
use udpipe_rs::{Model, Word};

// Embedded at compile time — no runtime file read, download, or cache. A corrupt
// embed is a build bug (panic). Model is Send + !Sync, so a Mutex serializes parses.
const MODEL_BYTES: &[u8] = include_bytes!("../../cc_transcript/sentiment/data/en-ewt.udpipe");

static MODEL: Lazy<Mutex<Model>> = Lazy::new(|| {
    Mutex::new(Model::load_from_memory(MODEL_BYTES).expect("embedded en-ewt.udpipe loads"))
});

// The parts of speech the lexicon scores; every other POS and punctuation is neutral.
const CONTENT_POS: [&str; 6] = ["ADJ", "ADV", "INTJ", "NOUN", "PROPN", "VERB"];

// UD-EWT emits no Polarity=Neg feature and every clitic negator ("n't", "cannot")
// lemmatizes to "not", so lemma membership is the whole negator rule.
const NEGATORS: [&str; 6] = ["never", "no", "none", "not", "nothing", "without"];

pub(crate) fn is_content(upos: &str) -> bool {
    CONTENT_POS.contains(&upos)
}

fn is_negator(lemma: &str) -> bool {
    NEGATORS.contains(&lemma)
}

fn is_clause_boundary(upos: &str) -> bool {
    matches!(upos, "PUNCT" | "CCONJ" | "SCONJ")
}

pub(crate) struct Token {
    pub form: String,
    pub lower: String,
    pub lemma: String,
    pub upos: String,
    pub start: usize,
    pub end: usize,
    pub negated: bool,
}

// Codepoint (not byte) offsets by length-walk: skip inter-token separator whitespace,
// then consume char_count(form) chars. The skip stops on the form's first char, so a
// token that IS whitespace (UDPipe emits U+0085/U+000B/U+000C as tokens) keeps its own
// span. Content-immune, so form normalization ("OpenAI"→"OpenAi", curly→straight quotes)
// leaves the span on the true source where a substring search would desync.
fn char_offsets(text: &str, words: &[Word]) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(words.len());
    let mut cursor = 0usize;
    for word in words {
        let first = word.form.chars().next();
        while cursor < chars.len() && chars[cursor].is_whitespace() && Some(chars[cursor]) != first {
            cursor += 1;
        }
        let end = cursor + word.form.chars().count();
        out.push((cursor, end));
        cursor = end;
    }
    out
}

// Forward, clause-local scope: within a sentence a negator flips every following
// content token until a clause boundary (PUNCT/CCONJ/SCONJ) or the sentence ends. A
// negator token is never itself marked negated, even inside an active scope. A coarse
// approximation: it does not handle "not only", litotes, or idioms, and an
// overt-boundary-free complement clause lets an outer negator reach inner content —
// acceptable for short feedback messages.
fn negation_flags(words: &[Word]) -> Vec<bool> {
    let mut flags = vec![false; words.len()];
    let mut active = false;
    let mut sentence = -1i32;
    for (i, word) in words.iter().enumerate() {
        if word.sentence_id != sentence {
            sentence = word.sentence_id;
            active = false;
        }
        let negator = is_negator(&word.lemma);
        if is_clause_boundary(&word.upostag) {
            active = false;
        } else if active && !negator && is_content(&word.upostag) {
            flags[i] = true;
        }
        if negator {
            active = true;
        }
    }
    flags
}

// The lock is held only across `parse` — the guard drops at the block's close, before
// the `?`, so a parse error (a NUL byte, an FFI failure) returns Err instead of
// panicking under the lock and poisoning the shared model for every later call.
pub(crate) fn analyze(text: &str) -> Result<Vec<Token>, String> {
    let words = {
        let model = MODEL.lock().expect("model mutex not poisoned");
        model.parse(text)
    }
    .map_err(|e| e.message)?;
    let offsets = char_offsets(text, &words);
    let negated = negation_flags(&words);
    Ok(words
        .into_iter()
        .zip(offsets)
        .zip(negated)
        .map(|((word, (start, end)), negated)| Token {
            lower: word.form.to_lowercase(),
            form: word.form,
            lemma: word.lemma,
            upos: word.upostag,
            start,
            end,
            negated,
        })
        .collect())
}

type AnalyzedToken = (String, String, String, String, usize, usize, i32, bool);

/// Analyze `text` with the embedded UDPipe model. Returns, per token,
/// `(form, lower, lemma, upos, char_start, char_end, polarity, negated)` where
/// polarity is the surface-keyed lexicon score and offsets are codepoint indices.
#[pyfunction]
pub fn nlp_analyze(py: Python<'_>, text: &str) -> PyResult<Vec<AnalyzedToken>> {
    py.detach(|| {
        Ok(analyze(text)
            .map_err(PyValueError::new_err)?
            .into_iter()
            .map(|t| {
                let polarity = crate::lexicon::polarity(&t.lower);
                (t.form, t.lower, t.lemma, t.upos, t.start, t.end, polarity, t.negated)
            })
            .collect())
    })
}
