from __future__ import annotations

import os
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.corrections import Correction
from cc_transcript.decisions import Decision
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.ids import EventUuid, SessionId, ToolDigest
from cc_transcript.mining import ANSWERED_PREFIX, ANSWERED_TRAILER
from cc_transcript.models import AssistantEvent, CcVersion, ContentBlock, EntryMeta, UserEvent
from cc_transcript.parser import load_rust_backend

SESSION = SessionId("11111111-1111-1111-1111-111111111111")
OTHER_SESSION = SessionId("22222222-2222-2222-2222-222222222222")
BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)

REAL_CORPUS_SAMPLE = 25
RUST_BACKEND = load_rust_backend()
requires_rust = pytest.mark.skipif(RUST_BACKEND is None, reason="_parser_rs extension is not built")
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
        raw_envelope(type="system", subtype="stop_hook_summary", content="hook ran"),
        raw_envelope(type="system", subtype="turn_duration"),
        {"type": "mode", "mode": "normal", "sessionId": "sess-1"},
        {"type": "permission-mode", "permissionMode": "bypassPermissions", "sessionId": "sess-1"},
        {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x", "nested": {"a": [1, 2, 3], "b": 1.5}},
        raw_envelope(type="attachment", attachment={"kind": "file", "size": 9999999999}),
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
