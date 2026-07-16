from __future__ import annotations

from collections.abc import Iterator

import pytest

from cc_transcript import _native
from cc_transcript.filterspec import (
    AGENT_INJECTION_GROUPS,
    COMMAND_ECHO_GROUPS,
    CONTINUATION_GROUPS,
    JUNK_USER_MESSAGE_RE,
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
    interrupt_marker,
    is_agent_injection,
    keep,
    labels_for,
    spec_to_json,
)
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    ModeEvent,
    TranscriptEvent,
    UserEvent,
)
from tests import testkit


def user(text: str, **kw: object) -> UserEvent:
    event = testkit.parse_event(testkit.user_line("u", text, session_id="s", **kw))
    assert isinstance(event, UserEvent)
    return event


def assistant(model: str = "claude-opus-4-7", text: str = "hi", *, tool: bool = False) -> AssistantEvent:
    blocks = (testkit.tool_use("t", "Bash", {}),) if tool else ()
    event = testkit.parse_event(testkit.assistant_line("a", text, model=model, blocks=blocks, session_id="s"))
    assert isinstance(event, AssistantEvent)
    return event


def spec(*clauses: Clause) -> FilterSpec:
    return FilterSpec(clauses=clauses)


def mode(value: str = "normal") -> ModeEvent:
    event = testkit.parse_event(testkit.mode_line(value, session_id="s"))
    assert isinstance(event, ModeEvent)
    return event


def attachment() -> AttachmentEvent:
    line = testkit.meta_fields("att", session_id="s") | {
        "type": "attachment",
        "attachment": {"type": "queued_command", "prompt": "go", "commandMode": "prompt"},
    }
    event = testkit.parse_event(line)
    assert isinstance(event, AttachmentEvent)
    return event


def test_keep_events_rejects_non_event_and_names_itself() -> None:
    # The events-in binding names itself, not mine(), so misuse points at the right API.
    with pytest.raises(TypeError, match=r"keep_events\(\) takes parsed transcript events"):
        _native.keep_events([object()], spec_to_json(spec()))


def test_apply_spec_streams_without_pulling_the_next_event() -> None:
    first = user("first substantive prompt")

    def gen() -> Iterator[TranscriptEvent]:
        yield first
        raise RuntimeError("apply_spec must not pull past the first event")

    survivors = apply_spec(gen(), spec())  # empty spec keeps everything
    assert next(survivors) is first


def test_kind_is_keeps_matching() -> None:
    s = spec(Clause(KindIs(frozenset({"user"})), negate=True))
    assert keep(user("hi"), s)
    assert not keep(assistant(), s)
    assert not keep(mode(), s)


def test_kind_is_attachment_keeps_and_drops() -> None:
    keep_attachments = spec(Clause(KindIs(frozenset({"attachment"})), negate=True))
    assert keep(attachment(), keep_attachments)
    assert not keep(user("hi"), keep_attachments)
    drop_attachments = spec(Clause(KindIs(frozenset({"attachment"}))))
    assert not keep(attachment(), drop_attachments)
    assert keep(user("hi"), drop_attachments)


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
    assert not keep(user("   <bash-stdout>123 passed</bash-stdout>"), s)  # leading whitespace: still an echo
    assert keep(user("run the bash input parser through pytest"), s)
    # A bash tag mentioned mid-sentence is authored prose, not an echo.
    assert keep(user("why does <bash-input>uv run pytest</bash-input> appear in my transcript?"), s)


JUNK_USER_MESSAGE_ROWS = [
    pytest.param("<command-name>commit</command-name>", True, id="command-name"),
    pytest.param("<command-message>running commit</command-message>", True, id="command-message"),
    pytest.param("<command-args>--all</command-args>", True, id="command-args"),
    pytest.param("<local-command-stdout>3 files changed</local-command-stdout>", True, id="local-command-stdout"),
    pytest.param("<local-command-stderr>fatal: not a git repo</local-command-stderr>", True, id="local-command-stderr"),
    pytest.param("<bash-input>uv run pytest</bash-input>", True, id="bash-echo"),
    pytest.param("<bash-stdout>123 passed</bash-stdout>", True, id="bash-stdout-echo"),
    pytest.param("   <bash-stdout>123 passed</bash-stdout>", True, id="bash-stdout-echo-leading-ws"),
    pytest.param(
        "why does <bash-input>uv run pytest</bash-input> appear in my transcript?", False, id="bash-tag-mid-text"
    ),
    pytest.param("the command-line tool works now", False, id="benign-neighbor"),
]


@pytest.mark.parametrize(("text", "is_junk"), JUNK_USER_MESSAGE_ROWS)
def test_junk_user_message_re_classifies_protocol_noise(text: str, is_junk: bool) -> None:
    assert bool(JUNK_USER_MESSAGE_RE.search(text)) is is_junk


def test_role_reminder_is_agent_injection_noise() -> None:
    s = spec(Clause(TextMatchesAny(AGENT_INJECTION_GROUPS), applies_to=frozenset({"user"})))
    assert not keep(user("[Role Reminder: You are a Coordinator. You NEVER edit files."), s)
    assert keep(user("remind me what the coordinator role does"), s)


AGENT_INJECTION_ROWS = [
    pytest.param("<teammate-message from='reviewer'>please rebase</teammate-message>", True, id="teammate-message"),
    pytest.param("<scheduled-task id='7'>run the suite</scheduled-task>", True, id="scheduled-task"),
    pytest.param("[Role Reminder: You are a Coordinator.", True, id="role-reminder-head-anchored"),
    pytest.param("# Augment Agent\nyou have these tools", True, id="augment-agent-head-anchored"),
    # Leading whitespace before the marker is tolerated — still a banner.
    pytest.param("   <teammate-message from='mate'>ping</teammate-message>", True, id="teammate-message-leading-ws"),
    # A real injected banner BEGINS with its marker; a relay tag mentioned mid-text is authored, not injected.
    pytest.param("as noted in the <teammate-message> above", False, id="teammate-tag-mid-text-no-match"),
    pytest.param("Why did the transcript contain <teammate-message from=a>?", False, id="authored-prompt-about-tag"),
    pytest.param("discussing the [Role Reminder] banner mid-sentence", False, id="role-reminder-mid-text-no-match"),
    # A combining mark (U+0301) after the tag name is not a portable word boundary — not a banner on either backend.
    pytest.param("<teammate-message\u0301>", False, id="teammate-message-combining-mark"),
    pytest.param("remind me what the teammate coordinator does", False, id="plain-prose"),
]


@pytest.mark.parametrize(("text", "injected"), AGENT_INJECTION_ROWS)
def test_is_agent_injection_helper(text: str, injected: bool) -> None:
    assert is_agent_injection(text) is injected


@pytest.mark.parametrize(("text", "injected"), AGENT_INJECTION_ROWS)
def test_is_agent_injection_matches_agent_injection_group_drop(text: str, injected: bool) -> None:
    s = spec(Clause(TextMatchesAny(AGENT_INJECTION_GROUPS), applies_to=frozenset({"user"})))
    assert is_agent_injection(text) is (not keep(user(text), s))


def test_interrupt_marker_ascii_pins_leading_i() -> None:
    assert (
        interrupt_marker("  [request INTERRUPTED by user for tool use]")
        == "[request INTERRUPTED by user for tool use]"
    )
    # The leading "i" is ASCII-pinned: re.IGNORECASE must no longer fold the dotted/
    # dotless-I forms (U+0130/U+0131) that Rust regex never matched — Rust parity.
    assert interrupt_marker("[Request ınterrupted by user]") is None
    assert interrupt_marker("[Request İnterrupted by user]") is None


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
