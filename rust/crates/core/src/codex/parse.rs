use memchr::memchr_iter;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::codex::types::{
    CodexEntry, CodexItem, CodexOther, CodexSession, Compacted, EventMsg, ResponseItem,
    ResponseItemPayload, SessionMeta, TokenUsage, TokenUsageInfo, WorldState,
};
use crate::parse::parse_timestamp;
use crate::value::{field, field_str, normalize_last_wins};

const AVG_CODEX_LINE_BYTES: usize = 700;

fn opt_str(value: &Value, key: &str) -> Option<String> {
    field_str(value, key).map(str::to_string)
}

fn opt_i64(value: &Value, key: &str) -> Option<i64> {
    field(value, key).and_then(JsonValueTrait::as_i64)
}

fn opt_bool(value: &Value, key: &str) -> Option<bool> {
    field(value, key).and_then(JsonValueTrait::as_bool)
}

fn opt_val(value: &Value, key: &str) -> Option<Value> {
    field(value, key).filter(|v| !v.is_null()).cloned()
}

fn values(value: &Value, key: &str) -> Vec<Value> {
    field(value, key)
        .and_then(|v| v.as_array())
        .map(|a| a.to_vec())
        .unwrap_or_default()
}

fn opt_values(value: &Value, key: &str) -> Option<Vec<Value>> {
    field(value, key)
        .and_then(|v| v.as_array())
        .map(|a| a.to_vec())
}

fn passthrough_turn_id(payload: &Value) -> Option<String> {
    field(payload, "internal_chat_message_metadata_passthrough")
        .and_then(|m| field_str(m, "turn_id"))
        .map(str::to_string)
}

pub fn parse_codex_bytes(bytes: &[u8]) -> CodexSession {
    let mut entries: Vec<CodexEntry> = Vec::with_capacity(bytes.len() / AVG_CODEX_LINE_BYTES + 1);
    let mut start = 0usize;
    let mut line_index = 0usize;
    for pos in memchr_iter(b'\n', bytes) {
        parse_codex_line(&bytes[start..pos], line_index, &mut entries);
        start = pos + 1;
        line_index += 1;
    }
    if start < bytes.len() {
        parse_codex_line(&bytes[start..], line_index, &mut entries);
    }
    fold_session(entries)
}

fn parse_codex_line(line: &[u8], line_index: usize, entries: &mut Vec<CodexEntry>) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    match sonic_rs::from_slice::<Value>(line) {
        Ok(mut value) => {
            normalize_last_wins(&mut value);
            let timestamp = field_str(&value, "timestamp").and_then(|s| parse_timestamp(s).ok());
            let item = parse_codex_item(&value, line);
            entries.push(CodexEntry {
                line_index,
                timestamp,
                item,
            });
        }
        Err(_) => entries.push(CodexEntry {
            line_index,
            timestamp: None,
            item: CodexItem::Other(CodexOther {
                ty: None,
                raw: String::from_utf8_lossy(line).into_owned(),
            }),
        }),
    }
}

fn parse_codex_item(value: &Value, line: &[u8]) -> CodexItem {
    let null = Value::default();
    let payload = field(value, "payload").unwrap_or(&null);
    match field_str(value, "type") {
        Some("session_meta") => CodexItem::SessionMeta(parse_session_meta(payload)),
        Some("response_item") => CodexItem::ResponseItem(parse_response_item(payload)),
        Some("event_msg") => CodexItem::EventMsg(parse_event_msg(payload)),
        Some("turn_context") => CodexItem::TurnContext(payload.clone()),
        Some("world_state") => CodexItem::WorldState(WorldState {
            full: opt_bool(payload, "full"),
            state: field(payload, "state").cloned().unwrap_or_default(),
        }),
        Some("compacted") => CodexItem::Compacted(parse_compacted(payload)),
        Some("inter_agent_communication_metadata") => CodexItem::InterAgentCommunicationMetadata {
            trigger_turn: opt_bool(payload, "trigger_turn"),
        },
        Some(other) => CodexItem::Other(CodexOther {
            ty: Some(other.to_string()),
            raw: String::from_utf8_lossy(line).into_owned(),
        }),
        None => CodexItem::Other(CodexOther {
            ty: None,
            raw: String::from_utf8_lossy(line).into_owned(),
        }),
    }
}

fn parse_session_meta(p: &Value) -> SessionMeta {
    SessionMeta {
        raw: p.clone(),
        id: opt_str(p, "id"),
        session_id: opt_str(p, "session_id"),
        parent_thread_id: opt_str(p, "parent_thread_id"),
        forked_from_id: opt_str(p, "forked_from_id"),
        thread_source: opt_str(p, "thread_source"),
        cwd: opt_str(p, "cwd"),
        originator: opt_str(p, "originator"),
        cli_version: opt_str(p, "cli_version"),
        model_provider: opt_str(p, "model_provider"),
        agent_path: opt_str(p, "agent_path"),
        agent_nickname: opt_str(p, "agent_nickname"),
        agent_role: opt_str(p, "agent_role"),
        instructions: opt_str(p, "instructions"),
        base_instructions: opt_val(p, "base_instructions"),
        source: opt_val(p, "source"),
        git: opt_val(p, "git"),
    }
}

fn parse_compacted(p: &Value) -> Compacted {
    Compacted {
        message: opt_str(p, "message"),
        replacement_history: field(p, "replacement_history")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(parse_response_item).collect()),
        window_number: opt_i64(p, "window_number"),
        first_window_id: opt_str(p, "first_window_id"),
        previous_window_id: opt_str(p, "previous_window_id"),
        window_id: opt_str(p, "window_id"),
    }
}

fn parse_response_item(p: &Value) -> ResponseItem {
    ResponseItem {
        id: opt_str(p, "id"),
        turn_id: passthrough_turn_id(p),
        payload: parse_response_payload(p),
    }
}

fn parse_response_payload(p: &Value) -> ResponseItemPayload {
    use ResponseItemPayload as R;
    match field_str(p, "type") {
        Some("message") if field(p, "content").is_some_and(|v| v.is_array()) => R::Message {
            role: opt_str(p, "role"),
            content: values(p, "content"),
            phase: opt_str(p, "phase"),
        },
        Some("message") => R::Other {
            ty: Some("message".to_string()),
            raw: p.clone(),
        },
        Some("agent_message") => R::AgentMessage {
            author: opt_str(p, "author"),
            recipient: opt_str(p, "recipient"),
            content: values(p, "content"),
        },
        Some("reasoning") => R::Reasoning {
            summary: values(p, "summary"),
            content: opt_values(p, "content"),
            encrypted_content: opt_str(p, "encrypted_content"),
        },
        Some("local_shell_call") => R::LocalShellCall {
            call_id: opt_str(p, "call_id"),
            status: opt_str(p, "status"),
            action: opt_val(p, "action"),
        },
        Some("function_call")
            if field_str(p, "call_id").is_some() && field_str(p, "name").is_some() =>
        {
            R::FunctionCall {
                name: opt_str(p, "name"),
                namespace: opt_str(p, "namespace"),
                arguments: opt_str(p, "arguments"),
                call_id: opt_str(p, "call_id"),
            }
        }
        Some("function_call") => R::Other {
            ty: Some("function_call".to_string()),
            raw: p.clone(),
        },
        Some("function_call_output") if field_str(p, "call_id").is_some() => {
            R::FunctionCallOutput {
                call_id: opt_str(p, "call_id"),
                output: field(p, "output").cloned().unwrap_or_default(),
            }
        }
        Some("function_call_output") => R::Other {
            ty: Some("function_call_output".to_string()),
            raw: p.clone(),
        },
        Some("custom_tool_call")
            if field_str(p, "call_id").is_some() && field_str(p, "name").is_some() =>
        {
            R::CustomToolCall {
                name: opt_str(p, "name"),
                namespace: opt_str(p, "namespace"),
                input: opt_str(p, "input"),
                status: opt_str(p, "status"),
                call_id: opt_str(p, "call_id"),
            }
        }
        Some("custom_tool_call") => R::Other {
            ty: Some("custom_tool_call".to_string()),
            raw: p.clone(),
        },
        Some("custom_tool_call_output") if field_str(p, "call_id").is_some() => {
            R::CustomToolCallOutput {
                call_id: opt_str(p, "call_id"),
                name: opt_str(p, "name"),
                output: field(p, "output").cloned().unwrap_or_default(),
            }
        }
        Some("custom_tool_call_output") => R::Other {
            ty: Some("custom_tool_call_output".to_string()),
            raw: p.clone(),
        },
        Some("tool_search_call") => R::ToolSearchCall {
            call_id: opt_str(p, "call_id"),
            status: opt_str(p, "status"),
            execution: opt_str(p, "execution"),
            arguments: field(p, "arguments").cloned().unwrap_or_default(),
        },
        Some("tool_search_output") => R::ToolSearchOutput {
            call_id: opt_str(p, "call_id"),
            status: opt_str(p, "status"),
            execution: opt_str(p, "execution"),
            tools: values(p, "tools"),
        },
        Some("web_search_call") => R::WebSearchCall {
            status: opt_str(p, "status"),
            action: opt_val(p, "action"),
        },
        Some("image_generation_call") => R::ImageGenerationCall {
            status: opt_str(p, "status"),
            revised_prompt: opt_str(p, "revised_prompt"),
            result: opt_str(p, "result"),
        },
        Some("compaction") | Some("compaction_summary") => R::Compaction {
            encrypted_content: opt_str(p, "encrypted_content"),
        },
        Some("context_compaction") => R::ContextCompaction {
            encrypted_content: opt_str(p, "encrypted_content"),
        },
        Some("ghost_snapshot") => R::GhostSnapshot(p.clone()),
        Some(other) => R::Other {
            ty: Some(other.to_string()),
            raw: p.clone(),
        },
        None => R::Other {
            ty: None,
            raw: p.clone(),
        },
    }
}

fn parse_event_msg(p: &Value) -> EventMsg {
    use EventMsg as E;
    match field_str(p, "type") {
        Some("token_count") => E::TokenCount {
            info: field(p, "info")
                .filter(|v| !v.is_null())
                .map(parse_token_info),
            rate_limits: opt_val(p, "rate_limits"),
        },
        Some("agent_message") => E::AgentMessage {
            message: opt_str(p, "message"),
            phase: opt_str(p, "phase"),
            memory_citation: opt_val(p, "memory_citation"),
        },
        Some("user_message") => E::UserMessage {
            message: opt_str(p, "message"),
            images: opt_values(p, "images"),
            local_images: opt_val(p, "local_images"),
            text_elements: opt_val(p, "text_elements"),
        },
        Some("agent_reasoning") => E::AgentReasoning {
            text: opt_str(p, "text"),
        },
        Some("agent_reasoning_raw_content") => E::AgentReasoningRawContent {
            text: opt_str(p, "text"),
        },
        Some("task_started") => E::TaskStarted {
            turn_id: opt_str(p, "turn_id"),
            started_at: opt_i64(p, "started_at"),
            model_context_window: opt_i64(p, "model_context_window"),
            collaboration_mode_kind: opt_str(p, "collaboration_mode_kind"),
        },
        Some("task_complete") => E::TaskComplete {
            turn_id: opt_str(p, "turn_id"),
            last_agent_message: opt_str(p, "last_agent_message"),
            completed_at: opt_i64(p, "completed_at"),
            duration_ms: opt_i64(p, "duration_ms"),
            time_to_first_token_ms: opt_i64(p, "time_to_first_token_ms"),
        },
        Some("turn_aborted") => E::TurnAborted {
            turn_id: opt_str(p, "turn_id"),
            reason: opt_str(p, "reason"),
            completed_at: opt_i64(p, "completed_at"),
            duration_ms: opt_i64(p, "duration_ms"),
        },
        Some("patch_apply_end") => E::PatchApplyEnd {
            call_id: opt_str(p, "call_id"),
            turn_id: opt_str(p, "turn_id"),
            status: opt_str(p, "status"),
            success: opt_bool(p, "success"),
            stdout: opt_str(p, "stdout"),
            stderr: opt_str(p, "stderr"),
            changes: opt_val(p, "changes"),
        },
        Some("context_compacted") => E::ContextCompacted,
        Some("sub_agent_activity") => E::SubAgentActivity {
            event_id: opt_str(p, "event_id"),
            occurred_at_ms: opt_i64(p, "occurred_at_ms"),
            agent_thread_id: opt_str(p, "agent_thread_id"),
            agent_path: opt_str(p, "agent_path"),
            kind: opt_str(p, "kind"),
        },
        Some("thread_rolled_back") => E::ThreadRolledBack {
            num_turns: opt_i64(p, "num_turns"),
        },
        Some("exec_command_end") => E::ExecCommandEnd(p.clone()),
        Some("mcp_tool_call_end") => E::McpToolCallEnd(p.clone()),
        Some("web_search_end") => E::WebSearchEnd(p.clone()),
        Some("image_generation_end") => E::ImageGenerationEnd(p.clone()),
        Some("thread_settings_applied") => E::ThreadSettingsApplied(p.clone()),
        Some("item_completed") => E::ItemCompleted(p.clone()),
        Some("thread_goal_updated") => E::ThreadGoalUpdated(p.clone()),
        Some("entered_review_mode") => E::EnteredReviewMode(p.clone()),
        Some("exited_review_mode") => E::ExitedReviewMode(p.clone()),
        Some("view_image_tool_call") => E::ViewImageToolCall(p.clone()),
        Some("collab_agent_spawn_end") => E::CollabAgentSpawnEnd(p.clone()),
        Some("collab_waiting_end") => E::CollabWaitingEnd(p.clone()),
        Some("collab_close_end") => E::CollabCloseEnd(p.clone()),
        Some(other) => E::Other {
            ty: Some(other.to_string()),
            raw: p.clone(),
        },
        None => E::Other {
            ty: None,
            raw: p.clone(),
        },
    }
}

fn parse_token_info(info: &Value) -> TokenUsageInfo {
    TokenUsageInfo {
        total_token_usage: field(info, "total_token_usage")
            .filter(|v| !v.is_null())
            .map(parse_token_usage),
        last_token_usage: field(info, "last_token_usage")
            .filter(|v| !v.is_null())
            .map(parse_token_usage),
        model_context_window: opt_i64(info, "model_context_window"),
    }
}

fn parse_token_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: opt_i64(usage, "input_tokens"),
        cached_input_tokens: opt_i64(usage, "cached_input_tokens"),
        output_tokens: opt_i64(usage, "output_tokens"),
        reasoning_output_tokens: opt_i64(usage, "reasoning_output_tokens"),
        total_tokens: opt_i64(usage, "total_tokens"),
    }
}

fn fold_session(entries: Vec<CodexEntry>) -> CodexSession {
    let meta = entries.iter().find_map(|e| match &e.item {
        CodexItem::SessionMeta(m) if m.id.as_deref().is_some_and(|id| !id.is_empty()) => Some(m),
        _ => None,
    });
    let session = match meta {
        Some(m) => CodexSession {
            entries: Vec::new(),
            rollout_thread_id: m.id.clone(),
            session_id: m.session_id.clone().or_else(|| m.id.clone()),
            parent_thread_id: m.parent_thread_id.clone(),
            forked_from_id: m.forked_from_id.clone(),
            cli_version: m.cli_version.clone(),
            originator: m.originator.clone(),
            cwd: m.cwd.clone(),
            instructions: m.instructions.clone(),
            base_instructions: m.base_instructions.clone(),
        },
        None => CodexSession {
            entries: Vec::new(),
            rollout_thread_id: None,
            session_id: None,
            parent_thread_id: None,
            forked_from_id: None,
            cli_version: None,
            originator: None,
            cwd: None,
            instructions: None,
            base_instructions: None,
        },
    };
    CodexSession { entries, ..session }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn fixture_bytes(tag: &str) -> Vec<u8> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/testdata/codex");
        let entry = std::fs::read_dir(dir)
            .expect("codex testdata dir")
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(&format!("{tag}.jsonl"))
            })
            .unwrap_or_else(|| panic!("fixture {tag} not found"));
        std::fs::read(entry.path()).expect("read fixture")
    }

    fn fixture(tag: &str) -> CodexSession {
        parse_codex_bytes(&fixture_bytes(tag))
    }

    fn top_key(item: &CodexItem) -> &'static str {
        match item {
            CodexItem::SessionMeta(_) => "session_meta",
            CodexItem::ResponseItem(_) => "response_item",
            CodexItem::EventMsg(_) => "event_msg",
            CodexItem::TurnContext(_) => "turn_context",
            CodexItem::WorldState(_) => "world_state",
            CodexItem::Compacted(_) => "compacted",
            CodexItem::InterAgentCommunicationMetadata { .. } => {
                "inter_agent_communication_metadata"
            }
            CodexItem::Other(_) => "other",
        }
    }

    fn ri_key(payload: &ResponseItemPayload) -> &'static str {
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
            R::Other { .. } => "other",
        }
    }

    fn em_key(msg: &EventMsg) -> &'static str {
        use EventMsg as E;
        match msg {
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
            E::Other { .. } => "other",
        }
    }

    fn tally(pairs: &[(&'static str, usize)]) -> BTreeMap<&'static str, usize> {
        pairs.iter().copied().collect()
    }

    fn top_tally(s: &CodexSession) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for e in &s.entries {
            *m.entry(top_key(&e.item)).or_default() += 1;
        }
        m
    }

    fn ri_tally(s: &CodexSession) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for e in &s.entries {
            if let CodexItem::ResponseItem(ri) = &e.item {
                *m.entry(ri_key(&ri.payload)).or_default() += 1;
            }
        }
        m
    }

    fn em_tally(s: &CodexSession) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for e in &s.entries {
            if let CodexItem::EventMsg(em) = &e.item {
                *m.entry(em_key(em)).or_default() += 1;
            }
        }
        m
    }

    struct Case {
        tag: &'static str,
        total: usize,
        top: &'static [(&'static str, usize)],
        ri: &'static [(&'static str, usize)],
        em: &'static [(&'static str, usize)],
    }

    #[test]
    fn all_fixtures_tally_exactly() {
        let cases = [
            Case {
                tag: "101",
                total: 11,
                top: &[
                    ("session_meta", 1),
                    ("response_item", 5),
                    ("event_msg", 4),
                    ("turn_context", 1),
                ],
                ri: &[
                    ("message", 2),
                    ("reasoning", 1),
                    ("function_call", 1),
                    ("function_call_output", 1),
                ],
                em: &[
                    ("user_message", 1),
                    ("token_count", 1),
                    ("agent_reasoning", 1),
                    ("agent_message", 1),
                ],
            },
            Case {
                tag: "202",
                total: 14,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 5),
                    ("response_item", 7),
                    ("turn_context", 1),
                ],
                ri: &[
                    ("message", 3),
                    ("ghost_snapshot", 1),
                    ("reasoning", 1),
                    ("function_call", 1),
                    ("function_call_output", 1),
                ],
                em: &[
                    ("task_started", 1),
                    ("user_message", 1),
                    ("token_count", 1),
                    ("agent_message", 1),
                    ("task_complete", 1),
                ],
            },
            Case {
                tag: "060f",
                total: 5,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 2),
                    ("turn_context", 1),
                    ("response_item", 1),
                ],
                ri: &[("message", 1)],
                em: &[("task_started", 1), ("user_message", 1)],
            },
            Case {
                tag: "303",
                total: 15,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 6),
                    ("response_item", 6),
                    ("world_state", 1),
                    ("turn_context", 1),
                ],
                ri: &[
                    ("message", 3),
                    ("reasoning", 1),
                    ("custom_tool_call", 1),
                    ("custom_tool_call_output", 1),
                ],
                em: &[
                    ("task_started", 1),
                    ("user_message", 1),
                    ("agent_message", 1),
                    ("token_count", 1),
                    ("patch_apply_end", 1),
                    ("task_complete", 1),
                ],
            },
            Case {
                tag: "404",
                total: 20,
                top: &[
                    ("session_meta", 2),
                    ("event_msg", 8),
                    ("response_item", 6),
                    ("world_state", 1),
                    ("turn_context", 2),
                    ("inter_agent_communication_metadata", 1),
                ],
                ri: &[
                    ("message", 2),
                    ("agent_message", 1),
                    ("reasoning", 1),
                    ("custom_tool_call", 1),
                    ("custom_tool_call_output", 1),
                ],
                em: &[
                    ("task_started", 2),
                    ("user_message", 1),
                    ("agent_message", 2),
                    ("token_count", 1),
                    ("sub_agent_activity", 1),
                    ("task_complete", 1),
                ],
            },
            Case {
                tag: "050a",
                total: 7,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 2),
                    ("turn_context", 1),
                    ("response_item", 3),
                ],
                ri: &[("message", 1), ("reasoning", 1), ("custom_tool_call", 1)],
                em: &[("task_started", 1), ("user_message", 1)],
            },
            Case {
                tag: "050b",
                total: 7,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 3),
                    ("turn_context", 1),
                    ("response_item", 2),
                ],
                ri: &[("message", 1), ("custom_tool_call", 1)],
                em: &[
                    ("task_started", 1),
                    ("user_message", 1),
                    ("turn_aborted", 1),
                ],
            },
            Case {
                tag: "050c",
                total: 15,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 8),
                    ("turn_context", 2),
                    ("response_item", 4),
                ],
                ri: &[("message", 4)],
                em: &[
                    ("task_started", 2),
                    ("user_message", 2),
                    ("agent_message", 2),
                    ("task_complete", 2),
                ],
            },
            Case {
                tag: "050d",
                total: 15,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 6),
                    ("turn_context", 2),
                    ("response_item", 4),
                    ("compacted", 1),
                    ("world_state", 1),
                ],
                ri: &[
                    ("message", 2),
                    ("custom_tool_call", 1),
                    ("custom_tool_call_output", 1),
                ],
                em: &[
                    ("task_started", 1),
                    ("user_message", 1),
                    ("token_count", 1),
                    ("context_compacted", 1),
                    ("agent_message", 1),
                    ("task_complete", 1),
                ],
            },
            Case {
                tag: "050e",
                total: 11,
                top: &[
                    ("session_meta", 1),
                    ("event_msg", 4),
                    ("turn_context", 1),
                    ("response_item", 5),
                ],
                ri: &[
                    ("message", 2),
                    ("reasoning", 1),
                    ("function_call", 1),
                    ("function_call_output", 1),
                ],
                em: &[
                    ("task_started", 1),
                    ("user_message", 1),
                    ("agent_message", 1),
                    ("task_complete", 1),
                ],
            },
        ];
        for case in &cases {
            let session = fixture(case.tag);
            assert_eq!(
                session.entries.len(),
                case.total,
                "{} entry count",
                case.tag
            );
            assert_eq!(
                top_tally(&session),
                tally(case.top),
                "{} top-level tally",
                case.tag
            );
            assert_eq!(
                ri_tally(&session),
                tally(case.ri),
                "{} response_item tally",
                case.tag
            );
            assert_eq!(
                em_tally(&session),
                tally(case.em),
                "{} event_msg tally",
                case.tag
            );
        }
    }

    #[test]
    fn fixtures_never_produce_top_or_payload_other() {
        for tag in [
            "101", "202", "060f", "303", "404", "050a", "050b", "050c", "050d", "050e",
        ] {
            let session = fixture(tag);
            for e in &session.entries {
                assert!(!matches!(e.item, CodexItem::Other(_)), "{tag} top other");
                match &e.item {
                    CodexItem::ResponseItem(ri) => assert!(
                        !matches!(ri.payload, ResponseItemPayload::Other { .. }),
                        "{tag} ri other"
                    ),
                    CodexItem::EventMsg(em) => {
                        assert!(!matches!(em, EventMsg::Other { .. }), "{tag} em other")
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn malformed_line_falls_to_top_other_with_raw_line() {
        let bytes = b"{not valid json\n{\"timestamp\":\"2026-01-20T04:00:05.000Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n";
        let session = parse_codex_bytes(bytes);
        assert_eq!(session.entries.len(), 2);
        let first = &session.entries[0];
        assert_eq!(first.line_index, 0);
        assert!(first.timestamp.is_none());
        match &first.item {
            CodexItem::Other(o) => {
                assert_eq!(o.ty, None);
                assert_eq!(o.raw, "{not valid json");
            }
            other => panic!("expected top Other, got {other:?}"),
        }
        assert!(matches!(session.entries[1].item, CodexItem::SessionMeta(_)));
    }

    #[test]
    fn unknown_top_type_falls_to_top_other_preserving_line() {
        let line =
            r#"{"timestamp":"2026-01-20T04:00:05.000Z","type":"future_kind","payload":{"a":1}}"#;
        let session = parse_codex_bytes(line.as_bytes());
        assert_eq!(session.entries.len(), 1);
        let e = &session.entries[0];
        assert!(e.timestamp.is_some());
        match &e.item {
            CodexItem::Other(o) => {
                assert_eq!(o.ty.as_deref(), Some("future_kind"));
                assert_eq!(o.raw, line);
            }
            other => panic!("expected top Other, got {other:?}"),
        }
    }

    #[test]
    fn unknown_response_item_payload_falls_to_payload_other() {
        let line = r#"{"timestamp":"2026-01-20T04:00:05.000Z","type":"response_item","payload":{"type":"future_ri","x":7}}"#;
        let session = parse_codex_bytes(line.as_bytes());
        let CodexItem::ResponseItem(ri) = &session.entries[0].item else {
            panic!("expected response_item");
        };
        match &ri.payload {
            ResponseItemPayload::Other { ty, raw } => {
                assert_eq!(ty.as_deref(), Some("future_ri"));
                assert_eq!(field(raw, "x").and_then(|v| v.as_i64()), Some(7));
            }
            other => panic!("expected payload Other, got {other:?}"),
        }
    }

    #[test]
    fn wrong_typed_message_content_falls_to_payload_other_preserving_raw() {
        let raw: Value =
            sonic_rs::from_str(r#"{"type":"message","role":"assistant","content":"not-an-array"}"#)
                .unwrap();
        match parse_response_payload(&raw) {
            ResponseItemPayload::Other { ty, raw } => {
                assert_eq!(ty.as_deref(), Some("message"));
                assert_eq!(
                    field(&raw, "content").and_then(|v| v.as_str()),
                    Some("not-an-array")
                );
            }
            other => panic!("expected payload Other, got {other:?}"),
        }
    }

    #[test]
    fn missing_message_content_falls_to_payload_other_preserving_raw() {
        let raw: Value = sonic_rs::from_str(r#"{"type":"message","role":"assistant"}"#).unwrap();
        match parse_response_payload(&raw) {
            ResponseItemPayload::Other { ty, raw: preserved } => {
                assert_eq!(ty.as_deref(), Some("message"));
                assert_eq!(preserved, raw);
            }
            other => panic!("expected payload Other, got {other:?}"),
        }
    }

    #[test]
    fn function_call_missing_call_id_falls_to_payload_other() {
        let raw: Value = sonic_rs::from_str(
            r#"{"type":"function_call","name":"exec_command","arguments":"{}"}"#,
        )
        .unwrap();
        match parse_response_payload(&raw) {
            ResponseItemPayload::Other { ty, raw: preserved } => {
                assert_eq!(ty.as_deref(), Some("function_call"));
                assert_eq!(preserved, raw);
            }
            other => panic!("expected payload Other, got {other:?}"),
        }
    }

    #[test]
    fn required_call_fields_reject_absent_or_wrong_types() {
        let cases = [
            (r#"{"type":"function_call","call_id":"c"}"#, "function_call"),
            (
                r#"{"type":"function_call_output","call_id":7}"#,
                "function_call_output",
            ),
            (
                r#"{"type":"custom_tool_call","call_id":"c"}"#,
                "custom_tool_call",
            ),
            (
                r#"{"type":"custom_tool_call_output"}"#,
                "custom_tool_call_output",
            ),
        ];
        for (json, expected_type) in cases {
            let raw: Value = sonic_rs::from_str(json).unwrap();
            match parse_response_payload(&raw) {
                ResponseItemPayload::Other { ty, raw: preserved } => {
                    assert_eq!(ty.as_deref(), Some(expected_type));
                    assert_eq!(preserved, raw);
                }
                other => panic!("expected payload Other, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_event_msg_payload_falls_to_payload_other() {
        let line = r#"{"timestamp":"2026-01-20T04:00:05.000Z","type":"event_msg","payload":{"type":"future_event","q":"keep"}}"#;
        let session = parse_codex_bytes(line.as_bytes());
        let CodexItem::EventMsg(em) = &session.entries[0].item else {
            panic!("expected event_msg");
        };
        match em {
            EventMsg::Other { ty, raw } => {
                assert_eq!(ty.as_deref(), Some("future_event"));
                assert_eq!(field(raw, "q").and_then(|v| v.as_str()), Some("keep"));
            }
            other => panic!("expected payload Other, got {other:?}"),
        }
    }

    #[test]
    fn blank_lines_skipped_but_physical_index_preserved() {
        let bytes = b"\n{\"timestamp\":\"2026-01-20T04:00:05.000Z\",\"type\":\"turn_context\",\"payload\":{}}\n  \n{\"timestamp\":\"2026-01-20T04:00:06.000Z\",\"type\":\"world_state\",\"payload\":{\"full\":true,\"state\":{}}}\n";
        let session = parse_codex_bytes(bytes);
        assert_eq!(session.entries.len(), 2);
        assert_eq!(session.entries[0].line_index, 1);
        assert!(matches!(session.entries[0].item, CodexItem::TurnContext(_)));
        assert_eq!(session.entries[1].line_index, 3);
        assert!(matches!(session.entries[1].item, CodexItem::WorldState(_)));
    }

    #[test]
    fn identity_fold_404_first_row_wins_and_tolerates_multiple() {
        let session = fixture("404");
        let metas = session
            .entries
            .iter()
            .filter(|e| matches!(e.item, CodexItem::SessionMeta(_)))
            .count();
        assert_eq!(metas, 2, "two head rows tolerated");
        assert_eq!(
            session.rollout_thread_id.as_deref(),
            Some("019f6800-3b4c-7d5e-9f60-000000000404"),
            "row0 own id wins"
        );
        assert_eq!(
            session.session_id.as_deref(),
            Some("019f67f0-2a3b-7c4d-8e5f-000000000303"),
            "shared root session_id"
        );
        assert_eq!(
            session.parent_thread_id.as_deref(),
            Some("019f67f0-2a3b-7c4d-8e5f-000000000303")
        );
        assert_eq!(
            session.forked_from_id.as_deref(),
            Some("019f67f0-2a3b-7c4d-8e5f-000000000303")
        );
        assert_eq!(session.cli_version.as_deref(), Some("0.144.5"));
    }

    #[test]
    fn session_meta_preserves_full_raw_payload() {
        let line = r#"{"timestamp":"2026-07-16T16:20:00.000Z","type":"session_meta","payload":{"id":"valid","timestamp":"2026-07-16T16:20:00.000Z","context_window":{"tokens":272000},"history_mode":"legacy","multi_agent_version":"v2","future_field":{"enabled":true}}}"#;
        let session = parse_codex_bytes(line.as_bytes());
        let CodexItem::SessionMeta(meta) = &session.entries[0].item else {
            panic!("expected session_meta");
        };
        assert_eq!(
            field(&meta.raw, "history_mode").and_then(|v| v.as_str()),
            Some("legacy")
        );
        assert_eq!(
            field(&meta.raw, "context_window")
                .and_then(|v| field(v, "tokens"))
                .and_then(|v| v.as_i64()),
            Some(272000)
        );
        assert_eq!(
            field(&meta.raw, "future_field")
                .and_then(|v| field(v, "enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn identity_fold_skips_integer_id_session_meta() {
        let mut bytes =
            br#"{"timestamp":"2026-07-16T16:20:00.000Z","type":"session_meta","payload":{"id":42}}
"#
            .to_vec();
        let fixture = fixture_bytes("303");
        bytes.extend(fixture.split(|b| *b == b'\n').next().unwrap());
        let session = parse_codex_bytes(&bytes);
        assert_eq!(session.entries.len(), 2);
        let CodexItem::SessionMeta(poison) = &session.entries[0].item else {
            panic!("expected session_meta");
        };
        assert!(poison.id.is_none());
        assert_eq!(field(&poison.raw, "id").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(
            session.rollout_thread_id.as_deref(),
            Some("019f67f0-2a3b-7c4d-8e5f-000000000303")
        );
        assert_eq!(
            session.session_id.as_deref(),
            Some("019f67f0-2a3b-7c4d-8e5f-000000000303")
        );
    }

    #[test]
    fn identity_fold_skips_empty_id_session_meta() {
        let bytes = br#"{"timestamp":"2026-07-16T16:20:00.000Z","type":"session_meta","payload":{"id":""}}
{"timestamp":"2026-07-16T16:20:01.000Z","type":"session_meta","payload":{"id":"valid","session_id":"root"}}"#;
        let session = parse_codex_bytes(bytes);
        assert_eq!(session.rollout_thread_id.as_deref(), Some("valid"));
        assert_eq!(session.session_id.as_deref(), Some("root"));
    }

    #[test]
    fn identity_instructions_vs_base_instructions_drift() {
        let old = fixture("101");
        assert!(
            old.instructions.is_some(),
            "old file carries plain instructions"
        );
        assert!(old.base_instructions.is_none());

        for tag in ["202", "303"] {
            let s = fixture(tag);
            assert!(s.instructions.is_none(), "{tag} has no plain instructions");
            let base = s.base_instructions.expect("base_instructions object");
            assert_eq!(
                field(&base, "text").and_then(|v| v.as_str()).is_some(),
                true,
                "{tag} base text"
            );
        }
    }

    #[test]
    fn passthrough_turn_id_lifted_onto_response_item() {
        let session = fixture("303");
        let items: Vec<(&str, Option<&str>)> = session
            .entries
            .iter()
            .map(|e| match &e.item {
                CodexItem::ResponseItem(ri) => (top_key(&e.item), ri.turn_id.as_deref()),
                _ => (top_key(&e.item), None),
            })
            .collect();
        let turn_id = Some("019f67f0-0aa1-7000-8000-0000000000c1");
        assert_eq!(
            items,
            vec![
                ("session_meta", None),
                ("event_msg", None),
                ("response_item", turn_id),
                ("world_state", None),
                ("turn_context", None),
                ("response_item", turn_id),
                ("event_msg", None),
                ("response_item", turn_id),
                ("event_msg", None),
                ("response_item", turn_id),
                ("response_item", turn_id),
                ("response_item", turn_id),
                ("event_msg", None),
                ("event_msg", None),
                ("event_msg", None),
            ]
        );
    }

    #[test]
    fn event_raw_slots_and_patch_status_are_preserved() {
        let bytes = br#"{"timestamp":"2026-07-16T16:20:00.000Z","type":"event_msg","payload":{"type":"user_message","local_images":{"shape":"object"},"text_elements":[{"kind":"text"}]}}
{"timestamp":"2026-07-16T16:20:01.000Z","type":"event_msg","payload":{"type":"agent_message","memory_citation":{"source":"memory"}}}
{"timestamp":"2026-07-16T16:20:02.000Z","type":"event_msg","payload":{"type":"agent_message","memory_citation":null}}
{"timestamp":"2026-07-16T16:20:03.000Z","type":"event_msg","payload":{"type":"patch_apply_end","status":"completed"}}"#;
        let session = parse_codex_bytes(bytes);
        let CodexItem::EventMsg(EventMsg::UserMessage {
            local_images,
            text_elements,
            ..
        }) = &session.entries[0].item
        else {
            panic!("expected user_message");
        };
        assert_eq!(
            local_images
                .as_ref()
                .and_then(|v| field(v, "shape"))
                .and_then(|v| v.as_str()),
            Some("object")
        );
        assert!(text_elements.as_ref().is_some_and(|v| v.is_array()));
        let CodexItem::EventMsg(EventMsg::AgentMessage {
            memory_citation, ..
        }) = &session.entries[1].item
        else {
            panic!("expected agent_message");
        };
        assert_eq!(
            memory_citation
                .as_ref()
                .and_then(|v| field(v, "source"))
                .and_then(|v| v.as_str()),
            Some("memory")
        );
        let CodexItem::EventMsg(EventMsg::AgentMessage {
            memory_citation, ..
        }) = &session.entries[2].item
        else {
            panic!("expected agent_message");
        };
        assert!(memory_citation.is_none());
        let CodexItem::EventMsg(EventMsg::PatchApplyEnd { status, .. }) = &session.entries[3].item
        else {
            panic!("expected patch_apply_end");
        };
        assert_eq!(status.as_deref(), Some("completed"));
    }

    #[test]
    fn old_response_items_without_passthrough_have_no_turn_id() {
        let session = fixture("101");
        assert!(session.entries.iter().all(|e| match &e.item {
            CodexItem::ResponseItem(ri) => ri.turn_id.is_none(),
            _ => true,
        }));
    }

    #[test]
    fn custom_tool_call_input_kept_verbatim() {
        let session = fixture("303");
        let input = session
            .entries
            .iter()
            .find_map(|e| match &e.item {
                CodexItem::ResponseItem(ri) => match &ri.payload {
                    ResponseItemPayload::CustomToolCall { input, .. } => input.as_deref(),
                    _ => None,
                },
                _ => None,
            })
            .expect("a custom_tool_call");
        assert_eq!(input, "python3 -c 'print(\"demo\"[::-1])'");
    }

    #[test]
    fn function_call_arguments_kept_verbatim() {
        let session = fixture("101");
        let args = session
            .entries
            .iter()
            .find_map(|e| match &e.item {
                CodexItem::ResponseItem(ri) => match &ri.payload {
                    ResponseItemPayload::FunctionCall { arguments, .. } => arguments.as_deref(),
                    _ => None,
                },
                _ => None,
            })
            .expect("a function_call");
        assert_eq!(
            args,
            r#"{"cmd": "python3 -c 'print(\"demo\"[::-1])'", "workdir": "/tmp/demo"}"#
        );
    }

    #[test]
    fn timestamp_parsed_from_z_suffixed_utc() {
        let session = fixture("101");
        let first = &session.entries[0];
        assert_eq!(first.line_index, 0);
        let ts = first.timestamp.expect("first entry timestamp");
        assert_eq!(ts.to_rfc3339(), "2026-01-20T04:00:05.100+00:00");
    }

    #[test]
    fn token_count_info_typed_when_present() {
        let line = r#"{"timestamp":"2026-01-20T04:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1,"total_tokens":18},"last_token_usage":{"input_tokens":3,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":4},"model_context_window":272000},"rate_limits":null}}"#;
        let session = parse_codex_bytes(line.as_bytes());
        let CodexItem::EventMsg(EventMsg::TokenCount { info, rate_limits }) =
            &session.entries[0].item
        else {
            panic!("expected token_count");
        };
        assert!(rate_limits.is_none());
        let info = info.as_ref().expect("info present");
        let total = info.total_token_usage.as_ref().expect("total usage");
        assert_eq!(total.total_tokens, Some(18));
        assert_eq!(total.cached_input_tokens, Some(2));
        assert_eq!(info.model_context_window, Some(272000));
    }

    #[test]
    fn compacted_replacement_history_typed_and_continues_turn() {
        let session = fixture("050d");
        let compacted = session
            .entries
            .iter()
            .find_map(|e| match &e.item {
                CodexItem::Compacted(c) => Some(c),
                _ => None,
            })
            .expect("a compacted entry");
        assert_eq!(compacted.window_number, Some(2));
        let history = compacted
            .replacement_history
            .as_ref()
            .expect("replacement history");
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history[0].payload,
            ResponseItemPayload::Message { .. }
        ));
        assert_eq!(
            history[0].turn_id.as_deref(),
            Some("019f6823-0aa1-7000-8000-0000000000c9")
        );
    }

    #[test]
    fn inter_agent_metadata_trigger_turn_captured() {
        let session = fixture("404");
        let trigger = session.entries.iter().find_map(|e| match &e.item {
            CodexItem::InterAgentCommunicationMetadata { trigger_turn } => Some(*trigger_turn),
            _ => None,
        });
        assert_eq!(trigger, Some(Some(true)));
    }

    #[test]
    fn empty_input_yields_empty_session() {
        let session = parse_codex_bytes(b"");
        assert!(session.entries.is_empty());
        assert!(session.rollout_thread_id.is_none());
    }
}
