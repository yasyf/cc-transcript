use std::collections::HashMap;
use std::collections::HashSet;

use chrono::{DateTime, FixedOffset};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use regex::Regex;
use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

use crate::event::{parse_timestamp, require_str, truthy_str};
use crate::filter::{compile_group_array, Kind};
use crate::protocol::{
    embedded_user_text, interrupt_marker, is_bare_interrupt_marker, ANSWERED_PREFIX, ANSWERED_TRAILER,
    DENIAL_PREFIX, INTERRUPT_MARKER_RE,
};
use crate::value::{block_type, content_text, field, field_bool, field_str};

// Source kinds (mining/sourcekind.py).
const TRANSCRIPT_MESSAGE: &str = "transcript_message";
const PLAN_REVIEW: &str = "plan_review";
const INTERRUPT_REJECTION: &str = "interrupt_rejection";
const REVIEW_COMMENT: &str = "review_comment";
const QUESTION_ANSWER: &str = "question_answer";

// Detector ids (mining/spec.py TRANSCRIPT_MESSAGE_DETECTOR etc.).
const DETECTOR_TRANSCRIPT_MESSAGE: &str = "transcript_message";
const DETECTOR_EXIT_PLAN_REJECTION: &str = "exit_plan_rejection";
const DETECTOR_PLAN_REENTRY: &str = "plan_reentry";
const DETECTOR_DENIAL: &str = "denial";
const DETECTOR_INTERRUPT: &str = "interrupt";
const DETECTOR_REVIEW_COMMENT: &str = "review_comment";
const DETECTOR_ASK_USER_QUESTION: &str = "ask_user_question";

// AskUserQuestion answered-round segment markers (mining/signals.py).
const ANSWER_PREVIEW_SEP: &str = " selected preview:\n";
const ANSWER_NOTES_SEP: &str = " notes: ";
const NO_OPTION_SELECTED: &str = "(no option selected)";

// Confidence bands (mining/confidence.py NONE / LOW); the hardcoded marker-correction seeds.
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
    plan_tools: HashSet<String>,
    denial_excluded_tools: HashSet<String>,
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
/// ``compile_review_format`` (mining/spec.py compile_review_format): the
/// ``(?:p)|...`` join from ``compile_group_array`` plus a ``(?m)`` prefix when
/// ``multiline`` is set.
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

// ── confidence fold (mining/spec.py run_confidence) ──────────────────────────

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
    /// Splits raw JSONL into event objects, dropping undecodable lines and
    /// non-object values (parser.py decode_line) so event indices agree with the
    /// Python reference.
    fn parse(raw: &[u8]) -> Self {
        let lines: Vec<Value> = raw
            .split(|&b| b == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .filter_map(|line| sonic_rs::from_slice::<Value>(line).ok())
            .filter(|value| value.is_object())
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

    /// Nearest preceding assistant index (mining/signals.py nearest_assistant_index).
    fn nearest_assistant_index(&self, index: usize) -> Option<i64> {
        (0..index).rev().find(|&i| self.kinds[i] == Kind::Assistant).map(|i| i as i64)
    }

    /// The first non-blank user message at or after ``index`` (mining/signals.py
    /// next_user_message).
    fn next_user_message(&self, index: usize) -> Option<usize> {
        (index..self.len()).find(|&i| self.kinds[i] == Kind::User && !self.texts[i].trim().is_empty())
    }

    /// The 40-event lookback for the most recent edit (mining/signals.py
    /// last_edit_index): scans ``range(index-1, max(index-lookback, 0)-1, -1)``
    /// for an assistant event using an edit tool.
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

/// Builds the ``tool_use_id -> ToolUseBlock`` map (filterspec.py tool_uses), keeping
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

/// matches_names (tools.py matches_names): exact membership, or the ``<tool>``
/// segment of an ``mcp__<server>__<tool>`` name split on the first two ``__``.
/// Alias closure happens Python-side (tools.py expand_tool_names); the spec JSON
/// arrives pre-expanded and the alias table never crosses the language boundary.
fn matches_names(actual: &str, names: &HashSet<String>) -> bool {
    names.contains(actual)
        || actual
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .is_some_and(|(_, tool)| names.contains(tool))
}

// ── text-shape helpers (mining/signals.py) ───────────────────────────────────

/// marker_in (mining/signals.py marker_in): whether any tool-result block's
/// content carries the interrupt marker.
fn marker_in(event: &Value) -> bool {
    message_content(event)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter(|b| block_type(b) == Some("tool_result"))
        .any(|b| result_content_text(b).is_some_and(|c| interrupt_marker(&c).is_some()))
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

// ── confidence band literals for marker_correction (mining/confidence.py weak /
// noise) ──────────────────────────────────────────────────────────────────────

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

/// correction_text (mining/signals.py correction_text): the first following
/// non-bare, non-structural user message — a forward loop that re-scans from the
/// last consumed index.
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

/// first_followup (mining/signals.py first_followup): the first following non-bare
/// user message.
fn first_followup(events: &Events, mut index: usize) -> Option<String> {
    while let Some(i) = events.next_user_message(index + 1) {
        index = i;
        if !is_bare_interrupt_marker(&events.texts[index]) {
            return Some(events.texts[index].clone());
        }
    }
    None
}

/// marker_correction (mining/signals.py marker_correction): a real correction
/// weak(bare_marker), else a followup noise(structural_only), else None.
fn marker_correction(events: &Events, index: usize, structural: &Regex) -> Option<ScoredText> {
    if let Some(correction) = correction_text(events, index, structural) {
        return Some(ScoredText { text: correction, signal: weak("bare_marker") });
    }
    first_followup(events, index).map(|followup| ScoredText { text: followup, signal: noise("structural_only") })
}

/// denial_correction (mining/signals.py denial_correction): the embedded
/// instruction calibrated, else the marker-correction fallback.
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

/// The denial tool-result blocks of a user event (mining/signals.py
/// denial_results): error blocks whose flattened content starts with the denial
/// banner.
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

// ── signal dict construction (mining/spec.py signal_to_dict) ─────────────────

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
            if !field_str(use_block, "name").is_some_and(|name| matches_names(name, &spec.plan_tools)) {
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
                if field_str(use_block, "name")
                    .is_some_and(|name| matches_names(name, &spec.denial_excluded_tools))
                {
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
        if !marker_in(&events.lines[index]) {
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

/// classify_provenance (mining/spec.py classify_provenance): typed for absent tool,
/// surfaced for a non-subagent main-chain tool (per tools.py matches_names), else claude.
fn classify_provenance(subagent_tools: &HashSet<String>, tool_name: Option<&str>, is_sidechain: bool) -> &'static str {
    match (tool_name, is_sidechain) {
        (None, _) => "typed",
        (Some(name), false) if !matches_names(name, subagent_tools) => "surfaced",
        _ => "claude",
    }
}

/// review_scan_texts (mining/signals.py review_scan_texts): the typed user text
/// plus each surfaced or claude tool-result, gated by the surfaces set.
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
    Ok(ReviewComment { file, line_start, line_end, comment })
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

/// StructuredFormat.extract (mining/formats.py StructuredFormat.extract): the
/// review comments for every finding object that carries a comment value.
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

/// regex_review_comments (mining/spec.py regex_review_comments): one comment per
/// regex match. Comment groups are stripped first, unmatched or stripped-empty
/// groups are skipped, and the rest join with the format's separator; line groups
/// are stripped then parsed, with unparseable values yielding None.
fn regex_review_comments(fmt: &CompiledRegexFormat, text: &str) -> Vec<ReviewComment> {
    fmt.regex
        .captures_iter(text)
        .map(|caps| {
            let group = |index: Option<usize>| index.and_then(|i| caps.get(i)).map(|m| m.as_str().to_string());
            let int_group = |index: Option<usize>| group(index).and_then(|v| v.trim().parse::<i64>().ok());
            ReviewComment {
                file: group(fmt.file_group),
                line_start: int_group(fmt.line_start_group),
                line_end: int_group(fmt.line_end_group),
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

/// review_comments (mining/signals.py review_comments): regex formats then
/// structured formats, in order. Callable formats are non-portable and never reach
/// the Rust backend.
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

// ── dispatch (mining/signals.py mine) ────────────────────────────────────────

/// The answered AskUserQuestion tool-result blocks of a user event
/// (signals.py answered_question_results): non-error blocks whose flattened
/// content starts with the answered banner.
fn answered_results(event: &Value) -> Vec<&Value> {
    message_content(event)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter(|b| block_type(b) == Some("tool_result"))
        .filter(|b| !field_bool(b, "is_error"))
        .filter(|b| result_content_text(b).is_some_and(|c| c.starts_with(ANSWERED_PREFIX)))
        .collect()
}

struct AnsweredPair<'a> {
    question: &'a Value,
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
        Some(at) => (&segment[..at], None, Some(&segment[at + ANSWER_NOTES_SEP.len()..])),
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
fn answered_pairs<'a>(body: &'a str, questions: &'a sonic_rs::Array) -> Vec<AnsweredPair<'a>> {
    let mut found: Vec<(&Value, usize, usize)> = Vec::new();
    let mut pos = 0usize;
    for question in questions.iter() {
        let Some(text) = field_str(question, "question") else { continue };
        let anchor = format!("\"{text}\"=");
        let Some(at) = find_anchor(body, &anchor, pos) else { continue };
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
        pairs.push(AnsweredPair { question, answer, preview, notes });
    }
    pairs
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
    if !(1..=labels.len()).contains(&n) || (!rest.is_empty() && !rest.starts_with([',', '.', ' ', ')'])) {
        return None;
    }
    Some((labels[n - 1].clone(), rest.is_empty()))
}

/// resolve_pick (signals.py resolve_pick): verbatim/multiSelect join → a full
/// pick; leading ordinal → resolved label with option_pick only when bare; else
/// pure freeform.
fn resolve_pick(answer: Option<&str>, labels: &[String]) -> (Vec<String>, bool) {
    let Some(answer) = answer else { return (Vec::new(), false) };
    if let Some(joined) = join_labels(answer, labels) {
        return (joined, true);
    }
    match ordinal_label(answer, labels) {
        Some((label, bare)) => (vec![label], bare),
        None => (Vec::new(), false),
    }
}

/// iter_ask_user_question_signals (signals.py iter_ask_user_question_signals):
/// one signal per answered question/answer pair, with the pick resolution facts
/// as evidence (absent preview/notes keys are omitted, never None).
fn iter_ask_user_question<'py>(
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
        for block in answered_results(event) {
            let Some(use_block) = field_str(block, "tool_use_id").and_then(|id| uses.get(id).copied())
            else {
                continue;
            };
            if field_str(use_block, "name") != Some("AskUserQuestion") {
                continue;
            }
            let Some(content) = result_content_text(block) else { continue };
            if !content.ends_with(ANSWERED_TRAILER)
                || content.len() < ANSWERED_PREFIX.len() + ANSWERED_TRAILER.len()
            {
                continue;
            }
            let body = &content[ANSWERED_PREFIX.len()..content.len() - ANSWERED_TRAILER.len()];
            let Some(questions) = field(use_block, "input")
                .and_then(|input| field(input, "questions"))
                .and_then(JsonContainerTrait::as_array)
            else {
                continue;
            };
            let trigger = events.nearest_assistant_index(index);
            for pair in answered_pairs(body, questions) {
                let labels: Vec<String> = field(pair.question, "options")
                    .and_then(JsonContainerTrait::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| field_str(option, "label"))
                    .map(String::from)
                    .collect();
                let (picked, option_pick) = resolve_pick(pair.answer, &labels);
                let recommended = picked.iter().any(|label| label.contains("(Recommended)"));
                let Some(text) = pair.notes.or(pair.answer) else { continue };
                let sig = if option_pick && pair.notes.map_or(true, str::is_empty) {
                    weak("option_pick")
                } else {
                    calibrated(&spec.calibrated, text, "freeform_answer")
                };
                let evidence = PyDict::new(py);
                evidence.set_item("question", field_str(pair.question, "question"))?;
                evidence.set_item("header", field_str(pair.question, "header"))?;
                evidence.set_item("multi_select", field_bool(pair.question, "multiSelect"))?;
                evidence.set_item("option_pick", option_pick)?;
                evidence.set_item("picked_labels", picked)?;
                evidence.set_item("recommended_pick", recommended)?;
                if let Some(preview) = pair.preview {
                    evidence.set_item("preview", preview)?;
                }
                if let Some(notes) = pair.notes {
                    evidence.set_item("notes", notes)?;
                }
                out.push(build_signal_dict(
                    py,
                    QUESTION_ANSWER,
                    DETECTOR_ASK_USER_QUESTION,
                    event,
                    index,
                    text,
                    trigger,
                    &sig,
                    None,
                    evidence,
                )?);
            }
        }
    }
    Ok(())
}

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
    if spec.detectors.contains(DETECTOR_ASK_USER_QUESTION) {
        iter_ask_user_question(py, &events, spec, &uses, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_bounds_variants() {
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("7").unwrap())).unwrap(), (Some(7), Some(7)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"24-51\"").unwrap())).unwrap(), (Some(24), Some(51)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"007\"").unwrap())).unwrap(), (Some(7), Some(7)));
        assert_eq!(line_bounds(Some(&sonic_rs::from_str("\"x\"").unwrap())).unwrap(), (None, None));
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
    }

    #[test]
    fn bump_clamps_to_unit_interval() {
        let high = bump(CandidateSig { confidence: 0.9, reasons: vec![], durable: true }, 0.25, "r");
        assert_eq!(high.confidence, 1.0);
        let low = bump(CandidateSig { confidence: 0.1, reasons: vec![], durable: true }, -0.25, "r");
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

    #[test]
    fn answered_pairs_slices_segments_and_skips_omitted() {
        let questions: Value =
            sonic_rs::from_str(r#"[{"question": "Q1"}, {"question": "Q2"}, {"question": "Q3"}]"#).unwrap();
        let body = "\"Q1\"=\"first, with comma\", \"Q3\"=\"he said \"hi\"\"";
        let pairs = answered_pairs(body, questions.as_array().unwrap());
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].answer, Some("first, with comma"));
        assert_eq!(pairs[1].answer, Some("he said \"hi\""));
    }

    #[test]
    fn join_labels_greedy_over_comma_containing_label() {
        let labels = vec!["BeforeEdit / AfterEdit".to_string(), "One, Two".to_string(), "Three".to_string()];
        assert_eq!(
            join_labels("One, Two, Three", &labels),
            Some(vec!["One, Two".to_string(), "Three".to_string()])
        );
        assert_eq!(join_labels("One, Two, Four", &labels), None);
        assert_eq!(join_labels("Three", &labels), Some(vec!["Three".to_string()]));
    }

    #[test]
    fn ordinal_label_bounds_and_separators() {
        let labels = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert_eq!(ordinal_label("3, and more", &labels), Some(("C".to_string(), false)));
        assert_eq!(ordinal_label("2", &labels), Some(("B".to_string(), true)));
        assert_eq!(ordinal_label("2026 timeline works", &labels), None);
        assert_eq!(ordinal_label("1x", &labels), None);
        assert_eq!(ordinal_label("freeform", &labels), None);
    }
}
