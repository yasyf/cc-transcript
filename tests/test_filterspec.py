from __future__ import annotations

from datetime import UTC, datetime

import pytest

from cc_transcript.filterspec import (
    AGENT_INJECTION_GROUPS,
    COMMAND_ECHO_GROUPS,
    CONTINUATION_GROUPS,
    Action,
    Clause,
    EntrypointIn,
    FilterSpec,
    KindIs,
    MetaFlag,
    ModelIs,
    TextEmpty,
    TextInSet,
    TextMatchesAny,
    WordCountAtMost,
    annotate_spec,
    apply_spec,
    keep,
    labels_for,
)
from cc_transcript.models import (
    AssistantEvent,
    EntryMeta,
    EventUuid,
    ModeEvent,
    SessionId,
    ToolUseBlock,
    ToolUseId,
    TranscriptEvent,
    UserEvent,
)


def meta(**overrides: object) -> EntryMeta:
    base: dict[str, object] = {
        "uuid": EventUuid("u"),
        "parent_uuid": None,
        "session_id": SessionId("s"),
        "timestamp": datetime(2026, 1, 1, tzinfo=UTC),
        "cwd": "/repo",
        "git_branch": "main",
        "cc_version": None,
        "is_sidechain": False,
        "is_meta": False,
        "entrypoint": "cli",
        "is_compact_summary": False,
        "is_visible_in_transcript_only": False,
    }
    return EntryMeta(**(base | overrides))  # type: ignore[arg-type]


def user(text: str, **kw: object) -> UserEvent:
    return UserEvent(meta=meta(**kw), text=text, blocks=(), interrupted=False)


def assistant(model: str = "claude-opus-4-7", text: str = "hi", *, tool: bool = False) -> AssistantEvent:
    blocks = (ToolUseBlock(id=ToolUseId("t"), name="Bash", input={}),) if tool else ()
    return AssistantEvent(meta=meta(), model=model, text=text, blocks=blocks, stop_reason=None, usage=None)


def spec(*clauses: Clause) -> FilterSpec:
    return FilterSpec(clauses=clauses)


def test_kind_is_keeps_matching() -> None:
    s = spec(Clause(KindIs(frozenset({"user"})), negate=True))
    assert keep(user("hi"), s)
    assert not keep(assistant(), s)
    assert not keep(ModeEvent(session_id=SessionId("s"), channel="mode", value="normal"), s)


@pytest.mark.parametrize(
    ("flag", "kw", "dropped"),
    [
        ("is_sidechain", {"is_sidechain": True}, True),
        ("is_sidechain", {}, False),
        ("is_meta", {"is_meta": True}, True),
        ("is_compact_summary", {"is_compact_summary": True}, True),
        ("is_visible_in_transcript_only", {"is_visible_in_transcript_only": True}, True),
    ],
)
def test_meta_flag(flag: str, kw: dict[str, object], dropped: bool) -> None:
    s = spec(Clause(MetaFlag(flag)))  # type: ignore[arg-type]
    assert keep(user("hi", **kw), s) is not dropped


def test_entrypoint_in() -> None:
    s = spec(Clause(EntrypointIn(frozenset({"sdk-cli"}))))
    assert not keep(user("hi", entrypoint="sdk-cli"), s)
    assert keep(user("hi", entrypoint="cli"), s)


def test_model_is_only_matches_assistant() -> None:
    s = spec(Clause(ModelIs(frozenset({"<synthetic>"}))))
    assert not keep(assistant("<synthetic>"), s)
    assert keep(assistant("claude-opus-4-7"), s)
    assert keep(user("<synthetic>"), s)


def test_text_empty_user_vs_assistant_tool_use() -> None:
    s = spec(
        Clause(TextEmpty(consider_tool_use=True), applies_to=frozenset({"assistant"})),
        Clause(TextEmpty(consider_tool_use=False), applies_to=frozenset({"user"})),
    )
    assert not keep(user("   "), s)
    assert not keep(assistant(text=""), s)
    assert keep(assistant(text="", tool=True), s)  # tool use rescues an empty assistant
    assert keep(user("real"), s)


def test_text_matches_any_named_groups_ignore_case() -> None:
    s = spec(Clause(TextMatchesAny((("greet", r"hello"),), ignore_case=True), applies_to=frozenset({"user"})))
    assert not keep(user("HELLO there"), s)
    assert keep(user("goodbye"), s)


@pytest.mark.parametrize(
    ("text", "dropped"),
    [
        ("go ahead", True),
        ("Go Ahead.", True),  # case + trailing punctuation normalized
        ("  continue  ", True),
        ("go ahead and commit the change", False),  # not a bare phrase
    ],
)
def test_text_in_set_normalization(text: str, dropped: bool) -> None:
    s = spec(Clause(TextInSet(frozenset({"go ahead", "continue"})), applies_to=frozenset({"user"})))
    assert keep(user(text), s) is not dropped


@pytest.mark.parametrize(("text", "dropped"), [("ok", True), ("two words", True), ("three words here", False)])
def test_word_count_at_most(text: str, dropped: bool) -> None:
    s = spec(Clause(WordCountAtMost(2), applies_to=frozenset({"user"})))
    assert keep(user(text), s) is not dropped


@pytest.mark.parametrize(
    ("text", "dropped"),
    [
        ("go ahead and commit and push all the repos", True),
        ("yea go ahead and make that change and push it", True),
        ("push the branch", True),
        ("yea commit and push", True),
        ("ok push the branch", True),
        ("looks good. commit, rebase, push", True),
        ("proceed", True),
        ("we hit rate limits, you must resume them", True),
        ("restart the subagents please", True),
        # genuine corrections must survive — 'push'/'commit' appear mid-sentence only
        ("we need to force-push, you should have rebased first", False),
        ("no you had it right the first time, call super", False),
        ("switch to structlog everywhere, do a commit that cleans up logging", False),
        ("I disagree, we should do the pandoc-native thing he suggested", False),
    ],
)
def test_continuation_group_drops_advance_directives_not_corrections(text: str, dropped: bool) -> None:
    s = spec(Clause(TextMatchesAny(CONTINUATION_GROUPS), applies_to=frozenset({"user"})))
    assert keep(user(text), s) is not dropped


def test_command_echo_drops_bash_mode_turns() -> None:
    s = spec(Clause(TextMatchesAny(COMMAND_ECHO_GROUPS), applies_to=frozenset({"user"})))
    assert not keep(user("<bash-input>uv run pytest</bash-input>"), s)
    assert not keep(user("<bash-stdout>123 passed</bash-stdout>"), s)
    assert keep(user("run the bash input parser through pytest"), s)


def test_role_reminder_is_agent_injection_noise() -> None:
    s = spec(Clause(TextMatchesAny(AGENT_INJECTION_GROUPS), applies_to=frozenset({"user"})))
    assert not keep(user("[Role Reminder: You are a Coordinator. You NEVER edit files."), s)
    assert keep(user("remind me what the coordinator role does"), s)


def test_applies_to_gates_evaluation() -> None:
    s = spec(Clause(WordCountAtMost(2), applies_to=frozenset({"user"})))
    assert not keep(user("hi"), s)
    assert keep(assistant(text="hi"), s)  # clause does not apply to assistants


def test_first_drop_wins_and_order_irrelevant_to_boolean() -> None:
    s = spec(
        Clause(MetaFlag("is_sidechain")),
        Clause(TextEmpty(consider_tool_use=False), applies_to=frozenset({"user"})),
    )
    assert not keep(user("   ", is_sidechain=True), s)


def test_tag_keeps_event_and_records_labels() -> None:
    s = spec(
        Clause(TextInSet(frozenset({"go ahead"})), action=Action.TAG, label="resume", applies_to=frozenset({"user"})),
        Clause(WordCountAtMost(5), action=Action.TAG, label="short", applies_to=frozenset({"user"})),
    )
    event = user("go ahead")
    assert keep(event, s)
    assert labels_for(event, s) == ("resume", "short")
    assert list(annotate_spec([event], s)) == [(event, ("resume", "short"))]


def test_tag_does_not_drop_but_drop_still_drops() -> None:
    s = spec(
        Clause(TextInSet(frozenset({"ok"})), action=Action.TAG, label="ack", applies_to=frozenset({"user"})),
        Clause(MetaFlag("is_sidechain")),
    )
    assert keep(user("ok"), s)
    assert not keep(user("ok", is_sidechain=True), s)


def test_apply_spec_filters_stream() -> None:
    keeper = user("please refactor")
    events: list[TranscriptEvent] = [keeper, user("   "), assistant("<synthetic>")]
    s = spec(
        Clause(KindIs(frozenset({"user"})), negate=True),
        Clause(TextEmpty(consider_tool_use=False), applies_to=frozenset({"user"})),
    )
    assert list(apply_spec(events, s)) == [keeper]
