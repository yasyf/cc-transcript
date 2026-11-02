use std::cell::Cell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::size_of;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use regex::Regex;
use sha2::{Digest, Sha256};
use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

use crate::filter::compile_group_array;
use crate::literals::mining::{
    ANSWER_NOTES_SEP, ANSWER_PREVIEW_SEP, DETECTOR_ASK_USER_QUESTION, DETECTOR_DENIAL,
    DETECTOR_EXIT_PLAN_REJECTION, DETECTOR_INTERRUPT, DETECTOR_PLAN_REENTRY,
    DETECTOR_REVIEW_COMMENT, DETECTOR_TRANSCRIPT_MESSAGE, INTERRUPT_REJECTION, LOW, NONE,
    NO_OPTION_SELECTED, PLAN_REVIEW, QUESTION_ANSWER, REVIEW_COMMENT, TRANSCRIPT_MESSAGE,
};
use crate::parse::parse_questions;
use crate::protocol::{
    embedded_user_text, interrupt_marker, is_bare_interrupt_marker, ANSWERED_PREFIX,
    ANSWERED_TRAILER, DENIAL_KIND_USER_REJECTED, INTERRUPT_MARKER_RE,
};
use crate::snapshot::{Cancellation, SnapshotError, Status, TranscriptSnapshot, WorkLimits};
use crate::snapshot_memory::value_charge;
use crate::types::{
    matches_names, tool_use_index, Entry, EntryMeta, Question, ToolResultBlock, ToolUseBlock,
    UserEntry,
};
use crate::value::{field, field_bool, field_str};

#[path = "mining_index.rs"]
mod index;

pub use index::ReviewMiningIndex;

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CandidateSig {
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub durable: bool,
}

#[derive(Debug)]
pub enum MiningError {
    Snapshot(SnapshotError),
    Invalid(String),
    Callback(Box<dyn std::error::Error + Send + Sync>),
}

pub type MiningResult<T> = Result<T, MiningError>;

impl From<SnapshotError> for MiningError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MiningEvidence {
    pub fields: Vec<(String, Value)>,
}

impl MiningEvidence {
    fn new<const N: usize>(fields: [(&str, Value); N]) -> Self {
        Self {
            fields: fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct MiningSignal {
    pub kind: String,
    pub detector: String,
    pub session_id: String,
    pub event_index: usize,
    pub event_uuid: String,
    pub occurred_at: DateTime<FixedOffset>,
    pub text: String,
    pub cc_version: Option<String>,
    pub trigger_index: Option<i64>,
    pub signal: CandidateSig,
    pub lower_bound: Option<i64>,
    pub evidence: MiningEvidence,
}

impl MiningSignal {
    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.kind.capacity()
            + self.detector.capacity()
            + self.session_id.capacity()
            + self.event_uuid.capacity()
            + self.text.capacity()
            + self.cc_version.as_ref().map_or(0, String::capacity)
            + self.signal.reasons.capacity() * size_of::<String>()
            + self
                .signal
                .reasons
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + self.evidence.fields.capacity() * size_of::<(String, Value)>()
            + self
                .evidence
                .fields
                .iter()
                .map(|(key, value)| {
                    let charge = value_charge(value);
                    key.capacity() + charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                })
                .sum::<usize>()
    }
}

pub trait SignalSink {
    fn push(&mut self, signal: MiningSignal) -> MiningResult<()>;
}

impl SignalSink for Vec<MiningSignal> {
    fn push(&mut self, signal: MiningSignal) -> MiningResult<()> {
        Vec::push(self, signal);
        Ok(())
    }
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

pub trait CallableReviewFormat: Send + Sync {
    fn name(&self) -> &str;
    fn fingerprint(&self) -> [u8; 32];
    fn bounded(&self) -> bool;
    fn visit(
        &self,
        text: &str,
        bounded: bool,
        preflight: &dyn Fn(usize) -> MiningResult<()>,
        emit: &mut dyn FnMut(ReviewComment) -> MiningResult<()>,
    ) -> MiningResult<()>;
}

struct CompiledReviewSpec {
    surfaces: HashSet<String>,
    regex_formats: Vec<CompiledRegexFormat>,
    callable_formats: Vec<Arc<dyn CallableReviewFormat>>,
    structured_formats: Vec<CompiledStructuredFormat>,
}

pub struct CompiledMiningSpec {
    config_fingerprint: [u8; 32],
    fingerprint: [u8; 32],
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

impl CompiledMiningSpec {
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

fn spec_fingerprint(root: &Value) -> Result<[u8; 32], String> {
    let canonical = crate::ids::canonical_json(root).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(canonical.as_bytes()).into())
}

pub fn compile_spec(spec_json: &str) -> Result<CompiledMiningSpec, String> {
    let root: Value =
        sonic_rs::from_str(spec_json).map_err(|e| format!("invalid mining spec json: {e}"))?;
    Ok(CompiledMiningSpec {
        config_fingerprint: spec_fingerprint(&root)?,
        fingerprint: spec_fingerprint(&root)?,
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

pub fn set_callable_formats(
    spec: &mut CompiledMiningSpec,
    formats: Vec<Arc<dyn CallableReviewFormat>>,
) {
    let mut fingerprint = Sha256::new();
    fingerprint.update(spec.config_fingerprint);
    for format in &formats {
        fingerprint.update(format.name().len().to_le_bytes());
        fingerprint.update(format.name().as_bytes());
        fingerprint.update(format.fingerprint());
        fingerprint.update([u8::from(format.bounded())]);
    }
    spec.fingerprint = fingerprint.finalize().into();
    spec.review.callable_formats = formats;
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
    fn check(&self) -> MiningResult<()> {
        self.cancel
            .check(self.limits.deadline_unix_ms)
            .map_err(MiningError::Snapshot)
    }

    fn preflight(&self, bytes: usize) -> MiningResult<()> {
        self.check()?;
        let usage = self.usage.get();
        if usage.items >= self.limits.max_items
            || bytes
                > self
                    .limits
                    .max_output_bytes
                    .saturating_sub(usage.output_bytes)
        {
            return Err(MiningError::Snapshot(SnapshotError::new(
                Status::OutputLimit,
                "mining output budget exceeded",
            )));
        }
        Ok(())
    }

    fn reserve(&self, bytes: usize) -> MiningResult<()> {
        self.preflight(bytes)?;
        let mut usage = self.usage.get();
        usage.items += 1;
        usage.output_bytes += bytes;
        self.usage.set(usage);
        Ok(())
    }
}

enum EventRows<'a> {
    Full(Vec<&'a Entry>),
    Indexed {
        index: &'a ReviewMiningIndex,
        overrides: &'a HashMap<usize, &'a Entry>,
    },
}

enum EventIndices<'a> {
    Full(std::ops::Range<usize>),
    Indexed(std::collections::btree_set::Iter<'a, usize>),
}

impl Iterator for EventIndices<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Full(indices) => indices.next(),
            Self::Indexed(indices) => indices.next().copied(),
        }
    }
}

enum ToolUseIndex<'a> {
    Full(HashMap<&'a str, &'a ToolUseBlock>),
    Indexed(&'a HashMap<String, ToolUseBlock>),
}

impl<'a> ToolUseIndex<'a> {
    fn get(&self, id: &str) -> Option<&'a ToolUseBlock> {
        match self {
            Self::Full(uses) => uses.get(id).copied(),
            Self::Indexed(uses) => uses.get(id),
        }
    }
}

struct Events<'a> {
    entries: EventRows<'a>,
    texts: Vec<String>,
    budget: Option<MiningBudget>,
}

impl<'a> Events<'a> {
    fn new(entries: Vec<&'a Entry>) -> Self {
        let texts = entries
            .iter()
            .map(|entry| match entry {
                Entry::User(user) => user.content.text(),
                _ => String::new(),
            })
            .collect();
        Self {
            entries: EventRows::Full(entries),
            texts,
            budget: None,
        }
    }

    fn index(&self) -> Option<&'a ReviewMiningIndex> {
        match self.entries {
            EventRows::Indexed { index, .. } => Some(index),
            EventRows::Full(_) => None,
        }
    }

    fn entry(&self, index: usize) -> &'a Entry {
        match &self.entries {
            EventRows::Full(entries) => entries[index],
            EventRows::Indexed {
                index: corpus,
                overrides,
            } => overrides
                .get(&index)
                .copied()
                .unwrap_or(&corpus.entries[index]),
        }
    }

    fn iter_entries(&self) -> impl Iterator<Item = (usize, &'a Entry)> + '_ {
        let indices = match self.index() {
            Some(index) => EventIndices::Indexed(index.pending.iter()),
            None => EventIndices::Full(0..self.len()),
        };
        indices.map(|index| (index, self.entry(index)))
    }

    fn text(&self, index: usize) -> &str {
        match self.index() {
            Some(corpus) => corpus.text(index),
            None => &self.texts[index],
        }
    }

    fn len(&self) -> usize {
        match &self.entries {
            EventRows::Full(entries) => entries.len(),
            EventRows::Indexed { index, .. } => index.entries.len(),
        }
    }

    fn check(&self) -> MiningResult<()> {
        self.budget.as_ref().map_or(Ok(()), MiningBudget::check)
    }

    fn preflight(&self, bytes: usize) -> MiningResult<()> {
        self.budget
            .as_ref()
            .map_or(Ok(()), |budget| budget.preflight(bytes))
    }

    fn reserve(&self, bytes: usize) -> MiningResult<()> {
        self.budget
            .as_ref()
            .map_or(Ok(()), |budget| budget.reserve(bytes))
    }

    fn nearest_assistant_index(&self, index: usize) -> Option<i64> {
        if let Some(corpus) = self.index() {
            return corpus
                .assistants
                .range(..index)
                .next_back()
                .map(|index| *index as i64);
        }
        (0..index)
            .rev()
            .find(|&i| matches!(self.entry(i), Entry::Assistant(_)))
            .map(|i| i as i64)
    }

    fn next_user_message(&self, index: usize) -> Option<(usize, &UserEntry)> {
        if let Some(corpus) = self.index() {
            let index = *corpus.users.range(index..).next()?;
            let Entry::User(user) = self.entry(index) else {
                unreachable!()
            };
            return Some((index, user));
        }
        (index..self.len()).find_map(|i| match self.entry(i) {
            Entry::User(user) if !self.text(i).trim().is_empty() => Some((i, user)),
            _ => None,
        })
    }

    fn last_edit_index(&self, index: usize, spec: &CompiledMiningSpec) -> Option<i64> {
        let lo = index.saturating_sub(spec.reentry_lookback);
        if let Some(corpus) = self.index() {
            return corpus
                .edits
                .range(lo..index)
                .next_back()
                .map(|index| *index as i64);
        }
        (lo..index)
            .rev()
            .find(|&i| {
                self.entry(i)
                    .tool_uses()
                    .any(|tool| matches_names(&tool.name, &spec.edit_tools))
            })
            .map(|i| i as i64)
    }

    fn embedded(&self, event: usize, ordinal: usize, result: &ToolResultBlock) -> Option<String> {
        match &self.entries {
            EventRows::Indexed { index, overrides } if !overrides.contains_key(&event) => {
                index.result_facts[&event][ordinal].embedded.clone()
            }
            _ => embedded_user_text(&result.content),
        }
    }

    fn interrupted(&self, event: usize, user: &UserEntry) -> bool {
        match &self.entries {
            EventRows::Indexed { index, overrides } if !overrides.contains_key(&event) => index
                .result_facts
                .get(&event)
                .is_some_and(|facts| facts.iter().any(|fact| fact.interrupt)),
            _ => marker_in(user),
        }
    }
}

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

fn correction_text(events: &Events<'_>, mut index: usize, structural: &Regex) -> Option<String> {
    if let Some(corpus) = events.index() {
        let target = corpus
            .dependencies
            .followup(index)
            .expect("indexed correction anchor")
            .correction?;
        return Some(events.text(target).to_owned());
    }
    while let Some((i, _)) = events.next_user_message(index + 1) {
        let text = events.text(i);
        if !is_bare_interrupt_marker(text) && !structural.is_match(text) {
            return Some(text.to_owned());
        }
        index = i;
    }
    None
}

fn first_followup(events: &Events<'_>, mut index: usize) -> Option<String> {
    if let Some(corpus) = events.index() {
        let target = corpus
            .dependencies
            .followup(index)
            .expect("indexed followup anchor")
            .first?;
        return Some(events.text(target).to_owned());
    }
    while let Some((i, _)) = events.next_user_message(index + 1) {
        index = i;
        if !is_bare_interrupt_marker(events.text(index)) {
            return Some(events.text(index).to_owned());
        }
    }
    None
}

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

fn denial_results(user: &UserEntry) -> impl Iterator<Item = (usize, &ToolResultBlock)> {
    user.tool_results()
        .enumerate()
        .filter(|(_, block)| block.denial_kind.as_deref() == Some(DENIAL_KIND_USER_REJECTED))
}

#[allow(clippy::too_many_arguments)]
fn build_signal(
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
    evidence: impl FnOnce() -> MiningResult<MiningEvidence>,
) -> MiningResult<MiningSignal> {
    let strings = kind.len()
        + detector.len()
        + meta.session_id.len()
        + meta.uuid.len()
        + text.len()
        + meta.version.as_ref().map_or(0, String::len)
        + sig.reasons.iter().map(String::len).sum::<usize>()
        + evidence_bytes;
    events.reserve(4096usize.saturating_add(strings.saturating_mul(6)))?;
    Ok(MiningSignal {
        kind: kind.to_owned(),
        detector: detector.to_owned(),
        session_id: meta.session_id.clone(),
        event_index,
        event_uuid: meta.uuid.clone(),
        occurred_at: meta.timestamp,
        text: text.to_owned(),
        cc_version: meta.version.clone(),
        trigger_index,
        signal: sig.clone(),
        lower_bound,
        evidence: evidence()?,
    })
}

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

fn iter_user_message(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        let text = events.text(index);
        if text.trim().is_empty() || is_bare_interrupt_marker(text) {
            continue;
        }
        let trigger = events.nearest_assistant_index(index);
        let sig = score_user_message(&spec.user_message, text, index as i64, trigger);
        out.push(build_signal(
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
            || Ok(MiningEvidence::default()),
        )?)?;
    }
    Ok(())
}

fn iter_plan_rejection(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &ToolUseIndex<'_>,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for (ordinal, result) in denial_results(user) {
            let Some(use_block) = uses.get(result.tool_use_id.as_str()) else {
                continue;
            };
            if !matches_names(&use_block.name, &spec.plan_tools) {
                continue;
            }
            let Some(text) = events.embedded(index, ordinal, result) else {
                continue;
            };
            let trigger = events.nearest_assistant_index(index);
            let sig = calibrated(&spec.calibrated, &text, "embedded_text");
            out.push(build_signal(
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
                || Ok(MiningEvidence::default()),
            )?)?;
        }
    }
    Ok(())
}

fn iter_plan_reentry(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::Mode(mode) = entry else { continue };
        if mode.value != "plan" {
            continue;
        }
        let next = match events.index() {
            Some(corpus) => corpus.dependencies.plan_user(index).map(|target| {
                let Entry::User(user) = events.entry(target) else {
                    unreachable!()
                };
                (target, user)
            }),
            None => events.next_user_message(index),
        };
        let Some((user_index, user)) = next else {
            continue;
        };
        let uuid = user.meta.uuid.as_str();
        if seen.contains(uuid)
            || events
                .index()
                .is_some_and(|corpus| corpus.plan_seen.contains(uuid))
            || is_bare_interrupt_marker(events.text(user_index))
        {
            continue;
        }
        let Some(edit) = events.last_edit_index(user_index, spec) else {
            continue;
        };
        seen.insert(uuid);
        let text = events.text(user_index);
        let trigger = events.nearest_assistant_index(user_index);
        let sig = calibrated(&spec.calibrated, text, "reentry_after_edit");
        out.push(build_signal(
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
            || Ok(MiningEvidence::default()),
        )?)?;
    }
    Ok(())
}

fn iter_tool_denial(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &ToolUseIndex<'_>,
    structural: &Regex,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for (ordinal, block) in denial_results(user) {
            let paired = uses.get(block.tool_use_id.as_str());
            if paired.is_some_and(|use_block| {
                matches_names(&use_block.name, &spec.denial_excluded_tools)
            }) {
                continue;
            }
            let embedded = events.embedded(index, ordinal, block);
            let Some(scored) = denial_correction(events, index, embedded, spec, structural) else {
                continue;
            };
            let trigger = events.nearest_assistant_index(index);
            let evidence_bytes = paired.map_or(0, |tool| {
                tool.name.len() + tool.file_path.as_ref().map_or(0, String::len)
            });
            out.push(build_signal(
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
                    Ok(match paired {
                        Some(use_block) => MiningEvidence::new([
                            ("tool", sonic_rs::json!(&use_block.name)),
                            ("file_path", sonic_rs::json!(use_block.file_path.as_deref())),
                        ]),
                        None => MiningEvidence::default(),
                    })
                },
            )?)?;
        }
    }
    Ok(())
}

fn iter_interrupt(
    events: &Events<'_>,
    structural: &Regex,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        if !events.interrupted(index, user) {
            continue;
        }
        let Some(scored) = marker_correction(events, index, structural) else {
            continue;
        };
        let trigger = events.nearest_assistant_index(index);
        out.push(build_signal(
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
            || Ok(MiningEvidence::default()),
        )?)?;
    }
    Ok(())
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ReviewComment {
    pub file: Option<String>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub comment: String,
}

struct ScanText<'a> {
    text: &'a str,
    provenance: &'static str,
    trigger_index: Option<i64>,
}

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

fn review_scan_texts<'a>(
    events: &'a Events<'a>,
    user: &'a UserEntry,
    index: usize,
    spec: &'a CompiledMiningSpec,
    uses: &'a ToolUseIndex<'a>,
) -> impl Iterator<Item = ScanText<'a>> + 'a {
    let surfaces = &spec.review.surfaces;
    let text = events.text(index);
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

fn first<'a>(obj: &'a Value, keys: &[String]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| field(obj, key))
        .find(|value| !value.is_null())
}

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

fn visit_review_comments(
    spec: &CompiledMiningSpec,
    text: &str,
    events: &Events<'_>,
    mut emit: impl FnMut(&str, ReviewComment) -> MiningResult<()>,
) -> MiningResult<()> {
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
        fmt.visit(
            text,
            events.budget.is_some(),
            &|bytes| events.preflight(bytes),
            &mut |comment| emit(fmt.name(), comment),
        )?;
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
                        review_comment(obj, fmt).map_err(MiningError::Invalid)?,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn iter_review_comment(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &ToolUseIndex<'_>,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
        events.check()?;
        let Entry::User(user) = entry else { continue };
        for scan in review_scan_texts(events, user, index, spec, uses) {
            visit_review_comments(spec, scan.text, events, |fmt_name, comment| {
                let sig = calibrated(&spec.calibrated, &comment.comment, "format_match");
                let evidence_bytes = fmt_name.len()
                    + comment.file.as_ref().map_or(0, String::len)
                    + scan.provenance.len();
                out.push(build_signal(
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
                        Ok(MiningEvidence::new([
                            ("format", sonic_rs::json!(fmt_name)),
                            ("file", sonic_rs::json!(&comment.file)),
                            ("line_start", sonic_rs::json!(comment.line_start)),
                            ("line_end", sonic_rs::json!(comment.line_end)),
                            ("provenance", sonic_rs::json!(scan.provenance)),
                        ]))
                    },
                )?)?;
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

fn iter_ask_user_question(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    uses: &ToolUseIndex<'_>,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    for (index, entry) in events.iter_entries() {
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
                out.push(build_signal(
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
                        let mut evidence = MiningEvidence::new([
                            ("question", sonic_rs::json!(&pair.question.question)),
                            ("header", sonic_rs::json!(pair.question.header.as_deref())),
                            ("multi_select", sonic_rs::json!(pair.question.multi_select)),
                            ("option_pick", sonic_rs::json!(option_pick)),
                            ("picked_labels", sonic_rs::json!(picked)),
                            ("recommended_pick", sonic_rs::json!(recommended)),
                        ]);
                        if let Some(preview) = pair.preview {
                            evidence
                                .fields
                                .push(("preview".to_owned(), sonic_rs::json!(preview)));
                        }
                        if let Some(notes) = pair.notes {
                            evidence
                                .fields
                                .push(("notes".to_owned(), sonic_rs::json!(notes)));
                        }
                        Ok(evidence)
                    },
                )?)?;
            }
        }
    }
    Ok(())
}

pub fn mine_entries(
    entries: Vec<&Entry>,
    spec: &CompiledMiningSpec,
) -> MiningResult<Vec<MiningSignal>> {
    let mut out = Vec::new();
    mine_entries_into(entries, spec, &mut out)?;
    Ok(out)
}

pub fn mine_entries_into(
    entries: Vec<&Entry>,
    spec: &CompiledMiningSpec,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    mine_events_impl(&Events::new(entries), spec, out)
}

pub struct PreparedMining<'a> {
    events: Events<'a>,
}

pub fn mine_snapshot(
    snapshot: &TranscriptSnapshot,
    spec: &CompiledMiningSpec,
    limits: WorkLimits,
    cancel: &Cancellation,
    usage: &mut MiningUsage,
) -> MiningResult<Vec<MiningSignal>> {
    let prepared = prepare_snapshot(snapshot, spec, limits, cancel, usage)?;
    let mut out = Vec::new();
    mine_prepared_into(prepared, spec, usage, &mut out)?;
    Ok(out)
}

pub fn prepare_snapshot<'a>(
    snapshot: &'a TranscriptSnapshot,
    spec: &CompiledMiningSpec,
    limits: WorkLimits,
    cancel: &Cancellation,
    usage: &mut MiningUsage,
) -> MiningResult<PreparedMining<'a>> {
    *usage = MiningUsage::default();
    if spec
        .review
        .callable_formats
        .iter()
        .any(|format| !format.bounded())
    {
        return Err(MiningError::Snapshot(SnapshotError::new(
            Status::Incomplete,
            "mining callback is not registered as bounded",
        )));
    }
    let (entries, texts) = (|| {
        cancel.check(limits.deadline_unix_ms)?;
        for chunk in &snapshot.chunks {
            for charge in &chunk.entry_charges {
                cancel.check(limits.deadline_unix_ms)?;
                if usage.events == limits.max_events {
                    return Err(crate::snapshot::SnapshotError::new(
                        crate::snapshot::Status::EntryLimit,
                        "mining input event budget exceeded",
                    ));
                }
                let bytes = charge
                    .owned_capacity_bytes
                    .saturating_add(charge.opaque_dom_accounted_bytes)
                    .saturating_add(size_of::<Entry>() + size_of::<&Entry>() + size_of::<String>());
                if bytes > limits.max_read_bytes.saturating_sub(usage.input_bytes) {
                    return Err(crate::snapshot::SnapshotError::new(
                        crate::snapshot::Status::SourceLimit,
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
    })()
    .map_err(MiningError::Snapshot)?;
    let budget = MiningBudget {
        limits,
        cancel: cancel.clone(),
        usage: Cell::new(*usage),
    };
    let events = Events {
        entries: EventRows::Full(entries),
        texts,
        budget: Some(budget),
    };
    Ok(PreparedMining { events })
}

pub fn mine_prepared_into(
    prepared: PreparedMining<'_>,
    spec: &CompiledMiningSpec,
    usage: &mut MiningUsage,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    let events = prepared.events;
    let result = mine_events_impl(&events, spec, out).and_then(|()| events.check());
    *usage = events
        .budget
        .as_ref()
        .expect("prepared mining budget")
        .usage
        .get();
    result
}

fn mine_indexed(
    index: &ReviewMiningIndex,
    spec: &CompiledMiningSpec,
    overrides: &HashMap<usize, &Entry>,
    limits: WorkLimits,
    cancel: &Cancellation,
    usage: &mut MiningUsage,
) -> MiningResult<Vec<MiningSignal>> {
    let mut events = Events {
        entries: EventRows::Indexed { index, overrides },
        texts: Vec::new(),
        budget: None,
    };
    for (_, entry) in events.iter_entries() {
        cancel.check(limits.deadline_unix_ms)?;
        if usage.events == limits.max_events {
            return Err(SnapshotError::new(
                Status::EntryLimit,
                "mining input event budget exceeded",
            )
            .into());
        }
        let charge = crate::snapshot_memory::entry_charge(entry);
        let bytes = charge.owned_capacity_bytes
            + charge.opaque_dom_accounted_bytes
            + size_of::<Entry>()
            + size_of::<&Entry>();
        if bytes > limits.max_read_bytes.saturating_sub(usage.input_bytes) {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "mining input byte budget exceeded",
            )
            .into());
        }
        usage.events += 1;
        usage.input_bytes += bytes;
    }
    events.budget = Some(MiningBudget {
        limits,
        cancel: cancel.clone(),
        usage: Cell::new(*usage),
    });
    let mut rows = Vec::new();
    let result = mine_events_impl(&events, spec, &mut rows).and_then(|()| events.check());
    *usage = events
        .budget
        .as_ref()
        .expect("indexed mining budget")
        .usage
        .get();
    result?;
    Ok(rows)
}

fn mine_events_impl(
    events: &Events<'_>,
    spec: &CompiledMiningSpec,
    out: &mut dyn SignalSink,
) -> MiningResult<()> {
    let uses = match events.index() {
        Some(index) => ToolUseIndex::Indexed(&index.uses),
        None => ToolUseIndex::Full(tool_use_index(
            (0..events.len()).map(|index| events.entry(index)),
        )),
    };
    let structural = structural_re(spec);
    if spec.detectors.contains(DETECTOR_TRANSCRIPT_MESSAGE) {
        iter_user_message(events, spec, out)?;
    }
    if spec.detectors.contains(DETECTOR_EXIT_PLAN_REJECTION) {
        iter_plan_rejection(events, spec, &uses, out)?;
    }
    if spec.detectors.contains(DETECTOR_PLAN_REENTRY) {
        iter_plan_reentry(events, spec, out)?;
    }
    if spec.detectors.contains(DETECTOR_DENIAL) {
        iter_tool_denial(events, spec, &uses, &structural, out)?;
    }
    if spec.detectors.contains(DETECTOR_INTERRUPT) {
        iter_interrupt(events, &structural, out)?;
    }
    if spec.detectors.contains(DETECTOR_REVIEW_COMMENT) {
        iter_review_comment(events, spec, &uses, out)?;
    }
    if spec.detectors.contains(DETECTOR_ASK_USER_QUESTION) {
        iter_ask_user_question(events, spec, &uses, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mining_spec(detectors: &[&str]) -> CompiledMiningSpec {
        let confidence = || CompiledConfSpec {
            stages: vec![ConfStage::Base {
                band: 0.9,
                reason: "base".into(),
            }],
        };
        CompiledMiningSpec {
            config_fingerprint: [0; 32],
            fingerprint: [0; 32],
            detectors: detectors.iter().map(|name| (*name).to_owned()).collect(),
            reentry_lookback: 40,
            edit_tools: HashSet::new(),
            plan_tools: HashSet::new(),
            denial_excluded_tools: HashSet::new(),
            subagent_tools: HashSet::new(),
            user_message: confidence(),
            calibrated: confidence(),
            review: CompiledReviewSpec {
                surfaces: HashSet::from(["typed".to_owned()]),
                regex_formats: Vec::new(),
                callable_formats: Vec::new(),
                structured_formats: Vec::new(),
            },
        }
    }

    fn index_limits() -> WorkLimits {
        WorkLimits {
            max_read_bytes: 64 * 1024 * 1024,
            max_source_read_bytes: 64 * 1024 * 1024,
            max_events: 100000,
            max_items: 100000,
            max_output_bytes: 64 * 1024 * 1024,
            max_discovery_entries: 1,
            max_sources: 1,
            deadline_unix_ms: u64::MAX,
        }
    }

    fn parsed_entry(value: Value) -> Entry {
        crate::parse::parse_bytes(&sonic_rs::to_vec(&value).unwrap(), |_| true)
            .unwrap()
            .pop()
            .unwrap()
    }

    fn user_entry(uuid: &str, text: &str) -> Entry {
        parsed_entry(sonic_rs::json!({
            "type":"user", "sessionId":"s", "uuid":uuid, "timestamp":"2026-11-02T00:00:00Z",
            "message":{"role":"user", "content":text}
        }))
    }

    fn tool_entry(uuid: &str, id: &str, name: &str, input: Value) -> Entry {
        parsed_entry(sonic_rs::json!({
            "type":"assistant", "sessionId":"s", "uuid":uuid, "timestamp":"2026-11-02T00:00:00Z",
            "message":{"role":"assistant", "model":"m", "content":[{"type":"tool_use", "id":id, "name":name, "input":input}]}
        }))
    }

    fn indexed_equals_reference(
        index: &ReviewMiningIndex,
        entries: &[Entry],
        spec: &CompiledMiningSpec,
    ) {
        let reference = mine_entries(entries.iter().collect(), spec).unwrap();
        let indexed = index.signals().unwrap().collect::<Vec<_>>();
        assert_eq!(
            sonic_rs::to_string(&indexed).unwrap(),
            sonic_rs::to_string(&reference).unwrap()
        );
    }

    #[test]
    fn indexed_plan_row_survives_unrelated_tool_result_rebinding_at_its_user_anchor() {
        let mut spec = mining_spec(&[DETECTOR_PLAN_REENTRY]);
        spec.edit_tools.insert("Edit".into());
        let mut index = ReviewMiningIndex::new(&spec).unwrap();
        let mut entries = vec![
            tool_entry("edit", "edit", "Edit", sonic_rs::json!({})),
            Entry::Mode(crate::types::ModeEntry {
                session_id: "s".into(),
                channel: crate::types::ModeChannel::Mode,
                value: "plan".into(),
            }),
            parsed_entry(sonic_rs::json!({
                "type":"user", "sessionId":"s", "uuid":"user", "timestamp":"2026-11-02T00:00:01Z",
                "message":{"role":"user", "content":[{"type":"text", "text":"please reenter planning"},{"type":"tool_result", "tool_use_id":"x", "content":"irrelevant"}]}
            })),
        ];
        index
            .append(&entries.iter().collect::<Vec<_>>(), &spec)
            .unwrap();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut MiningUsage::default(),
            )
            .unwrap();
        assert_eq!(index.signals().unwrap().count(), 1);
        indexed_equals_reference(&index, &entries, &spec);
        entries.push(tool_entry("later", "x", "Read", sonic_rs::json!({})));
        index.append(&[&entries[3]], &spec).unwrap();
        let mut usage = MiningUsage::default();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage,
            )
            .unwrap();
        assert_eq!(usage.events, 2);
        indexed_equals_reference(&index, &entries, &spec);
    }

    #[test]
    fn indexed_mining_preserves_distant_followups_and_unchanged_zero_input_work() {
        let mut spec = mining_spec(&[
            DETECTOR_TRANSCRIPT_MESSAGE,
            DETECTOR_DENIAL,
            DETECTOR_INTERRUPT,
        ]);
        spec.denial_excluded_tools.insert("ExitPlanMode".into());
        let mut index = ReviewMiningIndex::new(&spec).unwrap();
        let mut entries = vec![
            tool_entry("assistant", "edit", "Edit", sonic_rs::json!({})),
            parsed_entry(sonic_rs::json!({
                "type":"user", "sessionId":"s", "uuid":"marker", "timestamp":"2026-11-02T00:00:00Z",
                "toolDenialKind":"user-rejected", "message":{"role":"user", "content":[{
                    "type":"tool_result", "tool_use_id":"late", "is_error":true,
                    "content":"[Request interrupted by user]"
                }]}
            })),
        ];
        index
            .append(&entries.iter().collect::<Vec<_>>(), &spec)
            .unwrap();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut MiningUsage::default(),
            )
            .unwrap();
        indexed_equals_reference(&index, &entries, &spec);
        let start = entries.len();
        entries.extend((0..4096).map(|number| {
            user_entry(
                &format!("structural-{number}"),
                "[Request interrupted by user] structural followup",
            )
        }));
        index
            .append(&entries[start..].iter().collect::<Vec<_>>(), &spec)
            .unwrap();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut MiningUsage::default(),
            )
            .unwrap();
        indexed_equals_reference(&index, &entries, &spec);
        let start = entries.len();
        entries.push(user_entry(
            "correction",
            "please preserve these exact canonical words",
        ));
        index
            .append(&entries[start..].iter().collect::<Vec<_>>(), &spec)
            .unwrap();
        let mut usage = MiningUsage::default();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage,
            )
            .unwrap();
        assert_eq!(usage.events, 2);
        indexed_equals_reference(&index, &entries, &spec);
        let start = entries.len();
        entries.push(tool_entry(
            "late-use",
            "late",
            "ExitPlanMode",
            sonic_rs::json!({}),
        ));
        index
            .append(&entries[start..].iter().collect::<Vec<_>>(), &spec)
            .unwrap();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage,
            )
            .unwrap();
        assert_eq!(usage.events, 2);
        indexed_equals_reference(&index, &entries, &spec);
        index.append(&[], &spec).unwrap();
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage,
            )
            .unwrap();
        assert_eq!(
            (
                usage.events,
                usage.input_bytes,
                usage.items,
                usage.output_bytes
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn indexed_question_payloads_are_sparse_required_and_rebound_with_latest_questions() {
        let spec = mining_spec(&[DETECTOR_ASK_USER_QUESTION]);
        let mut index = ReviewMiningIndex::new(&spec).unwrap();
        let mut entries = vec![parsed_entry(sonic_rs::json!({
            "type":"user", "sessionId":"s", "uuid":"answer", "timestamp":"2026-11-02T00:00:00Z",
            "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"q", "content":"answered"}]},
            "toolUseResult":{"answers":{"Which?":"1"}}
        }))];
        index.append(&[&entries[0]], &spec).unwrap();
        assert!(index.required_payloads(&spec).unwrap().is_empty());
        index
            .process(
                &spec,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut MiningUsage::default(),
            )
            .unwrap();
        indexed_equals_reference(&index, &entries, &spec);
        for (uuid, header, label) in [("first", "first", "A"), ("second", "second", "B")] {
            entries.push(tool_entry(uuid, "q", "AskUserQuestion", sonic_rs::json!({
                "questions":[{"question":"Which?", "header":header, "options":[{"label":label}]}]
            })));
            index.append(&[entries.last().unwrap()], &spec).unwrap();
            assert_eq!(index.required_payloads(&spec).unwrap(), [0]);
            assert!(index
                .process(
                    &spec,
                    &HashMap::new(),
                    index_limits(),
                    &Cancellation::default(),
                    &mut MiningUsage::default()
                )
                .is_err());
            assert!(index.signals().is_err());
            let payloads = HashMap::from([(0, &entries[0])]);
            let mut usage = MiningUsage::default();
            index
                .process(
                    &spec,
                    &payloads,
                    index_limits(),
                    &Cancellation::default(),
                    &mut usage,
                )
                .unwrap();
            assert_eq!(usage.events, 2);
            indexed_equals_reference(&index, &entries, &spec);
        }
        let Entry::User(user) = &index.entries[0] else {
            unreachable!()
        };
        let result = user.tool_results().next().unwrap();
        assert!(result.content.is_empty());
        assert!(result.tool_use_result.is_none());
    }

    struct CountFormat(Arc<std::sync::atomic::AtomicUsize>);

    impl CallableReviewFormat for CountFormat {
        fn name(&self) -> &str {
            "count"
        }
        fn fingerprint(&self) -> [u8; 32] {
            [0; 32]
        }
        fn bounded(&self) -> bool {
            true
        }
        fn visit(
            &self,
            _text: &str,
            _bounded: bool,
            _preflight: &dyn Fn(usize) -> MiningResult<()>,
            _emit: &mut dyn FnMut(ReviewComment) -> MiningResult<()>,
        ) -> MiningResult<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    struct StopSink;

    struct IdentityFormat(u8);

    impl CallableReviewFormat for IdentityFormat {
        fn name(&self) -> &str {
            "identity"
        }
        fn fingerprint(&self) -> [u8; 32] {
            [self.0; 32]
        }
        fn bounded(&self) -> bool {
            true
        }
        fn visit(
            &self,
            _text: &str,
            _bounded: bool,
            _preflight: &dyn Fn(usize) -> MiningResult<()>,
            _emit: &mut dyn FnMut(ReviewComment) -> MiningResult<()>,
        ) -> MiningResult<()> {
            Ok(())
        }
    }

    #[test]
    fn indexed_mining_borrows_equivalent_policy_and_rejects_callback_replacement_before_mutation() {
        let mut first = mining_spec(&[DETECTOR_TRANSCRIPT_MESSAGE]);
        let mut equivalent = mining_spec(&[DETECTOR_TRANSCRIPT_MESSAGE]);
        let mut replacement = mining_spec(&[DETECTOR_TRANSCRIPT_MESSAGE]);
        set_callable_formats(&mut first, vec![Arc::new(IdentityFormat(1))]);
        set_callable_formats(&mut equivalent, vec![Arc::new(IdentityFormat(1))]);
        set_callable_formats(&mut replacement, vec![Arc::new(IdentityFormat(2))]);
        assert_eq!(first.fingerprint(), equivalent.fingerprint());
        assert_ne!(first.fingerprint(), replacement.fingerprint());
        let entry = user_entry("user", "preserve this historical candidate");
        let mut index = ReviewMiningIndex::new(&first).unwrap();
        index.append(&[&entry], &first).unwrap();
        let mut usage = MiningUsage::default();
        index
            .process(
                &equivalent,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage,
            )
            .unwrap();
        let before = sonic_rs::to_string(&index.signals().unwrap().collect::<Vec<_>>()).unwrap();
        assert!(index.append(&[], &replacement).is_err());
        assert!(index
            .process(
                &replacement,
                &HashMap::new(),
                index_limits(),
                &Cancellation::default(),
                &mut usage
            )
            .is_err());
        assert_eq!(index.event_count(), 1);
        assert_eq!(
            sonic_rs::to_string(&index.signals().unwrap().collect::<Vec<_>>()).unwrap(),
            before
        );
    }

    impl SignalSink for StopSink {
        fn push(&mut self, _signal: MiningSignal) -> MiningResult<()> {
            Err(MiningError::Invalid("sink stopped".into()))
        }
    }

    #[test]
    fn signal_sink_error_stops_before_later_detector_callbacks() {
        let raw = sonic_rs::json!({
            "type":"user", "sessionId":"s", "uuid":"u", "timestamp":"2026-11-02T00:00:00Z",
            "message":{"role":"user", "content":"ordinary user text"}
        });
        let entries =
            crate::parse::parse_bytes(&sonic_rs::to_vec(&raw).unwrap(), |_| true).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut spec = mining_spec(&[DETECTOR_TRANSCRIPT_MESSAGE, DETECTOR_REVIEW_COMMENT]);
        set_callable_formats(&mut spec, vec![Arc::new(CountFormat(Arc::clone(&calls)))]);
        let error = mine_entries_into(entries.iter().collect(), &spec, &mut StopSink).unwrap_err();
        assert!(matches!(error, MiningError::Invalid(message) if message == "sink stopped"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn question_evidence_keeps_original_field_order_and_optional_fields() {
        let questions =
            sonic_rs::json!([{"question":"Which?", "header":"Choice", "options":[{"label":"A"}]}]);
        let assistant = sonic_rs::json!({
            "type":"assistant", "sessionId":"s", "uuid":"a", "timestamp":"2026-11-02T00:00:00Z",
            "message":{"role":"assistant", "model":"m", "content":[{"type":"tool_use", "id":"q", "name":"AskUserQuestion", "input":{"questions":&questions}}]}
        });
        let user = sonic_rs::json!({
            "type":"user", "sessionId":"s", "uuid":"u", "timestamp":"2026-11-02T00:00:01Z",
            "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"q", "content":"answered"}]},
            "toolUseResult":{"questions":&questions, "answers":{"Which?":"A"}, "annotations":{"Which?":{"preview":"p", "notes":"n"}}}
        });
        let raw = format!(
            "{}\n{}\n",
            sonic_rs::to_string(&assistant).unwrap(),
            sonic_rs::to_string(&user).unwrap()
        );
        let entries = crate::parse::parse_bytes(raw.as_bytes(), |_| true).unwrap();
        let rows = mine_entries(
            entries.iter().collect(),
            &mining_spec(&[DETECTOR_ASK_USER_QUESTION]),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "n");
        assert_eq!(
            rows[0]
                .evidence
                .fields
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            [
                "question",
                "header",
                "multi_select",
                "option_pick",
                "picked_labels",
                "recommended_pick",
                "preview",
                "notes",
            ]
        );
    }

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
