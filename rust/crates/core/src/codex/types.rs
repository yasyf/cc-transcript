use chrono::{DateTime, FixedOffset};
use sonic_rs::Value;

#[derive(Debug)]
pub struct CodexEntry {
    pub line_index: usize,
    pub timestamp: Option<DateTime<FixedOffset>>,
    pub item: CodexItem,
}

#[derive(Debug)]
pub enum CodexItem {
    SessionMeta(SessionMeta),
    ResponseItem(ResponseItem),
    EventMsg(EventMsg),
    TurnContext(Value),
    WorldState(WorldState),
    Compacted(Compacted),
    InterAgentCommunicationMetadata { trigger_turn: Option<bool> },
    Other(CodexOther),
}

#[derive(Debug)]
pub struct CodexOther {
    pub ty: Option<String>,
    pub raw: String,
}

#[derive(Debug)]
pub struct SessionMeta {
    pub raw: Value,
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub forked_from_id: Option<String>,
    pub thread_source: Option<String>,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    pub model_provider: Option<String>,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub instructions: Option<String>,
    pub base_instructions: Option<Value>,
    pub source: Option<Value>,
    pub git: Option<Value>,
}

#[derive(Debug)]
pub struct WorldState {
    pub full: Option<bool>,
    pub state: Value,
}

#[derive(Debug)]
pub struct Compacted {
    pub message: Option<String>,
    pub replacement_history: Option<Vec<ResponseItem>>,
    pub window_number: Option<i64>,
    pub first_window_id: Option<String>,
    pub previous_window_id: Option<String>,
    pub window_id: Option<String>,
}

#[derive(Debug)]
pub struct ResponseItem {
    pub id: Option<String>,
    pub turn_id: Option<String>,
    pub payload: ResponseItemPayload,
}

#[derive(Debug)]
pub enum ResponseItemPayload {
    Message {
        role: Option<String>,
        content: Vec<Value>,
        phase: Option<String>,
    },
    AgentMessage {
        author: Option<String>,
        recipient: Option<String>,
        content: Vec<Value>,
    },
    Reasoning {
        summary: Vec<Value>,
        content: Option<Vec<Value>>,
        encrypted_content: Option<String>,
    },
    LocalShellCall {
        call_id: Option<String>,
        status: Option<String>,
        action: Option<Value>,
    },
    FunctionCall {
        name: Option<String>,
        namespace: Option<String>,
        arguments: Option<String>,
        call_id: Option<String>,
    },
    FunctionCallOutput {
        call_id: Option<String>,
        output: Value,
    },
    CustomToolCall {
        name: Option<String>,
        namespace: Option<String>,
        input: Option<String>,
        status: Option<String>,
        call_id: Option<String>,
    },
    CustomToolCallOutput {
        call_id: Option<String>,
        name: Option<String>,
        output: Value,
    },
    ToolSearchCall {
        call_id: Option<String>,
        status: Option<String>,
        execution: Option<String>,
        arguments: Value,
    },
    ToolSearchOutput {
        call_id: Option<String>,
        status: Option<String>,
        execution: Option<String>,
        tools: Vec<Value>,
    },
    WebSearchCall {
        status: Option<String>,
        action: Option<Value>,
    },
    ImageGenerationCall {
        status: Option<String>,
        revised_prompt: Option<String>,
        result: Option<String>,
    },
    Compaction {
        encrypted_content: Option<String>,
    },
    ContextCompaction {
        encrypted_content: Option<String>,
    },
    GhostSnapshot(Value),
    Other {
        ty: Option<String>,
        raw: Value,
    },
}

#[derive(Debug)]
pub struct TokenUsage {
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

#[derive(Debug)]
pub struct TokenUsageInfo {
    pub total_token_usage: Option<TokenUsage>,
    pub last_token_usage: Option<TokenUsage>,
    pub model_context_window: Option<i64>,
}

#[derive(Debug)]
pub enum EventMsg {
    TokenCount {
        info: Option<TokenUsageInfo>,
        rate_limits: Option<Value>,
    },
    AgentMessage {
        message: Option<String>,
        phase: Option<String>,
        memory_citation: Option<Value>,
    },
    UserMessage {
        message: Option<String>,
        images: Option<Vec<Value>>,
        local_images: Option<Value>,
        text_elements: Option<Value>,
    },
    AgentReasoning {
        text: Option<String>,
    },
    AgentReasoningRawContent {
        text: Option<String>,
    },
    TaskStarted {
        turn_id: Option<String>,
        started_at: Option<i64>,
        model_context_window: Option<i64>,
        collaboration_mode_kind: Option<String>,
    },
    TaskComplete {
        turn_id: Option<String>,
        last_agent_message: Option<String>,
        completed_at: Option<i64>,
        duration_ms: Option<i64>,
        time_to_first_token_ms: Option<i64>,
    },
    TurnAborted {
        turn_id: Option<String>,
        reason: Option<String>,
        completed_at: Option<i64>,
        duration_ms: Option<i64>,
    },
    PatchApplyEnd {
        call_id: Option<String>,
        turn_id: Option<String>,
        status: Option<String>,
        success: Option<bool>,
        stdout: Option<String>,
        stderr: Option<String>,
        changes: Option<Value>,
    },
    ContextCompacted,
    SubAgentActivity {
        event_id: Option<String>,
        occurred_at_ms: Option<i64>,
        agent_thread_id: Option<String>,
        agent_path: Option<String>,
        kind: Option<String>,
    },
    ThreadRolledBack {
        num_turns: Option<i64>,
    },
    ExecCommandEnd(Value),
    McpToolCallEnd(Value),
    WebSearchEnd(Value),
    ImageGenerationEnd(Value),
    ThreadSettingsApplied(Value),
    ItemCompleted(Value),
    ThreadGoalUpdated(Value),
    EnteredReviewMode(Value),
    ExitedReviewMode(Value),
    ViewImageToolCall(Value),
    CollabAgentSpawnEnd(Value),
    CollabWaitingEnd(Value),
    CollabCloseEnd(Value),
    Other {
        ty: Option<String>,
        raw: Value,
    },
}

#[derive(Debug)]
pub struct CodexSession {
    pub entries: Vec<CodexEntry>,
    pub rollout_thread_id: Option<String>,
    pub session_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub forked_from_id: Option<String>,
    pub cli_version: Option<String>,
    pub originator: Option<String>,
    pub cwd: Option<String>,
    pub instructions: Option<String>,
    pub base_instructions: Option<Value>,
}
