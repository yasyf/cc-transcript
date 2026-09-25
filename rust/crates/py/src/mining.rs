use std::cell::Cell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::size_of;

use chrono::{DateTime, Datelike, FixedOffset};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyIterator, PyString};
use regex::Regex;
use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

use cc_transcript_core::activity::{lift_session, sample_refs};
use cc_transcript_core::filter::compile_group_array;
use cc_transcript_core::literals::mining::{
    ANSWER_NOTES_SEP, ANSWER_PREVIEW_SEP, DETECTOR_ASK_USER_QUESTION, DETECTOR_DENIAL,
    DETECTOR_EXIT_PLAN_REJECTION, DETECTOR_INTERRUPT, DETECTOR_PLAN_REENTRY,
    DETECTOR_REVIEW_COMMENT, DETECTOR_TRANSCRIPT_MESSAGE, INTERRUPT_REJECTION, LOW, NONE,
    NO_OPTION_SELECTED, PLAN_REVIEW, QUESTION_ANSWER, REVIEW_COMMENT, TRANSCRIPT_MESSAGE,
};
use cc_transcript_core::parse::{parse_bytes, parse_questions};
use cc_transcript_core::protocol::{
    embedded_user_text, interrupt_marker, is_bare_interrupt_marker, ANSWERED_PREFIX,
    ANSWERED_TRAILER, DENIAL_KIND_USER_REJECTED, INTERRUPT_MARKER_RE,
};
use cc_transcript_core::snapshot::{
    Cancellation, SnapshotError, Status, TranscriptSnapshot, WorkLimits,
};
use cc_transcript_core::snapshot_memory::value_charge;
use cc_transcript_core::types::{
    matches_names, tool_use_index, Entry, EntryMeta, Question, ToolResultBlock, ToolUseBlock,
    UserEntry,
};
use cc_transcript_core::value::{field, field_bool, field_str};

use crate::views::convert::parse_err;

// FINDING_KEYS prefix is folded into finding_keys by structured_format_to_dict, so the
// compiled spec already carries the full alias list; no constant needed here.

#[derive(Clone)]
enum ConfStage {
    Base {
        band: f64,
        reason: String,
    },
    BumpIfSubstantive {
        groups: Regex,
        delta: f64,
        min_words: usize,
        reason: String,
    },
    DemoteIfHedged {
        groups: Regex,
        delta: f64,
        reason: String,
    },
    DemoteIfShort {
        max_words: usize,
        delta: f64,
        reason: String,
    },
    BumpIfProximate {
        within: i64,
        delta: f64,
        reason: String,
    },
    NoiseIfStructural {
        groups: Regex,
        band: f64,
        reason: String,
    },
}

struct CompiledConfSpec {
    stages: Vec<ConfStage>,
}

struct CandidateSig {
    confidence: f64,
    reasons: Vec<String>,
    durable: bool,
}

struct CompiledRegexFormat {
    name: String,
    regex: Regex,
    file_group: Option<usize>,
    line_start_group: Option<usize>,
    line_end_group: Option<usize>,
    comment_groups: Vec<usize>,
    join: String,
}

struct CompiledStructuredFormat {
    name: String,
    file_keys: Vec<String>,
    line_keys: Vec<String>,
    comment_keys: Vec<String>,
    fix_keys: Vec<String>,
    finding_keys: Vec<String>,
}

/// A review format whose pattern and extractor are Python code, held as strong
/// references and invoked single-threaded under the held GIL. Positional, never
/// keyed by name: two specs with identical JSON but different callables must not collide.
struct CompiledCallableFormat {
    name: String,
    pattern: Py<PyAny>,
    extract: Py<PyAny>,
    bounded: bool,
}

struct CompiledReviewSpec {
    surfaces: HashSet<String>,
    regex_formats: Vec<CompiledRegexFormat>,
    callable_formats: Vec<CompiledCallableFormat>,
    structured_formats: Vec<CompiledStructuredFormat>,
}

pub struct CompiledMiningSpec {
    detectors: HashSet<String>,
    reentry_lookback: usize,
    edit_tools: HashSet<String>,
    plan_tools: HashSet<String>,
    denial_excluded_tools: HashSet<String>,
    subagent_tools: HashSet<String>,
    user_message: CompiledConfSpec,
    calibrated: CompiledConfSpec,
    review: CompiledReviewSpec,
}

fn group_array<'a>(stage: &'a Value, key: &str) -> Result<&'a sonic_rs::Array, String> {
    field(stage, key)
        .and_then(JsonContainerTrait::as_array)
        .ok_or(format!("conf stage missing array '{key}'"))
}

fn float_field(stage: &Value, key: &str) -> Result<f64, String> {
    field(stage, key)
        .and_then(JsonValueTrait::as_f64)
        .ok_or(format!("conf stage missing float '{key}'"))
}

fn usize_field(stage: &Value, key: &str) -> Result<usize, String> {
    field(stage, key)
        .and_then(JsonValueTrait::as_u64)
        .ok_or(format!("conf stage missing int '{key}'"))
        .map(|n| n as usize)
}

fn i64_field(stage: &Value, key: &str) -> Result<i64, String> {
    field(stage, key)
        .and_then(JsonValueTrait::as_i64)
        .ok_or(format!("conf stage missing int '{key}'"))
}

fn str_field(stage: &Value, key: &str) -> Result<String, String> {
    field_str(stage, key)
        .ok_or(format!("conf stage missing string '{key}'"))
        .map(String::from)
}

fn opt_usize(value: &Value, key: &str) -> Option<usize> {
    field(value, key)
        .filter(|v| !v.is_null())
        .and_then(JsonValueTrait::as_u64)
        .map(|n| n as usize)
}

fn str_set(value: &Value, key: &str) -> HashSet<String> {
    field(value, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(String::from)
        .collect()
}

fn str_vec(value: &Value, key: &str) -> Vec<String> {
    field(value, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(String::from)
        .collect()
}

fn usize_vec(value: &Value, key: &str) -> Vec<usize> {
    field(value, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_u64)
        .map(|n| n as usize)
        .collect()
}

fn compile_conf_stage(stage: &Value) -> Result<ConfStage, String> {
    let ignore_case = field_bool(stage, "ignore_case");
    match field_str(stage, "kind").ok_or("conf stage missing 'kind'")? {
        "Base" => Ok(ConfStage::Base {
            band: float_field(stage, "band")?,
            reason: str_field(stage, "reason")?,
        }),
        "BumpIfSubstantive" => Ok(ConfStage::BumpIfSubstantive {
            groups: compile_group_array(group_array(stage, "groups")?, ignore_case)?,
            delta: float_field(stage, "delta")?,
            min_words: usize_field(stage, "min_words")?,
            reason: str_field(stage, "reason")?,
        }),
        "DemoteIfHedged" => Ok(ConfStage::DemoteIfHedged {
            groups: compile_group_array(group_array(stage, "groups")?, ignore_case)?,
            delta: float_field(stage, "delta")?,
            reason: str_field(stage, "reason")?,
        }),
        "DemoteIfShort" => Ok(ConfStage::DemoteIfShort {
            max_words: usize_field(stage, "max_words")?,
            delta: float_field(stage, "delta")?,
            reason: str_field(stage, "reason")?,
        }),
        "BumpIfProximate" => Ok(ConfStage::BumpIfProximate {
            within: i64_field(stage, "within")?,
            delta: float_field(stage, "delta")?,
            reason: str_field(stage, "reason")?,
        }),
        "NoiseIfStructural" => Ok(ConfStage::NoiseIfStructural {
            groups: compile_group_array(group_array(stage, "groups")?, ignore_case)?,
            band: float_field(stage, "band")?,
            reason: str_field(stage, "reason")?,
        }),
        other => Err(format!("unknown conf stage kind: {other}")),
    }
}

fn compile_conf_spec(spec: &Value) -> Result<CompiledConfSpec, String> {
    let stages = field(spec, "stages")
        .and_then(JsonContainerTrait::as_array)
        .ok_or("confidence spec missing 'stages' array")?
        .iter()
        .map(compile_conf_stage)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledConfSpec { stages })
}

fn compile_regex_format(fmt: &Value) -> Result<CompiledRegexFormat, String> {
    Ok(CompiledRegexFormat {
        name: str_field(fmt, "name")?,
        regex: compile_review_regex(fmt)?,
        file_group: opt_usize(fmt, "file_group"),
        line_start_group: opt_usize(fmt, "line_start_group"),
        line_end_group: opt_usize(fmt, "line_end_group"),
        comment_groups: usize_vec(fmt, "comment_groups"),
        join: field_str(fmt, "join").unwrap_or(" ").to_string(),
    })
}

/// Compiles a review-format regex with the same flags as
/// ``compile_groups`` (filterspec.py compile_groups, multiline branch): the
/// ``(?:p)|...`` join from ``compile_group_array`` plus a ``(?m)`` prefix when
/// ``multiline`` is set.
fn compile_review_regex(fmt: &Value) -> Result<Regex, String> {
    let groups = group_array(fmt, "groups")?;
    let joined = groups
        .iter()
        .filter_map(|group| {
            1usize
                .value_index_into(group)
                .and_then(JsonValueTrait::as_str)
        })
        .map(|pattern| format!("(?:{pattern})"))
        .collect::<Vec<_>>()
        .join("|");
    let multiline = if field_bool(fmt, "multiline") {
        "(?m)"
    } else {
        ""
    };
    let ignore_case = if field_bool(fmt, "ignore_case") {
        "(?i)"
    } else {
        ""
    };
    let pattern = format!("{multiline}{ignore_case}{joined}");
    Regex::new(&pattern).map_err(|e| format!("invalid review regex {pattern:?}: {e}"))
}

fn compile_structured_format(fmt: &Value) -> Result<CompiledStructuredFormat, String> {
    Ok(CompiledStructuredFormat {
        name: str_field(fmt, "name")?,
        file_keys: str_vec(fmt, "file_keys"),
        line_keys: str_vec(fmt, "line_keys"),
        comment_keys: str_vec(fmt, "comment_keys"),
        fix_keys: str_vec(fmt, "fix_keys"),
        finding_keys: str_vec(fmt, "finding_keys"),
    })
}

fn compile_review_spec(review: &Value) -> Result<CompiledReviewSpec, String> {
    let regex_formats = field(review, "regex_formats")
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .map(compile_regex_format)
        .collect::<Result<Vec<_>, _>>()?;
    let structured_formats = field(review, "structured_formats")
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .map(compile_structured_format)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledReviewSpec {
        surfaces: str_set(review, "surfaces"),
        regex_formats,
        callable_formats: Vec::new(),
        structured_formats,
    })
}

pub fn compile_spec(spec_json: &str) -> Result<CompiledMiningSpec, String> {
    let root: Value =
        sonic_rs::from_str(spec_json).map_err(|e| format!("invalid mining spec json: {e}"))?;
    Ok(CompiledMiningSpec {
        detectors: str_set(&root, "detectors"),
        reentry_lookback: field(&root, "reentry_lookback")
            .and_then(JsonValueTrait::as_u64)
            .ok_or("mining spec missing 'reentry_lookback'")? as usize,
        edit_tools: str_set(&root, "edit_tools"),
        plan_tools: str_set(&root, "plan_tools"),
        denial_excluded_tools: str_set(&root, "denial_excluded_tools"),
        subagent_tools: str_set(
            field(&root, "provenance").ok_or("mining spec missing 'provenance'")?,
            "subagent_tools",
        ),
        user_message: compile_conf_spec(
            field(&root, "user_message").ok_or("mining spec missing 'user_message'")?,
        )?,
        calibrated: compile_conf_spec(
            field(&root, "calibrated").ok_or("mining spec missing 'calibrated'")?,
        )?,
        review: compile_review_spec(field(&root, "review").ok_or("mining spec missing 'review'")?)?,
    })
}

/// Binds the per-spec callable review formats — ``(name, compiled re.Pattern,
/// extractor)`` in the JSON's callable-format order — onto a compiled spec.
pub fn attach_callable_formats(
    spec: &mut CompiledMiningSpec,
    formats: Vec<(String, Py<PyAny>, Py<PyAny>)>,
) {
    spec.review.callable_formats = formats
        .into_iter()
        .map(|(name, pattern, extract)| CompiledCallableFormat {
            name,
            pattern,
            extract,
            bounded: false,
        })
        .collect();
}

pub fn attach_bounded_callable_formats(
    spec: &mut CompiledMiningSpec,
    formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
) -> PyResult<()> {
    spec.review.callable_formats = formats
        .into_iter()
        .map(|(name, pattern, extract, bounded)| {
            if !bounded {
                return Err(crate::snapshots::error(SnapshotError::new(
                    Status::Incomplete,
                    format!("unbounded mining callback: {name}"),
                )));
            }
            Ok(CompiledCallableFormat {
                name,
                pattern,
                extract,
                bounded,
            })
        })
        .collect::<PyResult<_>>()?;
    Ok(())
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn bump(mut sig: CandidateSig, delta: f64, reason: &str) -> CandidateSig {
    sig.confidence = (sig.confidence + delta).clamp(0.0, 1.0);
    sig.reasons.push(reason.to_string());
    sig
}

fn apply_conf_stage(
    stage: &ConfStage,
    text: &str,
    index: i64,
    trigger: Option<i64>,
    sig: CandidateSig,
) -> CandidateSig {
    match stage {
        ConfStage::Base { band, reason } => CandidateSig {
            confidence: *band,
            reasons: {
                let mut reasons = sig.reasons;
                reasons.push(reason.clone());
                reasons
            },
            durable: sig.durable,
        },
        ConfStage::BumpIfSubstantive {
            groups,
            delta,
            min_words,
            reason,
        } if word_count(text) > *min_words && !groups.is_match(text) => bump(sig, *delta, reason),
        ConfStage::DemoteIfHedged {
            groups,
            delta,
            reason,
        } if groups.is_match(text) => bump(sig, *delta, reason),
        ConfStage::DemoteIfShort {
            max_words,
            delta,
            reason,
        } if word_count(text) <= *max_words => bump(sig, *delta, reason),
        ConfStage::BumpIfProximate {
            within,
            delta,
            reason,
        } if trigger.is_some_and(|t| index - t <= *within) => bump(sig, *delta, reason),
        _ => sig,
    }
}

fn run_confidence(
    spec: &CompiledConfSpec,
    text: &str,
    index: i64,
    trigger: Option<i64>,
    base: CandidateSig,
) -> CandidateSig {
    for stage in &spec.stages {
        if let ConfStage::NoiseIfStructural {
            groups,
            band,
            reason,
        } = stage
        {
            if groups.is_match(text) {
                return CandidateSig {
                    confidence: *band,
                    reasons: vec![reason.clone()],
                    durable: base.durable,
                };
            }
        }
    }
    spec.stages.iter().fold(base, |sig, stage| {
        apply_conf_stage(stage, text, index, trigger, sig)
    })
}

/// Scores ``text`` by folding ``calibrated`` over a ``firm(seed)`` base
/// (mining/spec.py calibrated: MEDIUM band, one seed reason).
fn calibrated(spec: &CompiledConfSpec, text: &str, seed: &str) -> CandidateSig {
    run_confidence(
        spec,
        text,
        0,
        None,
        CandidateSig {
            confidence: 0.5,
            reasons: vec![seed.to_string()],
            durable: true,
        },
    )
}

/// Scores a transcript user message (mining/spec.py score_user_message: NONE band,
/// empty reasons, durable=true seed).
fn score_user_message(
    spec: &CompiledConfSpec,
    text: &str,
    index: i64,
    trigger: Option<i64>,
) -> CandidateSig {
    run_confidence(
        spec,
        text,
        index,
        trigger,
        CandidateSig {
            confidence: NONE,
            reasons: vec![],
            durable: true,
        },
    )
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MiningUsage {
    pub events: usize,
    pub input_bytes: usize,
    pub items: usize,
    pub output_bytes: usize,
}

struct MiningBudget {
    limits: WorkLimits,
    cancel: Cancellation,
    usage: Cell<MiningUsage>,
}

impl MiningBudget {
    fn check(&self) -> PyResult<()> {
        self.cancel
            .check(self.limits.deadline_unix_ms)
            .map_err(crate::snapshots::error)
    }

    fn preflight(&self, bytes: usize) -> PyResult<()> {
        self.check()?;
        let usage = self.usage.get();
        if usage.items >= self.limits.max_items
            || bytes
                > self
                    .limits
                    .max_output_bytes
                    .saturating_sub(usage.output_bytes)
        {
            return Err(crate::snapshots::error(SnapshotError::new(
                Status::OutputLimit,
                "mining output budget exceeded",
            )));
        }
        Ok(())
    }

    fn reserve(&self, bytes: usize) -> PyResult<()> {
        self.preflight(bytes)?;
        let mut usage = self.usage.get();
        usage.items += 1;
        usage.output_bytes += bytes;
        self.usage.set(usage);
        Ok(())
    }
}

struct Events<'a> {
    entries: Vec<&'a Entry>,
    texts: Vec<String>,
    budget: Option<MiningBudget>,
}

impl<'a> Events<'a> {
    /// The user text per event, lifted so event indices and mined text agree with
    /// the parsed events by construction.
    fn new(entries: Vec<&'a Entry>) -> Self {
        let texts = entries
            .iter()
            .map(|entry| match entry {
                Entry::User(user) => user.content.text(),
                _ => String::new(),
            })
            .collect();
        Events {
            entries,
            texts,
            budget: None,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn check(&self) -> PyResult<()> {
        self.budget.as_ref().map_or(Ok(()), MiningBudget::check)
    }

    fn preflight(&self, bytes: usize) -> PyResult<()> {
        self.budget
            .as_ref()
            .map_or(Ok(()), |budget| budget.preflight(bytes))
    }

    fn reserve(&self, bytes: usize) -> PyResult<()> {
        self.budget
            .as_ref()
            .map_or(Ok(()), |budget| budget.reserve(bytes))
    }

    /// Nearest preceding assistant index (mining/signals.py nearest_assistant_index).
    fn nearest_assistant_index(&self, index: usize) -> Option<i64> {
        (0..index)
            .rev()
            .find(|&i| matches!(self.entries[i], Entry::Assistant(_)))
            .map(|i| i as i64)
    }

    /// The first non-blank user message at or after ``index`` (mining/signals.py
    /// next_user_message).
    fn next_user_message(&self, index: usize) -> Option<(usize, &UserEntry)> {
        (index..self.len()).find_map(|i| match &self.entries[i] {
            Entry::User(user) if !self.texts[i].trim().is_empty() => Some((i, user)),
            _ => None,
        })
    }

    /// The 40-event lookback for the most recent edit (mining/signals.py
    /// last_edit_index): scans ``range(index-1, max(index-lookback, 0)-1, -1)``
    /// for an assistant event using an edit tool.
    fn last_edit_index(&self, index: usize, spec: &CompiledMiningSpec) -> Option<i64> {
        let lo = index.saturating_sub(spec.reentry_lookback);
        (lo..index)
            .rev()
            .find(|&i| {
                self.entries[i]
                    .tool_uses()
                    .any(|tu| matches_names(&tu.name, &spec.edit_tools))
            })
            .map(|i| i as i64)
    }
}

/// marker_in (mining/signals.py marker_in): whether any tool-result block's
/// content carries the interrupt marker.
fn marker_in(user: &UserEntry) -> bool {
    user.tool_results()
        .any(|b| interrupt_marker(&b.content).is_some())
}

fn weak(reason: &str) -> CandidateSig {
    CandidateSig {
        confidence: LOW,
        reasons: vec![reason.to_string()],
        durable: true,
    }
}

fn noise(reason: &str) -> CandidateSig {
    CandidateSig {
        confidence: NONE,
        reasons: vec![reason.to_string()],
        durable: true,
    }
}

struct ScoredText {
    text: String,
    signal: CandidateSig,
}

/// correction_text (mining/signals.py correction_text): the first following
/// non-bare, non-structural user message — a forward loop that re-scans from the
/// last consumed index.
fn correction_text(events: &Events<'_>, mut index: usize, structural: &Regex) -> Option<String> {
    while let Some((i, _)) = events.next_user_message(index + 1) {
        let text = &events.texts[i];
        if !is_bare_interrupt_marker(text) && !structural.is_match(text) {
            return Some(text.clone());
        }
        index = i;
    }
    None
}

/// first_followup (mining/signals.py first_followup): the first following non-bare
/// user message.
fn first_followup(events: &Events<'_>, mut index: usize) -> Option<String> {
    while let Some((i, _)) = events.next_user_message(index + 1) {
        index = i;
        if !is_bare_interrupt_marker(&events.texts[index]) {
            return Some(events.texts[index].clone());
        }
    }
    None
}

/// marker_correction (mining/signals.py marker_correction): a real correction
/// weak(bare_marker), else a followup noise(structural_only), else None.
fn marker_correction(events: &Events<'_>, index: usize, structural: &Regex) -> Option<ScoredText> {
    if let Some(correction) = correction_text(events, index, structural) {
        return Some(ScoredText {
            text: correction,
            signal: weak("bare_marker"),
        });
    }
    first_followup(events, index).map(|followup| ScoredText {
        text: followup,
        signal: noise("structural_only"),
    })
}

/// denial_correction (mining/signals.py denial_correction): the embedded
/// instruction calibrated, else the marker-correction fallback.
fn denial_correction(
    events: &Events<'_>,
    index: usize,
    embedded: Option<String>,
    spec: &CompiledMiningSpec,
    structural: &Regex,
) -> Option<ScoredText> {
    match embedded {
        Some(text) if !text.is_empty() => {
            let signal = calibrated(&spec.calibrated, &text, "embedded_text");
            Some(ScoredText { text, signal })
        }
        _ => marker_correction(events, index, structural),
    }
}

/// The denial tool-result blocks of a user event (mining/signals.py denial_results via
/// facts.py is_denial): blocks the parse layer marked ``user-rejected`` — a human
/// rejection, never a permission-rule hook/guard block.
fn denial_results(user: &UserEntry) -> impl Iterator<Item = &ToolResultBlock> {
    user.tool_results()
        .filter(|b| b.denial_kind.as_deref() == Some(DENIAL_KIND_USER_REJECTED))
}

#[allow(clippy::too_many_arguments)]
fn build_signal_dict<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    kind: &str,
    detector: &str,
    meta: &EntryMeta,
    event_index: usize,
    text: &str,
    trigger_index: Option<i64>,
    sig: &CandidateSig,
    lower_bound: Option<i64>,
    evidence_bytes: usize,
    evidence: impl FnOnce() -> PyResult<Bound<'py, PyDict>>,
) -> PyResult<Bound<'py, PyDict>> {
    let strings = kind.len()
        + detector.len()
        + meta.session_id.len()
        + meta.uuid.len()
        + text.len()
        + meta.version.as_ref().map_or(0, String::len)
        + sig.reasons.iter().map(String::len).sum::<usize>()
        + evidence_bytes;
    events.reserve(4096usize.saturating_add(strings.saturating_mul(6)))?;
    let d = PyDict::new(py);
    d.set_item("kind", kind)?;
    d.set_item("detector", detector)?;
    d.set_item("session_id", &meta.session_id)?;
    d.set_item("event_index", event_index as i64)?;
    d.set_item("event_uuid", &meta.uuid)?;
    d.set_item("occurred_at", occurred_at_iso(py, meta.timestamp)?)?;
    d.set_item("text", text)?;
    d.set_item("cc_version", meta.version.as_deref())?;
    d.set_item("trigger_index", trigger_index)?;
    let signal = PyDict::new(py);
    signal.set_item("confidence", sig.confidence)?;
    signal.set_item("reasons", sig.reasons.clone())?;
    signal.set_item("durable", sig.durable)?;
    d.set_item("signal", signal)?;
    d.set_item("lower_bound", lower_bound)?;
    d.set_item("evidence", evidence()?)?;
    Ok(d)
}

/// occurred_at parity (R1): reproduce ``datetime.isoformat()`` byte-for-byte by
/// building a Python ``datetime`` from the chrono value and calling
/// ``.isoformat()`` — the identical oracle to ``signal_to_dict``.
fn occurred_at_iso<'py>(py: Python<'py>, dt: DateTime<FixedOffset>) -> PyResult<Bound<'py, PyAny>> {
    dt.into_pyobject(py)?.call_method0("isoformat")
}

/// Compiled structural-noise regex shared by the marker-correction paths
/// (mining/signals.py structural_re): the user-message spec's NoiseIfStructural
/// stage's groups when present, else the case-folded anchored interrupt marker
/// (filterspec.py INTERRUPT_MARKER_RE) — both backends derive it from the spec.
fn structural_re(spec: &CompiledMiningSpec) -> Regex {
    spec.user_message
        .stages
        .iter()
        .find_map(|stage| match stage {
            ConfStage::NoiseIfStructural { groups, .. } => Some(groups.clone()),
            _ => None,
        })
        .unwrap_or_else(|| INTERRUPT_MARKER_RE.clone())
}

fn iter_user_message<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        let text = &events.texts[index];
        if text.trim().is_empty() || is_bare_interrupt_marker(text) {
            continue;
        }
        let trigger = events.nearest_assistant_index(index);
        let sig = score_user_message(&spec.user_message, text, index as i64, trigger);
        out.push(build_signal_dict(
            py,
            events,
            TRANSCRIPT_MESSAGE,
            DETECTOR_TRANSCRIPT_MESSAGE,
            &user.meta,
            index,
            text,
            trigger,
            &sig,
            None,
            0,
            || Ok(PyDict::new(py)),
        )?);
    }
    Ok(())
}

fn iter_plan_rejection<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &HashMap<&str, &ToolUseBlock>,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for result in denial_results(user) {
            let Some(use_block) = uses.get(result.tool_use_id.as_str()) else {
                continue;
            };
            if !matches_names(&use_block.name, &spec.plan_tools) {
                continue;
            }
            let Some(text) = embedded_user_text(&result.content) else {
                continue;
            };
            let trigger = events.nearest_assistant_index(index);
            let sig = calibrated(&spec.calibrated, &text, "embedded_text");
            out.push(build_signal_dict(
                py,
                events,
                PLAN_REVIEW,
                DETECTOR_EXIT_PLAN_REJECTION,
                &user.meta,
                index,
                &text,
                trigger,
                &sig,
                None,
                0,
                || Ok(PyDict::new(py)),
            )?);
        }
    }
    Ok(())
}

fn iter_plan_reentry<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::Mode(mode) = entry else { continue };
        if mode.value != "plan" {
            continue;
        }
        let Some((user_index, user)) = events.next_user_message(index) else {
            continue;
        };
        let uuid = user.meta.uuid.as_str();
        if seen.contains(uuid) || is_bare_interrupt_marker(&events.texts[user_index]) {
            continue;
        }
        let Some(edit) = events.last_edit_index(user_index, spec) else {
            continue;
        };
        seen.insert(uuid);
        let text = &events.texts[user_index];
        let trigger = events.nearest_assistant_index(user_index);
        let sig = calibrated(&spec.calibrated, text, "reentry_after_edit");
        out.push(build_signal_dict(
            py,
            events,
            PLAN_REVIEW,
            DETECTOR_PLAN_REENTRY,
            &user.meta,
            user_index,
            text,
            trigger,
            &sig,
            Some(edit),
            0,
            || Ok(PyDict::new(py)),
        )?);
    }
    Ok(())
}

fn iter_tool_denial<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &HashMap<&str, &ToolUseBlock>,
    structural: &Regex,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for block in denial_results(user) {
            let paired = uses.get(block.tool_use_id.as_str()).copied();
            if paired.is_some_and(|use_block| {
                matches_names(&use_block.name, &spec.denial_excluded_tools)
            }) {
                continue;
            }
            let embedded = embedded_user_text(&block.content);
            let Some(scored) = denial_correction(events, index, embedded, spec, structural) else {
                continue;
            };
            let trigger = events.nearest_assistant_index(index);
            let evidence_bytes = paired.map_or(0, |tool| {
                tool.name.len() + tool.file_path.as_ref().map_or(0, String::len)
            });
            out.push(build_signal_dict(
                py,
                events,
                INTERRUPT_REJECTION,
                DETECTOR_DENIAL,
                &user.meta,
                index,
                &scored.text,
                trigger,
                &scored.signal,
                None,
                evidence_bytes,
                || {
                    let evidence = PyDict::new(py);
                    if let Some(use_block) = paired {
                        evidence.set_item("tool", &use_block.name)?;
                        evidence.set_item("file_path", use_block.file_path.as_deref())?;
                    }
                    Ok(evidence)
                },
            )?);
        }
    }
    Ok(())
}

fn iter_interrupt<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    structural: &Regex,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        if !marker_in(user) {
            continue;
        }
        let Some(scored) = marker_correction(events, index, structural) else {
            continue;
        };
        let trigger = events.nearest_assistant_index(index);
        out.push(build_signal_dict(
            py,
            events,
            INTERRUPT_REJECTION,
            DETECTOR_INTERRUPT,
            &user.meta,
            index,
            &scored.text,
            trigger,
            &scored.signal,
            None,
            0,
            || Ok(PyDict::new(py)),
        )?);
    }
    Ok(())
}

struct ReviewComment {
    file: Option<String>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    comment: String,
}

struct ScanText<'a> {
    text: &'a str,
    provenance: &'static str,
    trigger_index: Option<i64>,
}

/// classify_provenance (mining/spec.py classify_provenance): typed for absent tool,
/// surfaced for a non-subagent main-chain tool (per tools.py matches_names), else claude.
fn classify_provenance(
    subagent_tools: &HashSet<String>,
    tool_name: Option<&str>,
    is_sidechain: bool,
) -> &'static str {
    match (tool_name, is_sidechain) {
        (None, _) => "typed",
        (Some(name), false) if !matches_names(name, subagent_tools) => "surfaced",
        _ => "claude",
    }
}

/// review_scan_texts (mining/signals.py review_scan_texts): the typed user text
/// plus each surfaced or claude tool-result, gated by the surfaces set.
fn review_scan_texts<'a>(
    events: &'a Events<'a>,
    user: &'a UserEntry,
    index: usize,
    spec: &'a CompiledMiningSpec,
    uses: &'a HashMap<&'a str, &'a ToolUseBlock>,
) -> impl Iterator<Item = ScanText<'a>> + 'a {
    let surfaces = &spec.review.surfaces;
    let text = events.texts[index].as_str();
    let typed = (surfaces.contains("typed") && !text.trim().is_empty()).then(|| ScanText {
        text,
        provenance: "typed",
        trigger_index: events.nearest_assistant_index(index),
    });
    typed
        .into_iter()
        .chain(user.tool_results().filter_map(move |block| {
            let tool_name = uses
                .get(block.tool_use_id.as_str())
                .map(|tool| tool.name.as_str());
            let provenance =
                classify_provenance(&spec.subagent_tools, tool_name, user.meta.is_sidechain);
            (provenance != "typed" && surfaces.contains(provenance)).then_some(ScanText {
                text: &block.content,
                provenance,
                trigger_index: None,
            })
        }))
}

/// first (mining/formats.py first): the first present, non-null alias value.
fn first<'a>(obj: &'a Value, keys: &[String]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| field(obj, key))
        .find(|value| !value.is_null())
}

/// line_bounds (mining/formats.py line_bounds): int -> (n,n); "a-b" ->
/// first-partition split; all-digit string -> (n,n); else (None,None). A malformed
/// range parse raises, matching Python's propagated int() error.
fn line_bounds(value: Option<&Value>) -> Result<(Option<i64>, Option<i64>), String> {
    match value {
        Some(v) if v.is_i64() || v.is_u64() => {
            let n = v.as_i64().ok_or("line value out of i64 range")?;
            Ok((Some(n), Some(n)))
        }
        Some(v) => match v.as_str() {
            Some(s) if s.contains('-') => {
                let (start, end) = s.split_once('-').expect("contains '-'");
                let start = start
                    .trim()
                    .parse::<i64>()
                    .map_err(|e| format!("invalid line {start:?}: {e}"))?;
                let end = end
                    .trim()
                    .parse::<i64>()
                    .map_err(|e| format!("invalid line {end:?}: {e}"))?;
                Ok((Some(start), Some(end)))
            }
            Some(s) if !s.trim().is_empty() && s.trim().chars().all(|c| c.is_ascii_digit()) => {
                let n = s
                    .trim()
                    .parse::<i64>()
                    .map_err(|e| format!("invalid line {s:?}: {e}"))?;
                Ok((Some(n), Some(n)))
            }
            _ => Ok((None, None)),
        },
        None => Ok((None, None)),
    }
}

/// str(value) for a JSON scalar joined into a review comment, mirroring Python
/// ``str(part)``: strings verbatim, ints/floats/bools rendered Python-style.
fn json_str(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(b) = value.as_bool() {
        return if b {
            "True".to_string()
        } else {
            "False".to_string()
        };
    }
    if let Some(i) = value.as_i64() {
        return i.to_string();
    }
    if let Some(u) = value.as_u64() {
        return u.to_string();
    }
    if let Some(f) = value.as_f64() {
        return f.to_string();
    }
    sonic_rs::to_string(value).unwrap_or_default()
}

/// review_comment (mining/formats.py review_comment): builds a comment from a
/// finding object's aliased fields. The comment joins the comment value and any fix
/// value with " ".
fn review_comment(obj: &Value, fmt: &CompiledStructuredFormat) -> Result<ReviewComment, String> {
    let (line_start, line_end) = line_bounds(first(obj, &fmt.line_keys))?;
    let file = first(obj, &fmt.file_keys).map(json_str);
    let comment = [first(obj, &fmt.comment_keys), first(obj, &fmt.fix_keys)]
        .into_iter()
        .flatten()
        .map(json_str)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(ReviewComment {
        file,
        line_start,
        line_end,
        comment,
    })
}

/// findings (mining/formats.py findings): list -> items; dict -> the first
/// finding-array alias, else recurse into "result", else every confirmed*-prefixed
/// list.
fn findings<'a>(payload: &'a Value, keys: &[String], acc: &mut Vec<&'a Value>) {
    if let Some(items) = payload.as_array() {
        acc.extend(items.iter());
        return;
    }
    if !payload.is_object() {
        return;
    }
    if let Some(nested) = first(payload, keys) {
        if let Some(items) = nested.as_array() {
            acc.extend(items.iter());
            return;
        }
    }
    if let Some(result) = field(payload, "result").filter(|r| r.is_object()) {
        findings(result, keys, acc);
        return;
    }
    for (key, value) in payload.as_object().into_iter().flatten() {
        if key.starts_with("confirmed") {
            if let Some(items) = value.as_array() {
                acc.extend(items.iter());
            }
        }
    }
}

fn visit_review_comments<'py>(
    py: Python<'py>,
    spec: &CompiledMiningSpec,
    text: &str,
    events: &Events<'_>,
    mut emit: impl FnMut(&str, ReviewComment) -> PyResult<()>,
) -> PyResult<()> {
    for fmt in &spec.review.regex_formats {
        for caps in fmt.regex.captures_iter(text) {
            let bytes = fmt
                .comment_groups
                .iter()
                .filter_map(|&index| caps.get(index))
                .map(|value| value.as_str().len())
                .sum::<usize>()
                .saturating_add(
                    fmt.file_group
                        .and_then(|index| caps.get(index))
                        .map_or(0, |value| value.as_str().len()),
                )
                .saturating_add(fmt.join.len().saturating_mul(fmt.comment_groups.len()))
                .saturating_add(fmt.name.len());
            events.preflight(4096usize.saturating_add(bytes.saturating_mul(6)))?;
            let group = |index: Option<usize>| {
                index
                    .and_then(|index| caps.get(index))
                    .map(|value| value.as_str().to_string())
            };
            let int_group = |index: Option<usize>| {
                index
                    .and_then(|index| caps.get(index))
                    .and_then(|value| value.as_str().trim().parse::<i64>().ok())
            };
            emit(
                &fmt.name,
                ReviewComment {
                    file: group(fmt.file_group),
                    line_start: int_group(fmt.line_start_group),
                    line_end: int_group(fmt.line_end_group),
                    comment: fmt
                        .comment_groups
                        .iter()
                        .filter_map(|&index| caps.get(index))
                        .map(|value| value.as_str().trim())
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(&fmt.join),
                },
            )?;
        }
    }
    for fmt in &spec.review.callable_formats {
        events.preflight(0)?;
        if fmt
            .pattern
            .bind(py)
            .call_method1("search", (text,))?
            .is_none()
        {
            continue;
        }
        let extracted = fmt.extract.bind(py).call1((text,))?;
        if events.budget.is_some() && extracted.cast::<PyIterator>().is_err() {
            return Err(crate::snapshots::error(SnapshotError::new(
                Status::Incomplete,
                format!(
                    "bounded mining callback {} must return an iterator",
                    fmt.name
                ),
            )));
        }
        let mut iterator = extracted.try_iter()?;
        loop {
            events.preflight(0)?;
            let Some(item) = iterator.next() else { break };
            let item = item?;
            let comment = item.getattr("comment")?;
            let file = item.getattr("file")?;
            let bytes = comment
                .cast::<PyString>()?
                .to_str()?
                .len()
                .saturating_add(if file.is_none() {
                    0
                } else {
                    file.cast::<PyString>()?.to_str()?.len()
                })
                .saturating_add(fmt.name.len());
            events.preflight(4096usize.saturating_add(bytes.saturating_mul(6)))?;
            emit(
                &fmt.name,
                ReviewComment {
                    file: file.extract()?,
                    line_start: item.getattr("line_start")?.extract()?,
                    line_end: item.getattr("line_end")?.extract()?,
                    comment: comment.extract()?,
                },
            )?;
        }
    }
    if !spec.review.structured_formats.is_empty() {
        events.check()?;
        if let Ok(payload) = sonic_rs::from_str::<Value>(text) {
            for fmt in &spec.review.structured_formats {
                let mut found = Vec::new();
                findings(&payload, &fmt.finding_keys, &mut found);
                for obj in found
                    .into_iter()
                    .filter(|obj| obj.is_object())
                    .filter(|obj| first(obj, &fmt.comment_keys).is_some())
                {
                    let bytes = value_charge(obj)
                        .opaque_dom_accounted_bytes
                        .saturating_add(fmt.name.len());
                    events.preflight(4096usize.saturating_add(bytes.saturating_mul(6)))?;
                    emit(
                        &fmt.name,
                        review_comment(obj, fmt).map_err(PyValueError::new_err)?,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn iter_review_comment<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &HashMap<&str, &ToolUseBlock>,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for scan in review_scan_texts(events, user, index, spec, uses) {
            visit_review_comments(py, spec, scan.text, events, |fmt_name, comment| {
                let sig = calibrated(&spec.calibrated, &comment.comment, "format_match");
                let evidence_bytes = fmt_name.len()
                    + comment.file.as_ref().map_or(0, String::len)
                    + scan.provenance.len();
                out.push(build_signal_dict(
                    py,
                    events,
                    REVIEW_COMMENT,
                    DETECTOR_REVIEW_COMMENT,
                    &user.meta,
                    index,
                    &comment.comment,
                    scan.trigger_index,
                    &sig,
                    None,
                    evidence_bytes,
                    || {
                        let evidence = PyDict::new(py);
                        evidence.set_item("format", fmt_name)?;
                        evidence.set_item("file", &comment.file)?;
                        evidence.set_item("line_start", comment.line_start)?;
                        evidence.set_item("line_end", comment.line_end)?;
                        evidence.set_item("provenance", scan.provenance)?;
                        Ok(evidence)
                    },
                )?);
                Ok(())
            })?;
        }
    }
    Ok(())
}

struct AnsweredPair<'a> {
    question: &'a Question,
    answer: Option<&'a str>,
    preview: Option<&'a str>,
    notes: Option<&'a str>,
}

/// split_answer_segment (signals.py split_answer_segment): the answer head plus
/// the optional preview and notes parts, split on the first marker occurrence in
/// render order (answer, preview, notes).
fn split_answer_segment(segment: &str) -> (&str, Option<&str>, Option<&str>) {
    if let Some(at) = segment.find(ANSWER_PREVIEW_SEP) {
        let rest = &segment[at + ANSWER_PREVIEW_SEP.len()..];
        return match rest.find(ANSWER_NOTES_SEP) {
            Some(notes_at) => (
                &segment[..at],
                Some(&rest[..notes_at]),
                Some(&rest[notes_at + ANSWER_NOTES_SEP.len()..]),
            ),
            None => (&segment[..at], Some(rest), None),
        };
    }
    match segment.find(ANSWER_NOTES_SEP) {
        Some(at) => (
            &segment[..at],
            None,
            Some(&segment[at + ANSWER_NOTES_SEP.len()..]),
        ),
        None => (segment, None, None),
    }
}

/// find_anchor (signals.py find_anchor): the next anchor at or after `pos` that
/// renders at the body start or right after the ", " pair join, skipping the same
/// literal embedded inside an earlier freeform answer.
fn find_anchor(body: &str, anchor: &str, pos: usize) -> Option<usize> {
    let mut from = pos;
    while let Some(i) = body[from..].find(anchor) {
        let at = from + i;
        if at == 0 || body[..at].ends_with(", ") {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// answered_pairs (signals.py answered_pairs): anchors each question in order on
/// '"<question>"=', slices each pair's segment to the next found anchor, strips
/// the exact ', ' pair join on middle segments and the answer's exact closing '"',
/// and skips questions whose anchor never rendered.
fn answered_pairs<'a>(body: &'a str, questions: &'a [Question]) -> Vec<AnsweredPair<'a>> {
    let mut found: Vec<(&Question, usize, usize)> = Vec::new();
    let mut pos = 0usize;
    for question in questions {
        let anchor = format!("\"{}\"=", question.question);
        let Some(at) = find_anchor(body, &anchor, pos) else {
            continue;
        };
        found.push((question, at, at + anchor.len()));
        pos = at + anchor.len();
    }
    let mut pairs = Vec::new();
    for (i, &(question, _, value_at)) in found.iter().enumerate() {
        let end = found.get(i + 1).map_or(body.len(), |&(_, at, _)| at);
        let mut segment = &body[value_at..end];
        if i + 1 < found.len() {
            segment = segment.strip_suffix(", ").unwrap_or(segment);
        }
        let (head, preview, notes) = split_answer_segment(segment);
        let answer = if let Some(quoted) = head.strip_prefix('"') {
            Some(quoted.strip_suffix('"').unwrap_or(quoted))
        } else if head.starts_with(NO_OPTION_SELECTED) {
            None
        } else {
            continue;
        };
        pairs.push(AnsweredPair {
            question,
            answer,
            preview,
            notes,
        });
    }
    pairs
}

/// structured_answered_pairs (signals.py structured_answered_pairs): one pair per
/// question (the caller passes the payload's own `questions`, falling back to the
/// tool-use input's), answer from `answers` (None ⇒ the banner's `(no option
/// selected)`), preview/notes from `annotations`.
fn structured_answered_pairs<'a>(
    payload: &'a Value,
    questions: &'a [Question],
) -> Vec<AnsweredPair<'a>> {
    let answers = field(payload, "answers");
    let annotations = field(payload, "annotations");
    questions
        .iter()
        .map(|question| {
            let annotation = annotations.and_then(|a| field(a, &question.question));
            AnsweredPair {
                question,
                answer: answers.and_then(|a| field_str(a, &question.question)),
                preview: annotation.and_then(|a| field_str(a, "preview")),
                notes: annotation.and_then(|a| field_str(a, "notes")),
            }
        })
        .collect()
}

/// banner_answered_pairs (signals.py banner_answered_pairs): old-transcript fallback —
/// re-parse the prefix/trailer-wrapped answered banner against the tool-use questions.
fn banner_answered_pairs<'a>(
    block: &'a ToolResultBlock,
    use_block: &'a ToolUseBlock,
) -> Vec<AnsweredPair<'a>> {
    let content = &block.content;
    if block.is_error
        || !content.starts_with(ANSWERED_PREFIX)
        || !content.ends_with(ANSWERED_TRAILER)
        || content.len() < ANSWERED_PREFIX.len() + ANSWERED_TRAILER.len()
    {
        return Vec::new();
    }
    let Some(questions) = use_block.questions.as_deref() else {
        return Vec::new();
    };
    answered_pairs(
        &content[ANSWERED_PREFIX.len()..content.len() - ANSWERED_TRAILER.len()],
        questions,
    )
}

/// join_labels (signals.py join_labels): the multiSelect resolution — split the
/// answer on ', ' and greedily re-join consecutive parts until each accumulation
/// equals the next unused label in option order; succeeds only when every part is
/// consumed.
fn join_labels(answer: &str, labels: &[String]) -> Option<Vec<String>> {
    let mut picked: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut acc: Option<String> = None;
    for part in answer.split(", ") {
        let joined = match acc {
            None => part.to_string(),
            Some(prior) => format!("{prior}, {part}"),
        };
        match (start..labels.len()).find(|&i| labels[i] == joined) {
            Some(at) => {
                picked.push(labels[at].clone());
                start = at + 1;
                acc = None;
            }
            None => acc = Some(joined),
        }
    }
    (!picked.is_empty() && acc.is_none()).then_some(picked)
}

/// ordinal_label (signals.py ordinal_label): a leading ASCII-digit run in
/// 1..=len(options), followed by nothing or one of ',' '.' ' ' ')', resolves to
/// that option's label; the bool is whether the ordinal stood alone.
fn ordinal_label(answer: &str, labels: &[String]) -> Option<(String, bool)> {
    let rest = answer.trim_start_matches(|c: char| c.is_ascii_digit());
    let digits = &answer[..answer.len() - rest.len()];
    let n: usize = digits.parse().ok()?;
    if !(1..=labels.len()).contains(&n)
        || (!rest.is_empty() && !rest.starts_with([',', '.', ' ', ')']))
    {
        return None;
    }
    Some((labels[n - 1].clone(), rest.is_empty()))
}

/// resolve_pick (signals.py resolve_pick): verbatim/multiSelect join → a full
/// pick; leading ordinal → resolved label with option_pick only when bare; else
/// pure freeform.
fn resolve_pick(answer: Option<&str>, labels: &[String]) -> (Vec<String>, bool) {
    let Some(answer) = answer else {
        return (Vec::new(), false);
    };
    if let Some(joined) = join_labels(answer, labels) {
        return (joined, true);
    }
    match ordinal_label(answer, labels) {
        Some((label, bare)) => (vec![label], bare),
        None => (Vec::new(), false),
    }
}

/// iter_ask_user_question_signals (signals.py iter_ask_user_question_signals):
/// one signal per answered question/answer pair — pairs built structurally from the
/// `toolUseResult` payload when it carries `answers`, else banner-parsed — with the
/// pick resolution facts as evidence (absent preview/notes keys are omitted, never None).
fn iter_ask_user_question<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &HashMap<&str, &ToolUseBlock>,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for (index, entry) in events.entries.iter().enumerate() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for block in user.tool_results() {
            let Some(use_block) = uses.get(block.tool_use_id.as_str()) else {
                continue;
            };
            if use_block.name != "AskUserQuestion" {
                continue;
            }
            let structured = block
                .tool_use_result
                .as_ref()
                .filter(|payload| !block.is_error && field(payload, "answers").is_some());
            let structured_questions = structured.and_then(|payload| parse_questions(payload));
            let pairs = match structured {
                Some(payload) => structured_answered_pairs(
                    payload,
                    structured_questions
                        .as_deref()
                        .or(use_block.questions.as_deref())
                        .unwrap_or(&[]),
                ),
                None => banner_answered_pairs(block, use_block),
            };
            let trigger = events.nearest_assistant_index(index);
            for pair in pairs {
                let candidate_bytes = pair.question.question.len()
                    + pair.question.header.as_ref().map_or(0, String::len)
                    + pair.question.labels.iter().map(String::len).sum::<usize>()
                    + pair.answer.map_or(0, str::len)
                    + pair.preview.map_or(0, str::len)
                    + pair.notes.map_or(0, str::len);
                events.preflight(4096usize.saturating_add(candidate_bytes.saturating_mul(6)))?;
                let (picked, option_pick) = resolve_pick(pair.answer, &pair.question.labels);
                let recommended = picked.iter().any(|label| label.contains("(Recommended)"));
                let Some(text) = pair.notes.or(pair.answer) else {
                    continue;
                };
                let sig = if option_pick && pair.notes.is_none_or(str::is_empty) {
                    weak("option_pick")
                } else {
                    calibrated(&spec.calibrated, text, "freeform_answer")
                };
                let evidence_bytes = pair.question.question.len()
                    + pair.question.header.as_ref().map_or(0, String::len)
                    + picked.iter().map(String::len).sum::<usize>()
                    + pair.preview.map_or(0, str::len)
                    + pair.notes.map_or(0, str::len);
                out.push(build_signal_dict(
                    py,
                    events,
                    QUESTION_ANSWER,
                    DETECTOR_ASK_USER_QUESTION,
                    &user.meta,
                    index,
                    text,
                    trigger,
                    &sig,
                    None,
                    evidence_bytes,
                    || {
                        let evidence = PyDict::new(py);
                        evidence.set_item("question", &pair.question.question)?;
                        evidence.set_item("header", pair.question.header.as_deref())?;
                        evidence.set_item("multi_select", pair.question.multi_select)?;
                        evidence.set_item("option_pick", option_pick)?;
                        evidence.set_item("picked_labels", picked)?;
                        evidence.set_item("recommended_pick", recommended)?;
                        if let Some(preview) = pair.preview {
                            evidence.set_item("preview", preview)?;
                        }
                        if let Some(notes) = pair.notes {
                            evidence.set_item("notes", notes)?;
                        }
                        Ok(evidence)
                    },
                )?);
            }
        }
    }
    Ok(())
}

/// Runs the detector pipeline over already-parsed Python transcript events
/// (mining/engine.py mine).
pub fn mine_events<'py>(
    py: Python<'py>,
    events: &[Bound<'py, PyAny>],
    spec: &CompiledMiningSpec,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let entries = events
        .iter()
        .map(|event| view_entry(event, "mine"))
        .collect::<PyResult<Vec<_>>>()?;
    mine_events_impl(py, &Events::new(entries), spec)
}

pub fn mine_snapshot<'py>(
    py: Python<'py>,
    snapshot: &TranscriptSnapshot,
    spec: &CompiledMiningSpec,
    limits: WorkLimits,
    cancel: &Cancellation,
    usage: &mut MiningUsage,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    *usage = MiningUsage::default();
    if spec
        .review
        .callable_formats
        .iter()
        .any(|format| !format.bounded)
    {
        return Err(crate::snapshots::error(SnapshotError::new(
            Status::Incomplete,
            "mining callback is not registered as bounded",
        )));
    }
    let (entries, texts) = py
        .detach(|| {
            cancel.check(limits.deadline_unix_ms)?;
            for chunk in &snapshot.chunks {
                for charge in &chunk.entry_charges {
                    cancel.check(limits.deadline_unix_ms)?;
                    if usage.events == limits.max_events {
                        return Err(cc_transcript_core::snapshot::SnapshotError::new(
                            cc_transcript_core::snapshot::Status::EntryLimit,
                            "mining input event budget exceeded",
                        ));
                    }
                    let bytes = charge
                        .owned_capacity_bytes
                        .saturating_add(charge.opaque_dom_accounted_bytes)
                        .saturating_add(
                            size_of::<Entry>() + size_of::<&Entry>() + size_of::<String>(),
                        );
                    if bytes > limits.max_read_bytes.saturating_sub(usage.input_bytes) {
                        return Err(cc_transcript_core::snapshot::SnapshotError::new(
                            cc_transcript_core::snapshot::Status::SourceLimit,
                            "mining input byte budget exceeded",
                        ));
                    }
                    usage.events += 1;
                    usage.input_bytes += bytes;
                }
            }
            let mut entries = Vec::with_capacity(usage.events);
            let mut texts = Vec::with_capacity(usage.events);
            for entry in snapshot
                .chunks
                .iter()
                .flat_map(|chunk| chunk.entries.iter())
            {
                cancel.check(limits.deadline_unix_ms)?;
                entries.push(entry);
                texts.push(match entry {
                    Entry::User(user) => user.content.text(),
                    _ => String::new(),
                });
            }
            Ok((entries, texts))
        })
        .map_err(crate::snapshots::error)?;
    let budget = MiningBudget {
        limits,
        cancel: cancel.clone(),
        usage: Cell::new(*usage),
    };
    let events = Events {
        entries,
        texts,
        budget: Some(budget),
    };
    let result = mine_events_impl(py, &events, spec).and_then(|result| {
        events.check()?;
        Ok(result)
    });
    *usage = events.budget.as_ref().unwrap().usage.get();
    result
}

/// Borrows the shared `Entry` behind a native event view — the no-round-trip
/// handle mining and the filter bindings run on.
pub(crate) fn view_entry<'a>(event: &'a Bound<'_, PyAny>, caller: &str) -> PyResult<&'a Entry> {
    use crate::views::events::{
        AssistantEventView, AttachmentEventView, ModeEventView, OtherEventView, SystemEventView,
        UserEventView,
    };
    if let Ok(view) = event.cast::<UserEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<AssistantEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<SystemEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<ModeEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<OtherEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<AttachmentEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    Err(PyTypeError::new_err(format!(
        "{caller}() takes parsed transcript events, got {}",
        event.get_type().name()?
    )))
}

fn mine_events_impl<'py>(
    py: Python<'py>,
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let uses = tool_use_index(events.entries.iter().copied());
    let structural = structural_re(spec);
    let mut out: Vec<Bound<'py, PyDict>> = Vec::new();
    if spec.detectors.contains(DETECTOR_TRANSCRIPT_MESSAGE) {
        iter_user_message(py, events, spec, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_EXIT_PLAN_REJECTION) {
        iter_plan_rejection(py, events, spec, &uses, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_PLAN_REENTRY) {
        iter_plan_reentry(py, events, spec, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_DENIAL) {
        iter_tool_denial(py, events, spec, &uses, &structural, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_INTERRUPT) {
        iter_interrupt(py, events, &structural, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_REVIEW_COMMENT) {
        iter_review_comment(py, events, spec, &uses, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_ASK_USER_QUESTION) {
        iter_ask_user_question(py, events, spec, &uses, &mut out)?;
    }
    Ok(out)
}

/// The seeded negative-window draw over a transcript's completed turns
/// (mining/sampling.py sample_windows candidate build + draw): returns each drawn
/// anchor as `(turn_index, session_id, event_uuid)`, sorted by turn index.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (data, n, exclude, exclusion_radius, seed))]
pub(crate) fn mining_sample_refs(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] data: &[u8],
    n: usize,
    exclude: Vec<(String, String)>,
    exclusion_radius: usize,
    seed: String,
) -> PyResult<Vec<(usize, String, String)>> {
    py.detach(|| {
        let entries = parse_bytes(data, |_| true).map_err(parse_err)?;
        // Mirror the materialization drop: a year-0000 timestamp has no Python
        // datetime, so parse_events_from_bytes never yields it (context.rs does the same).
        let capped: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| entry.meta().is_none_or(|meta| meta.timestamp.year() >= 1))
            .collect();
        let session_id = capped
            .iter()
            .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()))
            .ok_or_else(|| PyValueError::new_err("transcript has no session id"))?;
        let lift = lift_session(&session_id, &capped);
        Ok(sample_refs(&lift, n, &exclude, exclusion_radius, &seed))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_bounds_variants() {
        assert_eq!(
            line_bounds(Some(&sonic_rs::from_str("7").unwrap())).unwrap(),
            (Some(7), Some(7))
        );
        assert_eq!(
            line_bounds(Some(&sonic_rs::from_str("\"24-51\"").unwrap())).unwrap(),
            (Some(24), Some(51))
        );
        assert_eq!(
            line_bounds(Some(&sonic_rs::from_str("\"007\"").unwrap())).unwrap(),
            (Some(7), Some(7))
        );
        assert_eq!(
            line_bounds(Some(&sonic_rs::from_str("\"x\"").unwrap())).unwrap(),
            (None, None)
        );
        assert_eq!(line_bounds(None).unwrap(), (None, None));
    }

    #[test]
    fn matches_names_exact_or_mcp_suffix() {
        let names: HashSet<String> =
            ["ExitPlanMode".to_string(), "ExitSpecMode".to_string()].into();
        assert!(matches_names("ExitSpecMode", &names));
        assert!(matches_names("mcp__conductor__ExitPlanMode", &names));
        assert!(!matches_names("mcp__ExitPlanMode", &names));
        assert!(!matches_names("AskUserQuestion", &names));

        let edit_names: HashSet<String> = ["Edit".to_string(), "syn_span_edit".to_string()].into();
        assert!(matches_names("mcp__cc-context__syn_span_edit", &edit_names));
        assert!(!matches_names(
            "mcp__cc-context__syn_span_read",
            &edit_names
        ));
    }

    #[test]
    fn bump_clamps_to_unit_interval() {
        let high = bump(
            CandidateSig {
                confidence: 0.9,
                reasons: vec![],
                durable: true,
            },
            0.25,
            "r",
        );
        assert_eq!(high.confidence, 1.0);
        let low = bump(
            CandidateSig {
                confidence: 0.1,
                reasons: vec![],
                durable: true,
            },
            -0.25,
            "r",
        );
        assert_eq!(low.confidence, 0.0);
    }

    #[test]
    fn split_answer_segment_variants() {
        assert_eq!(split_answer_segment("\"A\""), ("\"A\"", None, None));
        assert_eq!(
            split_answer_segment("\"A\" selected preview:\nraw text"),
            ("\"A\"", Some("raw text"), None)
        );
        assert_eq!(
            split_answer_segment("\"A\" selected preview:\nraw notes: typed"),
            ("\"A\"", Some("raw"), Some("typed"))
        );
        assert_eq!(
            split_answer_segment("(no option selected) notes: typed"),
            ("(no option selected)", None, Some("typed"))
        );
    }

    fn question(text: &str) -> Question {
        Question {
            question: text.to_string(),
            header: None,
            multi_select: false,
            labels: Vec::new(),
        }
    }

    #[test]
    fn answered_pairs_slices_segments_and_skips_omitted() {
        let questions = [question("Q1"), question("Q2"), question("Q3")];
        let body = "\"Q1\"=\"first, with comma\", \"Q3\"=\"he said \"hi\"\"";
        let pairs = answered_pairs(body, &questions);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].question.question, "Q1");
        assert_eq!(pairs[0].answer, Some("first, with comma"));
        assert_eq!(pairs[1].question.question, "Q3");
        assert_eq!(pairs[1].answer, Some("he said \"hi\""));
    }

    #[test]
    fn join_labels_greedy_over_comma_containing_label() {
        let labels = vec![
            "BeforeEdit / AfterEdit".to_string(),
            "One, Two".to_string(),
            "Three".to_string(),
        ];
        assert_eq!(
            join_labels("One, Two, Three", &labels),
            Some(vec!["One, Two".to_string(), "Three".to_string()])
        );
        assert_eq!(join_labels("One, Two, Four", &labels), None);
        assert_eq!(
            join_labels("Three", &labels),
            Some(vec!["Three".to_string()])
        );
    }

    #[test]
    fn ordinal_label_bounds_and_separators() {
        let labels = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert_eq!(
            ordinal_label("3, and more", &labels),
            Some(("C".to_string(), false))
        );
        assert_eq!(ordinal_label("2", &labels), Some(("B".to_string(), true)));
        assert_eq!(ordinal_label("2026 timeline works", &labels), None);
        assert_eq!(ordinal_label("1x", &labels), None);
        assert_eq!(ordinal_label("freeform", &labels), None);
    }
}
