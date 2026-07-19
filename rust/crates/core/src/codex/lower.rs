use std::collections::HashSet;

use chrono::{DateTime, FixedOffset};
use sha2::{Digest, Sha256};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::codex::protocol::{injection_wrapper, mcp_tool_name, output_exit_code};
use crate::codex::types::{
    CodexEntry, CodexItem, CodexSession, EventMsg, ResponseItem, ResponseItemPayload,
};
use crate::pystr;
use crate::types::{
    AssistantEntry, ContentBlock, Entry, EntryMeta, OtherEntry, ToolResultBlock, ToolUseBlock,
    UserContent, UserEntry,
};
use crate::value::field_str;

const UUID_NAMESPACE: &str = "cc-transcript-codex";
const MODEL_PLACEHOLDER: &str = "unknown";

/// The session-level token totals lowered from a codex rollout: the LAST observed
/// ``total_token_usage`` fields plus ``model_context_window`` and the count of
/// ``token_count`` events. Per-message usage stays ``None`` on every lowered
/// assistant entry — codex ``TokenCount`` totals are cumulative, not deltas.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodexUsageAggregate {
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub model_context_window: Option<i64>,
    pub token_count_events: usize,
}

/// A lowered codex session: the native CC entries (one per pre-filter line, order
/// preserved) that the whole lifted layer consumes unchanged, plus the session
/// token aggregate.
#[derive(Debug)]
pub struct CodexLowering {
    pub entries: Vec<Entry>,
    pub usage: CodexUsageAggregate,
}

/// Lowers a parsed codex rollout into the native CC `Entry` model. Every line
/// lowers to exactly one entry so `activity`/`query`/`render` materialize codex
/// turns from disk truth without a re-parse.
pub fn lower(session: &CodexSession) -> CodexLowering {
    let thread_id = session.rollout_thread_id.clone().unwrap_or_default();
    let (user_texts, assistant_texts) = echo_sets(session);
    let models = model_timeline(session);
    let usage = usage_aggregate(session);

    let mut cwd = session.cwd.clone();
    let mut entries = Vec::with_capacity(session.entries.len());
    for (pos, entry) in session.entries.iter().enumerate() {
        if let CodexItem::TurnContext(v) = &entry.item {
            if let Some(next) = field_str(v, "cwd") {
                cwd = Some(next.to_string());
            }
        }
        let prev = pos.checked_sub(1).map(|i| &session.entries[i]);
        entries.push(lower_entry(
            entry,
            pos,
            prev,
            &thread_id,
            cwd.as_deref(),
            &models,
            &user_texts,
            &assistant_texts,
        ));
    }
    CodexLowering { entries, usage }
}

fn echo_sets(session: &CodexSession) -> (HashSet<String>, HashSet<String>) {
    let mut user = HashSet::new();
    let mut assistant = HashSet::new();
    for entry in &session.entries {
        if let CodexItem::ResponseItem(ri) = &entry.item {
            if let ResponseItemPayload::Message { role, content, .. } = &ri.payload {
                match role.as_deref() {
                    Some("user") => {
                        user.insert(blocks_text(content));
                    }
                    Some("assistant") => {
                        assistant.insert(blocks_text(content));
                    }
                    _ => {}
                }
            }
        }
    }
    (user, assistant)
}

fn model_timeline(session: &CodexSession) -> Vec<(usize, String)> {
    session
        .entries
        .iter()
        .enumerate()
        .filter_map(|(pos, entry)| match &entry.item {
            CodexItem::TurnContext(v) => field_str(v, "model").map(|m| (pos, m.to_string())),
            _ => None,
        })
        .collect()
}

fn model_at(models: &[(usize, String)], pos: usize) -> String {
    models
        .iter()
        .rev()
        .find(|(idx, _)| *idx <= pos)
        .or_else(|| models.iter().find(|(idx, _)| *idx > pos))
        .map(|(_, model)| model.clone())
        .unwrap_or_else(|| MODEL_PLACEHOLDER.to_string())
}

fn usage_aggregate(session: &CodexSession) -> CodexUsageAggregate {
    let mut agg = CodexUsageAggregate::default();
    for entry in &session.entries {
        if let CodexItem::EventMsg(EventMsg::TokenCount { info, .. }) = &entry.item {
            agg.token_count_events += 1;
            let Some(info) = info else { continue };
            if let Some(total) = &info.total_token_usage {
                agg.input_tokens = total.input_tokens;
                agg.cached_input_tokens = total.cached_input_tokens;
                agg.output_tokens = total.output_tokens;
                agg.reasoning_output_tokens = total.reasoning_output_tokens;
                agg.total_tokens = total.total_tokens;
            }
            if info.model_context_window.is_some() {
                agg.model_context_window = info.model_context_window;
            }
        }
    }
    agg
}

#[allow(clippy::too_many_arguments)]
fn lower_entry(
    entry: &CodexEntry,
    pos: usize,
    prev: Option<&CodexEntry>,
    thread_id: &str,
    cwd: Option<&str>,
    models: &[(usize, String)],
    user_texts: &HashSet<String>,
    assistant_texts: &HashSet<String>,
) -> Entry {
    let Some(ts) = entry.timestamp else {
        return item_other(&entry.item);
    };
    let line = entry.line_index;
    match &entry.item {
        CodexItem::ResponseItem(ri) => {
            let uuid = event_uuid(thread_id, Some(ri), line);
            lower_payload(
                &ri.payload,
                prev.filter(|previous| previous.line_index + 1 == line)
                    .map(|previous| &previous.item),
                uuid,
                thread_id,
                ts,
                cwd,
                model_at(models, pos),
            )
        }
        CodexItem::EventMsg(em) => lower_event(
            em,
            event_uuid(thread_id, None, line),
            thread_id,
            ts,
            cwd,
            model_at(models, pos),
            user_texts,
            assistant_texts,
        ),
        CodexItem::Compacted(c) => user_entry(
            UserContent::Plain(c.message.clone().unwrap_or_default()),
            meta(
                event_uuid(thread_id, None, line),
                thread_id,
                ts,
                cwd,
                false,
                true,
            ),
            None,
        ),
        _ => item_other(&entry.item),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_payload(
    payload: &ResponseItemPayload,
    prev: Option<&CodexItem>,
    uuid: String,
    thread_id: &str,
    ts: DateTime<FixedOffset>,
    cwd: Option<&str>,
    model: String,
) -> Entry {
    use ResponseItemPayload as R;
    match payload {
        R::Message { role, content, .. } => match role.as_deref() {
            Some("assistant") => assistant_entry(
                model,
                vec![ContentBlock::Text(blocks_text(content))],
                meta(uuid, thread_id, ts, cwd, false, false),
            ),
            Some("developer") => user_entry(
                UserContent::Plain(blocks_text(content)),
                meta(uuid, thread_id, ts, cwd, true, false),
                None,
            ),
            Some("user") => {
                let text = blocks_text(content);
                let is_meta = injection_wrapper(&text).is_some();
                user_entry(
                    UserContent::Plain(text),
                    meta(uuid, thread_id, ts, cwd, is_meta, false),
                    None,
                )
            }
            _ => payload_other(payload),
        },
        R::AgentMessage { content, .. } => {
            let opens = matches!(
                prev,
                Some(CodexItem::InterAgentCommunicationMetadata {
                    trigger_turn: Some(true)
                })
            );
            user_entry(
                UserContent::Plain(blocks_text(content)),
                meta(uuid, thread_id, ts, cwd, !opens, false),
                None,
            )
        }
        R::Reasoning { summary, .. } => {
            let text = blocks_text(summary);
            if pystr::strip(&text).is_empty() {
                payload_other(payload)
            } else {
                assistant_entry(
                    model,
                    vec![ContentBlock::Thinking(text)],
                    meta(uuid, thread_id, ts, cwd, false, false),
                )
            }
        }
        R::FunctionCall {
            name,
            namespace,
            arguments: Some(arguments),
            call_id,
        } => tool_use_entry(
            call_id.clone().unwrap(),
            synth_name(name.as_deref().unwrap(), namespace.as_deref()),
            Value::from(arguments.as_str()),
            meta(uuid, thread_id, ts, cwd, false, false),
            model,
        ),
        R::CustomToolCall {
            name,
            input: Some(input),
            call_id,
            ..
        } => tool_use_entry(
            call_id.clone().unwrap(),
            name.clone().unwrap(),
            Value::from(input.as_str()),
            meta(uuid, thread_id, ts, cwd, false, false),
            model,
        ),
        R::FunctionCallOutput { call_id, output }
        | R::CustomToolCallOutput {
            call_id, output, ..
        } if !output.is_null() => tool_result_entry(
            call_id.clone().unwrap(),
            output,
            meta(uuid, thread_id, ts, cwd, false, false),
        ),
        _ => payload_other(payload),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_event(
    em: &EventMsg,
    uuid: String,
    thread_id: &str,
    ts: DateTime<FixedOffset>,
    cwd: Option<&str>,
    model: String,
    user_texts: &HashSet<String>,
    assistant_texts: &HashSet<String>,
) -> Entry {
    match em {
        EventMsg::UserMessage { message, .. } => {
            let text = message.clone().unwrap_or_default();
            if user_texts.contains(&text) {
                event_other(em)
            } else {
                user_entry(
                    UserContent::Plain(text),
                    meta(uuid, thread_id, ts, cwd, false, false),
                    None,
                )
            }
        }
        EventMsg::AgentMessage { message, .. } => {
            let text = message.clone().unwrap_or_default();
            if assistant_texts.contains(&text) {
                event_other(em)
            } else {
                assistant_entry(
                    model,
                    vec![ContentBlock::Text(text)],
                    meta(uuid, thread_id, ts, cwd, false, false),
                )
            }
        }
        EventMsg::TurnAborted { turn_id, .. } => {
            let interrupted = turn_id
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| uuid.clone());
            user_entry(
                UserContent::Plain(String::new()),
                meta(uuid, thread_id, ts, cwd, false, false),
                Some(interrupted),
            )
        }
        _ => event_other(em),
    }
}

fn synth_name(name: &str, namespace: Option<&str>) -> String {
    mcp_tool_name(name, namespace).unwrap_or_else(|| name.to_string())
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| field_str(block, "text"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn flatten_output(output: &Value) -> String {
    if let Some(text) = output.as_str() {
        return sonic_rs::from_str::<Value>(text)
            .ok()
            .and_then(|v| field_str(&v, "output").map(str::to_string))
            .unwrap_or_else(|| text.to_string());
    }
    if let Some(array) = output.as_array() {
        return array
            .iter()
            .filter_map(|block| field_str(block, "text"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    field_str(output, "output")
        .map(str::to_string)
        .unwrap_or_else(|| sonic_rs::to_string(output).unwrap_or_default())
}

fn event_uuid(thread_id: &str, response_item: Option<&ResponseItem>, line_index: usize) -> String {
    let item_id = response_item.and_then(|item| match &item.payload {
        ResponseItemPayload::FunctionCallOutput { .. }
        | ResponseItemPayload::CustomToolCallOutput { .. } => None,
        _ => item.id.as_deref(),
    });
    let input = match item_id {
        Some(id) => format!("{UUID_NAMESPACE}:{thread_id}:{id}"),
        None => format!("{UUID_NAMESPACE}:{thread_id}:line:{line_index}"),
    };
    uuid_from_digest(&Sha256::digest(input.as_bytes()))
}

fn uuid_from_digest(digest: &[u8]) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14],
        b[15]
    )
}

fn meta(
    uuid: String,
    thread_id: &str,
    ts: DateTime<FixedOffset>,
    cwd: Option<&str>,
    is_meta: bool,
    is_compact_summary: bool,
) -> EntryMeta {
    EntryMeta {
        uuid,
        parent_uuid: None,
        session_id: thread_id.to_string(),
        timestamp: ts,
        cwd: cwd.map(str::to_string),
        git_branch: None,
        version: None,
        is_sidechain: false,
        is_meta,
        entrypoint: None,
        is_compact_summary,
        is_visible_in_transcript_only: false,
        user_type: None,
        slug: None,
    }
}

fn user_entry(
    content: UserContent,
    meta: EntryMeta,
    interrupted_message_id: Option<String>,
) -> Entry {
    Entry::User(UserEntry {
        meta,
        content,
        prompt_id: None,
        prompt_source: None,
        queue_priority: None,
        image_paste_ids: None,
        source_tool_use_id: None,
        source_tool_assistant_uuid: None,
        mcp_meta: None,
        permission_mode: None,
        interrupted_message_id,
    })
}

fn assistant_entry(model: String, blocks: Vec<ContentBlock>, meta: EntryMeta) -> Entry {
    Entry::Assistant(AssistantEntry {
        meta,
        model,
        blocks,
        stop_reason: None,
        usage: None,
        request_id: None,
        forked_from: None,
        attribution: None,
        api_error: None,
    })
}

fn tool_use_entry(id: String, name: String, input: Value, meta: EntryMeta, model: String) -> Entry {
    assistant_entry(
        model,
        vec![ContentBlock::ToolUse(ToolUseBlock {
            id,
            name,
            run_in_background: None,
            subagent_type: None,
            file_path: None,
            questions: None,
            input,
        })],
        meta,
    )
}

fn tool_result_entry(tool_use_id: String, output: &Value, meta: EntryMeta) -> Entry {
    let tool_use_result = (output.as_str().is_none() && !output.is_null()).then(|| output.clone());
    user_entry(
        UserContent::Blocks(vec![ContentBlock::ToolResult(ToolResultBlock {
            tool_use_id,
            content: flatten_output(output),
            is_error: output_exit_code(output).is_some_and(|code| code != 0),
            is_async: false,
            tool_use_result,
            denial_kind: None,
        })]),
        meta,
        None,
    )
}

fn item_other(item: &CodexItem) -> Entry {
    let (tag, raw) = match item {
        CodexItem::SessionMeta(m) => ("session_meta".to_string(), m.raw.clone()),
        CodexItem::TurnContext(v) => ("turn_context".to_string(), v.clone()),
        CodexItem::WorldState(w) => ("world_state".to_string(), w.state.clone()),
        CodexItem::InterAgentCommunicationMetadata { trigger_turn } => (
            "inter_agent_communication_metadata".to_string(),
            sonic_rs::json!({ "trigger_turn": *trigger_turn }),
        ),
        CodexItem::Compacted(_) => ("compacted".to_string(), Value::default()),
        CodexItem::ResponseItem(ri) => return payload_other(&ri.payload),
        CodexItem::EventMsg(em) => return event_other(em),
        CodexItem::Other(o) => (
            o.ty.clone().unwrap_or_else(|| "other".to_string()),
            sonic_rs::from_str(&o.raw).unwrap_or_else(|_| Value::from(o.raw.as_str())),
        ),
    };
    Entry::Other(OtherEntry {
        ty: format!("codex-{tag}"),
        raw,
    })
}

fn payload_other(payload: &ResponseItemPayload) -> Entry {
    Entry::Other(OtherEntry {
        ty: format!("codex-{}", payload_tag(payload)),
        raw: payload_raw(payload),
    })
}

fn event_other(em: &EventMsg) -> Entry {
    Entry::Other(OtherEntry {
        ty: format!("codex-{}", event_tag(em)),
        raw: event_raw(em),
    })
}

fn payload_tag(payload: &ResponseItemPayload) -> &str {
    use ResponseItemPayload as R;
    match payload {
        R::Message { .. } => "message",
        R::AgentMessage { .. } => "agent_message",
        R::Reasoning { .. } => "reasoning",
        R::LocalShellCall { .. } => "local_shell_call",
        R::FunctionCall { .. } => "function_call",
        R::FunctionCallOutput { .. } => "function_call_output",
        R::CustomToolCall { .. } => "custom_tool_call",
        R::CustomToolCallOutput { .. } => "custom_tool_call_output",
        R::ToolSearchCall { .. } => "tool_search_call",
        R::ToolSearchOutput { .. } => "tool_search_output",
        R::WebSearchCall { .. } => "web_search_call",
        R::ImageGenerationCall { .. } => "image_generation_call",
        R::Compaction { .. } => "compaction",
        R::ContextCompaction { .. } => "context_compaction",
        R::GhostSnapshot(_) => "ghost_snapshot",
        R::Other { ty, .. } => ty.as_deref().unwrap_or("other"),
    }
}

fn payload_raw(payload: &ResponseItemPayload) -> Value {
    use ResponseItemPayload as R;
    match payload {
        R::ToolSearchCall { arguments, .. } => arguments.clone(),
        R::LocalShellCall { action, .. } | R::WebSearchCall { action, .. } => {
            action.clone().unwrap_or_default()
        }
        R::GhostSnapshot(v) => v.clone(),
        R::Other { raw, .. } => raw.clone(),
        _ => Value::default(),
    }
}

fn event_tag(em: &EventMsg) -> &str {
    use EventMsg as E;
    match em {
        E::TokenCount { .. } => "token_count",
        E::AgentMessage { .. } => "agent_message",
        E::UserMessage { .. } => "user_message",
        E::AgentReasoning { .. } => "agent_reasoning",
        E::AgentReasoningRawContent { .. } => "agent_reasoning_raw_content",
        E::TaskStarted { .. } => "task_started",
        E::TaskComplete { .. } => "task_complete",
        E::TurnAborted { .. } => "turn_aborted",
        E::PatchApplyEnd { .. } => "patch_apply_end",
        E::ContextCompacted => "context_compacted",
        E::SubAgentActivity { .. } => "sub_agent_activity",
        E::ThreadRolledBack { .. } => "thread_rolled_back",
        E::ExecCommandEnd(_) => "exec_command_end",
        E::McpToolCallEnd(_) => "mcp_tool_call_end",
        E::WebSearchEnd(_) => "web_search_end",
        E::ImageGenerationEnd(_) => "image_generation_end",
        E::ThreadSettingsApplied(_) => "thread_settings_applied",
        E::ItemCompleted(_) => "item_completed",
        E::ThreadGoalUpdated(_) => "thread_goal_updated",
        E::EnteredReviewMode(_) => "entered_review_mode",
        E::ExitedReviewMode(_) => "exited_review_mode",
        E::ViewImageToolCall(_) => "view_image_tool_call",
        E::CollabAgentSpawnEnd(_) => "collab_agent_spawn_end",
        E::CollabWaitingEnd(_) => "collab_waiting_end",
        E::CollabCloseEnd(_) => "collab_close_end",
        E::Other { ty, .. } => ty.as_deref().unwrap_or("other"),
    }
}

fn event_raw(em: &EventMsg) -> Value {
    use EventMsg as E;
    match em {
        E::ExecCommandEnd(v)
        | E::McpToolCallEnd(v)
        | E::WebSearchEnd(v)
        | E::ImageGenerationEnd(v)
        | E::ThreadSettingsApplied(v)
        | E::ItemCompleted(v)
        | E::ThreadGoalUpdated(v)
        | E::EnteredReviewMode(v)
        | E::ExitedReviewMode(v)
        | E::ViewImageToolCall(v)
        | E::CollabAgentSpawnEnd(v)
        | E::CollabWaitingEnd(v)
        | E::CollabCloseEnd(v)
        | E::Other { raw: v, .. } => v.clone(),
        _ => Value::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::activity::{lift_session, Turn};
    use crate::codex::parse_codex_bytes;

    const ALL_TAGS: &[&str] = &[
        "101", "202", "060f", "303", "404", "050a", "050b", "050c", "050d", "050e",
    ];

    const UUID_CASES: &[(&str, Option<&str>, usize)] = &[
        (
            "019f67f0-2a3b-7c4d-8e5f-000000000303",
            Some("rs_demo000000000000000000000000000000000001"),
            0,
        ),
        (
            "019f67f0-2a3b-7c4d-8e5f-000000000303",
            Some("ctc_demo00000000000000000000000000000001"),
            0,
        ),
        ("019f67f0-2a3b-7c4d-8e5f-000000000303", None, 5),
        ("019bd9c0-0a1b-7c2d-8e3f-000000000101", None, 0),
        ("019f6800-3b4c-7d5e-9f60-000000000404", None, 13),
        ("", None, 0),
    ];

    fn testdata_dir() -> PathBuf {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/testdata/codex"
        ))
        .to_path_buf()
    }

    fn fixture_bytes(tag: &str) -> Vec<u8> {
        let suffix = format!("{tag}.jsonl");
        let entry = std::fs::read_dir(testdata_dir())
            .expect("codex testdata dir")
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().ends_with(&suffix))
            .unwrap_or_else(|| panic!("fixture {tag} not found"));
        std::fs::read(entry.path()).expect("read fixture")
    }

    fn session(tag: &str) -> CodexSession {
        parse_codex_bytes(&fixture_bytes(tag))
    }

    fn sha_prefix(text: &str) -> String {
        Sha256::digest(text.as_bytes())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn assistant_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) | ContentBlock::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn block_descs(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text(_) => "Text".to_string(),
                ContentBlock::Thinking(_) => "Thinking".to_string(),
                ContentBlock::ToolUse(t) => format!("ToolUse:{}:{}", t.name, t.id),
                ContentBlock::ToolResult(r) => {
                    format!("ToolResult:{}:err={}", r.tool_use_id, r.is_error)
                }
                ContentBlock::Fallback(_) => "Fallback".to_string(),
                ContentBlock::Other { ty, .. } => format!("Other:{ty}"),
            })
            .collect()
    }

    fn tool_use_input_shas(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(tool) => Some(sha_prefix(
                    &crate::ids::canonical_json(&tool.input).unwrap(),
                )),
                _ => None,
            })
            .collect()
    }

    fn tool_result_content_shas(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(sha_prefix(&result.content)),
                _ => None,
            })
            .collect()
    }

    fn tool_result_errors(blocks: &[ContentBlock]) -> Vec<bool> {
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result.is_error),
                _ => None,
            })
            .collect()
    }

    fn describe(tag: &str, line: usize, entry: &Entry) -> Value {
        match entry {
            Entry::User(u) => sonic_rs::json!({
                "fixture": tag,
                "line": line,
                "kind": "User",
                "uuid": u.meta.uuid,
                "session_id": u.meta.session_id,
                "timestamp_epoch_ms": crate::types::epoch_ms(u.meta.timestamp),
                "cwd": u.meta.cwd,
                "parent_uuid": u.meta.parent_uuid,
                "is_meta": u.meta.is_meta,
                "is_sidechain": u.meta.is_sidechain,
                "is_compact_summary": u.meta.is_compact_summary,
                "interrupted": u.interrupted(),
                "model": Option::<String>::None,
                "blocks": block_descs(u.blocks()),
                "tool_use_input_sha": tool_use_input_shas(u.blocks()),
                "tool_result_content_sha": tool_result_content_shas(u.blocks()),
                "tool_result_is_error": tool_result_errors(u.blocks()),
                "text_sha": sha_prefix(&u.content.text()),
                "ty": Option::<String>::None,
                "usage": Option::<Value>::None,
            }),
            Entry::Assistant(a) => {
                assert!(a.usage.is_none());
                sonic_rs::json!({
                    "fixture": tag,
                    "line": line,
                    "kind": "Assistant",
                    "uuid": a.meta.uuid,
                    "session_id": a.meta.session_id,
                    "timestamp_epoch_ms": crate::types::epoch_ms(a.meta.timestamp),
                    "cwd": a.meta.cwd,
                    "parent_uuid": a.meta.parent_uuid,
                    "is_meta": a.meta.is_meta,
                    "is_sidechain": a.meta.is_sidechain,
                    "is_compact_summary": a.meta.is_compact_summary,
                    "interrupted": false,
                    "model": Some(a.model.clone()),
                    "blocks": block_descs(&a.blocks),
                    "tool_use_input_sha": tool_use_input_shas(&a.blocks),
                    "tool_result_content_sha": tool_result_content_shas(&a.blocks),
                    "tool_result_is_error": tool_result_errors(&a.blocks),
                    "text_sha": sha_prefix(&assistant_text(&a.blocks)),
                    "ty": Option::<String>::None,
                    "usage": Option::<Value>::None,
                })
            }
            Entry::Other(o) => sonic_rs::json!({
                "fixture": tag,
                "line": line,
                "kind": "Other",
                "uuid": Option::<String>::None,
                "session_id": Option::<String>::None,
                "timestamp_epoch_ms": Option::<i64>::None,
                "cwd": Option::<String>::None,
                "parent_uuid": Option::<String>::None,
                "is_meta": false,
                "is_sidechain": false,
                "is_compact_summary": false,
                "interrupted": false,
                "model": Option::<String>::None,
                "blocks": Vec::<String>::new(),
                "tool_use_input_sha": Vec::<String>::new(),
                "tool_result_content_sha": Vec::<String>::new(),
                "tool_result_is_error": Vec::<bool>::new(),
                "text_sha": Option::<String>::None,
                "ty": Some(o.ty.clone()),
                "usage": Option::<Value>::None,
            }),
            other => unreachable!("codex lowering never emits {other:?}"),
        }
    }

    fn golden_lines() -> String {
        let mut lines = Vec::new();
        for tag in ALL_TAGS {
            let session = session(tag);
            let lowered = lower(&session);
            assert_eq!(
                session.entries.len(),
                lowered.entries.len(),
                "one lowered entry per line ({tag})"
            );
            for (raw, entry) in session.entries.iter().zip(&lowered.entries) {
                lines.push(
                    crate::ids::canonical_json(&describe(tag, raw.line_index, entry)).unwrap(),
                );
            }
        }
        lines.join("\n")
    }

    #[test]
    fn lowering_golden_matches_frozen_corpus() {
        let actual = golden_lines();
        let path = testdata_dir().join("lowering_golden.json");
        if std::env::var("REGEN_CODEX_GOLDEN").is_ok() {
            std::fs::write(&path, format!("{actual}\n")).expect("write golden");
        }
        let expected = std::fs::read_to_string(&path).expect("read lowering golden");
        assert_eq!(actual, expected.trim_end_matches('\n'));
    }

    #[test]
    fn event_uuid_matches_frozen_corpus() {
        let mut rows = UUID_CASES
            .iter()
            .map(|(thread_id, item_id, line_index)| {
                let response_item = item_id.map(|id| ResponseItem {
                    id: Some(id.to_string()),
                    turn_id: None,
                    payload: ResponseItemPayload::Reasoning {
                        summary: Vec::new(),
                        content: None,
                        encrypted_content: None,
                    },
                });
                let uuid = event_uuid(thread_id, response_item.as_ref(), *line_index);
                assert_eq!(uuid.len(), 36, "rfc4122 shape");
                assert_eq!(uuid.as_bytes()[14], b'8', "version nibble 8");
                let input = match item_id {
                    Some(id) => format!("{UUID_NAMESPACE}:{thread_id}:{id}"),
                    None => format!("{UUID_NAMESPACE}:{thread_id}:line:{line_index}"),
                };
                crate::ids::canonical_json(&sonic_rs::json!({ "input": input, "uuid": uuid }))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let output_thread = "output-thread";
        let outputs = [
            ResponseItem {
                id: Some("shared-output-id".to_string()),
                turn_id: None,
                payload: ResponseItemPayload::FunctionCallOutput {
                    call_id: Some("call-function".to_string()),
                    output: Value::from("ok"),
                },
            },
            ResponseItem {
                id: Some("shared-output-id".to_string()),
                turn_id: None,
                payload: ResponseItemPayload::CustomToolCallOutput {
                    call_id: Some("call-custom".to_string()),
                    name: Some("exec".to_string()),
                    output: Value::from("ok"),
                },
            },
        ];
        for (line_index, output) in (6..).zip(&outputs) {
            let uuid = event_uuid(output_thread, Some(output), line_index);
            assert_eq!(uuid.len(), 36, "rfc4122 shape");
            assert_eq!(uuid.as_bytes()[14], b'8', "version nibble 8");
            let input = format!("{UUID_NAMESPACE}:{output_thread}:line:{line_index}");
            rows.push(
                crate::ids::canonical_json(&sonic_rs::json!({ "input": input, "uuid": uuid }))
                    .unwrap(),
            );
        }
        let actual = rows.join("\n");
        let path = testdata_dir().join("uuid_fixtures.json");
        if std::env::var("REGEN_CODEX_GOLDEN").is_ok() {
            std::fs::write(&path, format!("{actual}\n")).expect("write uuid fixtures");
        }
        let expected = std::fs::read_to_string(&path).expect("read uuid fixtures");
        assert_eq!(actual, expected.trim_end_matches('\n'));
    }

    #[test]
    fn lowered_uuids_unique_within_each_file() {
        for tag in ALL_TAGS {
            let lowered = lower(&session(tag));
            let uuids: Vec<&str> = lowered
                .entries
                .iter()
                .filter_map(|e| e.meta().map(|m| m.uuid.as_str()))
                .collect();
            let unique: HashSet<&str> = uuids.iter().copied().collect();
            assert_eq!(uuids.len(), unique.len(), "duplicate lowered uuid in {tag}");
        }
    }

    fn zip_lower(tag: &str) -> (CodexSession, Vec<Entry>) {
        let session = session(tag);
        let lowered = lower(&session);
        (session, lowered.entries)
    }

    fn lower_inline(input: &str) -> Vec<Entry> {
        lower(&parse_codex_bytes(input.as_bytes())).entries
    }

    #[test]
    fn unknown_tags_keep_their_stored_type() {
        let entries = lower_inline(concat!(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"response_item","payload":{"type":"future_response","value":1}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:01Z","type":"event_msg","payload":{"type":"future_response","value":2}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:02Z","type":"future_response","payload":{"value":3}}"#,
        ));
        let types = entries
            .iter()
            .map(|entry| match entry {
                Entry::Other(other) => other.ty.as_str(),
                other => panic!("future_response must lower to Other, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "codex-future_response",
                "codex-future_response",
                "codex-future_response"
            ]
        );
    }

    #[test]
    fn unsupported_message_roles_lower_to_other() {
        let entries = lower_inline(concat!(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"input_text","text":"system"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:01Z","type":"response_item","payload":{"type":"message","content":[{"type":"input_text","text":"missing"}]}}"#,
        ));
        assert!(entries
            .iter()
            .all(|entry| matches!(entry, Entry::Other(other) if other.ty == "codex-message")));
    }

    #[test]
    fn custom_tool_call_name_stays_verbatim_with_namespace() {
        let entries = lower_inline(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc-demo","call_id":"call-demo","name":"exec","namespace":"demo","input":"pwd"}}"#,
        );
        let tool = entries[0].tool_uses().next().expect("custom tool use");
        assert_eq!(tool.name, "exec");
    }

    #[test]
    fn absent_tool_payloads_lower_to_other() {
        let entries = lower_inline(concat!(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"response_item","payload":{"type":"function_call","call_id":"call-function","name":"exec_command"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:01Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-custom","name":"exec"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-function"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-19T00:00:03Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-custom","output":null}}"#,
        ));
        let types = entries
            .iter()
            .map(|entry| match entry {
                Entry::Other(other) => other.ty.as_str(),
                other => panic!("missing tool payload must lower to Other, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "codex-function_call",
                "codex-custom_tool_call",
                "codex-function_call_output",
                "codex-custom_tool_call_output"
            ]
        );
    }

    #[test]
    fn inter_agent_pairing_requires_physical_adjacency() {
        let input = concat!(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}"#,
            "\n\n",
            r#"{"timestamp":"2026-07-19T00:00:01Z","type":"response_item","payload":{"type":"agent_message","content":[{"type":"input_text","text":"not adjacent"}]}}"#,
        );
        let session = parse_codex_bytes(input.as_bytes());
        assert_eq!(session.entries[1].line_index, 2);
        let entries = lower(&session).entries;
        let Entry::User(user) = &entries[1] else {
            panic!("agent_message must lower to User");
        };
        assert!(user.meta.is_meta);
    }

    #[test]
    fn nonzero_function_call_output_is_error() {
        let entries = lower_inline(
            r#"{"timestamp":"2026-07-19T00:00:00Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-failed","output":"{\"output\":\"failed\",\"metadata\":{\"exit_code\":9}}"}}"#,
        );
        let result = entries[0]
            .tool_results()
            .next()
            .expect("function call result");
        assert!(result.is_error);
    }

    #[test]
    fn usage_aggregate_matches_fixture_totals() {
        assert_eq!(
            lower(&session("101")).usage,
            CodexUsageAggregate {
                input_tokens: None,
                cached_input_tokens: None,
                output_tokens: None,
                reasoning_output_tokens: None,
                total_tokens: None,
                model_context_window: None,
                token_count_events: 1,
            }
        );
        assert_eq!(
            lower(&session("303")).usage,
            CodexUsageAggregate {
                input_tokens: Some(1200),
                cached_input_tokens: Some(800),
                output_tokens: Some(240),
                reasoning_output_tokens: Some(128),
                total_tokens: Some(1440),
                model_context_window: Some(272000),
                token_count_events: 1,
            }
        );
    }

    #[test]
    fn echo_dedup_twins_user_message_untwinned_agent_message() {
        let (session, entries) = zip_lower("303");
        for (raw, entry) in session.entries.iter().zip(&entries) {
            if matches!(&raw.item, CodexItem::EventMsg(EventMsg::UserMessage { .. })) {
                assert!(
                    matches!(entry, Entry::Other(_)),
                    "303 twinned user_message must lower to Other"
                );
            }
        }
        let (session, entries) = zip_lower("404");
        let mut saw_untwinned = false;
        for (raw, entry) in session.entries.iter().zip(&entries) {
            if matches!(
                &raw.item,
                CodexItem::EventMsg(EventMsg::AgentMessage { .. })
            ) {
                assert!(
                    matches!(entry, Entry::Assistant(_)),
                    "404 un-twinned agent_message must lower to Assistant[Text]"
                );
                saw_untwinned = true;
            }
        }
        assert!(
            saw_untwinned,
            "404 must carry un-twinned agent_message echoes"
        );
    }

    #[test]
    fn inter_agent_agent_message_opens_a_turn() {
        let (session, entries) = zip_lower("404");
        let mut checked = false;
        for (raw, entry) in session.entries.iter().zip(&entries) {
            let CodexItem::ResponseItem(ri) = &raw.item else {
                continue;
            };
            if !matches!(&ri.payload, ResponseItemPayload::AgentMessage { .. }) {
                continue;
            }
            let Entry::User(u) = entry else {
                panic!("inter-agent agent_message must be Entry::User");
            };
            assert!(!u.meta.is_meta);
            assert!(!u.meta.is_sidechain);
            assert!(!u.meta.is_compact_summary);
            assert!(!u.interrupted());
            assert!(!u.is_agent_injected());
            assert!(!pystr::strip(&u.content.text()).is_empty());
            assert_eq!(u.content.text(), "Please run the reproduction script.");
            checked = true;
        }
        assert!(checked, "404 must carry the inter-agent agent_message");
    }

    #[test]
    fn developer_message_is_meta() {
        let (session, entries) = zip_lower("303");
        let mut checked = false;
        for (raw, entry) in session.entries.iter().zip(&entries) {
            if let CodexItem::ResponseItem(ri) = &raw.item {
                if matches!(&ri.payload, ResponseItemPayload::Message { role, .. } if role.as_deref() == Some("developer"))
                {
                    let Entry::User(u) = entry else {
                        panic!("developer message must be Entry::User");
                    };
                    assert!(u.meta.is_meta, "developer message must be is_meta");
                    checked = true;
                }
            }
        }
        assert!(checked, "303 must carry a developer message");
    }

    #[test]
    fn authored_text_before_environment_context_opens_a_turn() {
        let (session, entries) = zip_lower("060f");
        let mut prompt = None;
        for (raw, entry) in session.entries.iter().zip(&entries) {
            if let CodexItem::ResponseItem(ri) = &raw.item {
                if matches!(&ri.payload, ResponseItemPayload::Message { role, .. } if role.as_deref() == Some("user"))
                {
                    let Entry::User(u) = entry else {
                        panic!("user message must be Entry::User");
                    };
                    assert!(
                        !u.meta.is_meta,
                        "authored text before a wrapper must not set is_meta"
                    );
                    prompt = Some(u.content.text());
                }
            }
        }
        let prompt = prompt.expect("060f must carry the authored user message");
        let sid = session.rollout_thread_id.clone().unwrap_or_default();
        let lifted = lift_session(&sid, &entries);
        assert!(
            lifted.turns.iter().any(|turn| turn.prompt == prompt),
            "authored response_item must open a turn"
        );
    }

    #[test]
    fn turn_aborted_becomes_interrupted_blank_user() {
        let (session, entries) = zip_lower("050b");
        let mut checked = false;
        for (raw, entry) in session.entries.iter().zip(&entries) {
            if matches!(&raw.item, CodexItem::EventMsg(EventMsg::TurnAborted { .. })) {
                let Entry::User(u) = entry else {
                    panic!("turn_aborted must be Entry::User");
                };
                assert!(u.interrupted(), "turn_aborted user must be interrupted()");
                assert!(u.content.text().is_empty(), "aborted user content is blank");
                checked = true;
            }
        }
        assert!(checked, "050b must carry a turn_aborted");
    }

    #[test]
    fn compacted_is_compact_summary_and_never_emits_replacement_history() {
        let session = session("050d");
        let lowered = lower(&session);
        assert_eq!(
            lowered.entries.len(),
            session.entries.len(),
            "replacement_history must not materialize extra entries"
        );
        let mut checked = false;
        for (raw, entry) in session.entries.iter().zip(&lowered.entries) {
            if matches!(&raw.item, CodexItem::Compacted(_)) {
                let Entry::User(u) = entry else {
                    panic!("compacted must be Entry::User");
                };
                assert!(
                    u.meta.is_compact_summary,
                    "compacted must set is_compact_summary"
                );
                checked = true;
            }
        }
        assert!(checked, "050d must carry a compacted entry");
    }

    #[test]
    fn function_call_arguments_kept_verbatim() {
        let (session, entries) = zip_lower("202");
        let mut checked = false;
        for entry in &entries {
            for tu in entry.tool_uses() {
                if tu.name == "exec_command" {
                    assert_eq!(
                        tu.input.as_str().unwrap(),
                        r#"{"cmd": "ls /tmp/demo", "workdir": "/tmp/demo"}"#
                    );
                    checked = true;
                }
            }
        }
        let _ = &session;
        assert!(checked, "202 must carry the exec_command function_call");
    }

    #[test]
    fn mcp_function_call_name_synthesized() {
        let lowered = lower(&session("050e"));
        let names: Vec<&str> = lowered
            .entries
            .iter()
            .flat_map(Entry::tool_uses)
            .map(|tu| tu.name.as_str())
            .collect();
        assert!(
            names.contains(&"mcp__demo_server__list_items"),
            "050e must synthesize the MCP tool name, got {names:?}"
        );
    }

    fn lifted_turns(entries: &[Entry], session_id: &str) -> Vec<usize> {
        lift_session(session_id, entries)
            .turns
            .iter()
            .enumerate()
            .filter_map(|(i, t)| (!t.prompt.is_empty()).then_some(i))
            .collect()
    }

    fn real_turn_count(tag: &str) -> usize {
        let session = session(tag);
        let entries = lower(&session).entries;
        let sid = session.rollout_thread_id.clone().unwrap_or_default();
        lifted_turns(&entries, &sid).len()
    }

    #[test]
    fn money_two_prompts_split_into_two_turns() {
        assert_eq!(real_turn_count("050c"), 2, "050c is two turns");
    }

    #[test]
    fn money_compaction_does_not_split_the_turn() {
        assert_eq!(
            real_turn_count("050d"),
            1,
            "compaction stays within one turn"
        );
    }

    #[test]
    fn money_inter_agent_turn_opens() {
        let session = session("404");
        let entries = lower(&session).entries;
        let sid = session.rollout_thread_id.clone().unwrap_or_default();
        let lifted = lift_session(&sid, &entries);
        assert!(
            lifted
                .turns
                .iter()
                .any(|t| t.prompt == "Please run the reproduction script."),
            "404 inter-agent prompt must open a turn"
        );
    }

    fn find_tool_use<'a>(
        turns: &'a [Turn<'a>],
        name: &str,
    ) -> Option<&'a crate::activity::ToolUse<'a>> {
        turns
            .iter()
            .flat_map(|t| &t.tool_uses)
            .find(|tu| tu.name == name)
    }

    #[test]
    fn money_paired_tool_use_has_result_and_duration() {
        for tag in ["303", "050e"] {
            let session = session(tag);
            let entries = lower(&session).entries;
            let sid = session.rollout_thread_id.clone().unwrap_or_default();
            let lifted = lift_session(&sid, &entries);
            let tool = lifted
                .turns
                .iter()
                .flat_map(|t| &t.tool_uses)
                .next()
                .unwrap_or_else(|| panic!("{tag} must carry a tool use"));
            assert!(tool.result.is_some(), "{tag} tool use must pair a result");
            assert!(
                tool.duration_ms().is_some(),
                "{tag} paired tool use must have a duration"
            );
        }
    }

    #[test]
    fn money_dangling_tool_use_has_no_result() {
        let session = session("050a");
        let entries = lower(&session).entries;
        let sid = session.rollout_thread_id.clone().unwrap_or_default();
        let lifted = lift_session(&sid, &entries);
        let tool = find_tool_use(&lifted.turns, "exec").expect("050a must carry a dangling call");
        assert!(
            tool.result.is_none(),
            "050a dangling call must have no result"
        );
        assert!(tool.duration_ms().is_none(), "no result means no duration");
    }

    #[test]
    fn money_abort_shows_interrupted_user_in_turn() {
        let session = session("050b");
        let entries = lower(&session).entries;
        let sid = session.rollout_thread_id.clone().unwrap_or_default();
        let lifted = lift_session(&sid, &entries);
        assert!(
            lifted.turns.iter().any(|t| t
                .events
                .iter()
                .any(|e| matches!(e, Entry::User(u) if u.interrupted()))),
            "050b lifted turns must expose the interrupted user entry"
        );
    }
}
