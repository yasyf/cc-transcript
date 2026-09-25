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

pub fn entry_charge(entry: &Entry) -> MemoryCharge {
    entry.heap_charge()
}

pub fn value_charge(value: &Value) -> MemoryCharge {
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
