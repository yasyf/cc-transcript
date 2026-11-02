use std::mem::size_of;
use std::ops::{Add, AddAssign};

use chrono::{DateTime, FixedOffset};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::types::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryCharge {
    pub owned_capacity_bytes: usize,
    pub opaque_dom_accounted_bytes: usize,
}

impl Add for MemoryCharge {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            owned_capacity_bytes: self.owned_capacity_bytes + other.owned_capacity_bytes,
            opaque_dom_accounted_bytes: self.opaque_dom_accounted_bytes
                + other.opaque_dom_accounted_bytes,
        }
    }
}

impl AddAssign for MemoryCharge {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

trait HeapCharge {
    fn heap_charge(&self) -> MemoryCharge;
}

impl HeapCharge for String {
    fn heap_charge(&self) -> MemoryCharge {
        MemoryCharge {
            owned_capacity_bytes: self.capacity(),
            ..MemoryCharge::default()
        }
    }
}

impl<T: HeapCharge> HeapCharge for Option<T> {
    fn heap_charge(&self) -> MemoryCharge {
        self.as_ref()
            .map_or_else(MemoryCharge::default, HeapCharge::heap_charge)
    }
}

impl<T: HeapCharge> HeapCharge for Vec<T> {
    fn heap_charge(&self) -> MemoryCharge {
        self.iter().fold(
            MemoryCharge {
                owned_capacity_bytes: self.capacity() * size_of::<T>(),
                ..MemoryCharge::default()
            },
            |charge, item| charge + item.heap_charge(),
        )
    }
}

impl HeapCharge for Value {
    fn heap_charge(&self) -> MemoryCharge {
        value_charge(self)
    }
}

macro_rules! inline_charge {
    ($($ty:ty),+ $(,)?) => {
        $(impl HeapCharge for $ty {
            fn heap_charge(&self) -> MemoryCharge {
                MemoryCharge::default()
            }
        })+
    };
}

macro_rules! struct_charge {
    ($ty:ident { $($field:ident),+ $(,)? }) => {
        impl HeapCharge for $ty {
            fn heap_charge(&self) -> MemoryCharge {
                let Self { $($field),+ } = self;
                MemoryCharge::default() $(+ $field.heap_charge())+
            }
        }
    };
}

macro_rules! enum_charge {
    ($ty:ident { $($variant:ident),+ $(,)? }) => {
        impl HeapCharge for $ty {
            fn heap_charge(&self) -> MemoryCharge {
                match self {
                    $(Self::$variant(inner) => inner.heap_charge()),+
                }
            }
        }
    };
}

inline_charge!(bool, i64, DateTime<FixedOffset>);

impl HeapCharge for ModeChannel {
    fn heap_charge(&self) -> MemoryCharge {
        match self {
            Self::Mode | Self::PermissionMode => MemoryCharge::default(),
        }
    }
}

struct_charge!(EntryMeta {
    uuid,
    parent_uuid,
    session_id,
    timestamp,
    cwd,
    git_branch,
    version,
    is_sidechain,
    is_meta,
    entrypoint,
    is_compact_summary,
    is_visible_in_transcript_only,
    user_type,
    slug,
});
struct_charge!(Attribution {
    plugin,
    skill,
    mcp_server,
    mcp_tool
});
struct_charge!(ApiError {
    error,
    status,
    details
});
struct_charge!(Question {
    question,
    header,
    multi_select,
    labels
});
struct_charge!(ToolUseBlock {
    id,
    name,
    run_in_background,
    subagent_type,
    file_path,
    questions,
    input,
});
struct_charge!(ToolResultBlock {
    tool_use_id,
    content,
    is_error,
    is_async,
    tool_use_result,
    denial_kind,
});
struct_charge!(FallbackBlock {
    from_model,
    to_model
});
struct_charge!(UserEntry {
    meta,
    content,
    prompt_id,
    prompt_source,
    queue_priority,
    image_paste_ids,
    source_tool_use_id,
    source_tool_assistant_uuid,
    mcp_meta,
    permission_mode,
    interrupted_message_id,
});
struct_charge!(AssistantEntry {
    meta,
    model,
    blocks,
    stop_reason,
    usage,
    request_id,
    forked_from,
    attribution,
    api_error,
});
struct_charge!(HookInfo {
    command,
    duration_ms
});
struct_charge!(StopHookSummary {
    hook_count,
    hook_infos,
    hook_errors,
    hook_additional_context,
    prevented_continuation,
    stop_reason,
    has_output,
    tool_use_id,
});
struct_charge!(PreservedSegment {
    head_uuid,
    anchor_uuid,
    tail_uuid
});
struct_charge!(PreservedMessages {
    anchor_uuid,
    uuids,
    all_uuids
});
struct_charge!(CompactBoundary {
    trigger,
    pre_tokens,
    post_tokens,
    duration_ms,
    cumulative_dropped_tokens,
    pre_compact_discovered_tools,
    preserved_segment,
    preserved_messages,
    logical_parent_uuid,
    precomputed,
});
struct_charge!(TurnDuration {
    duration_ms,
    message_count,
    pending_workflow_count,
    pending_background_agent_count,
});
struct_charge!(ModelRefusalFallback {
    api_refusal_category,
    api_refusal_explanation,
    trigger,
    direction,
    original_model,
    fallback_model,
    retracted_message_uuids,
    refused_user_message_uuid,
});
struct_charge!(SystemEntry {
    meta,
    subtype,
    content,
    level,
    detail
});
struct_charge!(ModeEntry {
    session_id,
    channel,
    value
});
struct_charge!(OtherEntry { ty, raw });
struct_charge!(HookSuccess {
    hook_name,
    hook_event,
    tool_use_id,
    command,
    content,
    stdout,
    stderr,
    exit_code,
    duration_ms,
});
struct_charge!(HookBlockingError {
    hook_name,
    hook_event,
    tool_use_id,
    blocking_error
});
struct_charge!(HookNonBlockingError {
    hook_name,
    hook_event,
    tool_use_id,
    command,
    stdout,
    stderr,
    exit_code,
    duration_ms,
});
struct_charge!(HookCancelled {
    hook_name,
    hook_event,
    tool_use_id,
    command,
    duration_ms,
    timed_out,
    timeout_ms,
});
struct_charge!(HookAdditionalContext {
    hook_name,
    hook_event,
    tool_use_id,
    content
});
struct_charge!(AsyncHookResponse {
    hook_name,
    hook_event,
    process_id,
    stdout,
    stderr,
    exit_code,
    response,
});
struct_charge!(QueuedCommand {
    prompt,
    command_mode,
    origin
});
struct_charge!(DeferredToolsDelta {
    added_names,
    removed_names,
    raw
});
struct_charge!(AttachmentEntry {
    meta,
    attachment_type,
    detail
});
struct_charge!(CacheCreation {
    ephemeral_5m_input_tokens,
    ephemeral_1h_input_tokens
});
struct_charge!(ServerToolUse {
    web_search_requests,
    web_fetch_requests
});
struct_charge!(Usage {
    input_tokens,
    output_tokens,
    cache_read_input_tokens,
    cache_creation_input_tokens,
    cache_creation,
    service_tier,
    inference_geo,
    server_tool_use,
});

enum_charge!(UserContent { Plain, Blocks });
enum_charge!(SystemDetail {
    StopHookSummary,
    CompactBoundary,
    TurnDuration,
    ModelRefusalFallback,
    Other,
});
enum_charge!(AttachmentDetail {
    HookSuccess,
    HookBlockingError,
    HookNonBlockingError,
    HookCancelled,
    HookAdditionalContext,
    AsyncHookResponse,
    QueuedCommand,
    DeferredToolsDelta,
    Other,
});
enum_charge!(Entry {
    User,
    Assistant,
    System,
    Mode,
    Other,
    Attachment
});

impl HeapCharge for ContentBlock {
    fn heap_charge(&self) -> MemoryCharge {
        match self {
            Self::Text(text) | Self::Thinking(text) => text.heap_charge(),
            Self::ToolUse(tool) => tool.heap_charge(),
            Self::ToolResult(result) => result.heap_charge(),
            Self::Fallback(fallback) => fallback.heap_charge(),
            Self::Other { ty, raw } => ty.heap_charge() + raw.heap_charge(),
        }
    }
}

pub const SONIC_PADDING_BYTES: usize = 64;
pub const SONIC_SERIALIZER_BYTES: usize = 128;
pub const SONIC_SHARED_HEADER_BYTES: usize =
    2 * size_of::<usize>() + size_of::<Vec<u8>>() + 3 * size_of::<usize>();
pub const BUMP_CHUNK_FOOTER_BYTES: usize = 48;
pub const BUMP_FIRST_CHUNK_USABLE_BYTES: usize = (1 << 9) - 64;
pub const BUMP_CHUNK_ROUNDING_BYTES: usize = 4096;

pub fn arena_node_bound(source_len: usize) -> Option<usize> {
    source_len
        .checked_mul(size_of::<Value>())?
        .checked_add(size_of::<Value>())
}

pub fn arena_bound(source_len: usize) -> Option<usize> {
    arena_node_bound(source_len)?
        .max(BUMP_FIRST_CHUNK_USABLE_BYTES)
        .checked_add(
            BUMP_CHUNK_FOOTER_BYTES
                + BUMP_CHUNK_ROUNDING_BYTES
                + SONIC_SHARED_HEADER_BYTES
                + SONIC_PADDING_BYTES,
        )?
        .checked_add(source_len)
}

pub fn sonic_node_buffer_bytes(source_len: usize) -> usize {
    size_of::<Vec<Value>>() + (source_len / 2 + 2) * size_of::<Value>()
}

pub fn dom_parse_bound(source_len: usize) -> Option<usize> {
    arena_bound(source_len)?.checked_add(sonic_node_buffer_bytes(source_len))
}

/// A sonic-rs arena an entry keeps alive: a clone of its root and its source length.
#[derive(Debug, Clone)]
pub struct SourceArena {
    pub root: Value,
    pub source_len: usize,
}

#[cfg(test)]
thread_local! {
    pub(crate) static DOM_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn count_dom_work(units: usize) {
    DOM_WORK.with(|work| work.set(work.get() + units));
}

pub fn pins_arena(value: &Value) -> bool {
    match value.get_type() {
        JsonType::Null | JsonType::Boolean => false,
        JsonType::Array => !value.as_array().unwrap().is_empty(),
        JsonType::Object => !value.as_object().unwrap().is_empty(),
        JsonType::Number | JsonType::String => true,
    }
}

pub fn container_node_bytes(value: &Value) -> usize {
    #[cfg(test)]
    count_dom_work(1);
    match value.get_type() {
        JsonType::Array => {
            let array = value.as_array().unwrap();
            if array.is_empty() {
                0
            } else {
                (array.len() + 1) * size_of::<Value>()
                    + array.iter().map(container_node_bytes).sum::<usize>()
            }
        }
        JsonType::Object => {
            let object = value.as_object().unwrap();
            if object.is_empty() {
                0
            } else {
                (2 * object.len() + 1) * size_of::<Value>()
                    + object
                        .iter()
                        .map(|(_, item)| container_node_bytes(item))
                        .sum::<usize>()
            }
        }
        _ => 0,
    }
}

pub fn arena_requested_bytes(root: &Value) -> usize {
    2 * size_of::<Value>() + container_node_bytes(root)
}

/// A sonic-rs arena's charge: `Arc<Shared>`, padded source, first bump chunk. Later chunks
/// and rounding are declared residue, at most `6 * requested + 4096` (sonic_arena_model.rs).
pub fn arena_bytes(root: &Value, source_len: usize) -> usize {
    SONIC_SHARED_HEADER_BYTES
        + source_len
        + SONIC_PADDING_BYTES
        + arena_requested_bytes(root).max(BUMP_FIRST_CHUNK_USABLE_BYTES)
        + BUMP_CHUNK_FOOTER_BYTES
}

pub fn arena_charge(arena: &SourceArena) -> MemoryCharge {
    MemoryCharge {
        owned_capacity_bytes: 0,
        opaque_dom_accounted_bytes: arena_bytes(&arena.root, arena.source_len),
    }
}

pub fn block_charge(block: &ContentBlock) -> MemoryCharge {
    block.heap_charge()
        + MemoryCharge {
            owned_capacity_bytes: size_of::<ContentBlock>(),
            opaque_dom_accounted_bytes: 0,
        }
}

pub fn entry_charge(entry: &Entry) -> MemoryCharge {
    entry.heap_charge()
}

pub(crate) fn tool_use_charge(tool: &ToolUseBlock) -> MemoryCharge {
    tool.heap_charge()
}

pub fn value_charge(value: &Value) -> MemoryCharge {
    #[cfg(test)]
    count_dom_work(1);
    let opaque_dom_accounted_bytes = match value.get_type() {
        JsonType::Null | JsonType::Boolean => 0,
        JsonType::Number => value
            .as_raw_number()
            .map_or(0, |number| number.as_str().len()),
        JsonType::String => value.as_str().unwrap().len(),
        JsonType::Array => {
            let array = value.as_array().unwrap();
            array.capacity() * size_of::<Value>()
                + array
                    .iter()
                    .map(|item| value_charge(item).opaque_dom_accounted_bytes)
                    .sum::<usize>()
        }
        JsonType::Object => {
            let object = value.as_object().unwrap();
            object.capacity() * size_of::<(Value, Value)>()
                + object
                    .iter()
                    .map(|(key, item)| key.len() + value_charge(item).opaque_dom_accounted_bytes)
                    .sum::<usize>()
        }
    };
    MemoryCharge {
        owned_capacity_bytes: 0,
        opaque_dom_accounted_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_entry;

    fn parse(raw: &str) -> Entry {
        parse_entry(sonic_rs::from_str(raw).unwrap()).unwrap()
    }

    #[test]
    fn plain_user_counts_reserved_strings_without_entry_inline_storage() {
        let mut entry = parse(
            r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":"hello"}}"#,
        );
        let Entry::User(user) = &mut entry else {
            panic!()
        };
        user.meta.uuid.reserve(64);
        let UserContent::Plain(text) = &user.content else {
            panic!()
        };
        let expected =
            user.meta.uuid.capacity() + user.meta.session_id.capacity() + text.capacity();
        assert_eq!(
            entry_charge(&entry),
            MemoryCharge {
                owned_capacity_bytes: expected,
                opaque_dom_accounted_bytes: 0,
            }
        );
    }

    #[test]
    fn assistant_counts_vector_reservation_and_nested_dom_separately() {
        let mut entry = parse(
            r#"{"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"model":"m","content":[{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"a.rs","extra":["abc",true]}}]}}"#,
        );
        let before = entry_charge(&entry);
        let Entry::Assistant(assistant) = &mut entry else {
            panic!()
        };
        let old_capacity = assistant.blocks.capacity();
        assistant.blocks.reserve(16);
        let extra = (assistant.blocks.capacity() - old_capacity) * size_of::<ContentBlock>();
        let after = entry_charge(&entry);
        assert_eq!(
            after.owned_capacity_bytes,
            before.owned_capacity_bytes + extra
        );
        assert_eq!(
            after.opaque_dom_accounted_bytes,
            before.opaque_dom_accounted_bytes
        );
        assert!(after.opaque_dom_accounted_bytes > 0);
    }

    #[test]
    fn opaque_dom_charge_walks_public_container_capacities() {
        let value: Value = sonic_rs::from_str(r#"{"a":["xyz",true,null]}"#).unwrap();
        let object = value.as_object().unwrap();
        let array = object.iter().next().unwrap().1.as_array().unwrap();
        assert_eq!(
            value_charge(&value),
            MemoryCharge {
                owned_capacity_bytes: 0,
                opaque_dom_accounted_bytes: object.capacity() * size_of::<(Value, Value)>()
                    + 1
                    + array.capacity() * size_of::<Value>()
                    + 3,
            }
        );
    }

    #[test]
    fn numeric_vector_and_nested_strings_count_capacity() {
        let mut values = vec![1_i64, 2];
        values.reserve(8);
        assert_eq!(
            values.heap_charge().owned_capacity_bytes,
            values.capacity() * size_of::<i64>()
        );
        let strings = vec![String::with_capacity(128), String::from("text")];
        assert_eq!(
            strings.heap_charge().owned_capacity_bytes,
            strings.capacity() * size_of::<String>()
                + strings.iter().map(String::capacity).sum::<usize>()
        );
    }

    #[test]
    fn key_heavy_objects_charge_both_dom_slots_even_with_null_values() {
        let raw = format!(
            "{{{}}}",
            (0..128)
                .map(|i| format!("\"k{i}\":null"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let value: Value = sonic_rs::from_str(&raw).unwrap();
        let charge = value_charge(&value);
        assert_eq!(charge.owned_capacity_bytes, 0);
        assert!(charge.opaque_dom_accounted_bytes >= 256 * size_of::<Value>());
        assert!(charge.opaque_dom_accounted_bytes > raw.len() * 2);
    }
}

use crate::codex::types as codex;
impl HeapCharge for usize {
    fn heap_charge(&self) -> MemoryCharge {
        MemoryCharge::default()
    }
}
impl HeapCharge for SourceArena {
    fn heap_charge(&self) -> MemoryCharge {
        arena_charge(self)
    }
}
impl HeapCharge for codex::CodexEntry {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::CodexEntry {
            line_index,
            timestamp,
            item,
            source,
        } = self;
        MemoryCharge::default()
            + line_index.heap_charge()
            + timestamp.heap_charge()
            + item.heap_charge()
            + source.heap_charge()
    }
}
impl HeapCharge for codex::CodexOther {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::CodexOther { ty, raw } = self;
        MemoryCharge::default() + ty.heap_charge() + raw.heap_charge()
    }
}
impl HeapCharge for codex::SessionMeta {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::SessionMeta {
            raw,
            id,
            session_id,
            parent_thread_id,
            forked_from_id,
            thread_source,
            cwd,
            originator,
            cli_version,
            model_provider,
            agent_path,
            agent_nickname,
            agent_role,
            instructions,
            base_instructions,
            source,
            git,
        } = self;
        MemoryCharge::default()
            + raw.heap_charge()
            + id.heap_charge()
            + session_id.heap_charge()
            + parent_thread_id.heap_charge()
            + forked_from_id.heap_charge()
            + thread_source.heap_charge()
            + cwd.heap_charge()
            + originator.heap_charge()
            + cli_version.heap_charge()
            + model_provider.heap_charge()
            + agent_path.heap_charge()
            + agent_nickname.heap_charge()
            + agent_role.heap_charge()
            + instructions.heap_charge()
            + base_instructions.heap_charge()
            + source.heap_charge()
            + git.heap_charge()
    }
}
impl HeapCharge for codex::WorldState {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::WorldState { full, state } = self;
        MemoryCharge::default() + full.heap_charge() + state.heap_charge()
    }
}
impl HeapCharge for codex::Compacted {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::Compacted {
            message,
            replacement_history,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = self;
        MemoryCharge::default()
            + message.heap_charge()
            + replacement_history.heap_charge()
            + window_number.heap_charge()
            + first_window_id.heap_charge()
            + previous_window_id.heap_charge()
            + window_id.heap_charge()
    }
}
impl HeapCharge for codex::ResponseItem {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::ResponseItem {
            id,
            turn_id,
            payload,
        } = self;
        MemoryCharge::default() + id.heap_charge() + turn_id.heap_charge() + payload.heap_charge()
    }
}
impl HeapCharge for codex::TokenUsage {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
        } = self;
        MemoryCharge::default()
            + input_tokens.heap_charge()
            + cached_input_tokens.heap_charge()
            + output_tokens.heap_charge()
            + reasoning_output_tokens.heap_charge()
            + total_tokens.heap_charge()
    }
}
impl HeapCharge for codex::TokenUsageInfo {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::TokenUsageInfo {
            total_token_usage,
            last_token_usage,
            model_context_window,
        } = self;
        MemoryCharge::default()
            + total_token_usage.heap_charge()
            + last_token_usage.heap_charge()
            + model_context_window.heap_charge()
    }
}
impl HeapCharge for codex::CodexSession {
    fn heap_charge(&self) -> MemoryCharge {
        let codex::CodexSession {
            entries,
            rollout_thread_id,
            session_id,
            parent_thread_id,
            forked_from_id,
            cli_version,
            originator,
            cwd,
            instructions,
            base_instructions,
        } = self;
        MemoryCharge::default()
            + entries.heap_charge()
            + rollout_thread_id.heap_charge()
            + session_id.heap_charge()
            + parent_thread_id.heap_charge()
            + forked_from_id.heap_charge()
            + cli_version.heap_charge()
            + originator.heap_charge()
            + cwd.heap_charge()
            + instructions.heap_charge()
            + base_instructions.heap_charge()
    }
}
impl HeapCharge for codex::CodexItem {
    fn heap_charge(&self) -> MemoryCharge {
        match self {
            codex::CodexItem::SessionMeta(inner) => inner.heap_charge(),
            codex::CodexItem::ResponseItem(inner) => inner.heap_charge(),
            codex::CodexItem::EventMsg(inner) => inner.heap_charge(),
            codex::CodexItem::TurnContext(inner) => inner.heap_charge(),
            codex::CodexItem::WorldState(inner) => inner.heap_charge(),
            codex::CodexItem::Compacted(inner) => inner.heap_charge(),
            codex::CodexItem::InterAgentCommunicationMetadata { trigger_turn } => {
                MemoryCharge::default() + trigger_turn.heap_charge()
            }
            codex::CodexItem::Other(inner) => inner.heap_charge(),
        }
    }
}
impl HeapCharge for codex::ResponseItemPayload {
    fn heap_charge(&self) -> MemoryCharge {
        match self {
            codex::ResponseItemPayload::Message {
                role,
                content,
                phase,
            } => {
                MemoryCharge::default()
                    + role.heap_charge()
                    + content.heap_charge()
                    + phase.heap_charge()
            }
            codex::ResponseItemPayload::AgentMessage {
                author,
                recipient,
                content,
            } => {
                MemoryCharge::default()
                    + author.heap_charge()
                    + recipient.heap_charge()
                    + content.heap_charge()
            }
            codex::ResponseItemPayload::Reasoning {
                summary,
                content,
                encrypted_content,
            } => {
                MemoryCharge::default()
                    + summary.heap_charge()
                    + content.heap_charge()
                    + encrypted_content.heap_charge()
            }
            codex::ResponseItemPayload::LocalShellCall {
                call_id,
                status,
                action,
            } => {
                MemoryCharge::default()
                    + call_id.heap_charge()
                    + status.heap_charge()
                    + action.heap_charge()
            }
            codex::ResponseItemPayload::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
            } => {
                MemoryCharge::default()
                    + name.heap_charge()
                    + namespace.heap_charge()
                    + arguments.heap_charge()
                    + call_id.heap_charge()
            }
            codex::ResponseItemPayload::FunctionCallOutput { call_id, output } => {
                MemoryCharge::default() + call_id.heap_charge() + output.heap_charge()
            }
            codex::ResponseItemPayload::CustomToolCall {
                name,
                namespace,
                input,
                status,
                call_id,
            } => {
                MemoryCharge::default()
                    + name.heap_charge()
                    + namespace.heap_charge()
                    + input.heap_charge()
                    + status.heap_charge()
                    + call_id.heap_charge()
            }
            codex::ResponseItemPayload::CustomToolCallOutput {
                call_id,
                name,
                output,
            } => {
                MemoryCharge::default()
                    + call_id.heap_charge()
                    + name.heap_charge()
                    + output.heap_charge()
            }
            codex::ResponseItemPayload::ToolSearchCall {
                call_id,
                status,
                execution,
                arguments,
            } => {
                MemoryCharge::default()
                    + call_id.heap_charge()
                    + status.heap_charge()
                    + execution.heap_charge()
                    + arguments.heap_charge()
            }
            codex::ResponseItemPayload::ToolSearchOutput {
                call_id,
                status,
                execution,
                tools,
            } => {
                MemoryCharge::default()
                    + call_id.heap_charge()
                    + status.heap_charge()
                    + execution.heap_charge()
                    + tools.heap_charge()
            }
            codex::ResponseItemPayload::WebSearchCall { status, action } => {
                MemoryCharge::default() + status.heap_charge() + action.heap_charge()
            }
            codex::ResponseItemPayload::ImageGenerationCall {
                status,
                revised_prompt,
                result,
            } => {
                MemoryCharge::default()
                    + status.heap_charge()
                    + revised_prompt.heap_charge()
                    + result.heap_charge()
            }
            codex::ResponseItemPayload::Compaction { encrypted_content } => {
                MemoryCharge::default() + encrypted_content.heap_charge()
            }
            codex::ResponseItemPayload::ContextCompaction { encrypted_content } => {
                MemoryCharge::default() + encrypted_content.heap_charge()
            }
            codex::ResponseItemPayload::GhostSnapshot(inner) => inner.heap_charge(),
            codex::ResponseItemPayload::Other { ty, raw } => {
                MemoryCharge::default() + ty.heap_charge() + raw.heap_charge()
            }
        }
    }
}
impl HeapCharge for codex::EventMsg {
    fn heap_charge(&self) -> MemoryCharge {
        match self {
            codex::EventMsg::TokenCount { info, rate_limits } => {
                MemoryCharge::default() + info.heap_charge() + rate_limits.heap_charge()
            }
            codex::EventMsg::AgentMessage {
                message,
                phase,
                memory_citation,
            } => {
                MemoryCharge::default()
                    + message.heap_charge()
                    + phase.heap_charge()
                    + memory_citation.heap_charge()
            }
            codex::EventMsg::UserMessage {
                message,
                images,
                local_images,
                text_elements,
            } => {
                MemoryCharge::default()
                    + message.heap_charge()
                    + images.heap_charge()
                    + local_images.heap_charge()
                    + text_elements.heap_charge()
            }
            codex::EventMsg::AgentReasoning { text } => {
                MemoryCharge::default() + text.heap_charge()
            }
            codex::EventMsg::AgentReasoningRawContent { text } => {
                MemoryCharge::default() + text.heap_charge()
            }
            codex::EventMsg::TaskStarted {
                turn_id,
                started_at,
                model_context_window,
                collaboration_mode_kind,
            } => {
                MemoryCharge::default()
                    + turn_id.heap_charge()
                    + started_at.heap_charge()
                    + model_context_window.heap_charge()
                    + collaboration_mode_kind.heap_charge()
            }
            codex::EventMsg::TaskComplete {
                turn_id,
                last_agent_message,
                completed_at,
                duration_ms,
                time_to_first_token_ms,
            } => {
                MemoryCharge::default()
                    + turn_id.heap_charge()
                    + last_agent_message.heap_charge()
                    + completed_at.heap_charge()
                    + duration_ms.heap_charge()
                    + time_to_first_token_ms.heap_charge()
            }
            codex::EventMsg::TurnAborted {
                turn_id,
                reason,
                completed_at,
                duration_ms,
            } => {
                MemoryCharge::default()
                    + turn_id.heap_charge()
                    + reason.heap_charge()
                    + completed_at.heap_charge()
                    + duration_ms.heap_charge()
            }
            codex::EventMsg::PatchApplyEnd {
                call_id,
                turn_id,
                status,
                success,
                stdout,
                stderr,
                changes,
            } => {
                MemoryCharge::default()
                    + call_id.heap_charge()
                    + turn_id.heap_charge()
                    + status.heap_charge()
                    + success.heap_charge()
                    + stdout.heap_charge()
                    + stderr.heap_charge()
                    + changes.heap_charge()
            }
            codex::EventMsg::ContextCompacted => MemoryCharge::default(),
            codex::EventMsg::SubAgentActivity {
                event_id,
                occurred_at_ms,
                agent_thread_id,
                agent_path,
                kind,
            } => {
                MemoryCharge::default()
                    + event_id.heap_charge()
                    + occurred_at_ms.heap_charge()
                    + agent_thread_id.heap_charge()
                    + agent_path.heap_charge()
                    + kind.heap_charge()
            }
            codex::EventMsg::ThreadRolledBack { num_turns } => {
                MemoryCharge::default() + num_turns.heap_charge()
            }
            codex::EventMsg::ExecCommandEnd(inner) => inner.heap_charge(),
            codex::EventMsg::McpToolCallEnd(inner) => inner.heap_charge(),
            codex::EventMsg::WebSearchEnd(inner) => inner.heap_charge(),
            codex::EventMsg::ImageGenerationEnd(inner) => inner.heap_charge(),
            codex::EventMsg::ThreadSettingsApplied(inner) => inner.heap_charge(),
            codex::EventMsg::ItemCompleted(inner) => inner.heap_charge(),
            codex::EventMsg::ThreadGoalUpdated(inner) => inner.heap_charge(),
            codex::EventMsg::EnteredReviewMode(inner) => inner.heap_charge(),
            codex::EventMsg::ExitedReviewMode(inner) => inner.heap_charge(),
            codex::EventMsg::ViewImageToolCall(inner) => inner.heap_charge(),
            codex::EventMsg::CollabAgentSpawnEnd(inner) => inner.heap_charge(),
            codex::EventMsg::CollabWaitingEnd(inner) => inner.heap_charge(),
            codex::EventMsg::CollabCloseEnd(inner) => inner.heap_charge(),
            codex::EventMsg::Other { ty, raw } => {
                MemoryCharge::default() + ty.heap_charge() + raw.heap_charge()
            }
        }
    }
}
pub fn codex_entry_charge(entry: &codex::CodexEntry) -> MemoryCharge {
    entry.heap_charge()
}
