from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import anyio
import orjson
import pytest

from cc_transcript import parse_event
from cc_transcript.models import (
    ApiError,
    AssistantEvent,
    AttachmentEvent,
    Attribution,
    CacheCreation,
    CompactBoundary,
    EventUuid,
    FallbackBlock,
    HookAdditionalContext,
    HookCancelled,
    HookInfo,
    HookSuccess,
    McpServer,
    ModeEvent,
    ModelRefusalFallback,
    ModelUsage,
    OtherAttachment,
    OtherBlock,
    OtherEvent,
    OtherSystemDetail,
    PreservedMessages,
    PreservedSegment,
    QueuedCommand,
    ServerToolUse,
    SessionId,
    StopHookSummary,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    TurnDuration,
    Usage,
    UserEvent,
)
from cc_transcript.parser import parse_events_async, parse_events_from_bytes, parse_print_result
from tests.support import raw_envelope as envelope

TESTDATA = Path(__file__).parent / "testdata"


def user_str() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "  fix the bug  "})


def user_blocks() -> dict[str, Any]:
    return envelope(
        type="user",
        message={
            "role": "user",
            "content": [
                {"type": "text", "text": "here is context"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok output", "is_error": False},
            ],
        },
    )


def user_tool_result_list_content() -> dict[str, Any]:
    return envelope(
        type="user",
        message={
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_2",
                    "content": [{"type": "text", "text": "line a"}, {"type": "text", "text": "line b"}],
                    "is_error": True,
                }
            ],
        },
    )


def user_async_tool_result() -> dict[str, Any]:
    return envelope(
        type="user",
        toolUseResult={"isAsync": True, "status": "async_launched"},
        message={
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "toolu_async", "content": "launched", "is_error": False}
            ],
        },
    )


def user_sidechain() -> dict[str, Any]:
    return envelope(type="user", isSidechain=True, message={"role": "user", "content": "subagent prompt"})


def user_meta() -> dict[str, Any]:
    return envelope(type="user", isMeta=True, message={"role": "user", "content": "meta note"})


def user_interrupt() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "[Request interrupted by user]"})


def user_interrupt_casefolded_leading_whitespace() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "  [request INTERRUPTED by user for tool use]"})


def user_marker_mid_text() -> dict[str, Any]:
    return envelope(
        type="user", message={"role": "user", "content": "she quoted [Request interrupted by user] mid-text"}
    )


def user_agent_injection() -> dict[str, Any]:
    return envelope(
        type="user",
        message={"role": "user", "content": "<teammate-message from='reviewer'>please rebase</teammate-message>"},
    )


def assistant_text() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "done"}],
        },
    )


def assistant_with_usage() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "done"}],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 25437,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 25437},
                "service_tier": "standard",
                "inference_geo": "not_available",
                "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
            },
        },
    )


def assistant_thinking_tool() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "tool_use", "id": "toolu_9", "name": "Read", "input": {"file_path": "/x"}},
            ],
        },
    )


def assistant_synthetic() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "<synthetic>",
            "stop_reason": None,
            "content": [{"type": "text", "text": "noop"}],
        },
    )


def assistant_fallback() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": "tool_use",
            "content": [{"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"}}],
        },
    )


def assistant_unknown_block() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": None,
            "content": [{"type": "future_block", "payload": {"n": 1}}],
        },
    )


def user_all_envelope_fields() -> dict[str, Any]:
    return envelope(
        type="user",
        userType="external",
        slug="slug-value-11",
        promptId="prompt-id-22",
        promptSource="queued",
        queuePriority="later",
        imagePasteIds=[7, 42],
        sourceToolUseID="toolu_src_33",
        sourceToolAssistantUUID="asst-uuid-44",
        mcpMeta={"_meta": {"frontLoadedTabGroupId": 1149555059}},
        permissionMode="plan",
        message={"role": "user", "content": "envelope with all new fields"},
    )


def assistant_with_attribution() -> dict[str, Any]:
    return envelope(
        type="assistant",
        userType="external",
        slug="slug-asst-55",
        requestId="req-id-66",
        forkedFrom="forked-77",
        attributionPlugin="plugin-88",
        attributionSkill="skill-99",
        attributionMcpServer="server-aa",
        attributionMcpTool="tool-bb",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "attributed turn"}],
        },
    )


def assistant_api_error() -> dict[str, Any]:
    return envelope(
        type="assistant",
        isApiErrorMessage=True,
        error="rate_limit",
        apiErrorStatus=429,
        errorDetails="retry after 60s",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": None,
            "content": [{"type": "text", "text": "API Error: 429"}],
        },
    )


def assistant_error_without_flag() -> dict[str, Any]:
    return envelope(
        type="assistant",
        error="rate_limit",
        apiErrorStatus=429,
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "no flag, no api_error"}],
        },
    )


def user_field_only_interrupt() -> dict[str, Any]:
    return envelope(
        type="user",
        interruptedMessageId="msg_field_only_01",
        message={"role": "user", "content": "the assistant kept going so I stopped it"},
    )


def user_marker_only_interrupt() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "[Request interrupted by user]"})


def system_entry() -> dict[str, Any]:
    return envelope(type="system", subtype="stop_hook_summary", content="hook ran")


def mode_entry() -> dict[str, Any]:
    return {"type": "mode", "mode": "normal", "sessionId": "sess-1"}


def permission_mode_entry() -> dict[str, Any]:
    return {"type": "permission-mode", "permissionMode": "bypassPermissions", "sessionId": "sess-1"}


def summary_entry() -> dict[str, Any]:
    return {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x"}


def attachment_entry() -> dict[str, Any]:
    return envelope(type="attachment", attachment={"kind": "file"})


def queue_operation_entry() -> dict[str, Any]:
    return {"type": "queue-operation", "operation": "enqueue"}


def test_user_str_content() -> None:
    event = parse_event(user_str())
    assert isinstance(event, UserEvent)
    assert event.text == "  fix the bug  "
    assert event.blocks == ()
    assert event.interrupted is False
    assert event.is_agent_injected is False
    assert event.meta.uuid == EventUuid("uuid-1")
    assert event.meta.parent_uuid == EventUuid("uuid-0")
    assert event.meta.session_id == SessionId("sess-1")
    assert event.meta.timestamp == datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)
    assert event.meta.cwd == "/repo"
    assert event.meta.git_branch == "main"
    assert event.meta.cc_version == "1.2.3"
    assert event.meta.entrypoint == "cli"


def test_user_block_content() -> None:
    event = parse_event(user_blocks())
    assert isinstance(event, UserEvent)
    assert event.text == "here is context"
    assert event.blocks == (
        TextBlock("here is context"),
        ToolResultBlock(tool_use_id=ToolUseId("toolu_1"), content="ok output", is_error=False),
    )


def test_tool_result_list_content_flattens_and_errors() -> None:
    event = parse_event(user_tool_result_list_content())
    assert isinstance(event, UserEvent)
    assert event.text == ""
    assert event.blocks == (ToolResultBlock(tool_use_id=ToolUseId("toolu_2"), content="line aline b", is_error=True),)


def test_tool_result_is_async_from_entry_level_flag() -> None:
    event = parse_event(user_async_tool_result())
    assert isinstance(event, UserEvent)
    assert event.blocks == (
        ToolResultBlock(
            tool_use_id=ToolUseId("toolu_async"),
            content="launched",
            is_error=False,
            is_async=True,
            tool_use_result={"isAsync": True, "status": "async_launched"},
        ),
    )


def test_tool_result_is_async_defaults_false() -> None:
    event = parse_event(user_blocks())
    assert isinstance(event, UserEvent)
    (block,) = (b for b in event.blocks if isinstance(b, ToolResultBlock))
    assert block.is_async is False


def test_user_sidechain_meta_flags() -> None:
    event = parse_event(user_sidechain())
    assert isinstance(event, UserEvent)
    assert event.meta.is_sidechain is True
    meta_event = parse_event(user_meta())
    assert isinstance(meta_event, UserEvent)
    assert meta_event.meta.is_meta is True


def test_user_interrupt() -> None:
    event = parse_event(user_interrupt())
    assert isinstance(event, UserEvent)
    assert event.interrupted is True


def test_user_interrupt_casefolded_leading_whitespace() -> None:
    event = parse_event(user_interrupt_casefolded_leading_whitespace())
    assert isinstance(event, UserEvent)
    assert event.interrupted is True


def test_user_marker_mid_text_is_not_interrupted() -> None:
    event = parse_event(user_marker_mid_text())
    assert isinstance(event, UserEvent)
    assert event.interrupted is False


def test_user_agent_injection_flags_relay_banner() -> None:
    event = parse_event(user_agent_injection())
    assert isinstance(event, UserEvent)
    assert event.is_agent_injected is True
    assert event.interrupted is False
    prose = parse_event(user_str())
    assert isinstance(prose, UserEvent)
    assert prose.is_agent_injected is False


def test_assistant_text() -> None:
    event = parse_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert event.model == "claude-opus-4-7"
    assert event.text == "done"
    assert event.stop_reason == "end_turn"
    assert event.blocks == (TextBlock("done"),)


def test_assistant_usage_parses_cache_creation_split() -> None:
    event = parse_event(assistant_with_usage())
    assert isinstance(event, AssistantEvent)
    assert event.usage == Usage(
        input_tokens=10,
        output_tokens=7,
        cache_read_input_tokens=0,
        cache_creation_input_tokens=25437,
        cache_creation=CacheCreation(ephemeral_5m_input_tokens=0, ephemeral_1h_input_tokens=25437),
        service_tier="standard",
        inference_geo="not_available",
        server_tool_use=ServerToolUse(web_search_requests=0, web_fetch_requests=0),
    )


def test_assistant_without_usage_is_none() -> None:
    event = parse_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert "usage" not in assistant_text()["message"]
    assert event.usage is None


def test_assistant_thinking_and_tool_use() -> None:
    event = parse_event(assistant_thinking_tool())
    assert isinstance(event, AssistantEvent)
    assert event.text == ""
    assert event.stop_reason == "tool_use"
    assert event.blocks == (
        ThinkingBlock("let me think"),
        ToolUseBlock(id=ToolUseId("toolu_9"), name="Read", input={"file_path": "/x"}),
    )


def test_assistant_synthetic_is_not_filtered() -> None:
    event = parse_event(assistant_synthetic())
    assert isinstance(event, AssistantEvent)
    assert event.model == "<synthetic>"
    assert event.stop_reason is None


def test_assistant_fallback_block() -> None:
    event = parse_event(assistant_fallback())
    assert isinstance(event, AssistantEvent)
    assert event.text == ""
    assert event.blocks == (FallbackBlock(from_model="claude-fable-5", to_model="claude-opus-4-8"),)


def test_assistant_unknown_block_degrades_to_other() -> None:
    event = parse_event(assistant_unknown_block())
    assert isinstance(event, AssistantEvent)
    assert event.blocks == (OtherBlock(type="future_block", raw={"type": "future_block", "payload": {"n": 1}}),)


def test_user_envelope_and_prompt_fields() -> None:
    event = parse_event(user_all_envelope_fields())
    assert isinstance(event, UserEvent)
    assert event.meta.user_type == "external"
    assert event.meta.slug == "slug-value-11"
    assert event.prompt_id == "prompt-id-22"
    assert event.prompt_source == "queued"
    assert event.queue_priority == "later"
    assert event.image_paste_ids == (7, 42)
    assert event.source_tool_use_id == ToolUseId("toolu_src_33")
    assert event.source_tool_assistant_uuid == EventUuid("asst-uuid-44")
    assert event.mcp_meta == {"_meta": {"frontLoadedTabGroupId": 1149555059}}
    assert event.permission_mode == "plan"


def test_user_new_fields_default_none_on_bare_record() -> None:
    event = parse_event(user_str())
    assert isinstance(event, UserEvent)
    assert event.meta.user_type is None
    assert event.meta.slug is None
    assert event.prompt_id is None
    assert event.prompt_source is None
    assert event.queue_priority is None
    assert event.image_paste_ids is None
    assert event.source_tool_use_id is None
    assert event.source_tool_assistant_uuid is None
    assert event.mcp_meta is None
    assert event.permission_mode is None


def test_image_paste_ids_empty_list_is_empty_tuple_not_none() -> None:
    event = parse_event(
        envelope(type="user", imagePasteIds=[], message={"role": "user", "content": "no images"})
    )
    assert isinstance(event, UserEvent)
    assert event.image_paste_ids == ()


def test_assistant_attribution_and_request_fields() -> None:
    event = parse_event(assistant_with_attribution())
    assert isinstance(event, AssistantEvent)
    assert event.request_id == "req-id-66"
    assert event.forked_from == "forked-77"
    assert event.attribution == Attribution(
        plugin="plugin-88", skill="skill-99", mcp_server="server-aa", mcp_tool="tool-bb"
    )
    assert event.meta.user_type == "external"
    assert event.meta.slug == "slug-asst-55"


def test_assistant_without_attribution_is_none() -> None:
    event = parse_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert event.attribution is None
    assert event.request_id is None
    assert event.forked_from is None


def test_assistant_attribution_present_with_single_field() -> None:
    event = parse_event(
        envelope(
            type="assistant",
            attributionSkill="lonely-skill",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": None,
                "content": [{"type": "text", "text": "x"}],
            },
        )
    )
    assert isinstance(event, AssistantEvent)
    assert event.attribution == Attribution(
        plugin=None, skill="lonely-skill", mcp_server=None, mcp_tool=None
    )


def test_assistant_api_error_populated_when_flag_set() -> None:
    event = parse_event(assistant_api_error())
    assert isinstance(event, AssistantEvent)
    assert event.api_error == ApiError(error="rate_limit", status=429, details="retry after 60s")


def test_assistant_api_error_none_without_flag() -> None:
    event = parse_event(assistant_error_without_flag())
    assert isinstance(event, AssistantEvent)
    assert event.api_error is None


def test_assistant_api_error_none_on_plain_record() -> None:
    event = parse_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert event.api_error is None


def test_user_field_only_interrupt_surfaces_id_and_flags_interrupted() -> None:
    event = parse_event(user_field_only_interrupt())
    assert isinstance(event, UserEvent)
    assert event.interrupted is True
    assert event.interrupted_message_id == "msg_field_only_01"


def test_user_marker_only_interrupt_has_no_id() -> None:
    event = parse_event(user_marker_only_interrupt())
    assert isinstance(event, UserEvent)
    assert event.interrupted is True
    assert event.interrupted_message_id is None


def test_user_interrupted_message_id_none_on_bare_record() -> None:
    event = parse_event(user_str())
    assert isinstance(event, UserEvent)
    assert event.interrupted_message_id is None
    assert event.interrupted is False


def test_system_entry() -> None:
    event = parse_event(system_entry())
    assert isinstance(event, SystemEvent)
    assert event.subtype == "stop_hook_summary"
    assert event.content == "hook ran"


def test_system_stop_hook_summary_detail() -> None:
    event = parse_event(
        envelope(
            type="system",
            subtype="stop_hook_summary",
            content="hook ran",
            level="info",
            hookCount=2,
            hookInfos=[{"command": "fmt", "durationMs": 12}, {"command": "lint"}, {"durationMs": 99}],
            hookErrors=["boom"],
            hookAdditionalContext=["ctx"],
            preventedContinuation=True,
            stopReason="stop-here",
            hasOutput=True,
            toolUseID="toolu_hook",
        )
    )
    assert isinstance(event, SystemEvent)
    assert event.level == "info"
    assert event.detail == StopHookSummary(
        hook_count=2,
        hook_infos=(HookInfo(command="fmt", duration_ms=12), HookInfo(command="lint")),
        hook_errors=("boom",),
        hook_additional_context=("ctx",),
        prevented_continuation=True,
        stop_reason="stop-here",
        has_output=True,
        tool_use_id=ToolUseId("toolu_hook"),
    )


def test_system_compact_boundary_detail() -> None:
    event = parse_event(
        envelope(
            type="system",
            subtype="compact_boundary",
            level="warning",
            logicalParentUuid="uuid-logical",
            compactMetadata={
                "trigger": "manual",
                "preTokens": 1000,
                "postTokens": 200,
                "cumulativeDroppedTokens": 800,
                "durationMs": 4242,
                "preCompactDiscoveredTools": ["Read", "Bash"],
                "preservedSegment": {"headUuid": "uuid-head", "anchorUuid": "uuid-anchor", "tailUuid": "uuid-tail"},
                "preservedMessages": {
                    "anchorUuid": "uuid-anchor",
                    "uuids": ["uuid-head", "uuid-tail"],
                    "allUuids": ["uuid-head", "uuid-mid", "uuid-tail"],
                },
                "precomputed": True,
            },
        )
    )
    assert isinstance(event, SystemEvent)
    assert event.level == "warning"
    assert event.detail == CompactBoundary(
        trigger="manual",
        pre_tokens=1000,
        post_tokens=200,
        duration_ms=4242,
        cumulative_dropped_tokens=800,
        pre_compact_discovered_tools=("Read", "Bash"),
        preserved_segment=PreservedSegment(
            head_uuid=EventUuid("uuid-head"),
            anchor_uuid=EventUuid("uuid-anchor"),
            tail_uuid=EventUuid("uuid-tail"),
        ),
        preserved_messages=PreservedMessages(
            anchor_uuid=EventUuid("uuid-anchor"),
            uuids=(EventUuid("uuid-head"), EventUuid("uuid-tail")),
            all_uuids=(EventUuid("uuid-head"), EventUuid("uuid-mid"), EventUuid("uuid-tail")),
        ),
        logical_parent_uuid=EventUuid("uuid-logical"),
        precomputed=True,
    )


def test_system_turn_duration_detail() -> None:
    event = parse_event(
        envelope(
            type="system",
            subtype="turn_duration",
            durationMs=5000,
            messageCount=7,
            pendingWorkflowCount=2,
            pendingBackgroundAgentCount=1,
        )
    )
    assert isinstance(event, SystemEvent)
    assert event.level is None
    assert event.detail == TurnDuration(
        duration_ms=5000,
        message_count=7,
        pending_workflow_count=2,
        pending_background_agent_count=1,
    )


def test_system_model_refusal_fallback_detail() -> None:
    event = parse_event(
        envelope(
            type="system",
            subtype="model_refusal_fallback",
            level="error",
            apiRefusalCategory="policy",
            apiRefusalExplanation="nope",
            trigger="refusal",
            direction="downgrade",
            originalModel="claude-opus-4-8",
            fallbackModel="claude-sonnet-4",
            retractedMessageUuids=["uuid-r1", "uuid-r2"],
            refusedUserMessageUuid="uuid-refused",
        )
    )
    assert isinstance(event, SystemEvent)
    assert event.detail == ModelRefusalFallback(
        api_refusal_category="policy",
        api_refusal_explanation="nope",
        trigger="refusal",
        direction="downgrade",
        original_model="claude-opus-4-8",
        fallback_model="claude-sonnet-4",
        retracted_message_uuids=(EventUuid("uuid-r1"), EventUuid("uuid-r2")),
        refused_user_message_uuid=EventUuid("uuid-refused"),
    )


def test_system_sparse_detail_uses_defaults() -> None:
    event = parse_event(envelope(type="system", subtype="stop_hook_summary"))
    assert isinstance(event, SystemEvent)
    assert event.level is None
    assert event.detail == StopHookSummary()
    assert event.detail.hook_infos == ()


def test_system_unknown_subtype_is_other_detail_verbatim() -> None:
    record = envelope(type="system", subtype="weird_future", content="mystery")
    event = parse_event(record)
    assert isinstance(event, SystemEvent)
    assert event.subtype == "weird_future"
    assert event.detail == OtherSystemDetail(raw=record)


def test_system_local_command_is_other_detail() -> None:
    record = envelope(type="system", subtype="local_command", content="/clear", level="info")
    event = parse_event(record)
    assert isinstance(event, SystemEvent)
    assert event.level == "info"
    assert event.detail == OtherSystemDetail(raw=record)


def test_mode_entry() -> None:
    event = parse_event(mode_entry())
    assert event == ModeEvent(session_id=SessionId("sess-1"), channel="mode", value="normal")


def test_permission_mode_entry() -> None:
    event = parse_event(permission_mode_entry())
    assert event == ModeEvent(session_id=SessionId("sess-1"), channel="permission-mode", value="bypassPermissions")


@pytest.mark.parametrize(
    ("data", "expected_type"),
    [
        pytest.param(summary_entry(), "summary", id="summary"),
        pytest.param(queue_operation_entry(), "queue-operation", id="queue-operation"),
    ],
)
def test_other_events(data: dict[str, Any], expected_type: str) -> None:
    event = parse_event(data)
    assert isinstance(event, OtherEvent)
    assert event.type == expected_type
    assert event.raw == data


def test_attachment_untyped_is_other_attachment() -> None:
    data = attachment_entry()
    event = parse_event(data)
    assert isinstance(event, AttachmentEvent)
    assert event.attachment_type == ""
    assert event.detail == OtherAttachment(raw=data)
    assert event.meta.session_id == SessionId("sess-1")


def test_attachment_hook_success_is_typed() -> None:
    event = parse_event(
        envelope(
            type="attachment",
            attachment={
                "type": "hook_success",
                "hookName": "PostToolUse:Bash",
                "hookEvent": "PostToolUse",
                "toolUseID": "toolu_1",
                "command": "ruff",
                "content": "ctx",
                "stdout": "OUT",
                "stderr": "",
                "exitCode": 0,
                "durationMs": 12,
            },
        )
    )
    assert isinstance(event, AttachmentEvent)
    assert event.attachment_type == "hook_success"
    assert event.detail == HookSuccess(
        hook_name="PostToolUse:Bash",
        hook_event="PostToolUse",
        tool_use_id=ToolUseId("toolu_1"),
        command="ruff",
        content="ctx",
        stdout="OUT",
        stderr="",
        exit_code=0,
        duration_ms=12,
    )


def test_attachment_queued_command_is_typed() -> None:
    event = parse_event(
        envelope(type="attachment", attachment={"type": "queued_command", "prompt": "go", "commandMode": "prompt"})
    )
    assert isinstance(event, AttachmentEvent)
    assert event.detail == QueuedCommand(prompt="go", command_mode="prompt")


@pytest.mark.parametrize(
    "payload",
    [
        pytest.param("malformed-string-payload", id="string"),
        pytest.param([1, "list-payload"], id="list"),
        pytest.param(None, id="null"),
        pytest.param(7, id="int"),
    ],
)
def test_attachment_non_mapping_payload_degrades(payload: object) -> None:
    data = envelope(type="attachment", attachment=payload)
    event = parse_event(data)
    assert isinstance(event, AttachmentEvent)
    assert event.attachment_type == ""
    assert event.detail == OtherAttachment(raw=data)


def test_attachment_non_string_type_reads_empty() -> None:
    data = envelope(type="attachment", attachment={"type": 7, "weird": True})
    event = parse_event(data)
    assert isinstance(event, AttachmentEvent)
    assert event.attachment_type == ""
    assert event.detail == OtherAttachment(raw=data)


def test_attachment_scalars_are_type_gated() -> None:
    event = parse_event(
        envelope(
            type="attachment",
            attachment={
                "type": "hook_success",
                "hookName": 12,
                "hookEvent": None,
                "toolUseID": "",
                "command": ["not", "a", "string"],
                "content": 3,
                "stdout": True,
                "stderr": "KEPT-STDERR",
                "exitCode": 1.5,
                "durationMs": "42",
            },
        )
    )
    assert isinstance(event, AttachmentEvent)
    assert event.detail == HookSuccess(stderr="KEPT-STDERR")


def test_attachment_cancelled_bool_and_int_gates() -> None:
    event = parse_event(
        envelope(
            type="attachment",
            attachment={"type": "hook_cancelled", "hookName": "Stop", "timedOut": "yes", "timeoutMs": 30.5},
        )
    )
    assert isinstance(event, AttachmentEvent)
    assert event.detail == HookCancelled(hook_name="Stop")


def test_attachment_context_content_requires_list() -> None:
    event = parse_event(
        envelope(
            type="attachment",
            attachment={"type": "hook_additional_context", "hookName": "SessionStart", "content": "not-a-list"},
        )
    )
    assert isinstance(event, AttachmentEvent)
    assert event.detail == HookAdditionalContext(hook_name="SessionStart")


def test_parse_events_from_bytes_skips_blank_and_undecodable() -> None:
    raw = b"\n".join(
        [
            orjson.dumps(user_str()),
            b"",
            b"   ",
            b"{not valid json",
            orjson.dumps(assistant_text()),
        ]
    )
    events = parse_events_from_bytes(raw)
    assert [type(e).__name__ for e in events] == ["UserEvent", "AssistantEvent"]


def test_parse_events_from_bytes_skips_non_dict_json() -> None:
    raw = b"\n".join([b"null", b"42", b'"text"', b"[1, 2]", orjson.dumps(user_str())])
    events = parse_events_from_bytes(raw)
    assert [type(e).__name__ for e in events] == ["UserEvent"]


def test_unexpected_user_content_shape_raises_value_error() -> None:
    with pytest.raises(ValueError, match="unexpected user content shape: NoneType"):
        parse_event(envelope(type="user", message={"role": "user", "content": None}))


def test_parse_events_async_reads_file(tmp_path: Path) -> None:
    path = tmp_path / "t.jsonl"
    path.write_bytes(b"\n".join([orjson.dumps(user_str()), orjson.dumps(mode_entry())]))
    events = anyio.run(parse_events_async, path)
    assert [type(e).__name__ for e in events] == ["UserEvent", "ModeEvent"]


def test_parse_print_result_haiku_envelope() -> None:
    env = parse_print_result((TESTDATA / "haiku_envelope.json").read_bytes())

    assert env.total_cost_usd == 0.05759
    assert env.model_usage["claude-haiku-4-5-20251001"] == ModelUsage(
        input_tokens=28,
        output_tokens=250,
        cache_read_input_tokens=51020,
        cache_creation_input_tokens=25605,
        web_search_requests=0,
        cost_usd=0.05759,
        context_window=200000,
        max_output_tokens=32000,
    )

    assert env.usage.input_tokens == 28
    assert env.usage.output_tokens == 250
    assert env.usage.cache_read_input_tokens == 51020
    assert env.usage.cache_creation_input_tokens == 25605
    assert env.usage.cache_creation == CacheCreation(ephemeral_5m_input_tokens=0, ephemeral_1h_input_tokens=25605)
    assert env.usage.service_tier == "standard"
    assert env.usage.inference_geo == "not_available"
    assert env.usage.server_tool_use == ServerToolUse(web_search_requests=0, web_fetch_requests=0)

    assert env.structured_output == {"answer": "pong"}
    assert env.num_turns == 3
    assert env.is_error is False
    assert env.result == "pong"
    assert env.stop_reason == "end_turn"
    assert env.session_id == SessionId("01fb78f3-fc93-4eb1-b399-07e6a60c3e2c")
    assert env.fast_mode_state == "off"
    assert env.permission_denials == ()

    assert env.init is not None
    assert McpServer(name="plugin:cc-review:cc-review", status="connected") in env.init.mcp_servers
    assert "Bash" in env.init.tools and "Read" in env.init.tools
    assert "deep-research" in env.init.skills
    assert any(p.name == "cc-review" for p in env.init.plugins)

    assert any(
        isinstance(block, ToolUseBlock) and block.name == "StructuredOutput" and block.input == {"answer": "pong"}
        for message in env.messages
        for block in message.blocks
    )
    assert any(
        isinstance(block, ToolResultBlock) and block.content == "Structured output provided successfully"
        for message in env.messages
        for block in message.blocks
    )
    assert any(message.role == "assistant" and TextBlock("pong") in message.blocks for message in env.messages)
    assert any(message.model == "claude-haiku-4-5-20251001" for message in env.messages)
