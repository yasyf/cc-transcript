use std::collections::HashMap;
use std::collections::HashSet;

use chrono::{DateTime, FixedOffset};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use regex::Regex;
use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

use crate::event::{parse_timestamp, require_str, truthy_str};
use crate::filter::{compile_group_array, Kind};
use crate::value::{block_type, content_text, field, field_bool, field_str};

// Raw CC-injected protocol strings (filterspec.py:74-76) and the interrupt marker
// prefix (event.rs:17 / INTERRUPT_MARKER_GROUPS).
const DENIAL_PREFIX: &str =
    "The user doesn't want to proceed with this tool use. The tool use was rejected";
const USER_SAID_MARKER: &str = "To tell you how to proceed, the user said:\n";
const USER_SAID_TRAILER: &str = "Note: The user's next message";
const INTERRUPT_MARKER: &str = r"\[Request interrupted by user";

// Source kinds (sourcekind.py:14-17).
const TRANSCRIPT_MESSAGE: &str = "transcript_message";
const PLAN_REVIEW: &str = "plan_review";
const INTERRUPT_REJECTION: &str = "interrupt_rejection";
const REVIEW_COMMENT: &str = "review_comment";

// Detector ids (spec.py:53-58).
const DETECTOR_TRANSCRIPT_MESSAGE: &str = "transcript_message";
const DETECTOR_EXIT_PLAN_REJECTION: &str = "exit_plan_rejection";
const DETECTOR_PLAN_REENTRY: &str = "plan_reentry";
const DETECTOR_DENIAL: &str = "denial";
const DETECTOR_INTERRUPT: &str = "interrupt";
const DETECTOR_REVIEW_COMMENT: &str = "review_comment";

// Confidence bands (confidence.py:15-19); the hardcoded marker-correction seeds.
const NONE: f64 = 0.0;
const LOW: f64 = 0.25;

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

struct CompiledReviewSpec {
    surfaces: HashSet<String>,
    regex_formats: Vec<CompiledRegexFormat>,
    structured_formats: Vec<CompiledStructuredFormat>,
}

pub struct CompiledMiningSpec {
    detectors: HashSet<String>,
    reentry_lookback: usize,
    edit_tools: HashSet<String>,
    subagent_tools: HashSet<String>,
    user_message: CompiledConfSpec,
    calibrated: CompiledConfSpec,
    review: CompiledReviewSpec,
}

// ── spec compilation ────────────────────────────────────────────────────────

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
/// ``compile_review_format`` (spec.py:333): the ``(?:p)|...`` join from
/// ``compile_group_array`` plus a ``(?m)`` prefix when ``multiline`` is set.
fn compile_review_regex(fmt: &Value) -> Result<Regex, String> {
    let groups = group_array(fmt, "groups")?;
    let joined = groups
        .iter()
        .filter_map(|group| 1usize.value_index_into(group).and_then(JsonValueTrait::as_str))
        .map(|pattern| format!("(?:{pattern})"))
        .collect::<Vec<_>>()
        .join("|");
    let multiline = if field_bool(fmt, "multiline") { "(?m)" } else { "" };
    let ignore_case = if field_bool(fmt, "ignore_case") { "(?i)" } else { "" };
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

// ── confidence fold (run_confidence, spec.py:272) ────────────────────────────

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn bump(mut sig: CandidateSig, delta: f64, reason: &str) -> CandidateSig {
    sig.confidence = (sig.confidence + delta).clamp(0.0, 1.0);
    sig.reasons.push(reason.to_string());
    sig
}

fn apply_conf_stage(stage: &ConfStage, text: &str, index: i64, trigger: Option<i64>, sig: CandidateSig) -> CandidateSig {
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
        ConfStage::BumpIfSubstantive { groups, delta, min_words, reason }
            if word_count(text) > *min_words && !groups.is_match(text) =>
        {
            bump(sig, *delta, reason)
        }
        ConfStage::DemoteIfHedged { groups, delta, reason } if groups.is_match(text) => {
            bump(sig, *delta, reason)
        }
        ConfStage::DemoteIfShort { max_words, delta, reason } if word_count(text) <= *max_words => {
            bump(sig, *delta, reason)
        }
        ConfStage::BumpIfProximate { within, delta, reason }
            if trigger.is_some_and(|t| index - t <= *within) =>
        {
            bump(sig, *delta, reason)
        }
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
        if let ConfStage::NoiseIfStructural { groups, band, reason } = stage {
            if groups.is_match(text) {
                return CandidateSig {
                    confidence: *band,
                    reasons: vec![reason.clone()],
                    durable: base.durable,
                };
            }
        }
    }
    spec.stages
        .iter()
        .fold(base, |sig, stage| apply_conf_stage(stage, text, index, trigger, sig))
}

/// Scores ``text`` by folding ``calibrated`` over a ``firm(seed)`` base
/// (spec.py:312-314: MEDIUM band, one seed reason).
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

/// Scores a transcript user message (spec.py:317-319: NONE band, empty reasons,
/// durable=true seed).
fn score_user_message(spec: &CompiledConfSpec, text: &str, index: i64, trigger: Option<i64>) -> CandidateSig {
    run_confidence(
        spec,
        text,
        index,
        trigger,
        CandidateSig { confidence: NONE, reasons: vec![], durable: true },
    )
}

// ── event view & cross-event pass ────────────────────────────────────────────

fn event_kind(data: &Value) -> Kind {
    match field_str(data, "type") {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        Some("system") => Kind::System,
        Some("mode") | Some("permission-mode") => Kind::Mode,
        _ => Kind::Other,
    }
}

fn message_content(data: &Value) -> Option<&Value> {
    field(field(data, "message")?, "content")
}

/// The joined text of a user/assistant event, mirroring ``content_text``.
fn event_text(data: &Value, kind: Kind) -> String {
    match kind {
        Kind::User | Kind::Assistant => message_content(data).map(content_text).unwrap_or_default(),
        _ => String::new(),
    }
}

struct Events {
    lines: Vec<Value>,
    kinds: Vec<Kind>,
    texts: Vec<String>,
}

impl Events {
    fn parse(raw: &[u8]) -> Self {
        let lines: Vec<Value> = raw
            .split(|&b| b == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .filter_map(|line| sonic_rs::from_slice::<Value>(line).ok())
            .collect();
        let kinds = lines.iter().map(event_kind).collect::<Vec<_>>();
        let texts = lines
            .iter()
            .zip(&kinds)
            .map(|(line, &kind)| event_text(line, kind))
            .collect();
        Events { lines, kinds, texts }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    /// Nearest preceding assistant index (signals.py:177).
    fn nearest_assistant_index(&self, index: usize) -> Option<i64> {
        (0..index).rev().find(|&i| self.kinds[i] == Kind::Assistant).map(|i| i as i64)
    }

    /// The first non-blank user message at or after ``index`` (signals.py:137).
    fn next_user_message(&self, index: usize) -> Option<usize> {
        (index..self.len()).find(|&i| self.kinds[i] == Kind::User && !self.texts[i].trim().is_empty())
    }

    /// The 40-event lookback for the most recent edit (signals.py:125): scans
    /// ``range(index-1, max(index-lookback, 0)-1, -1)`` for an assistant event
    /// using an edit tool.
    fn last_edit_index(&self, index: usize, spec: &CompiledMiningSpec) -> Option<i64> {
        let lo = index.saturating_sub(spec.reentry_lookback);
        (lo..index)
            .rev()
            .find(|&i| self.kinds[i] == Kind::Assistant && self.uses_edit_tool(i, &spec.edit_tools))
            .map(|i| i as i64)
    }

    fn uses_edit_tool(&self, index: usize, edit_tools: &HashSet<String>) -> bool {
        message_content(&self.lines[index])
            .and_then(JsonContainerTrait::as_array)
            .into_iter()
            .flatten()
            .any(|block| {
                block_type(block) == Some("tool_use")
                    && field_str(block, "name").is_some_and(|name| edit_tools.contains(name))
            })
    }
}

/// Builds the ``tool_use_id -> ToolUseBlock`` map (filterspec.py:383), keeping
/// last-write-wins on duplicate ids to match the Python dict comprehension.
fn tool_uses(events: &Events) -> HashMap<String, &Value> {
    let mut map = HashMap::new();
    for (line, &kind) in events.lines.iter().zip(&events.kinds) {
        if kind != Kind::Assistant {
            continue;
        }
        for block in message_content(line).and_then(JsonContainerTrait::as_array).into_iter().flatten() {
            if block_type(block) == Some("tool_use") {
                if let Some(id) = field_str(block, "id") {
                    map.insert(id.to_string(), block);
                }
            }
        }
    }
    map
}

// ── text-shape helpers (signals.py) ──────────────────────────────────────────

/// embedded_user_text (signals.py:119): the verbatim instruction wrapped between
/// the USER_SAID markers, or None.
fn embedded_user_text(content: &str) -> Option<String> {
    let start = content.find(USER_SAID_MARKER)?;
    let after = &content[start + USER_SAID_MARKER.len()..];
    Some(after.split(USER_SAID_TRAILER).next().unwrap_or(after).trim().to_string())
}

/// interrupt_marker (signals.py:153): the bracketed interrupt prefix at the head
/// of ``content`` (after lstrip), through the closing ``]`` when present, else the
/// matched marker prefix. ``INTERRUPT_MARKER_RE`` matches case-insensitively from
/// the start (``.match()``), so the leading-prefix test is case-folded too.
fn interrupt_marker(content: &str) -> Option<String> {
    static MARKER: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(&format!("(?i)^{INTERRUPT_MARKER}")).expect("interrupt regex"));
    let stripped = content.trim_start();
    let matched = MARKER.find(stripped)?;
    match stripped.find(']') {
        Some(end) => Some(stripped[..=end].to_string()),
        None => Some(matched.as_str().to_string()),
    }
}

/// is_bare_interrupt_marker (signals.py:161): the text is only the marker.
fn is_bare_interrupt_marker(text: &str) -> bool {
    match interrupt_marker(text) {
        None => false,
        Some(marker) => text.trim()[marker.trim().len()..].trim().is_empty(),
    }
}

/// marker_in (signals.py:165): the first interrupt marker found in a tool-result
/// block's content, or None.
fn marker_in(event: &Value) -> Option<String> {
    message_content(event)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter(|b| block_type(b) == Some("tool_result"))
        .find_map(|b| interrupt_marker(&result_content_text(b)?))
}

/// The flattened text of a tool-result block: the string content, or the joined
/// text blocks (event.rs flatten_result_content).
fn result_content_text(block: &Value) -> Option<String> {
    let content = field(block, "content")?;
    Some(match content.as_str() {
        Some(s) => s.to_string(),
        None => content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| block_type(b) == Some("text"))
            .filter_map(|b| field_str(b, "text"))
            .collect(),
    })
}

const STRUCTURAL_INTERRUPT_RE: &str = INTERRUPT_MARKER;

// ── confidence band literals for marker_correction (signals.py:198-203) ──────

fn weak(reason: &str) -> CandidateSig {
    CandidateSig { confidence: LOW, reasons: vec![reason.to_string()], durable: true }
}

fn noise(reason: &str) -> CandidateSig {
    CandidateSig { confidence: NONE, reasons: vec![reason.to_string()], durable: true }
}

struct ScoredText {
    text: String,
    signal: CandidateSig,
}

/// correction_text (signals.py:181): the first following non-bare, non-structural
/// user message — a forward loop that re-scans from the last consumed index.
fn correction_text(events: &Events, mut index: usize, structural: &Regex) -> Option<String> {
    while let Some(i) = events.next_user_message(index + 1) {
        let text = &events.texts[i];
        if !is_bare_interrupt_marker(text) && !structural.is_match(text) {
            return Some(text.clone());
        }
        index = i;
    }
    None
}

/// first_followup (signals.py:190): the first following non-bare user message.
fn first_followup(events: &Events, mut index: usize) -> Option<String> {
    while let Some(i) = events.next_user_message(index + 1) {
        index = i;
        if !is_bare_interrupt_marker(&events.texts[index]) {
            return Some(events.texts[index].clone());
        }
    }
    None
}

/// marker_correction (signals.py:198): a real correction weak(bare_marker), else
/// a followup noise(structural_only), else None.
fn marker_correction(events: &Events, index: usize, structural: &Regex) -> Option<ScoredText> {
    if let Some(correction) = correction_text(events, index, structural) {
        return Some(ScoredText { text: correction, signal: weak("bare_marker") });
    }
    first_followup(events, index).map(|followup| ScoredText { text: followup, signal: noise("structural_only") })
}

/// denial_correction (signals.py:206): the embedded instruction calibrated, else
/// the marker-correction fallback.
fn denial_correction(
    events: &Events,
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

/// The denial tool-result blocks of a user event (signals.py:109): error blocks
/// whose flattened content starts with the denial banner.
fn denial_results(event: &Value) -> Vec<&Value> {
    message_content(event)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter(|b| block_type(b) == Some("tool_result"))
        .filter(|b| field_bool(b, "is_error"))
        .filter(|b| result_content_text(b).is_some_and(|c| c.starts_with(DENIAL_PREFIX)))
        .collect()
}

// ── signal dict construction (signal_to_dict, spec.py:501) ───────────────────

#[allow(clippy::too_many_arguments)]
fn build_signal_dict<'py>(
    py: Python<'py>,
    kind: &str,
    detector: &str,
    meta: &Value,
    event_index: usize,
    text: &str,
    trigger_index: Option<i64>,
    sig: &CandidateSig,
    lower_bound: Option<i64>,
    evidence: Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("kind", kind)?;
    d.set_item("detector", detector)?;
    d.set_item("session_id", require_str(meta, "sessionId")?)?;
    d.set_item("event_index", event_index as i64)?;
    d.set_item("event_uuid", require_str(meta, "uuid")?)?;
    d.set_item("occurred_at", occurred_at_iso(py, require_str(meta, "timestamp")?)?)?;
    d.set_item("text", text)?;
    d.set_item("cc_version", truthy_str(meta, "version"))?;
    d.set_item("trigger_index", trigger_index)?;
    let signal = PyDict::new(py);
    signal.set_item("confidence", sig.confidence)?;
    signal.set_item("reasons", sig.reasons.clone())?;
    signal.set_item("durable", sig.durable)?;
    d.set_item("signal", signal)?;
    d.set_item("lower_bound", lower_bound)?;
    d.set_item("evidence", evidence)?;
    Ok(d)
}

/// occurred_at parity (R1): reproduce ``datetime.isoformat()`` byte-for-byte by
/// building a Python ``datetime`` from the chrono value and calling
/// ``.isoformat()`` — the identical oracle to ``signal_to_dict``.
fn occurred_at_iso<'py>(py: Python<'py>, raw: &str) -> PyResult<Bound<'py, PyAny>> {
    let dt: DateTime<FixedOffset> = parse_timestamp(raw)?;
    dt.into_pyobject(py)?.call_method0("isoformat")
}

// ── detectors ────────────────────────────────────────────────────────────────

/// Compiled structural-noise regex shared by the marker-correction paths
/// (STRUCTURAL_NOISE_RE, filterspec.py:524). Built from the user-message spec's
/// NoiseIfStructural stage so it tracks the spec's group set exactly.
fn structural_re(spec: &CompiledMiningSpec) -> Regex {
    spec.user_message
        .stages
        .iter()
        .find_map(|stage| match stage {
            ConfStage::NoiseIfStructural { groups, .. } => Some(groups.clone()),
            _ => None,
        })
        .unwrap_or_else(|| Regex::new(STRUCTURAL_INTERRUPT_RE).expect("interrupt fallback regex"))
}

fn iter_user_message<'py>(
    py: Python<'py>,
    events: &Events,
    spec: &CompiledMiningSpec,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for index in 0..events.len() {
        if events.kinds[index] != Kind::User {
            continue;
        }
        let text = &events.texts[index];
        if text.trim().is_empty() || is_bare_interrupt_marker(text) {
            continue;
        }
        let trigger = events.nearest_assistant_index(index);
        let sig = score_user_message(&spec.user_message, text, index as i64, trigger);
        out.push(build_signal_dict(
            py,
            TRANSCRIPT_MESSAGE,
            DETECTOR_TRANSCRIPT_MESSAGE,
            &events.lines[index],
            index,
            text,
            trigger,
            &sig,
            None,
            PyDict::new(py),
        )?);
    }
    Ok(())
}

fn iter_plan_rejection<'py>(
    py: Python<'py>,
    events: &Events,
    spec: &CompiledMiningSpec,
    uses: &HashMap<String, &Value>,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for index in 0..events.len() {
        if events.kinds[index] != Kind::User {
            continue;
        }
        let event = &events.lines[index];
        for result in denial_results(event) {
            let Some(tool_use_id) = field_str(result, "tool_use_id") else { continue };
            let Some(use_block) = uses.get(tool_use_id) else { continue };
            if field_str(use_block, "name") != Some("ExitPlanMode") {
                continue;
            }
            let Some(content) = result_content_text(result) else { continue };
            let Some(text) = embedded_user_text(&content) else { continue };
            let trigger = events.nearest_assistant_index(index);
            let sig = calibrated(&spec.calibrated, &text, "embedded_text");
            out.push(build_signal_dict(
                py,
                PLAN_REVIEW,
                DETECTOR_EXIT_PLAN_REJECTION,
                event,
                index,
                &text,
                trigger,
                &sig,
                None,
                PyDict::new(py),
            )?);
        }
    }
    Ok(())
}

fn iter_plan_reentry<'py>(
    py: Python<'py>,
    events: &Events,
    spec: &CompiledMiningSpec,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for index in 0..events.len() {
        let event = &events.lines[index];
        let is_plan_mode = events.kinds[index] == Kind::Mode
            && (field_str(event, "mode") == Some("plan") || field_str(event, "permissionMode") == Some("plan"));
        if !is_plan_mode {
            continue;
        }
        let Some(user_index) = events.next_user_message(index) else { continue };
        let user_event = &events.lines[user_index];
        let Some(uuid) = field_str(user_event, "uuid") else { continue };
        if seen.contains(uuid) || is_bare_interrupt_marker(&events.texts[user_index]) {
            continue;
        }
        let Some(edit) = events.last_edit_index(user_index, spec) else { continue };
        seen.insert(uuid.to_string());
        let text = events.texts[user_index].clone();
        let trigger = events.nearest_assistant_index(user_index);
        let sig = calibrated(&spec.calibrated, &text, "reentry_after_edit");
        out.push(build_signal_dict(
            py,
            PLAN_REVIEW,
            DETECTOR_PLAN_REENTRY,
            user_event,
            user_index,
            &text,
            trigger,
            &sig,
            Some(edit),
            PyDict::new(py),
        )?);
    }
    Ok(())
}

fn iter_tool_denial<'py>(
    py: Python<'py>,
    events: &Events,
    spec: &CompiledMiningSpec,
    uses: &HashMap<String, &Value>,
    structural: &Regex,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for index in 0..events.len() {
        if events.kinds[index] != Kind::User {
            continue;
        }
        let event = &events.lines[index];
        for block in denial_results(event) {
            let paired = field_str(block, "tool_use_id").and_then(|id| uses.get(id).copied());
            if let Some(use_block) = paired {
                if matches!(field_str(use_block, "name"), Some("ExitPlanMode") | Some("AskUserQuestion")) {
                    continue;
                }
            }
            let embedded = result_content_text(block).and_then(|c| embedded_user_text(&c));
            let Some(scored) = denial_correction(events, index, embedded, spec, structural) else { continue };
            let trigger = events.nearest_assistant_index(index);
            let evidence = PyDict::new(py);
            if let Some(use_block) = paired {
                evidence.set_item("tool", field_str(use_block, "name"))?;
                evidence.set_item("file_path", field(use_block, "input").and_then(|i| field_str(i, "file_path")))?;
            }
            out.push(build_signal_dict(
                py,
                INTERRUPT_REJECTION,
                DETECTOR_DENIAL,
                event,
                index,
                &scored.text,
                trigger,
                &scored.signal,
                None,
                evidence,
            )?);
        }
    }
    Ok(())
}

fn iter_interrupt<'py>(
    py: Python<'py>,
    events: &Events,
    structural: &Regex,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    for index in 0..events.len() {
        if events.kinds[index] != Kind::User {
            continue;
        }
        if marker_in(&events.lines[index]).is_none() {
            continue;
        }
        let Some(scored) = marker_correction(events, index, structural) else { continue };
        let trigger = events.nearest_assistant_index(index);
        out.push(build_signal_dict(
            py,
            INTERRUPT_REJECTION,
            DETECTOR_INTERRUPT,
            &events.lines[index],
            index,
            &scored.text,
            trigger,
            &scored.signal,
            None,
            PyDict::new(py),
        )?);
    }
    Ok(())
}

// ── review-comment detector (formats.py port) ────────────────────────────────

struct ReviewComment {
    file: Option<String>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    comment: String,
}

struct ScanText {
    text: String,
    provenance: String,
    trigger_index: Option<i64>,
}

/// classify_provenance (spec.py:322): typed for absent tool, surfaced for a
/// non-subagent main-chain tool, else claude.
fn classify_provenance(subagent_tools: &HashSet<String>, tool_name: Option<&str>, is_sidechain: bool) -> &'static str {
    match (tool_name, is_sidechain) {
        (None, _) => "typed",
        (Some(name), false) if !subagent_tools.contains(name) => "surfaced",
        _ => "claude",
    }
}

/// review_scan_texts (signals.py:338): the typed user text plus each surfaced or
/// claude tool-result, gated by the surfaces set.
fn review_scan_texts(
    events: &Events,
    index: usize,
    spec: &CompiledMiningSpec,
    names: &HashMap<String, String>,
) -> Vec<ScanText> {
    let event = &events.lines[index];
    let surfaces = &spec.review.surfaces;
    let mut scans = Vec::new();
    let text = &events.texts[index];
    if surfaces.contains("typed") && !text.trim().is_empty() {
        scans.push(ScanText {
            text: text.clone(),
            provenance: "typed".to_string(),
            trigger_index: events.nearest_assistant_index(index),
        });
    }
    let is_sidechain = field_bool(event, "isSidechain");
    for block in message_content(event).and_then(JsonContainerTrait::as_array).into_iter().flatten() {
        if block_type(block) != Some("tool_result") {
            continue;
        }
        let tool_name = field_str(block, "tool_use_id").and_then(|id| names.get(id)).map(String::as_str);
        let provenance = classify_provenance(&spec.subagent_tools, tool_name, is_sidechain);
        if provenance == "typed" || !surfaces.contains(provenance) {
            continue;
        }
        if let Some(content) = result_content_text(block) {
            scans.push(ScanText { text: content, provenance: provenance.to_string(), trigger_index: None });
        }
    }
    scans
}

/// first (formats.py:74): the first present, non-null alias value.
fn first<'a>(obj: &'a Value, keys: &[String]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| field(obj, key))
        .find(|value| !value.is_null())
}

/// line_bounds (formats.py:78): int -> (n,n); "a-b" -> first-partition split;
/// all-digit string -> (n,n); else (None,None). A malformed range parse raises,
/// matching Python's propagated int() error.
fn line_bounds(value: Option<&Value>) -> Result<(Option<i64>, Option<i64>), String> {
    match value {
        Some(v) if v.is_i64() || v.is_u64() => {
            let n = v.as_i64().ok_or("line value out of i64 range")?;
            Ok((Some(n), Some(n)))
        }
        Some(v) => match v.as_str() {
            Some(s) if s.contains('-') => {
                let (start, end) = s.split_once('-').expect("contains '-'");
                let start = start.trim().parse::<i64>().map_err(|e| format!("invalid line {start:?}: {e}"))?;
                let end = end.trim().parse::<i64>().map_err(|e| format!("invalid line {end:?}: {e}"))?;
                Ok((Some(start), Some(end)))
            }
            Some(s) if !s.trim().is_empty() && s.trim().chars().all(|c| c.is_ascii_digit()) => {
                let n = s.trim().parse::<i64>().map_err(|e| format!("invalid line {s:?}: {e}"))?;
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
        return if b { "True".to_string() } else { "False".to_string() };
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

/// review_comment (formats.py:91): builds a comment from a finding object's
/// aliased fields. The comment joins the comment value and any fix value with " ".
fn review_comment(obj: &Value, fmt: &CompiledStructuredFormat) -> Result<ReviewComment, String> {
    let (line_start, line_end) = line_bounds(first(obj, &fmt.line_keys))?;
    let file = first(obj, &fmt.file_keys).map(json_str);
    let comment = [first(obj, &fmt.comment_keys), first(obj, &fmt.fix_keys)]
        .into_iter()
        .flatten()
        .map(json_str)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(ReviewComment { file, line_start, line_end, comment })
}

/// findings (formats.py:107): list -> items; dict -> the first finding-array
/// alias, else recurse into "result", else every confirmed*-prefixed list.
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

/// StructuredFormat.extract (formats.py:65): the review comments for every finding
/// object that carries a comment value.
fn extract_structured_format(payload: &Value, fmt: &CompiledStructuredFormat) -> Result<Vec<ReviewComment>, String> {
    let mut found = Vec::new();
    findings(payload, &fmt.finding_keys, &mut found);
    found
        .into_iter()
        .filter(|obj| obj.is_object())
        .filter(|obj| first(obj, &fmt.comment_keys).is_some())
        .map(|obj| review_comment(obj, fmt))
        .collect()
}

/// regex_review_comments (spec.py:338): one comment per regex match, joining the
/// stripped, present comment groups; falsy/unmatched groups are skipped.
fn regex_review_comments(fmt: &CompiledRegexFormat, text: &str) -> Vec<ReviewComment> {
    fmt.regex
        .captures_iter(text)
        .map(|caps| {
            let group = |index: Option<usize>| index.and_then(|i| caps.get(i)).map(|m| m.as_str().to_string());
            let int_group = |index: Option<usize>| group(index).map(|v| v.parse::<i64>());
            ReviewComment {
                file: group(fmt.file_group),
                line_start: int_group(fmt.line_start_group).transpose().unwrap_or(None),
                line_end: int_group(fmt.line_end_group).transpose().unwrap_or(None),
                comment: fmt
                    .comment_groups
                    .iter()
                    .filter_map(|&i| caps.get(i))
                    .map(|m| m.as_str().trim())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(&fmt.join),
            }
        })
        .collect()
}

/// review_comments (signals.py:387): regex formats then structured formats, in
/// order. Callable formats are non-portable and never reach the Rust backend.
fn review_comments(spec: &CompiledMiningSpec, text: &str) -> Result<Vec<(String, ReviewComment)>, String> {
    let mut out: Vec<(String, ReviewComment)> = spec
        .review
        .regex_formats
        .iter()
        .flat_map(|fmt| regex_review_comments(fmt, text).into_iter().map(move |c| (fmt.name.clone(), c)))
        .collect();
    if !spec.review.structured_formats.is_empty() {
        if let Ok(payload) = sonic_rs::from_str::<Value>(text) {
            for fmt in &spec.review.structured_formats {
                for comment in extract_structured_format(&payload, fmt)? {
                    out.push((fmt.name.clone(), comment));
                }
            }
        }
    }
    Ok(out)
}

fn iter_review_comment<'py>(
    py: Python<'py>,
    events: &Events,
    spec: &CompiledMiningSpec,
    uses: &HashMap<String, &Value>,
    out: &mut Vec<Bound<'py, PyDict>>,
) -> PyResult<()> {
    let names: HashMap<String, String> = uses
        .iter()
        .filter_map(|(id, block)| field_str(block, "name").map(|name| (id.clone(), name.to_string())))
        .collect();
    for index in 0..events.len() {
        if events.kinds[index] != Kind::User {
            continue;
        }
        for scan in review_scan_texts(events, index, spec, &names) {
            for (fmt_name, comment) in
                review_comments(spec, &scan.text).map_err(pyo3::exceptions::PyValueError::new_err)?
            {
                let event = &events.lines[index];
                let evidence = PyDict::new(py);
                evidence.set_item("format", &fmt_name)?;
                evidence.set_item("file", &comment.file)?;
                evidence.set_item("line_start", comment.line_start)?;
                evidence.set_item("line_end", comment.line_end)?;
                evidence.set_item("provenance", &scan.provenance)?;
                let sig = calibrated(&spec.calibrated, &comment.comment, "format_match");
                out.push(build_signal_dict(
                    py,
                    REVIEW_COMMENT,
                    DETECTOR_REVIEW_COMMENT,
                    event,
                    index,
                    &comment.comment,
                    scan.trigger_index,
                    &sig,
                    None,
                    evidence,
                )?);
            }
        }
    }
    Ok(())
}

// ── dispatch (mine, signals.py:409) ──────────────────────────────────────────

pub fn mine<'py>(
    py: Python<'py>,
    raw: &[u8],
    spec: &CompiledMiningSpec,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let events = Events::parse(raw);
    let uses = tool_uses(&events);
    let structural = structural_re(spec);
    let mut out: Vec<Bound<'py, PyDict>> = Vec::new();
    if spec.detectors.contains(DETECTOR_TRANSCRIPT_MESSAGE) {
        iter_user_message(py, &events, spec, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_EXIT_PLAN_REJECTION) {
        iter_plan_rejection(py, &events, spec, &uses, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_PLAN_REENTRY) {
        iter_plan_reentry(py, &events, spec, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_DENIAL) {
        iter_tool_denial(py, &events, spec, &uses, &structural, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_INTERRUPT) {
        iter_interrupt(py, &events, &structural, &mut out)?;
    }
    if spec.detectors.contains(DETECTOR_REVIEW_COMMENT) {
        iter_review_comment(py, &events, spec, &uses, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_text_extracts_between_markers() {
        let content = format!("{DENIAL_PREFIX}\n\n{USER_SAID_MARKER}do it this way\n{USER_SAID_TRAILER} ...");
        assert_eq!(embedded_user_text(&content), Some("do it this way".to_string()));
    }

    #[test]
    fn embedded_text_missing_marker_is_none() {
        assert_eq!(embedded_user_text("no marker here"), None);
    }

    #[test]
    fn interrupt_marker_through_bracket() {
        assert_eq!(
            interrupt_marker("[Request interrupted by user]"),
            Some("[Request interrupted by user]".to_string())
        );
        assert_eq!(
            interrupt_marker("  [Request interrupted by user for tool use]rest"),
            Some("[Request interrupted by user for tool use]".to_string())
        );
        assert_eq!(interrupt_marker("hello"), None);
    }

    #[test]
    fn bare_marker_detection() {
        assert!(is_bare_interrupt_marker("[Request interrupted by user]"));
        assert!(!is_bare_interrupt_marker("[Request interrupted by user] no do it differently"));
    }

    #[test]
    fn line_bounds_variants() {
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("7").unwrap())).unwrap(), (Some(7), Some(7)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"24-51\"").unwrap())).unwrap(), (Some(24), Some(51)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"007\"").unwrap())).unwrap(), (Some(7), Some(7)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"x\"").unwrap())).unwrap(), (None, None));
        assert_eq!(line_bounds(None).unwrap(), (None, None));
    }

    #[test]
    fn bump_clamps_to_unit_interval() {
        let high = bump(CandidateSig { confidence: 0.9, reasons: vec![], durable: true }, 0.25, "r");
        assert_eq!(high.confidence, 1.0);
        let low = bump(CandidateSig { confidence: 0.1, reasons: vec![], durable: true }, -0.25, "r");
        assert_eq!(low.confidence, 0.0);
    }
}
