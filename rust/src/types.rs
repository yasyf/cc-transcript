use std::collections::{HashMap, HashSet};

use chrono::{DateTime, FixedOffset};
use sonic_rs::Value;

use crate::protocol::{interrupt_marker, is_agent_injection};

/// Envelope metadata shared by the conversational entry kinds (user, assistant,
/// system). Mode and Other entries carry no envelope on disk.
#[derive(Debug)]
pub struct EntryMeta {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub session_id: String,
    pub timestamp: DateTime<FixedOffset>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub is_sidechain: bool,
    pub is_meta: bool,
    pub entrypoint: Option<String>,
    pub is_compact_summary: bool,
    pub is_visible_in_transcript_only: bool,
    pub user_type: Option<String>,
    pub slug: Option<String>,
}

/// The plugin/skill/MCP attribution of an assistant turn, present only when the
/// entry carries at least one of the four attribution fields.
#[derive(Debug)]
pub struct Attribution {
    pub plugin: Option<String>,
    pub skill: Option<String>,
    pub mcp_server: Option<String>,
    pub mcp_tool: Option<String>,
}

/// The upstream API error an assistant turn failed with, present only when the
/// entry's ``isApiErrorMessage`` flag is set.
#[derive(Debug)]
pub struct ApiError {
    pub error: Option<String>,
    pub status: Option<i64>,
    pub details: Option<String>,
}

/// One AskUserQuestion round, lifted from a tool-use input's ``questions``
/// array. Questions without string text are dropped — ``answered_pairs``
/// (mining/signals.py) never anchors them.
#[derive(Debug)]
pub struct Question {
    pub question: String,
    pub header: Option<String>,
    pub multi_select: bool,
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub run_in_background: Option<bool>,
    pub subagent_type: Option<String>,
    /// The target file, when the input names one (mining denial evidence).
    pub file_path: Option<String>,
    /// The AskUserQuestion rounds, when the input carries a questions array.
    pub questions: Option<Vec<Question>>,
    /// The verbatim input payload; Python receives it exactly as written.
    pub input: Value,
}

#[derive(Debug)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
    /// Computed at parse time from the payload's ``isAsync`` marker (parser.py parse_user_blocks).
    pub is_async: bool,
    /// The record-level ``toolUseResult`` payload verbatim; Python receives it exactly as written.
    pub tool_use_result: Option<Value>,
    /// The tool-denial kind, computed at parse time (parser.py parse_tool_result_block): the
    /// record-level ``toolDenialKind`` when present, else ``user-rejected`` for a legacy-banner
    /// error block, else None.
    pub denial_kind: Option<String>,
}

#[derive(Debug)]
pub struct FallbackBlock {
    pub from_model: String,
    pub to_model: String,
}

#[derive(Debug)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
    Fallback(FallbackBlock),
    Other { ty: String, raw: Value },
}

/// A user message body: the plain-string content verbatim, or its parsed blocks
/// in document order (text and tool_result blocks; other kinds are dropped, as
/// in the Python parser).
#[derive(Debug)]
pub enum UserContent {
    Plain(String),
    Blocks(Vec<ContentBlock>),
}

impl UserContent {
    /// The joined text the Python model carries: the plain content verbatim, or
    /// the space-joined text blocks.
    pub fn text(&self) -> String {
        match self {
            UserContent::Plain(s) => s.clone(),
            UserContent::Blocks(blocks) => joined_text(blocks),
        }
    }
}

#[derive(Debug)]
pub struct UserEntry {
    pub meta: EntryMeta,
    pub content: UserContent,
    pub prompt_id: Option<String>,
    pub prompt_source: Option<String>,
    pub queue_priority: Option<String>,
    pub image_paste_ids: Option<Vec<i64>>,
    pub source_tool_use_id: Option<String>,
    pub source_tool_assistant_uuid: Option<String>,
    pub mcp_meta: Option<Value>,
    pub permission_mode: Option<String>,
    pub interrupted_message_id: Option<String>,
}

impl UserEntry {
    pub fn blocks(&self) -> &[ContentBlock] {
        match &self.content {
            UserContent::Plain(_) => &[],
            UserContent::Blocks(blocks) => blocks,
        }
    }

    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResultBlock> {
        self.blocks().iter().filter_map(|b| match b {
            ContentBlock::ToolResult(tr) => Some(tr),
            _ => None,
        })
    }

    pub fn interrupted(&self) -> bool {
        interrupt_marker(&self.content.text()).is_some()
    }

    pub fn is_agent_injected(&self) -> bool {
        is_agent_injection(&self.content.text())
    }
}

#[derive(Debug)]
pub struct AssistantEntry {
    pub meta: EntryMeta,
    pub model: String,
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    pub request_id: Option<String>,
    pub forked_from: Option<String>,
    pub attribution: Option<Attribution>,
    pub api_error: Option<ApiError>,
}

#[derive(Debug)]
pub struct HookInfo {
    pub command: String,
    pub duration_ms: Option<i64>,
}

#[derive(Debug)]
pub struct StopHookSummary {
    pub hook_count: Option<i64>,
    pub hook_infos: Vec<HookInfo>,
    pub hook_errors: Vec<String>,
    pub hook_additional_context: Vec<String>,
    pub prevented_continuation: bool,
    pub stop_reason: Option<String>,
    pub has_output: bool,
    pub tool_use_id: Option<String>,
}

#[derive(Debug)]
pub struct PreservedSegment {
    pub head_uuid: Option<String>,
    pub anchor_uuid: Option<String>,
    pub tail_uuid: Option<String>,
}

#[derive(Debug)]
pub struct PreservedMessages {
    pub anchor_uuid: Option<String>,
    pub uuids: Vec<String>,
    pub all_uuids: Vec<String>,
}

#[derive(Debug)]
pub struct CompactBoundary {
    pub trigger: Option<String>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub cumulative_dropped_tokens: Option<i64>,
    pub pre_compact_discovered_tools: Vec<String>,
    pub preserved_segment: Option<PreservedSegment>,
    pub preserved_messages: Option<PreservedMessages>,
    pub logical_parent_uuid: Option<String>,
    pub precomputed: Option<bool>,
}

#[derive(Debug)]
pub struct TurnDuration {
    pub duration_ms: Option<i64>,
    pub message_count: Option<i64>,
    pub pending_workflow_count: Option<i64>,
    pub pending_background_agent_count: Option<i64>,
}

#[derive(Debug)]
pub struct ModelRefusalFallback {
    pub api_refusal_category: Option<String>,
    pub api_refusal_explanation: Option<String>,
    pub trigger: Option<String>,
    pub direction: Option<String>,
    pub original_model: Option<String>,
    pub fallback_model: Option<String>,
    pub retracted_message_uuids: Vec<String>,
    pub refused_user_message_uuid: Option<String>,
}

/// The typed detail of a system entry. Recognized subtypes carry their typed
/// struct; every other subtype carries the full record verbatim under `Other`,
/// so no system entry is lossy.
#[derive(Debug)]
pub enum SystemDetail {
    StopHookSummary(StopHookSummary),
    CompactBoundary(CompactBoundary),
    TurnDuration(TurnDuration),
    ModelRefusalFallback(ModelRefusalFallback),
    Other(Value),
}

#[derive(Debug)]
pub struct SystemEntry {
    pub meta: EntryMeta,
    pub subtype: String,
    pub content: Option<String>,
    pub level: Option<String>,
    pub detail: SystemDetail,
}

#[derive(Debug)]
pub enum ModeChannel {
    Mode,
    PermissionMode,
}

impl ModeChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModeChannel::Mode => "mode",
            ModeChannel::PermissionMode => "permission-mode",
        }
    }
}

#[derive(Debug)]
pub struct ModeEntry {
    pub session_id: String,
    pub channel: ModeChannel,
    pub value: String,
}

#[derive(Debug)]
pub struct OtherEntry {
    pub ty: String,
    /// The full decoded payload, passed through to Python verbatim.
    pub raw: Value,
}

/// One parsed JSONL transcript line. Each line is parsed exactly once into this
/// model; Python objects are materialized from it afterwards.
#[derive(Debug)]
pub enum Entry {
    User(UserEntry),
    Assistant(AssistantEntry),
    System(SystemEntry),
    Mode(ModeEntry),
    Other(OtherEntry),
}

impl Entry {
    pub fn meta(&self) -> Option<&EntryMeta> {
        match self {
            Entry::User(u) => Some(&u.meta),
            Entry::Assistant(a) => Some(&a.meta),
            Entry::System(s) => Some(&s.meta),
            Entry::Mode(_) | Entry::Other(_) => None,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Entry::Mode(m) => Some(&m.session_id),
            _ => self.meta().map(|m| m.session_id.as_str()),
        }
    }

    pub fn blocks(&self) -> &[ContentBlock] {
        match self {
            Entry::User(user) => user.blocks(),
            Entry::Assistant(a) => &a.blocks,
            _ => &[],
        }
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = &ToolUseBlock> {
        self.blocks().iter().filter_map(|b| match b {
            ContentBlock::ToolUse(tu) => Some(tu),
            _ => None,
        })
    }

    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResultBlock> {
        self.blocks().iter().filter_map(|b| match b {
            ContentBlock::ToolResult(tr) => Some(tr),
            _ => None,
        })
    }
}

/// The ``tool_use_id -> ToolUseBlock`` join over a parsed transcript
/// (filterspec.py tool_uses), last-write-wins on duplicate ids to match the
/// Python dict comprehension.
pub fn tool_use_index(entries: &[Entry]) -> HashMap<&str, &ToolUseBlock> {
    entries
        .iter()
        .flat_map(Entry::tool_uses)
        .map(|tu| (tu.id.as_str(), tu))
        .collect()
}

/// matches_names (tools.py matches_names): exact membership, or the ``<tool>``
/// segment of an ``mcp__<server>__<tool>`` name split on the first two ``__``.
/// Alias closure happens Python-side (tools.py expand_tool_names); spec JSON
/// arrives pre-expanded and the alias table never crosses the language boundary.
pub(crate) fn matches_names(actual: &str, names: &HashSet<String>) -> bool {
    names.contains(actual)
        || actual
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .is_some_and(|(_, tool)| names.contains(tool))
}

/// The space-joined text of the ``Text`` blocks — the ``text`` field of the
/// Python ``AssistantEvent``/``PrintMessage``.
pub fn joined_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug)]
pub struct CacheCreation {
    pub ephemeral_5m_input_tokens: i64,
    pub ephemeral_1h_input_tokens: i64,
}

#[derive(Debug)]
pub struct ServerToolUse {
    pub web_search_requests: i64,
    pub web_fetch_requests: i64,
}

#[derive(Debug)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_creation: Option<CacheCreation>,
    pub service_tier: Option<String>,
    pub inference_geo: Option<String>,
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug)]
pub struct ModelUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub web_search_requests: i64,
    pub cost_usd: f64,
    pub context_window: i64,
    pub max_output_tokens: i64,
}

#[derive(Debug)]
pub struct McpServer {
    pub name: String,
    pub status: String,
}

#[derive(Debug)]
pub struct Plugin {
    pub name: String,
    pub path: String,
    pub source: String,
}

#[derive(Debug)]
pub struct InitInfo {
    pub mcp_servers: Vec<McpServer>,
    pub plugins: Vec<Plugin>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
}

#[derive(Debug)]
pub enum PrintBody {
    User(UserContent),
    Assistant {
        model: Option<String>,
        blocks: Vec<ContentBlock>,
    },
}

#[derive(Debug)]
pub struct PrintMessage {
    pub body: PrintBody,
    pub uuid: Option<String>,
    pub session_id: String,
}

/// A parsed ``--print`` envelope: the result element plus the optional init
/// element and the conversational messages.
#[derive(Debug)]
pub struct PrintResult {
    pub total_cost_usd: f64,
    pub model_usage: Vec<(String, ModelUsage)>,
    pub usage: Usage,
    pub structured_output: Option<Value>,
    pub num_turns: i64,
    pub is_error: bool,
    pub result: Option<String>,
    pub session_id: String,
    pub fast_mode_state: Option<String>,
    pub stop_reason: Option<String>,
    pub permission_denials: Vec<Value>,
    pub init: Option<InitInfo>,
    pub messages: Vec<PrintMessage>,
}
