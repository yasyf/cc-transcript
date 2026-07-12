from __future__ import annotations

import os
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript import _parser_rs
from cc_transcript.corrections import Correction
from cc_transcript.decisions import Decision
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.ids import EventUuid, SessionId, ToolDigest
from cc_transcript.mining import ANSWERED_PREFIX, ANSWERED_TRAILER, DENIAL_PREFIX
from cc_transcript.models import AssistantEvent, CcVersion, ContentBlock, EntryMeta, UserEvent

SESSION = SessionId("11111111-1111-1111-1111-111111111111")
OTHER_SESSION = SessionId("22222222-2222-2222-2222-222222222222")
BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)

REAL_CORPUS_SAMPLE = 25
RUST_BACKEND = _parser_rs
requires_rust = pytest.mark.skipif(False, reason="_parser_rs is a hard requirement")
rust_not_disabled = pytest.mark.skipif(
    bool(os.environ.get("CC_TRANSCRIPT_DISABLE_RUST")), reason="Rust force-disabled via CC_TRANSCRIPT_DISABLE_RUST"
)


def meta(
    uuid: str,
    *,
    session: SessionId = SESSION,
    base: datetime = BASE,
    secs: int = 0,
    is_meta: bool = False,
    is_sidechain: bool = False,
    is_compact_summary: bool = False,
) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=session,
        timestamp=base + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=is_sidechain,
        is_meta=is_meta,
        entrypoint="cli",
        is_compact_summary=is_compact_summary,
        is_visible_in_transcript_only=False,
    )


def user(
    uuid: str,
    text: str = "",
    *,
    session: SessionId = SESSION,
    base: datetime = BASE,
    blocks: tuple[ContentBlock, ...] = (),
    secs: int = 0,
    interrupted: bool = False,
    is_meta: bool = False,
    is_sidechain: bool = False,
    is_compact_summary: bool = False,
    is_agent_injected: bool = False,
) -> UserEvent:
    return UserEvent(
        meta=meta(
            uuid,
            session=session,
            base=base,
            secs=secs,
            is_meta=is_meta,
            is_sidechain=is_sidechain,
            is_compact_summary=is_compact_summary,
        ),
        text=text,
        blocks=blocks,
        interrupted=interrupted,
        is_agent_injected=is_agent_injected,
    )


def assistant(
    uuid: str,
    text: str = "",
    *,
    session: SessionId = SESSION,
    base: datetime = BASE,
    model: str = "claude-opus-4-7",
    blocks: tuple[ContentBlock, ...] = (),
    secs: int = 0,
) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, session=session, base=base, secs=secs),
        model=model,
        text=text,
        blocks=blocks,
        stop_reason=None,
        usage=None,
    )


def raw_envelope(**overrides: Any) -> dict[str, Any]:
    return {
        "uuid": "uuid-1",
        "parentUuid": "uuid-0",
        "sessionId": "sess-1",
        "timestamp": "2026-01-02T03:04:05.000Z",
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "1.2.3",
        "isSidechain": False,
        "entrypoint": "cli",
    } | overrides


def fixture_entries() -> list[dict[str, Any]]:
    return [
        raw_envelope(type="user", message={"role": "user", "content": "  fix the bug  "}),
        raw_envelope(
            type="user",
            parentUuid=None,
            version="",
            message={
                "role": "user",
                "content": [
                    {"type": "text", "text": "here is context"},
                    {"type": "text", "text": "and more"},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok output", "is_error": False},
                    {"type": "document", "source": {"kind": "file"}},
                ],
            },
        ),
        raw_envelope(
            type="user",
            isSidechain=True,
            isMeta=True,
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_2",
                        "content": [
                            {"type": "text", "text": "line a"},
                            {"type": "tool_reference", "id": "x"},
                            {"type": "image", "source": {"data": "..."}},
                            {"type": "text", "text": "line b"},
                        ],
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(type="user", message={"role": "user", "content": "[Request interrupted by user]"}),
        raw_envelope(type="user", message={"role": "user", "content": "  [request INTERRUPTED by user for tool use]"}),
        raw_envelope(type="user", message={"role": "user", "content": "she quoted [Request interrupted by user] mid"}),
        raw_envelope(
            type="user",
            toolUseResult={"isAsync": True, "status": "async_launched"},
            message={
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_async", "content": "launched", "is_error": False}
                ],
            },
        ),
        # Object toolUseResult payloads (Bash, Edit, Task, AskUserQuestion), one plain-string
        # denial payload, and an absent payload (toolu_1, above) — every payload shape both
        # backends must clone onto ToolResultBlock.tool_use_result byte-for-byte.
        raw_envelope(
            type="user",
            toolUseResult={
                "stdout": "hello",
                "stderr": "",
                "interrupted": False,
                "isImage": False,
                "noOutputExpected": False,
                "returnCodeInterpretation": "Command exited with code 0",
            },
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_bash", "content": "hello", "is_error": False}],
            },
        ),
        raw_envelope(
            type="user",
            toolUseResult={
                "filePath": "/x.py",
                "oldString": "alpha",
                "newString": "beta",
                "replaceAll": False,
                "userModified": False,
                "staleRecovered": False,
                "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-alpha", "+beta"]}],
                "originalFile": "alpha\n",
            },
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_edit", "content": "ok", "is_error": False}],
            },
        ),
        raw_envelope(
            type="user",
            toolUseResult={
                "agentId": "agent-77",
                "agentType": "Explore",
                "status": "completed",
                "totalDurationMs": 1234,
                "totalTokens": 567,
                "totalToolUseCount": 3,
                "toolStats": {"Read": 2, "Grep": 1},
                "usage": {"input_tokens": 10, "output_tokens": 5},
                "content": [{"type": "text", "text": "found it"}],
                "prompt": "investigate the parser",
                "resolvedModel": "claude-opus-4-8",
            },
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_task", "content": "found it", "is_error": False}],
            },
        ),
        raw_envelope(
            type="user",
            toolUseResult={
                "questions": [
                    {
                        "question": "Which approach?",
                        "header": "Approach",
                        "options": [{"label": "A", "description": "first"}, {"label": "B", "description": "second"}],
                        "multiSelect": False,
                    }
                ],
                "answers": {"Which approach?": "A"},
                "annotations": {"Which approach?": {"preview": "picked A", "notes": "with a caveat"}},
            },
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_ask", "content": "A", "is_error": False}],
            },
        ),
        raw_envelope(
            type="user",
            toolUseResult="The user doesn't want to proceed with this tool use.",
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_deny", "content": "denied", "is_error": True}],
            },
        ),
        # Structured toolDenialKind, both kinds, plus a legacy banner-only and a plain error —
        # every denial_kind precedence path both backends must resolve identically at parse time.
        raw_envelope(
            type="user",
            toolDenialKind="user-rejected",
            toolUseResult=f"{DENIAL_PREFIX}. structured and bannered",
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_deny_struct_banner",
                        "content": f"{DENIAL_PREFIX}. structured and bannered",
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(
            type="user",
            toolDenialKind="user-rejected",
            toolUseResult="just take a different approach entirely",
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_deny_struct_nobanner",
                        "content": "just take a different approach entirely",
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(
            type="user",
            toolDenialKind="permission-rule",
            toolUseResult="Error: BLOCKED: policy forbids writing to /etc",
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_deny_permission",
                        "content": "Error: BLOCKED: policy forbids writing to /etc",
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(
            type="user",
            toolUseResult=f"{DENIAL_PREFIX}. legacy banner without the structured field",
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_deny_legacy",
                        "content": f"{DENIAL_PREFIX}. legacy banner without the structured field",
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(
            type="user",
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_plain_error",
                        "content": "ENOENT: no such file",
                        "is_error": True,
                    }
                ],
            },
        ),
        raw_envelope(
            type="user",
            isCompactSummary=True,
            isVisibleInTranscriptOnly=True,
            message={"role": "user", "content": "compact unicode héllo 🤖 漢字"},
        ),
        raw_envelope(
            type="user",
            message={"role": "user", "content": "<teammate-message from='reviewer'>please rebase</teammate-message>"},
        ),
        raw_envelope(
            type="user",
            message={"role": "user", "content": "<scheduled-task id='7'>run the nightly suite</scheduled-task>"},
        ),
        # Head-anchored role-reminder marker appearing mid-text: the group is start-anchored, so both
        # backends must agree is_agent_injected is False — guards Rust against over-matching.
        raw_envelope(
            type="user",
            message={"role": "user", "content": "we discussed the [Role Reminder] banner mid-sentence"},
        ),
        # A relay tag mentioned mid-text is authored, not injected — start-anchored, False on both.
        raw_envelope(
            type="user",
            message={"role": "user", "content": "why did the transcript contain <teammate-message from=a> above"},
        ),
        # A combining mark (U+0301) after the tag name is not a portable word boundary: Python re once
        # matched here while the Rust regex crate did not — the follower class restores False on both.
        raw_envelope(
            type="user",
            message={"role": "user", "content": "<teammate-message\u0301>"},
        ),
        raw_envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-7",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "let me read"},
                    {"type": "thinking", "thinking": "hmm 🤔"},
                    {
                        "type": "tool_use",
                        "id": "toolu_9",
                        "name": "Read",
                        "input": {"file_path": "/x", "limit": 100, "ratio": 1.5, "flag": True, "none": None},
                    },
                ],
            },
        ),
        raw_envelope(
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
        ),
        raw_envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "<synthetic>",
                "stop_reason": None,
                "content": [{"type": "text", "text": "noop"}],
            },
        ),
        raw_envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"}}
                ],
            },
        ),
        raw_envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": None,
                "content": [{"type": "future_block", "payload": {"n": 1, "deep": [True, None, 2.5]}}],
            },
        ),
        # A user record carrying every new envelope/meta field, each with a distinct value so a
        # positional misassignment in the Rust event.rs dataclass build cannot survive parity.
        raw_envelope(
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
        ),
        # An assistant record carrying requestId/forkedFrom and all four attribution keys, each
        # distinct — attribution is materialized because at least one attribution field is present.
        raw_envelope(
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
        ),
        # An API-error assistant record: the isApiErrorMessage flag is set, so the full
        # error/status/details quartet materializes into ApiError with distinct values.
        raw_envelope(
            type="assistant",
            isApiErrorMessage=True,
            error="rate_limit",
            apiErrorStatus=429,
            errorDetails="detail-cc",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": None,
                "content": [{"type": "text", "text": "API Error: 429 rate_limit"}],
            },
        ),
        # error/apiErrorStatus present but no isApiErrorMessage flag: api_error must stay None
        # on both backends — the flag gates materialization, not the quartet's presence.
        raw_envelope(
            type="assistant",
            error="rate_limit",
            apiErrorStatus=429,
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "error field but no flag"}],
            },
        ),
        # A field-only interrupt: interruptedMessageId is present with no text marker, so the
        # OR-merge must flag interrupted True and surface the msg_ id on both backends.
        raw_envelope(
            type="user",
            interruptedMessageId="msg_parity_dd",
            message={"role": "user", "content": "field-only interrupt with no marker text"},
        ),
        # A marker-only interrupt (old format): the text marker flags interrupted, and
        # interrupted_message_id stays None because the raw field is absent.
        raw_envelope(
            type="user",
            message={"role": "user", "content": "[request interrupted by user for tool use]"},
        ),
        # System entries with distinctive per-field values guard positional
        # misassignment of level/detail across both backends.
        raw_envelope(
            type="system",
            subtype="stop_hook_summary",
            content="hook ran",
            level="info",
            hookCount=3,
            hookInfos=[{"command": "fmt", "durationMs": 12}, {"command": "lint"}, {"durationMs": 99}],
            hookErrors=["boom", "kaboom"],
            hookAdditionalContext=["ctx-a", "ctx-b"],
            preventedContinuation=True,
            stopReason="stop-here",
            hasOutput=True,
            toolUseID="toolu_hook",
        ),
        raw_envelope(
            type="system",
            subtype="compact_boundary",
            parentUuid=None,
            content="compacted",
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
        ),
        raw_envelope(
            type="system",
            subtype="turn_duration",
            durationMs=5000,
            messageCount=7,
            pendingWorkflowCount=2,
            pendingBackgroundAgentCount=1,
        ),
        raw_envelope(
            type="system",
            subtype="model_refusal_fallback",
            content="refused",
            level="error",
            apiRefusalCategory="policy",
            apiRefusalExplanation="nope",
            trigger="refusal",
            direction="downgrade",
            originalModel="claude-opus-4-8",
            fallbackModel="claude-sonnet-4",
            retractedMessageUuids=["uuid-r1", "uuid-r2"],
            refusedUserMessageUuid="uuid-refused",
        ),
        # A sparse typed subtype: every optional field absent -> detail defaults.
        raw_envelope(type="system", subtype="turn_duration"),
        # A known-but-untyped subtype and an unknown future subtype both fall to
        # OtherSystemDetail carrying the full record verbatim.
        raw_envelope(type="system", subtype="local_command", content="/clear", level="info"),
        raw_envelope(type="system", subtype="weird_future", content="mystery"),
        {"type": "mode", "mode": "normal", "sessionId": "sess-1"},
        {"type": "permission-mode", "permissionMode": "bypassPermissions", "sessionId": "sess-1"},
        {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x", "nested": {"a": [1, 2, 3], "b": 1.5}},
        # One record per typed AttachmentDetail (distinctive values), a queued
        # command, an unknown attachment type -> OtherAttachment, and a typeless
        # attachment -> OtherAttachment with an empty attachment_type.
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_success",
                "hookName": "PostToolUse:Bash",
                "hookEvent": "PostToolUse",
                "toolUseID": "toolu_hook",
                "command": "uvx capt-hook run",
                "content": "hook-injected context",
                "stdout": "HOOK-STDOUT",
                "stderr": "",
                "exitCode": 0,
                "durationMs": 42,
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_blocking_error",
                "hookName": "Stop",
                "hookEvent": "Stop",
                "toolUseID": "toolu_stop",
                "blockingError": {"blockingError": "review your changes"},
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_non_blocking_error",
                "hookName": "PostToolUse:Edit",
                "hookEvent": "PostToolUse",
                "toolUseID": "toolu_edit",
                "command": "ruff check",
                "stdout": "",
                "stderr": "E501 line too long",
                "exitCode": 1,
                "durationMs": 88,
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_cancelled",
                "hookName": "UserPromptSubmit",
                "hookEvent": "UserPromptSubmit",
                "toolUseID": "toolu_ups",
                "command": "ccstatusline --hook",
                "durationMs": 289,
                "timedOut": True,
                "timeoutMs": 30000,
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_additional_context",
                "hookName": "SessionStart",
                "hookEvent": "SessionStart",
                "toolUseID": "SessionStart",
                "content": ["first context line", "second context line"],
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "async_hook_response",
                "hookName": "SessionStart:startup",
                "hookEvent": "SessionStart",
                "processId": "async_hook_9001",
                "stdout": "",
                "stderr": "ASYNC-STDERR",
                "exitCode": 1,
                "response": {"decision": "approve"},
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={"type": "queued_command", "prompt": "run the tests", "commandMode": "prompt"},
        ),
        raw_envelope(type="attachment", attachment={"type": "skill_listing", "names": ["a", "b"], "skillCount": 2}),
        raw_envelope(type="attachment", attachment={"kind": "file", "size": 9999999999}),
        # A sidechain attachment: MetaFlag clauses and sidechain stats must see it.
        raw_envelope(
            type="attachment",
            isSidechain=True,
            attachment={"type": "queued_command", "prompt": "sidechain queued command", "commandMode": "prompt"},
        ),
        # Malformed attachment shapes degrade identically on both backends: a
        # non-mapping payload or non-string type -> OtherAttachment with an empty
        # attachment_type; mistyped scalars and a non-list content read as absent
        # (parse.rs opt_str / opt_i64 / str_array parity).
        raw_envelope(type="attachment", attachment="malformed-string-payload"),
        raw_envelope(type="attachment", attachment=[1, "malformed-list-payload"]),
        raw_envelope(type="attachment", attachment={"type": 7, "weird": True}),
        raw_envelope(
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
        ),
        raw_envelope(
            type="attachment",
            attachment={"type": "hook_cancelled", "hookName": "Stop", "timedOut": "yes", "timeoutMs": 30.5},
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "hook_additional_context",
                "hookName": "SessionStart",
                "content": "not-a-list-so-reads-empty",
            },
        ),
        raw_envelope(
            type="attachment",
            attachment={
                "type": "queued_command",
                "prompt": [{"type": "text", "text": "list prompts read as absent"}],
                "commandMode": 9,
            },
        ),
        {"type": "queue-operation", "operation": "enqueue", "items": []},
        {"type": "file-history-snapshot", "snapshot": {"files": ["a", "b"]}},
        {"type": "ai-title", "title": "Some Title"},
        {"type": "last-prompt", "prompt": "do the thing"},
        {"type": "started", "ts": 123},
        {"type": "result", "ok": True, "count": 0},
    ]


def fixture_bytes() -> bytes:
    return b"\n".join(
        [
            orjson.dumps(fixture_entries()[0]),
            b"",
            b"   ",
            b"{not valid json",
            *(orjson.dumps(e) for e in fixture_entries()),
        ]
    )


def real_corpus() -> list[Path]:
    if not CLAUDE_PROJECTS_DIR.exists():
        return []
    paths = sorted(CLAUDE_PROJECTS_DIR.rglob("*.jsonl"), key=lambda p: p.stat().st_size)
    if not paths:
        return []
    agents = [p for p in paths if p.name.startswith("agent-")][:5]
    others = [p for p in paths if not p.name.startswith("agent-")]
    spread = [others[i] for i in range(0, len(others), max(1, len(others) // REAL_CORPUS_SAMPLE))]
    return list(dict.fromkeys(agents + spread))[:REAL_CORPUS_SAMPLE]


# An answered AskUserQuestion round captured byte-verbatim from the mirror corpus
# (captain-hook session 1786702b, toolu_01CmqCAkZFs5TP2WUJAukLa3): the second
# question carries nested double quotes and its ordinal-shorthand answer embeds
# commas, so the pair anchors and exact-suffix strips are exercised on real bytes.
TOMBSTONE_QUESTION = "What should the hook do when a tombstone comment is confirmed?"
TOMBSTONE_LABELS = ("Advisory warn (Recommended)", "Block the edit", "Turn-end Stop gate")
MATCHER_QUESTION = (
    'Which cheap-trigger matcher should run on the extracted comments? (You said "ast/nlp triggered" — AST comment '
    "extraction is in either way; this is about the text-matching layer. Evidence: spaCy's en_core_web_sm has no "
    "runtime provisioning path — RESOURCES.spacy raises if it's missing, and nothing in pack install provisions it — "
    "so NLP would error/no-op in the ~17 general-pack repos until each machine installs the model.)"
)
MATCHER_LABELS = ("Regex phrases (Recommended)", "Regex + NLP lemma layer", "NLP-only (as originally floated)")
MATCHER_ANSWER = (
    "3, and figure out why NLP is not being installed when you install capt-hook, it should be downloading the "
    "model live whenyou start it up, look at the git history"
)
ROUND1_CONTENT = (
    f'{ANSWERED_PREFIX}"{TOMBSTONE_QUESTION}"="Advisory warn (Recommended)", '
    f'"{MATCHER_QUESTION}"="{MATCHER_ANSWER}"{ANSWERED_TRAILER}'
)


ANCHOR = EventUuid("anchor-1")
DIGEST_A = ToolDigest("a" * 64)
DIGEST_B = ToolDigest("b" * 64)
DIGEST_C = ToolDigest("c" * 64)
DIGESTS = (DIGEST_A, DIGEST_B, DIGEST_C)

BASE_CORRECTION = Correction(
    ts_ms=1_000,
    session_id=SESSION,
    source="cc-pushback",
    anchor_uuid=ANCHOR,
    incorrect_digest=DIGEST_A,
    incorrect_file="/a.py",
    incorrect_old="alpha = 1",
    incorrect_new="alpha = 2",
    correction_origin="session",
    correction_file="/a.py",
    correction_old="alpha = 2",
    correction_new="alpha = 3",
    correction_commit=None,
    overlap=0.5,
    detail={"rule": "overlap", "turn": 3},
)

BASE_DECISION = Decision(
    ts_ms=0,
    session_id=SESSION,
    source="captain-hook",
    kind="no-defensive-code",
    source_file="primitives/nudge.py",
    event="PreToolUse",
    action="nudge",
    tool_name="Edit",
    tool_digest=DIGEST_A,
    event_uuid=EventUuid("e-1"),
    message="prefer failing fast",
    detail={"rule": "defensive", "turn": 3},
)


def correction(**overrides: Any) -> Correction:
    return replace(BASE_CORRECTION, **overrides)


def decision(ts_ms: int, **overrides: Any) -> Decision:
    return replace(BASE_DECISION, ts_ms=ts_ms, **overrides)


def correction_distinct(ts_ms: int, *, session_id: SessionId = SESSION, seq: int = 0) -> Correction:
    return correction(ts_ms=ts_ms, session_id=session_id, incorrect_digest=DIGESTS[seq])


def decision_distinct(ts_ms: int, *, session_id: SessionId = SESSION, seq: int = 0) -> Decision:
    return decision(ts_ms, session_id=session_id)
