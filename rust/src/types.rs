use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use sonic_rs::Value;

use crate::protocol::interrupt_marker;

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
    pub is_async: bool,
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
}

#[derive(Debug)]
pub struct AssistantEntry {
    pub meta: EntryMeta,
    pub model: String,
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug)]
pub struct SystemEntry {
    pub meta: EntryMeta,
    pub subtype: String,
    pub content: Option<String>,
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

    // Consumed at the pyo3 boundary in a later stage; only tests exercise it yet.
    #[allow(dead_code)]
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
