"""Golden regression + callback + crash-safety coverage for the Rust mining executor.

The Rust backend is the sole mining executor, reached through the one ``mine``
(events-in) entry: every case parses transcript bytes through the public parser and
mines the resulting events. This module freezes the executor's correct output —
captured from the historical Python reference and the sole correct reference for
callable review formats — into ``testdata/mining_golden.json`` and asserts ``mine``
still reproduces it: a hand-built battery with one case per detector (plus
near-misses, case-folds, and unicode), the structured/banner AskUserQuestion rounds,
and a captain-hook-shaped review spec that exercises the pyo3 callback side-channel
(superset-inline lookahead and conductor-workstream multi-pass callables alongside a
conductor-finding regex). A lookaround :class:`RegexReviewFormat`, which the Rust
``regex`` crate rejects, raises at mine time; a closing set of regressions pins the
parser-accepted inputs that once crashed the mine or discarded a transcript.
"""

from __future__ import annotations

import json
import re
from collections.abc import Iterator
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

import orjson
import pytest

from cc_transcript.filterspec import (
    ANSWERED_PREFIX,
    ANSWERED_TRAILER,
    DENIAL_PREFIX,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)
from cc_transcript.mining.confidence import MEDIUM
from cc_transcript.mining.engine import mine
from cc_transcript.mining.formats import ReviewComment, StructuredFormat
from cc_transcript.mining.signals import ANSWER_NOTES_SEP, ANSWER_PREVIEW_SEP, NO_OPTION_SELECTED
from cc_transcript.mining.spec import (
    Base,
    BumpIfProximate,
    CallableReviewFormat,
    ConfidenceSpec,
    DemoteIfShort,
    MiningSpec,
    RegexReviewFormat,
    ReviewSpec,
    signal_to_dict,
)
from cc_transcript.parser import parse, parse_events_from_bytes
from cc_transcript.tools import register_mcp_tool, unregister_mcp_tool
from tests.support import (
    MATCHER_LABELS,
    MATCHER_QUESTION,
    ROUND1_CONTENT,
    TOMBSTONE_LABELS,
    TOMBSTONE_QUESTION,
    fixture_bytes,
    requires_rust,
)
from tests.support import (
    raw_envelope as envelope,
)

if TYPE_CHECKING:
    from typing import Any

    from cc_transcript.mining.signals import MiningSignal

SPEC = MiningSpec()

GOLDEN: dict[str, Any] = json.loads(
    (Path(__file__).resolve().parent / "testdata" / "mining_golden.json").read_text(encoding="utf-8")
)


@pytest.fixture(autouse=True)
def _register_syn_span_edit() -> Iterator[None]:
    """Registers the synthetic MCP edit tool the reentry battery case relies on."""
    register_mcp_tool("syn_span_edit", "Edit", {"path": "path", "content": "content", "delete": "delete"})
    try:
        yield
    finally:
        unregister_mcp_tool("syn_span_edit")

# A portable review spec: an inline ``file:line: comment`` regex plus a JSON
# structured format exercising int / "96" / "24-51" line forms and a fix-key append.
INLINE_REGEX = RegexReviewFormat(
    name="inline",
    groups=(("inline", r"^(?P<f>[\w./]+):(?P<l>\d+):\s*(?P<c>.+)$"),),
    file_group=1,
    line_start_group=2,
    line_end_group=2,
    comment_groups=(3,),
)
STRUCTURED = StructuredFormat(name="findings_fmt", fix_keys=("fix", "suggestion"))
REVIEW_SPEC = MiningSpec(review=ReviewSpec(regex_formats=(INLINE_REGEX,), structured_formats=(STRUCTURED,)))

# A bracket format whose groups can carry padding, be whitespace-only, or hold an
# unparseable line — pinning the strip-then-filter comment join and the
# strip-then-parse-or-None line-group semantics on both backends.
PADDED_REGEX = RegexReviewFormat(
    name="padded",
    groups=(("padded", r"^R\[(?P<f>[^\]]*)\]\[(?P<l>[^\]]*)\]\[(?P<a>[^\]]*)\]\[(?P<b>[^\]]*)\]\[(?P<c>[^\]]*)\]$"),),
    file_group=1,
    line_start_group=2,
    line_end_group=2,
    comment_groups=(3, 4, 5),
)
PADDED_REVIEW_SPEC = MiningSpec(review=ReviewSpec(regex_formats=(PADDED_REGEX,)))

# A portable spec whose user_message stages carry no NoiseIfStructural: the
# marker-correction structural regex must fall back to the anchored, case-folded
# interrupt marker on BOTH backends — the spec is the contract, not the default
# STRUCTURAL_NOISE_RE constant.
NO_STRUCTURAL_SPEC = MiningSpec(
    user_message=ConfidenceSpec((Base(MEDIUM, "user_message"), DemoteIfShort(), BumpIfProximate()))
)
STRUCTURED_PAYLOAD = orjson.dumps(
    {
        "findings": [
            {"file": "a.py", "line": 96, "comment": "int line form", "fix": "guard the None case"},
            {"file": "b.py", "line": "96", "comment": "string-int line form"},
            {"file": "c.py", "line": "24-51", "comment": "range line form", "suggestion": "narrow the slice"},
            {"path": "d.py", "message": "no line cited at all"},
        ]
    }
).decode()


# ── raw-bytes builders (the JSONL envelope shape parse_events_from_bytes reads) ──


def to_bytes(entries: list[dict[str, Any]]) -> bytes:
    return b"\n".join(orjson.dumps(entry) for entry in entries)


def assistant(uuid: str, *blocks: dict[str, Any], **overrides: Any) -> dict[str, Any]:
    return envelope(
        type="assistant",
        uuid=uuid,
        message={"role": "assistant", "model": "claude-opus-4-8", "stop_reason": "tool_use", "content": list(blocks)},
        **overrides,
    )


def tool_use(tool_use_id: str, name: str, **inp: Any) -> dict[str, Any]:
    return {"type": "tool_use", "id": tool_use_id, "name": name, "input": inp}


def user_text(uuid: str, text: str, **overrides: Any) -> dict[str, Any]:
    return envelope(type="user", uuid=uuid, message={"role": "user", "content": text}, **overrides)


def user_result(
    uuid: str, tool_use_id: str, content: str, *, is_error: bool = True, **overrides: Any
) -> dict[str, Any]:
    return envelope(
        type="user",
        uuid=uuid,
        message={
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}],
        },
        **overrides,
    )


def plan_mode(**overrides: Any) -> dict[str, Any]:
    return {"type": "mode", "mode": "plan", "sessionId": "sess-1"} | overrides


def denial(embedded: str | None = None) -> str:
    """The CC denial banner, optionally wrapping the user's verbatim instruction."""
    banner = f"{DENIAL_PREFIX}.\n\n"
    if embedded is None:
        return banner
    return f"{banner}{USER_SAID_MARKER}{embedded}\n{USER_SAID_TRAILER} will follow."


def answered(pairs: str) -> str:
    """An answered AskUserQuestion round's tool-result content."""
    return f"{ANSWERED_PREFIX}{pairs}{ANSWERED_TRAILER}"


def auq_question(text: str, header: str, *labels: str, multi_select: bool = False) -> dict[str, Any]:
    return {
        "question": text,
        "header": header,
        "multiSelect": multi_select,
        "options": [{"label": label} for label in labels],
    }


def auq_round(questions: list[dict[str, Any]], content: str, *, is_error: bool = False) -> list[dict[str, Any]]:
    return [
        assistant("a1", tool_use("t1", "AskUserQuestion", questions=questions)),
        user_result("u1", "t1", content, is_error=is_error),
    ]


def render_pairs(
    questions: list[dict[str, Any]], answers: dict[str, str], annotations: dict[str, dict[str, str]]
) -> str:
    """Render a round's answers/annotations into the ANSWERED banner's pair body."""
    segments = []
    for question in questions:
        text = question["question"]
        answer = answers.get(text)
        head = f'"{text}"="{answer}"' if answer is not None else f'"{text}"={NO_OPTION_SELECTED}'
        annotation = annotations.get(text, {})
        if (preview := annotation.get("preview")) is not None:
            head += f"{ANSWER_PREVIEW_SEP}{preview}"
        if (notes := annotation.get("notes")) is not None:
            head += f"{ANSWER_NOTES_SEP}{notes}"
        segments.append(head)
    return ", ".join(segments)


def auq_payload(
    questions: list[dict[str, Any]], answers: dict[str, str], annotations: dict[str, dict[str, str]] | None = None
) -> dict[str, Any]:
    return {"questions": questions, "answers": answers, "annotations": annotations or {}}


def auq_round_structured(
    questions: list[dict[str, Any]],
    answers: dict[str, str],
    annotations: dict[str, dict[str, str]] | None = None,
    *,
    content: str,
    is_error: bool = False,
) -> list[dict[str, Any]]:
    """A round whose result carries the structured toolUseResult payload."""
    return [
        assistant("a1", tool_use("t1", "AskUserQuestion", questions=questions)),
        user_result(
            "u1", "t1", content, is_error=is_error, toolUseResult=auq_payload(questions, answers, annotations)
        ),
    ]


def auq_round_banner(
    questions: list[dict[str, Any]], answers: dict[str, str], annotations: dict[str, dict[str, str]] | None = None
) -> list[dict[str, Any]]:
    """The same round rendered banner-only, with no structured payload."""
    return auq_round(questions, answered(render_pairs(questions, answers, annotations or {})))


# ── the per-detector battery: one parameterized case per shape, plus near-misses ──


def battery() -> dict[str, tuple[list[dict[str, Any]], MiningSpec]]:
    return {
        # ── transcript_message: substantive / short / hedged / structural-noise ──
        "user_substantive": (
            [
                assistant("a1", {"type": "text", "text": "I will refactor"}),
                user_text("u1", "this is completely wrong, please rewrite the parser module entirely"),
            ],
            SPEC,
        ),
        "user_short_followup": ([assistant("a1", {"type": "text", "text": "done"}), user_text("u1", "no stop")], SPEC),
        "user_hedged": (
            [
                assistant("a1", {"type": "text", "text": "done"}),
                user_text("u1", "maybe you could possibly reconsider the whole approach here"),
            ],
            SPEC,
        ),
        "user_structural_noise": (
            [
                assistant("a1", {"type": "text", "text": "done"}),
                user_text("u1", "<system-reminder> background context follows in this block here</system-reminder>"),
            ],
            SPEC,
        ),
        "user_unicode": (
            [
                assistant("a1", {"type": "text", "text": "done"}),
                user_text("u1", "this is wrong héllo 🤖 漢字 fix the whole thing now"),
            ],
            SPEC,
        ),
        "user_empty_skipped": ([assistant("a1", {"type": "text", "text": "x"}), user_text("u1", "   ")], SPEC),
        # ── exit_plan_rejection: embedded text + bare-marker near-miss ──
        "plan_rejection_embedded": (
            [
                assistant("a1", tool_use("t1", "ExitPlanMode", plan="do X")),
                user_result("u1", "t1", denial("actually take a totally different direction with the design")),
            ],
            SPEC,
        ),
        "plan_rejection_bare_no_embedded": (
            [
                assistant("a1", tool_use("t1", "ExitPlanMode", plan="do X")),
                user_result("u1", "t1", denial(None)),
                user_text("u2", "no, take a totally different direction please"),
            ],
            SPEC,
        ),
        # Alias-closed plan tools: an ExitSpecMode denial is a plan rejection, never a denial.
        "plan_rejection_exitspecmode_alias": (
            [
                assistant("a1", tool_use("t1", "ExitSpecMode", plan="do X")),
                user_result("u1", "t1", denial("the plan skips the rollout ordering entirely")),
            ],
            SPEC,
        ),
        # MCP suffix matching: mcp__<server>__ExitPlanMode counts as a plan tool too.
        "plan_rejection_mcp_exitplanmode": (
            [
                assistant("a1", tool_use("t1", "mcp__conductor__ExitPlanMode", plan="do X")),
                user_result("u1", "t1", denial("route the plan through the staging service first")),
            ],
            SPEC,
        ),
        # ── plan_reentry: edit before plan-mode re-entry, and the 40-event boundary ──
        "plan_reentry_after_edit": (
            [
                assistant("a1", tool_use("e1", "Edit", file_path="/x.py")),
                plan_mode(),
                user_text("u1", "the edit was wrong, reconsider the plan from scratch"),
            ],
            SPEC,
        ),
        "plan_reentry_after_syn_mcp_edit": (
            [
                assistant("a1", tool_use("e1", "mcp__cc-context__syn_span_edit", file_path="/x.py")),
                plan_mode(),
                user_text("u1", "the edit was wrong, reconsider the plan from scratch"),
            ],
            SPEC,
        ),
        # Edit sits exactly REENTRY_LOOKBACK (40) events before the user re-entry:
        # last_edit_index scans range(user-1, user-40-1, -1), so distance 40 is included.
        "plan_reentry_lookback_at_boundary": (
            [
                assistant("e0", tool_use("e1", "Edit", file_path="/x.py")),
                *(assistant(f"f{i}", {"type": "text", "text": "thinking"}) for i in range(38)),
                plan_mode(),
                user_text("uu", "reconsider this plan completely and start over"),
            ],
            SPEC,
        ),
        # One event further back (distance 41) — beyond the lookback, so no reentry fires.
        "plan_reentry_lookback_beyond": (
            [
                assistant("e0", tool_use("e1", "Edit", file_path="/x.py")),
                *(assistant(f"f{i}", {"type": "text", "text": "thinking"}) for i in range(39)),
                plan_mode(),
                user_text("uu", "reconsider this plan completely and start over"),
            ],
            SPEC,
        ),
        # ── denial: embedded + followup + NotebookEdit/no-file_path + skip-cases ──
        "denial_embedded_with_filepath": (
            [
                assistant("a1", tool_use("t1", "Edit", file_path="/x.py")),
                user_result("u1", "t1", denial("do not touch that file, edit y.py instead")),
            ],
            SPEC,
        ),
        "denial_followup_after_bare": (
            [
                assistant("a1", tool_use("t1", "Bash", command="rm -rf /")),
                user_result("u1", "t1", denial(None)),
                user_text("u2", "never run destructive commands like that one"),
            ],
            SPEC,
        ),
        "denial_notebookedit_no_filepath": (
            [
                assistant("a1", tool_use("t1", "NotebookEdit", cell_id="c1")),
                user_result("u1", "t1", denial("edit the markdown cell instead of code")),
            ],
            SPEC,
        ),
        "denial_non_error_not_a_denial": (
            [
                assistant("a1", tool_use("t1", "Bash", command="ls")),
                user_result("u1", "t1", denial("ignored"), is_error=False),
            ],
            SPEC,
        ),
        "denial_askuserquestion_skipped": (
            [
                assistant("a1", tool_use("t1", "AskUserQuestion", question="?")),
                user_result("u1", "t1", denial("answer text here")),
            ],
            SPEC,
        ),
        # ── structured toolDenialKind: user-rejected mines without a banner; permission-rule never ──
        "denial_structured_user_rejected_no_banner": (
            [
                assistant("a1", tool_use("t1", "Bash", command="rm -rf /tmp/x")),
                user_result("u1", "t1", "stop that right now", toolDenialKind="user-rejected"),
                user_text("u2", "never run destructive commands like that one"),
            ],
            SPEC,
        ),
        "denial_permission_rule_not_mined": (
            [
                assistant("a1", tool_use("t1", "Bash", command="rm -rf /tmp/x")),
                user_result("u1", "t1", "Error: BLOCKED: policy forbids that", toolDenialKind="permission-rule"),
            ],
            SPEC,
        ),
        "denial_structured_user_rejected_non_error": (
            [
                assistant("a1", tool_use("t1", "Bash", command="rm -rf /tmp/x")),
                user_result("u1", "t1", "stop", is_error=False, toolDenialKind="user-rejected"),
                user_text("u2", "never run destructive commands like that one"),
            ],
            SPEC,
        ),
        # ── interrupt: bare marker + correction, and case-folded marker ──
        "interrupt_marker_then_correction": (
            [
                assistant("a1", tool_use("t1", "Bash", command="sleep 100")),
                user_result("u1", "t1", "[Request interrupted by user]"),
                user_text("u2", "stop that, do something else entirely instead"),
            ],
            SPEC,
        ),
        "interrupt_marker_casefolded": (
            [
                assistant("a1", tool_use("t1", "Bash", command="x")),
                user_result("u1", "t1", "[request INTERRUPTED by user for tool use]"),
                user_text("u2", "do it the other way around please"),
            ],
            SPEC,
        ),
        "interrupt_bare_no_correction": (
            [assistant("a1", {"type": "text", "text": "x"}), user_text("u1", "[Request interrupted by user]")],
            SPEC,
        ),
        # Custom user_message stages WITHOUT NoiseIfStructural: correction scanning
        # falls back to the interrupt-only structural regex, so the system-reminder
        # between the marker and the real correction is mined by both backends.
        "interrupt_custom_spec_structural_between": (
            [
                assistant("a1", tool_use("t1", "Bash", command="sleep 100")),
                user_result("u1", "t1", "[Request interrupted by user]"),
                user_text("u2", "<system-reminder>injected background context for the session</system-reminder>"),
                user_text("u3", "actually stop and rework the entire approach"),
            ],
            NO_STRUCTURAL_SPEC,
        ),
        # The interrupt-only fallback is case-folded on both sides: a case-folded,
        # non-bare marker fragment is structural noise, so the next message mines.
        "interrupt_custom_spec_casefolded_fallback": (
            [
                assistant("a1", tool_use("t1", "Bash", command="x")),
                user_result("u1", "t1", "[Request interrupted by user]"),
                user_text("u2", "[request INTERRUPTED by user] leftover fragment"),
                user_text("u3", "use the other module instead of this one"),
            ],
            NO_STRUCTURAL_SPEC,
        ),
        # ── review_comment: portable regex format + structured int/"96"/"24-51"/fix ──
        "review_regex_inline": (
            [
                assistant("a1", {"type": "text", "text": "ok"}),
                user_text("u1", "src/foo.py:42: this needs a guard\nsrc/bar.py:7: rename this symbol"),
            ],
            REVIEW_SPEC,
        ),
        "review_structured_line_forms": (
            [assistant("a1", {"type": "text", "text": "ok"}), user_text("u1", STRUCTURED_PAYLOAD)],
            REVIEW_SPEC,
        ),
        "review_structured_nonjson_no_match": (
            [
                assistant("a1", {"type": "text", "text": "ok"}),
                user_text("u1", "this is just prose, not a JSON document"),
            ],
            REVIEW_SPEC,
        ),
        # Whitespace-padded groups: the line group strips to 12 and the comment is
        # the single-space join of the stripped, non-empty groups on both backends.
        "review_regex_whitespace_padded_groups": (
            [
                assistant("a1", {"type": "text", "text": "ok"}),
                user_text("u1", "R[x.py][ 12 ][ first ][   ][ second ]"),
            ],
            PADDED_REVIEW_SPEC,
        ),
        # An unparseable line group yields None on both backends, never a crash.
        "review_regex_unparseable_line_group": (
            [
                assistant("a1", {"type": "text", "text": "ok"}),
                user_text("u1", "R[y.py][12a][note the guard][][]"),
            ],
            PADDED_REVIEW_SPEC,
        ),
        # ── ask_user_question: pick/freeform resolution, preview/notes, and skip-cases ──
        "auq_single_pick": (
            auq_round(
                [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                answered('"Which adapter?"="Storage (Recommended)"'),
            ),
            SPEC,
        ),
        "auq_fixture_round1_nested_quotes": (
            auq_round(
                [
                    auq_question(TOMBSTONE_QUESTION, "Enforcement", *TOMBSTONE_LABELS),
                    auq_question(MATCHER_QUESTION, "Matcher", *MATCHER_LABELS),
                ],
                ROUND1_CONTENT,
            ),
            SPEC,
        ),
        "auq_ordinal_shorthand": (
            auq_round(
                [
                    auq_question(
                        "Name the contexts?", "Names", "BeforeEdit / AfterEdit (Recommended)", "EditOld / EditNew"
                    )
                ],
                answered('"Name the contexts?"="1, but shouldnt those be default contexts? were they not before?"'),
            ),
            SPEC,
        ),
        "auq_multiselect_join": (
            auq_round(
                [
                    auq_question(
                        "Which docs pages?",
                        "Docs",
                        "Getting started",
                        "How it works, end to end",
                        "CLI reference",
                        multi_select=True,
                    )
                ],
                answered('"Which docs pages?"="Getting started, How it works, end to end"'),
            ),
            SPEC,
        ),
        "auq_preview": (
            auq_round(
                [auq_question("How far should enable go?", "Install", "Full turnkey (Recommended)", "Install only")],
                answered(
                    '"How far should enable go?"="Full turnkey (Recommended)" selected preview:\n'
                    "$ tool enable\n==> done"
                ),
            ),
            SPEC,
        ),
        "auq_no_option_notes": (
            auq_round(
                [auq_question("Add CI coverage?", "CI", "Add the guard", "Skip CI guard")],
                answered('"Add CI coverage?"=(no option selected) notes: fix it upstream so it skips invalid files'),
            ),
            SPEC,
        ),
        # A pick carrying notes scores on the notes like a freeform answer, not the
        # flat option_pick weak floor — the notes-aware branch on both backends.
        "auq_pick_with_notes": (
            auq_round(
                [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                answered('"Which adapter?"="Storage (Recommended)" notes: and never use the memory one again'),
            ),
            SPEC,
        ),
        # A pick whose segment ends in a bare ' notes: ' yields empty notes: both
        # backends treat empty notes as absent and fall back to the weak option_pick.
        "auq_pick_trailing_empty_notes": (
            auq_round(
                [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                answered('"Which adapter?"="Storage (Recommended)" notes: '),
            ),
            SPEC,
        ),
        "auq_pair_omitted": (
            auq_round(
                [
                    auq_question("First unanswered?", "One", "A", "B"),
                    auq_question("Second answered?", "Two", "C", "D"),
                ],
                answered('"Second answered?"="C"'),
            ),
            SPEC,
        ),
        "auq_malformed_question_skipped": (
            auq_round(
                [
                    {"header": "Broken", "multiSelect": False, "options": [{"label": "A"}]},
                    auq_question("Second answered?", "Two", "C", "D"),
                ],
                answered('"Second answered?"="C"'),
            ),
            SPEC,
        ),
        "auq_error_zero": (
            auq_round(
                [auq_question("Which adapter?", "Adapter", "Storage", "Memory")],
                answered('"Which adapter?"="Storage"'),
                is_error=True,
            ),
            SPEC,
        ),
        "auq_unpaired_zero": (
            [
                assistant("a1", tool_use("t1", "AskUserQuestion", questions=[auq_question("Q?", "H", "A")])),
                user_result("u1", "t9", answered('"Q?"="A"'), is_error=False),
            ],
            SPEC,
        ),
        "auq_answer_embeds_later_anchor": (
            auq_round(
                [auq_question("First?", "One", "A", "B"), auq_question("Second?", "Two", "C", "D")],
                answered('"First?"="I think "Second?"=maybe", "Second?"="C"'),
            ),
            SPEC,
        ),
        "auq_answer_trailing_quote": (
            auq_round(
                [auq_question("Which name?", "Name", "Alpha", "Beta")],
                answered('"Which name?"="call it "beta""'),
            ),
            SPEC,
        ),
        # ── structured-first: pairs built from the toolUseResult payload, not the banner ──
        "auq_structured_only": (
            auq_round_structured(
                [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                {"Which adapter?": "Storage (Recommended)"},
                content="answered",
            ),
            SPEC,
        ),
        "auq_structured_multiselect": (
            auq_round_structured(
                [
                    auq_question(
                        "Which docs pages?",
                        "Docs",
                        "Getting started",
                        "How it works, end to end",
                        "CLI reference",
                        multi_select=True,
                    )
                ],
                {"Which docs pages?": "Getting started, How it works, end to end"},
                content="answered",
            ),
            SPEC,
        ),
        "auq_structured_omitted": (
            auq_round_structured(
                [
                    auq_question("First unanswered?", "One", "A", "B"),
                    auq_question("Second answered?", "Two", "C", "D"),
                ],
                {"Second answered?": "C"},
                {"First unanswered?": {"notes": "leave this one alone"}},
                content="answered",
            ),
            SPEC,
        ),
        "auq_structured_annotations": (
            auq_round_structured(
                [auq_question("How far should enable go?", "Install", "Full turnkey (Recommended)", "Install only")],
                {"How far should enable go?": "Full turnkey (Recommended)"},
                {"How far should enable go?": {"preview": "$ tool enable\n==> done", "notes": "make it turnkey"}},
                content="answered",
            ),
            SPEC,
        ),
        # An error round carrying a full structured payload mines nothing: the
        # structured branch is gated on !is_error just like the banner path.
        "auq_structured_error_zero": (
            auq_round_structured(
                [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                {"Which adapter?": "Storage (Recommended)"},
                content=answered('"Which adapter?"="Storage (Recommended)"'),
                is_error=True,
            ),
            SPEC,
        ),
        # A malformed annotation leaf (numeric notes) reads as absent on both
        # backends: the plain answer signal, no notes key in evidence.
        "auq_structured_numeric_notes_leaf": (
            [
                assistant(
                    "a1",
                    tool_use(
                        "t1",
                        "AskUserQuestion",
                        questions=[auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                    ),
                ),
                user_result(
                    "u1",
                    "t1",
                    "answered",
                    is_error=False,
                    toolUseResult={
                        "questions": [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                        "answers": {"Which adapter?": "Storage (Recommended)"},
                        "annotations": {"Which adapter?": {"notes": 3}},
                    },
                ),
            ],
            SPEC,
        ),
        # A payload with answers but no questions key falls back to the tool-use
        # input's questions instead of mining nothing.
        "auq_structured_no_questions_key": (
            [
                assistant(
                    "a1",
                    tool_use(
                        "t1",
                        "AskUserQuestion",
                        questions=[auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
                    ),
                ),
                user_result(
                    "u1",
                    "t1",
                    answered('"Which adapter?"="Storage (Recommended)"'),
                    is_error=False,
                    toolUseResult={"answers": {"Which adapter?": "Storage (Recommended)"}},
                ),
            ],
            SPEC,
        ),
    }


def rust_dicts(raw: bytes, spec: MiningSpec) -> list[dict[str, Any]]:
    return [signal_to_dict(signal) for signal in mine(parse_events_from_bytes(raw), spec)]


def assert_signal_valid(signal: MiningSignal) -> None:
    assert isinstance(signal.occurred_at, datetime)
    assert isinstance(signal.text, str)


def callable_review_spec() -> MiningSpec:
    def extract(text: str) -> tuple[ReviewComment, ...]:
        return (ReviewComment(file="cb.py", line_start=1, line_end=1, comment=text.strip()),)

    fmt = CallableReviewFormat(name="cb", pattern=re.compile(r"CALLABLE_MARKER"), extract=extract)
    return MiningSpec(review=ReviewSpec(callable_formats=(fmt,)))


def lookaround_review_spec() -> MiningSpec:
    # Lookbehind the Rust regex crate rejects: a Rust-executed RegexReviewFormat that
    # must raise at mine time, not silently mine nothing (D4 — no fancy-regex fallback).
    fmt = RegexReviewFormat(
        name="look",
        groups=(("look", r"(?<=PREFIX )(?P<c>\w+)"),),
        file_group=None,
        line_start_group=None,
        line_end_group=None,
        comment_groups=(1,),
    )
    return MiningSpec(review=ReviewSpec(regex_formats=(fmt,)))


# captain-hook's three real review formats: conductor-finding executes as a Rust
# RegexReviewFormat, while superset-inline (lookahead) and conductor-workstream
# (multi-pass) reach Rust only through the pyo3 callback side-channel.
SUPERSET_INLINE_RE = re.compile(r"^In ((?=\S*[./]|\S+?:L)\S+?)(?::L(\d+)(?:-(\d+))?)?: (.+)$", re.MULTILINE)
CONDUCTOR_WORKSTREAM_RE = re.compile(
    r"^### (?P<id>[A-Z][\w-]*\d*)\s*\[(?P<kind>[A-Z]+)\]\s*—\s*(?P<title>.+)$", re.MULTILINE
)
CONDUCTOR_FINDING_FMT = RegexReviewFormat(
    name="conductor-finding",
    groups=(
        (
            "conductor-finding",
            r"^- file: (\S+?):(\d+)\s*$(?:\n- theme: .+$)?(?:\n- claim: (.+)$)?(?:\n- suggestion: (.+)$)?",
        ),
    ),
    file_group=1,
    line_start_group=2,
    line_end_group=None,
    comment_groups=(3, 4),
    multiline=True,
)

CAPTAIN_HOOK_REVIEW = (
    "In cc_transcript/filter.py:L12-15: this branch never runs, drop it\n"
    "In parser.py: missing a newline guard here\n"
    "\n"
    "### T1 [CORRECTNESS] — Guard against the empty spec\n"
    "FIX: add an early return\n"
    "Tests: cover the empty-spec case\n"
    "\n"
    "- file: cc_transcript/mining/spec.py:88\n"
    "- theme: dead code\n"
    "- claim: UNPORTABLE_RE gives false confidence\n"
    "- suggestion: delete the gate\n"
)


def extract_superset_inline(text: str) -> tuple[ReviewComment, ...]:
    return tuple(
        ReviewComment(
            file=match.group(1),
            line_start=int(match.group(2)) if match.group(2) else None,
            line_end=int(match.group(3)) if match.group(3) else None,
            comment=match.group(4).strip(),
        )
        for match in SUPERSET_INLINE_RE.finditer(text)
    )


def extract_conductor_workstream(text: str) -> tuple[ReviewComment, ...]:
    headers = list(CONDUCTOR_WORKSTREAM_RE.finditer(text))
    return tuple(
        ReviewComment(
            file=None,
            line_start=None,
            line_end=None,
            comment=" ".join(
                [f"{header.group('id')} [{header.group('kind')}] {header.group('title').strip()}"]
                + [
                    line.group(0).strip()
                    for line in re.finditer(r"^(?:FIX|Tests): .+$", text[header.end() : end], re.MULTILINE)
                ]
            ),
        )
        for header, end in zip(headers, [*(h.start() for h in headers[1:]), len(text)], strict=True)
    )


def captain_hook_review_spec() -> MiningSpec:
    return MiningSpec(
        review=ReviewSpec(
            regex_formats=(CONDUCTOR_FINDING_FMT,),
            callable_formats=(
                CallableReviewFormat("superset-inline", SUPERSET_INLINE_RE, extract_superset_inline),
                CallableReviewFormat("conductor-workstream", CONDUCTOR_WORKSTREAM_RE, extract_conductor_workstream),
            ),
            surfaces=frozenset({"typed"}),
        )
    )


# ── tests ────────────────────────────────────────────────────────────────────


@requires_rust
def test_fixture_corpus_mining_golden() -> None:
    assert rust_dicts(fixture_bytes(), SPEC) == GOLDEN["fixture"]


@requires_rust
@pytest.mark.parametrize("name", battery())
def test_battery_detector_golden(name: str) -> None:
    entries, spec = battery()[name]
    assert rust_dicts(to_bytes(entries), spec) == GOLDEN["battery"][name]


@pytest.mark.parametrize("name", ["plan_rejection_exitspecmode_alias", "plan_rejection_mcp_exitplanmode"])
def test_alias_plan_denials_mine_as_plan_rejections_not_denials(name: str) -> None:
    entries, spec = battery()[name]
    detectors = {signal["detector"] for signal in rust_dicts(to_bytes(entries), spec)}
    assert "exit_plan_rejection" in detectors
    assert "denial" not in detectors


# Rounds exercising the structured-first path against its banner rendering: single
# pick, pick+notes, comma-bearing multiSelect labels, an omitted answer, preview+notes,
# and a full two-question round.
STRUCTURED_ROUNDS: dict[str, tuple[list[dict[str, Any]], dict[str, str], dict[str, dict[str, str]]]] = {
    "single_pick": (
        [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
        {"Which adapter?": "Storage (Recommended)"},
        {},
    ),
    "pick_with_notes": (
        [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
        {"Which adapter?": "Storage (Recommended)"},
        {"Which adapter?": {"notes": "and never use the memory one again"}},
    ),
    "multiselect_comma_label": (
        [
            auq_question(
                "Which docs pages?",
                "Docs",
                "Getting started",
                "How it works, end to end",
                "CLI reference",
                multi_select=True,
            )
        ],
        {"Which docs pages?": "Getting started, How it works, end to end"},
        {},
    ),
    "omitted_with_notes": (
        [auq_question("First unanswered?", "One", "A", "B"), auq_question("Second answered?", "Two", "C", "D")],
        {"Second answered?": "C"},
        {"First unanswered?": {"notes": "leave this one alone"}},
    ),
    "preview_and_notes": (
        [auq_question("How far should enable go?", "Install", "Full turnkey (Recommended)", "Install only")],
        {"How far should enable go?": "Full turnkey (Recommended)"},
        {"How far should enable go?": {"preview": "$ tool enable\n==> done", "notes": "make it turnkey"}},
    ),
    "full_round": (
        [
            auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory"),
            auq_question(
                "Which docs pages?",
                "Docs",
                "Getting started",
                "How it works, end to end",
                "CLI reference",
                multi_select=True,
            ),
        ],
        {"Which adapter?": "Storage (Recommended)", "Which docs pages?": "Getting started, How it works, end to end"},
        {"Which adapter?": {"notes": "and never use the memory one again"}},
    ),
}


@requires_rust
@pytest.mark.parametrize("name", STRUCTURED_ROUNDS)
def test_ask_user_question_structured_matches_banner(name: str) -> None:
    """Structured payload, banner, and both-present all mine the frozen banner signals."""
    questions, answers, annotations = STRUCTURED_ROUNDS[name]
    structured = to_bytes(auq_round_structured(questions, answers, annotations, content="answered"))
    banner = to_bytes(auq_round_banner(questions, answers, annotations))
    both = to_bytes(
        auq_round_structured(
            questions, answers, annotations, content=answered(render_pairs(questions, answers, annotations))
        )
    )
    expected = GOLDEN["structured_rounds"][name]
    assert expected
    for raw in (structured, banner, both):
        assert rust_dicts(raw, SPEC) == expected


@requires_rust
def test_ask_user_question_structured_omitted_matches_no_option_selected() -> None:
    """A question absent from ``answers`` mines exactly as the banner's ``(no option selected)``."""
    questions = [auq_question("First unanswered?", "One", "A", "B"), auq_question("Second answered?", "Two", "C", "D")]
    answers = {"Second answered?": "C"}
    annotations = {"First unanswered?": {"notes": "leave this one alone"}}
    structured = rust_dicts(to_bytes(auq_round_structured(questions, answers, annotations, content="answered")), SPEC)
    assert structured == rust_dicts(to_bytes(auq_round_banner(questions, answers, annotations)), SPEC)
    omitted = next(signal for signal in structured if signal["evidence"]["question"] == "First unanswered?")
    assert omitted["evidence"]["option_pick"] is False
    assert omitted["evidence"]["picked_labels"] == []
    assert omitted["text"] == "leave this one alone"


@requires_rust
def test_ask_user_question_structured_annotations_flow_into_evidence() -> None:
    """Preview and notes from ``annotations`` land in evidence exactly as the banner path's do."""
    questions = [auq_question("How far should enable go?", "Install", "Full turnkey (Recommended)", "Install only")]
    answers = {"How far should enable go?": "Full turnkey (Recommended)"}
    annotations = {"How far should enable go?": {"preview": "$ tool enable\n==> done", "notes": "make it turnkey"}}
    structured = rust_dicts(to_bytes(auq_round_structured(questions, answers, annotations, content="answered")), SPEC)
    assert structured == rust_dicts(to_bytes(auq_round_banner(questions, answers, annotations)), SPEC)
    [signal] = structured
    assert signal["evidence"]["preview"] == "$ tool enable\n==> done"
    assert signal["evidence"]["notes"] == "make it turnkey"
    assert signal["text"] == "make it turnkey"


@requires_rust
def test_ask_user_question_error_round_with_payload_mines_nothing() -> None:
    """An is_error round yields zero signals."""
    questions = [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")]
    answers = {"Which adapter?": "Storage (Recommended)"}
    raw = to_bytes(
        auq_round_structured(questions, answers, content=answered(render_pairs(questions, answers, {})), is_error=True)
    )
    assert rust_dicts(raw, SPEC) == []


@requires_rust
def test_ask_user_question_payload_without_questions_falls_back_to_use_questions() -> None:
    """A payload carrying only ``answers`` mines via the tool-use input's questions."""
    questions = [auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")]
    answers = {"Which adapter?": "Storage (Recommended)"}
    raw = to_bytes(
        [
            assistant("a1", tool_use("t1", "AskUserQuestion", questions=questions)),
            user_result(
                "u1",
                "t1",
                answered(render_pairs(questions, answers, {})),
                is_error=False,
                toolUseResult={"answers": answers},
            ),
        ]
    )
    signals = rust_dicts(raw, SPEC)
    banner_signals = rust_dicts(to_bytes(auq_round_banner(questions, answers)), SPEC)
    assert banner_signals
    assert signals == banner_signals


def test_structured_user_rejected_denial_mines_without_banner() -> None:
    entries, spec = battery()["denial_structured_user_rejected_no_banner"]
    detectors = {signal["detector"] for signal in rust_dicts(to_bytes(entries), spec)}
    assert "denial" in detectors


def test_permission_rule_block_is_not_mined_as_denial() -> None:
    entries, spec = battery()["denial_permission_rule_not_mined"]
    assert all(signal["detector"] != "denial" for signal in rust_dicts(to_bytes(entries), spec))


@requires_rust
def test_non_object_lines_skipped_keep_event_index() -> None:
    """Bare scalars and arrays between events never shift mined event_index (parser.py decode_line)."""
    entries = [
        assistant("a1", {"type": "text", "text": "I will refactor"}),
        user_text("u1", "this is completely wrong, please rewrite the parser module entirely"),
    ]
    raw = b"\n".join([orjson.dumps(entries[0]), b"42", b"[1, 2]", b'"scalar"', orjson.dumps(entries[1])])
    assert [signal["event_index"] for signal in rust_dicts(raw, SPEC)] == [1]


@requires_rust
def test_mine_rehydrates_objects_matching_golden() -> None:
    raw = fixture_bytes()
    signals = list(mine(parse_events_from_bytes(raw), SPEC))
    for signal in signals:
        assert_signal_valid(signal)
    assert [signal_to_dict(signal) for signal in signals] == GOLDEN["fixture"]


@requires_rust
def test_captain_hook_review_spec_mines_via_callback() -> None:
    """The load-bearing consumer: a conductor-finding regex plus superset-inline and
    conductor-workstream callables mine identical candidates on the Rust path, the two
    callables invoked through the pyo3 side-channel."""
    entries = [assistant("a1", {"type": "text", "text": "here is my review"}), user_text("u1", CAPTAIN_HOOK_REVIEW)]
    signals = rust_dicts(to_bytes(entries), captain_hook_review_spec())
    assert signals == GOLDEN["captain_hook"]
    formats = sorted(s["evidence"]["format"] for s in signals if s["detector"] == "review_comment")
    assert formats == ["conductor-finding", "conductor-workstream", "superset-inline", "superset-inline"]


@requires_rust
def test_callable_review_format_mines_via_callback() -> None:
    entries = [
        assistant("a1", {"type": "text", "text": "ok"}),
        user_text("u1", "CALLABLE_MARKER here is the inline comment"),
    ]
    signals = rust_dicts(to_bytes(entries), callable_review_spec())
    assert signals == GOLDEN["callable"]
    assert [s["text"] for s in signals if s["detector"] == "review_comment"] == [
        "CALLABLE_MARKER here is the inline comment"
    ]


@requires_rust
def test_lookaround_regex_review_format_raises() -> None:
    """A RegexReviewFormat with lookbehind — rejected by the Rust regex crate — raises at
    mine time rather than silently mining nothing (D4: no fancy-regex fallback)."""
    entries = [assistant("a1", {"type": "text", "text": "ok"}), user_text("u1", "PREFIX matched")]
    with pytest.raises(ValueError):
        list(mine(parse_events_from_bytes(to_bytes(entries)), lookaround_review_spec()))


# ── crash-safety regressions over inputs the parser accepts ───────────────────
#
# mine(events) is the one mining path, so every consumer runs through it. These pin
# the three inputs the parser accepts but that once crashed the mine or discarded a
# transcript: a non-object tool input, a non-finite toolUseResult number, and an
# un-materializable event alongside good ones.


@requires_rust
def test_non_object_tool_input_reads_as_none_and_mines() -> None:
    """A non-object tool input (which the parser accepts verbatim) makes ``.file_path``
    and ``.questions`` read as None instead of raising ``AttributeError``, so
    ``mine(events)`` survives a transcript that carries one (#1)."""
    entries = [
        assistant("a1", {"type": "tool_use", "id": "t1", "name": "Bash", "input": "not-a-dict"}),
        user_text("u1", "this is completely wrong, please rewrite the parser module entirely"),
    ]
    assert [signal["detector"] for signal in rust_dicts(to_bytes(entries), SPEC)] == ["transcript_message"]


@requires_rust
def test_non_finite_tool_use_result_number_mines_without_crash(tmp_path: Path) -> None:
    """A ``toolUseResult`` number the Rust parse path materializes to ``inf`` (a huge
    literal like ``1e9999``) maps to null instead of crashing the JSON re-encode, and
    the AskUserQuestion signal is still mined intact (#3)."""
    entries = [
        assistant(
            "a1",
            tool_use(
                "t1",
                "AskUserQuestion",
                questions=[auq_question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
            ),
        ),
        user_result(
            "u1",
            "t1",
            answered('"Which adapter?"="Storage (Recommended)"'),
            is_error=False,
            toolUseResult={"answers": {"Which adapter?": "Storage (Recommended)"}, "weird": "__NONFINITE__"},
        ),
    ]
    path = tmp_path / "nonfinite.jsonl"
    path.write_bytes(to_bytes(entries).replace(b'"__NONFINITE__"', b"1e9999"))
    events = list(parse(path).events)
    signals = list(mine(events, SPEC))
    assert [signal.detector for signal in signals] == ["ask_user_question"]
    assert signals[0].text == "Storage (Recommended)"


@requires_rust
def test_parse_file_skips_unmaterializable_event_keeps_rest(tmp_path: Path) -> None:
    """A single event that cannot materialize to Python (a year-zero timestamp below
    ``MINYEAR``) is dropped, and the rest of the transcript survives rather than the
    whole file reading as empty (#4)."""
    corrupt = orjson.dumps(user_text("z1", "hello", timestamp="0000-01-01T00:00:00Z"))
    good = [assistant("a1", {"type": "text", "text": "hi"}), user_text("u1", "normal message")]
    path = tmp_path / "corrupt.jsonl"
    path.write_bytes(corrupt + b"\n" + to_bytes(good))
    events = list(parse(path).events)
    assert [type(event).__name__ for event in events] == ["AssistantEvent", "UserEvent"]
