"""A stub-consuming sample of the v14 public surface, type-checked in CI.

Never executed: `uv run ty check tests/typecheck_sample.py` (and pyright) prove
the ``_native.pyi`` stub and the facades give callers precise types — events
narrow through match, blocks carry typed calls, and the sync entry points
compose. A checker error here is a stub or facade regression.
"""

from __future__ import annotations

from pathlib import Path

from cc_transcript import (
    NOISE_SPEC,
    AssistantEvent,
    BashCall,
    Budget,
    CorrectionLog,
    EditCall,
    EventList,
    Session,
    SessionActivity,
    ToolUseBlock,
    Transcript,
    UserEvent,
    Watcher,
    discover,
    parse,
    parse_tool_call,
    render_tool_call,
    resolve,
    stream,
    tool_digest,
)
from cc_transcript.ids import SessionId


def total_events(paths: list[Path]) -> int:
    return sum(len(transcript.events) for transcript in stream(paths, drop=NOISE_SPEC, prefetch=8))


def transcript_paths(transcript: Transcript) -> tuple[Path | None, float]:
    return transcript.path, transcript.mtime


def first_prompt(path: Path) -> str | None:
    events: EventList = parse(path).events
    for event in events:
        match event:
            case UserEvent(text=text) if text:
                return text
            case AssistantEvent():
                return None
    return None


def bash_commands(transcript: Transcript) -> list[str]:
    return [
        block.call.command
        for event in transcript.events
        if isinstance(event, (UserEvent, AssistantEvent))
        for block in event.blocks
        if isinstance(block, ToolUseBlock) and isinstance(block.call, BashCall)
    ]


def edit_targets(inputs: list[tuple[str, dict[str, object]]]) -> list[str]:
    return [
        call.file_path
        for name, raw in inputs
        if isinstance(call := parse_tool_call(name, raw), EditCall)
    ]


def render_first_call(transcript: Transcript) -> str | None:
    for event in transcript.events:
        if isinstance(event, (UserEvent, AssistantEvent)):
            for block in event.blocks:
                if isinstance(block, ToolUseBlock):
                    return render_tool_call(block.call, budget=Budget(tool_chars=200))
    return None


def session_shape(session_id: SessionId) -> tuple[int, int] | None:
    if resolve(session_id) is None:
        return None
    activity = SessionActivity.from_session(session_id)
    session = Session.from_activity(activity)
    return len(activity.turns), len(session.tool_calls)


def watch_once(roots: list[Path]) -> list[str]:
    return [str(item.session_id) for item in Watcher(roots, from_start=True).tick()]


def digest_and_corrections(session_id: SessionId) -> int:
    digest = tool_digest("Bash", {"command": "ls"})
    return len(CorrectionLog.open().by_digest(session_id, incorrect_digest=digest))


def everything(root: Path) -> int:
    return total_events(discover(root))
