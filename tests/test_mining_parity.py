"""Rust mining executor == Python mining interpreter, over raw transcript bytes.

The dual-backend :func:`~cc_transcript.mining.engine.mine_signals` is the boundary
both sides feed: the Python reference parses bytes to events and runs
:func:`~cc_transcript.mining.signals.mine`; the Rust fast path parses and detects in
one pass and rehydrates the dicts. The raw-bytes parity contract is

    ``[signal_to_dict(s) for s in mine(parse_events_from_bytes(raw), spec)]``
        ``== _parser_rs.mine_signals(raw, mining_spec_to_json(spec))``

compared dict-by-value. This module proves it three ways: a hand-built battery with
one case per detector (plus near-misses, case-folds, and unicode), a deterministic
sample of real mirror transcripts, and a force-Python switch that must reproduce the
Rust result. A portability guard confirms a callable/lookaround review format never
reaches Rust yet still mines correctly through the Python path. Skips when the
extension is unavailable.
"""

from __future__ import annotations

import re
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
from cc_transcript.mining.engine import mine_signals, rust_mine_backend
from cc_transcript.mining.formats import ReviewComment, StructuredFormat
from cc_transcript.mining.signals import mine
from cc_transcript.mining.spec import (
    Base,
    BumpIfProximate,
    CallableReviewFormat,
    ConfidenceSpec,
    DemoteIfShort,
    MiningSpec,
    RegexReviewFormat,
    ReviewSpec,
    mining_spec_is_portable,
    mining_spec_to_json,
    signal_to_dict,
)
from cc_transcript.parser import parse_events_from_bytes
from tests.support import (
    MATCHER_LABELS,
    MATCHER_QUESTION,
    ROUND1_CONTENT,
    TOMBSTONE_LABELS,
    TOMBSTONE_QUESTION,
    fixture_bytes,
    requires_rust,
    rust_not_disabled,
)
from tests.support import (
    raw_envelope as envelope,
)

if TYPE_CHECKING:
    from typing import Any

    from cc_transcript.mining.signals import MiningSignal

SPEC = MiningSpec()

# Deterministic real-corpus sample. The mirror holds ~11k transcripts; mining every
# one would make this test minutes long, so we sample a fixed, reproducible slice.
# This cap is intentional and visible — never silently truncated. See mirror_corpus().
MIRROR_DIR = "/Users/yasyf/.cc-pushback/mirrors/yasyf"
MIRROR_SAMPLE = 150

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
        "plan_reentry_after_ccx_mcp_edit": (
            [
                assistant("a1", tool_use("e1", "mcp__cc-context__ccx_code_edit", file_path="/x.py")),
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
    }


def callable_review_spec() -> MiningSpec:
    def extract(text: str) -> tuple[ReviewComment, ...]:
        return (ReviewComment(file="cb.py", line_start=1, line_end=1, comment=text.strip()),)

    fmt = CallableReviewFormat(name="cb", pattern=re.compile(r"CALLABLE_MARKER"), extract=extract)
    return MiningSpec(review=ReviewSpec(callable_formats=(fmt,)))


def lookaround_review_spec() -> MiningSpec:
    fmt = RegexReviewFormat(
        name="look",
        groups=(("look", r"(?<=PREFIX )(?P<c>\w+)"),),
        file_group=None,
        line_start_group=None,
        line_end_group=None,
        comment_groups=(1,),
    )
    return MiningSpec(review=ReviewSpec(regex_formats=(fmt,)))


def unportable_cases() -> dict[str, tuple[MiningSpec, list[dict[str, Any]], str]]:
    return {
        "callable_format": (
            callable_review_spec(),
            [
                assistant("a1", {"type": "text", "text": "ok"}),
                user_text("u1", "CALLABLE_MARKER here is the inline comment"),
            ],
            "CALLABLE_MARKER here is the inline comment",
        ),
        "lookaround_regex": (
            lookaround_review_spec(),
            [assistant("a1", {"type": "text", "text": "ok"}), user_text("u1", "PREFIX matched")],
            "matched",
        ),
    }


def py_dicts(raw: bytes, spec: MiningSpec) -> list[dict[str, Any]]:
    return [signal_to_dict(signal) for signal in mine(parse_events_from_bytes(raw), spec)]


def rust_dicts(raw: bytes, spec: MiningSpec) -> list[dict[str, Any]]:
    from cc_transcript import _parser_rs

    return _parser_rs.mine_signals(raw, mining_spec_to_json(spec))


def assert_parity(raw: bytes, spec: MiningSpec) -> None:
    """The raw-bytes contract: Python dicts == Rust dicts, by value."""
    assert rust_dicts(raw, spec) == py_dicts(raw, spec)


def assert_signal_eq(a: MiningSignal, b: MiningSignal) -> None:
    assert signal_to_dict(a) == signal_to_dict(b)
    assert a.occurred_at == b.occurred_at
    assert a == b


def mirror_corpus() -> list[Path]:
    """A deterministic stride sample of real mirror transcripts, capped at MIRROR_SAMPLE.

    Sorts every ``*.jsonl`` under the mirror by path and takes a fixed stride so the
    same files are picked on every run. The MIRROR_SAMPLE cap is deliberate and
    visible — the full ~11k corpus would make this test minutes long.
    """
    mirror = Path(MIRROR_DIR)
    if not mirror.exists():
        return []
    paths = sorted(mirror.rglob("*.jsonl"))
    if not paths:
        return []
    return paths[:: max(1, len(paths) // MIRROR_SAMPLE)][:MIRROR_SAMPLE]


# ── tests ────────────────────────────────────────────────────────────────────


@requires_rust
@rust_not_disabled
def test_default_spec_is_portable() -> None:
    assert rust_mine_backend(SPEC) is not None


@requires_rust
def test_fixture_corpus_mining_parity() -> None:
    assert_parity(fixture_bytes(), SPEC)


@requires_rust
@pytest.mark.parametrize("name", battery())
def test_battery_detector_parity(name: str) -> None:
    entries, spec = battery()[name]
    assert_parity(to_bytes(entries), spec)


@pytest.mark.parametrize("name", ["plan_rejection_exitspecmode_alias", "plan_rejection_mcp_exitplanmode"])
def test_alias_plan_denials_mine_as_plan_rejections_not_denials(name: str) -> None:
    entries, spec = battery()[name]
    detectors = {signal["detector"] for signal in py_dicts(to_bytes(entries), spec)}
    assert "exit_plan_rejection" in detectors
    assert "denial" not in detectors


def test_structured_user_rejected_denial_mines_without_banner() -> None:
    entries, spec = battery()["denial_structured_user_rejected_no_banner"]
    detectors = {signal["detector"] for signal in py_dicts(to_bytes(entries), spec)}
    assert "denial" in detectors


def test_permission_rule_block_is_not_mined_as_denial() -> None:
    entries, spec = battery()["denial_permission_rule_not_mined"]
    assert all(signal["detector"] != "denial" for signal in py_dicts(to_bytes(entries), spec))


@requires_rust
@pytest.mark.parametrize("path", mirror_corpus(), ids=lambda p: f"{p.parent.name}/{p.name}")
def test_mirror_corpus_mining_parity(path: Path) -> None:
    # Logs the sample cap so the corpus test's reach is never silent.
    print(f"[mirror sample cap = {MIRROR_SAMPLE}] {path}")
    assert_parity(path.read_bytes(), SPEC)


@requires_rust
def test_mirror_corpus_has_teeth() -> None:
    """The sampled corpus must actually mine signals, or its parity proves nothing."""
    sample = mirror_corpus()
    if not sample:
        pytest.skip(f"no transcripts under {MIRROR_DIR}")
    producing = sum(1 for path in sample if py_dicts(path.read_bytes(), SPEC))
    print(f"[mirror sample] cap={MIRROR_SAMPLE} sampled={len(sample)} produced_signals={producing}")
    assert producing > 0


@requires_rust
def test_non_object_lines_skipped_parity() -> None:
    """Bare scalars and arrays between events never shift mined event_index (parser.py decode_line)."""
    entries = [
        assistant("a1", {"type": "text", "text": "I will refactor"}),
        user_text("u1", "this is completely wrong, please rewrite the parser module entirely"),
    ]
    raw = b"\n".join([orjson.dumps(entries[0]), b"42", b"[1, 2]", b'"scalar"', orjson.dumps(entries[1])])
    assert_parity(raw, SPEC)
    assert [signal["event_index"] for signal in py_dicts(raw, SPEC)] == [1]


@requires_rust
def test_rehydration_yields_objects_identical_to_python() -> None:
    raw = fixture_bytes()
    rust = list(mine_signals(raw, SPEC))
    python = list(mine(parse_events_from_bytes(raw), SPEC))
    assert len(rust) == len(python)
    for r, p in zip(rust, python, strict=True):
        assert_signal_eq(r, p)


@requires_rust
@pytest.mark.parametrize("name", unportable_cases())
def test_unportable_review_format_stays_python_yet_mines(name: str) -> None:
    spec, entries, expected_text = unportable_cases()[name]
    assert mining_spec_is_portable(spec) is False
    assert rust_mine_backend(spec) is None
    raw = to_bytes(entries)
    via_entry = [signal_to_dict(s) for s in mine_signals(raw, spec)]
    assert via_entry == py_dicts(raw, spec)
    assert [s["text"] for s in via_entry if s["detector"] == "review_comment"] == [expected_text]


def test_disable_rust_matches_rust_path(monkeypatch: pytest.MonkeyPatch) -> None:
    """CC_TRANSCRIPT_DISABLE_RUST=1 reproduces the Rust path for the same inputs."""
    raw = fixture_bytes()
    if rust_mine_backend(SPEC) is None:
        pytest.skip("_parser_rs unavailable; cannot compare the disabled path to Rust")
    rust = rust_dicts(raw, SPEC)
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    assert rust_mine_backend(SPEC) is None
    assert [signal_to_dict(s) for s in mine_signals(raw, SPEC)] == rust
