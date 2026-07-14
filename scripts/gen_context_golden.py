"""Generate the durable-context parity golden from the Python reference.

Freezes ``cc_transcript.context`` outputs so ``tests/test_context_parity.py`` can
replay them against both the Python reference (a drift guard) and the Rust
``_parser_rs`` port. Sections:

* ``captures`` — ``capture_window(...).to_json()`` over synthesized transcripts (and
  the smallest corpus files when present), each at several ``(before, after,
  preview_chars)`` budgets, plus ``render_preview`` at a few turn budgets. Raw JSONL
  is embedded base64, so the test never needs the gitignored corpus.
* ``windows`` — hand-built windows (summary fidelity, null trigger, empty refs) whose
  ``to_json`` must round-trip byte-stably and whose ``render_preview`` must match.
* ``rejects`` — payloads ``from_json`` must reject with ``SchemaError``.

Run: ``uv run --no-sync python scripts/gen_context_golden.py``
"""

from __future__ import annotations

import base64
import json
import sys
from dataclasses import replace
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from cc_transcript.activity import SessionActivity  # noqa: E402
from cc_transcript.context import ContextWindow, TurnRef, capture_window  # noqa: E402
from cc_transcript.filterspec import event_meta  # noqa: E402
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId  # noqa: E402
from cc_transcript.parser import parse_events_from_bytes  # noqa: E402
from cc_transcript.render import Budget  # noqa: E402

CORPUS = REPO_ROOT / ".fixtures" / "corpus"
GOLDEN = REPO_ROOT / "tests" / "testdata" / "context_golden.json"
CORPUS_SAMPLE = 3

SID = "22222222-2222-2222-2222-222222222222"
UNICODE = "héllo 🤖 漢字 café́ — the quick brown fox jumps over the lazy dog again and again"
LONG = "o" * 120

BUDGETS: tuple[tuple[int, int, int], ...] = ((6, 2, 200), (2, 1, 50), (3, 0, 24), (1, 1, 5), (0, 0, 200))
PREVIEW_BUDGETS: tuple[int, ...] = (200, 24, 5)

REJECTS: tuple[str, ...] = ("[]", '{"schema":"cc-transcript.context/3"}', '{"anchor":null}', "{}")


def line(uuid: str, secs: int, kind: str, message: dict, sid: str = SID, parent: str | None = None, **extra: object) -> str:
    return json.dumps(
        {
            "type": kind,
            "uuid": uuid,
            "parentUuid": parent,
            "sessionId": sid,
            "timestamp": f"2026-02-01T09:00:{secs:02d}.000Z",
            "message": message,
        }
        | extra
    )


def user_line(uuid: str, secs: int, text: str, sid: str = SID) -> str:
    return line(uuid, secs, "user", {"role": "user", "content": text}, sid)


def assistant_line(uuid: str, secs: int, content: list[dict], sid: str = SID) -> str:
    return line(uuid, secs, "assistant", {"role": "assistant", "model": "claude-opus-4-8", "content": content}, sid)


def basic_transcript() -> bytes:
    lines = [
        user_line("u0", 0, "one"),
        assistant_line(
            "a0",
            1,
            [
                {"type": "text", "text": "working"},
                {"type": "tool_use", "id": "t1", "name": "Edit", "input": {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"}},
            ],
        ),
        user_line("u1", 2, "two"),
        assistant_line("a1", 3, [{"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "uv run pytest"}}]),
        user_line("u2", 4, "three"),
        assistant_line("a2", 5, [{"type": "tool_use", "id": "t3", "name": "Edit", "input": {"file_path": "/a.py", "old_string": LONG, "new_string": "n" * 120}}]),
        user_line("u3", 6, "four"),
        assistant_line("a3", 7, [{"type": "text", "text": "done"}]),
    ]
    return "\n".join(lines).encode("utf-8")


def leading_preamble_transcript() -> bytes:
    # A turn-0 (assistant events before the first real prompt) plus unicode clipped
    # by code point, and an empty-prompt assistant-only turn is exercised by the
    # leading assistant block.
    lines = [
        assistant_line("p0", 0, [{"type": "text", "text": "preamble before any prompt"}]),
        user_line("u0", 1, UNICODE),
        assistant_line(
            "a0",
            2,
            [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "text", "text": UNICODE},
                {"type": "tool_use", "id": "g1", "name": "Grep", "input": {"pattern": "parse_bytes", "output_mode": "content"}},
            ],
        ),
        user_line("u1", 3, "next"),
        assistant_line("a1", 4, [{"type": "text", "text": "   "}, {"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": "/w.py", "content": "# " + LONG}}]),
    ]
    return "\n".join(lines).encode("utf-8")


def ask_transcript() -> bytes:
    questions = [
        {"question": "Which adapter?", "header": "Adapter", "multiSelect": False, "options": [{"label": "Storage (Recommended)"}, {"label": "Memory"}]},
        {"question": "Name the contexts?", "header": "Names", "multiSelect": True, "options": [{"label": "BeforeEdit"}, {"label": "AfterEdit"}]},
    ]
    result = {
        "questions": questions,
        "answers": {"Which adapter?": "Storage (Recommended)", "Name the contexts?": "BeforeEdit, AfterEdit"},
        "annotations": {"Which adapter?": {"preview": "Storage (Recommended)", "notes": "and never use the memory one again"}},
    }
    lines = [
        user_line("u0", 0, "set up the adapter"),
        assistant_line("a0", 1, [{"type": "text", "text": "asking"}, {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {"questions": questions}}]),
        line(
            "u1",
            2,
            "user",
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "q1", "content": "answered", "is_error": False}]},
            toolUseResult=result,
        ),
        user_line("u2", 3, "thanks"),
        assistant_line("a2", 4, [{"type": "text", "text": "done"}]),
    ]
    return "\n".join(lines).encode("utf-8")


def anchors(activity: SessionActivity) -> list[tuple[str, str | None]]:
    seen: set[tuple[str, str | None]] = set()
    picks: list[tuple[str, str | None]] = []
    for turn in activity.turns:
        for event in turn.events[:1]:
            if (meta := event_meta(event)) is not None and (key := (str(meta.uuid), None)) not in seen:
                seen.add(key)
                picks.append(key)
        for use in turn.tool_uses[:1]:
            key = (str(use.ref.event_uuid), None if use.ref.tool_use_id is None else str(use.ref.tool_use_id))
            if key not in seen:
                seen.add(key)
                picks.append(key)
    return picks


def anchor_ref(sid: str, uuid: str, tool_use_id: str | None) -> EventRef:
    return EventRef(SessionId(sid), EventUuid(uuid), None if tool_use_id is None else ToolUseId(tool_use_id))


def capture_cases(sid: str, activity: SessionActivity) -> list[dict]:
    cases: list[dict] = []
    for uuid, tool_use_id in anchors(activity):
        for before, after, preview_chars in BUDGETS:
            try:
                window = capture_window(activity, anchor_ref(sid, uuid, tool_use_id), before=before, after=after, preview_chars=preview_chars)
            except (ValueError, OverflowError):
                continue
            cases.append(
                {
                    "anchor_uuid": uuid,
                    "anchor_tool_use_id": tool_use_id,
                    "before": before,
                    "after": after,
                    "preview_chars": preview_chars,
                    "to_json": window.to_json(),
                    "previews": [
                        {"turn_chars": tc, "expected": window.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc))}
                        for tc in PREVIEW_BUDGETS
                    ],
                }
            )
    return cases


def capture_section(id_: str, raw: bytes, sid: str) -> dict:
    activity = SessionActivity.from_events(SessionId(sid), parse_events_from_bytes(raw))
    return {"id": id_, "jsonl_b64": base64.b64encode(raw).decode("ascii"), "session_id": sid, "cases": capture_cases(sid, activity)}


def window_section(window: ContextWindow) -> dict:
    data = window.to_json()
    return {
        "to_json": data,
        "previews": [
            {"turn_chars": tc, "expected": window.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc))}
            for tc in PREVIEW_BUDGETS
        ],
    }


def hand_built_windows() -> list[dict]:
    activity = SessionActivity.from_events(SessionId(SID), parse_events_from_bytes(basic_transcript()))
    base = capture_window(activity, anchor_ref(SID, "a2", "t3"), before=2, after=1, preview_chars=50)
    return [
        window_section(replace(base, fidelity="summary")),
        window_section(
            ContextWindow(
                anchor=anchor_ref(SID, "a2", "t3"),
                before=(TurnRef(role="user", refs=(), preview="converted prose", tool_digests=()),),
                trigger=None,
                after=(),
                fidelity="summary",
                preview_chars=200,
            )
        ),
        window_section(
            ContextWindow(anchor=anchor_ref(SID, "z", None), before=(), trigger=None, after=(), fidelity="full", preview_chars=0)
        ),
    ]


def corpus_sections() -> list[dict]:
    if not CORPUS.exists():
        return []
    sections: list[dict] = []
    for path in sorted(CORPUS.rglob("*.jsonl"), key=lambda p: (p.stat().st_size, str(p)))[:CORPUS_SAMPLE]:
        raw = path.read_bytes()
        events = parse_events_from_bytes(raw)
        sid = next((str(meta.session_id) for event in events if (meta := event_meta(event)) is not None), None)
        if sid is None:
            continue
        sections.append(capture_section(str(path.relative_to(REPO_ROOT)), raw, sid))
    return sections


def main() -> None:
    captures = [
        capture_section("basic", basic_transcript(), SID),
        capture_section("preamble", leading_preamble_transcript(), SID),
        capture_section("ask", ask_transcript(), SID),
        *corpus_sections(),
    ]
    golden = {"captures": captures, "windows": hand_built_windows(), "rejects": list(REJECTS)}
    GOLDEN.write_text(json.dumps(golden, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"wrote {sum(len(c['cases']) for c in captures)} capture cases across {len(captures)} transcripts to {GOLDEN.name}")


if __name__ == "__main__":
    main()
