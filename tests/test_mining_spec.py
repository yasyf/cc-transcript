"""The declarative mining spec: JSON round-trip and the regex-review-format
extractor that the Rust backend must reproduce.
"""

from __future__ import annotations

import json
import re

from cc_transcript.mining import (
    HEDGE_GROUPS,
    CallableReviewFormat,
    MiningSpec,
    RegexReviewFormat,
    ReviewSpec,
    StructuredFormat,
    mining_spec_to_json,
    signal_to_dict,
)
from cc_transcript.mining.confidence import HIGH, MEDIUM, CandidateSignal
from cc_transcript.mining.signals import MiningSignal
from cc_transcript.mining.sourcekind import REVIEW_COMMENT
from cc_transcript.mining.spec import regex_review_comments
from cc_transcript.models import CcVersion, EntryMeta, EventUuid, SessionId

CONDUCTOR_FINDING_RE = re.compile(
    r"^- file: (?P<file>\S+?):(?P<line>\d+)\s*$"
    r"(?:\n- theme: .+$)?"
    r"(?:\n- claim: (?P<claim>.+)$)?"
    r"(?:\n- suggestion: (?P<suggestion>.+)$)?",
    re.MULTILINE,
)
SUPERSET_INLINE_RE = re.compile(r"^In ((?=\S*[./]|\S+?:L)\S+?)(?::L(\d+)(?:-(\d+))?)?: (.+)$", re.MULTILINE)

CONDUCTOR_FINDING = RegexReviewFormat(
    name="conductor-finding",
    groups=(("conductor-finding", CONDUCTOR_FINDING_RE.pattern),),
    file_group=1,
    line_start_group=2,
    line_end_group=None,
    comment_groups=(3, 4),
    multiline=True,
    ignore_case=False,
)
SUPERSET_INLINE = CallableReviewFormat(name="superset-inline", pattern=SUPERSET_INLINE_RE, extract=lambda _: ())
WORKFLOW_FINDING = StructuredFormat(
    name="workflow-finding",
    file_keys=("file", "path", "file_path", "location"),
    line_keys=("line", "line_start", "lines"),
    comment_keys=("comment", "message", "description", "evidence", "detail", "problem", "why", "issue"),
    fix_keys=("suggested_fix", "suggestion", "fix"),
    finding_keys=("confirmedHigh", "confirmedCritical"),
)


def test_default_spec_to_json_shape() -> None:
    payload = json.loads(mining_spec_to_json(MiningSpec()))
    assert payload["detectors"] == [
        "ask_user_question",
        "denial",
        "exit_plan_rejection",
        "interrupt",
        "plan_reentry",
        "review_comment",
        "transcript_message",
    ]
    assert payload["reentry_lookback"] == 40
    assert payload["edit_tools"] == [
        "Create",
        "Edit",
        "MultiEdit",
        "NotebookEdit",
        "Write",
        "ccx_code_edit",
        "ccx_code_replace",
    ]
    assert payload["plan_tools"] == ["ExitPlanMode", "ExitSpecMode"]
    assert payload["denial_excluded_tools"] == ["AskUserQuestion", "ExitPlanMode", "ExitSpecMode"]
    assert payload["provenance"] == {"subagent_tools": ["Agent", "Task"]}
    assert payload["review"] == {
        "surfaces": ["surfaced", "typed"],
        "regex_formats": [],
        "structured_formats": [],
    }


def test_default_user_message_stages_serialize_in_fold_order() -> None:
    stages = json.loads(mining_spec_to_json(MiningSpec()))["user_message"]["stages"]
    assert [stage["kind"] for stage in stages] == [
        "NoiseIfStructural",
        "Base",
        "DemoteIfShort",
        "BumpIfProximate",
    ]
    assert stages[1] == {"kind": "Base", "band": 0.5, "reason": "user_message"}
    assert stages[2] == {"kind": "DemoteIfShort", "max_words": 2, "delta": -0.25, "reason": "short_followup"}
    assert stages[3] == {"kind": "BumpIfProximate", "within": 2, "delta": 0.25, "reason": "trigger_proximate"}


def test_default_calibrated_stages_serialize_in_fold_order() -> None:
    stages = json.loads(mining_spec_to_json(MiningSpec()))["calibrated"]["stages"]
    assert [stage["kind"] for stage in stages] == ["BumpIfSubstantive", "DemoteIfHedged"]
    hedged = stages[1]
    assert hedged["kind"] == "DemoteIfHedged"
    assert hedged["delta"] == -0.25
    assert hedged["ignore_case"] is True
    assert hedged["reason"] == "hedged"
    assert hedged["groups"] == [list(group) for group in HEDGE_GROUPS]


def test_review_spec_to_json_serializes_each_format_arm() -> None:
    spec = MiningSpec(
        review=ReviewSpec(
            regex_formats=(CONDUCTOR_FINDING,),
            callable_formats=(SUPERSET_INLINE,),
            structured_formats=(WORKFLOW_FINDING,),
            surfaces=frozenset({"typed", "surfaced"}),
        )
    )
    review = json.loads(mining_spec_to_json(spec))["review"]
    assert review["surfaces"] == ["surfaced", "typed"]
    assert review["regex_formats"] == [
        {
            "kind": "RegexReviewFormat",
            "name": "conductor-finding",
            "groups": [["conductor-finding", CONDUCTOR_FINDING_RE.pattern]],
            "file_group": 1,
            "line_start_group": 2,
            "line_end_group": None,
            "comment_groups": [3, 4],
            "join": " ",
            "multiline": True,
            "ignore_case": False,
        }
    ]
    assert review["structured_formats"][0]["finding_keys"] == [
        "findings",
        "bugs",
        "improvements",
        "issues",
        "items",
        "verdicts",
        "confirmedHigh",
        "confirmedCritical",
    ]


def test_regex_review_comments_joins_claim_and_suggestion() -> None:
    text = (
        "- file: a.py:24\n- theme: bug\n"
        "- claim: this leaks a file descriptor\n- suggestion: use a context manager"
    )
    comments = regex_review_comments(CONDUCTOR_FINDING, text)
    assert comments[0].file == "a.py"
    assert comments[0].line_start == 24
    assert comments[0].line_end is None
    assert comments[0].comment == "this leaks a file descriptor use a context manager"


def test_regex_review_comments_skips_missing_optional_group() -> None:
    text = "- file: b.py:9\n- claim: only a claim here"
    comments = regex_review_comments(CONDUCTOR_FINDING, text)
    assert comments[0].comment == "only a claim here"


def test_regex_review_comments_matches_legacy_extractor_byte_for_byte() -> None:
    text = (
        "- file: a.py:24\n- theme: bug\n"
        "- claim: this leaks a file descriptor\n- suggestion: use a context manager"
    )
    legacy = tuple(
        (
            match.group("file"),
            int(match.group("line")),
            " ".join(part.strip() for part in (match.group("claim"), match.group("suggestion")) if part),
        )
        for match in CONDUCTOR_FINDING_RE.finditer(text)
    )
    ported = tuple((c.file, c.line_start, c.comment) for c in regex_review_comments(CONDUCTOR_FINDING, text))
    assert ported == legacy


def meta(uuid: str) -> EntryMeta:
    from datetime import UTC, datetime

    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SessionId("sess-1"),
        timestamp=datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=False,
        is_meta=False,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def test_signal_to_dict_canonical_shape() -> None:
    signal = MiningSignal(
        kind=REVIEW_COMMENT,
        detector="review_comment",
        session_id=SessionId("sess-1"),
        event_index=7,
        event_uuid=EventUuid("u7"),
        occurred_at=meta("u7").timestamp,
        text="guard against None",
        cc_version=CcVersion("1.2.3"),
        trigger_index=5,
        signal=CandidateSignal(HIGH, ("format_match", "substantive")),
        lower_bound=None,
        evidence={"format": "conductor-finding", "file": "a.py", "line_start": 24, "line_end": 51},
    )
    assert signal_to_dict(signal) == {
        "kind": "review_comment",
        "detector": "review_comment",
        "session_id": "sess-1",
        "event_index": 7,
        "event_uuid": "u7",
        "occurred_at": "2026-01-01T12:00:00+00:00",
        "text": "guard against None",
        "cc_version": "1.2.3",
        "trigger_index": 5,
        "signal": {"confidence": 0.75, "reasons": ["format_match", "substantive"], "durable": True},
        "lower_bound": None,
        "evidence": {"format": "conductor-finding", "file": "a.py", "line_start": 24, "line_end": 51},
    }


def test_signal_to_dict_round_trips_through_json() -> None:
    signal = MiningSignal(
        kind=REVIEW_COMMENT,
        detector="review_comment",
        session_id=SessionId("sess-1"),
        event_index=0,
        event_uuid=EventUuid("u0"),
        occurred_at=meta("u0").timestamp,
        text="rename",
        cc_version=None,
        trigger_index=None,
        signal=CandidateSignal(MEDIUM, ("format_match",)),
    )
    assert json.loads(json.dumps(signal_to_dict(signal)))["cc_version"] is None
