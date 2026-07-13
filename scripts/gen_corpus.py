"""Deterministic synthetic Claude Code transcript corpus for the v14 baseline harness.

Writes a fixed ``.jsonl`` corpus to ``.fixtures/corpus/`` from a seeded RNG — no
wall clock and no entropy anywhere, so the same ``--seed`` and file plan reproduce
the exact same bytes on every machine. File mtimes are pinned with :func:`os.utime`
so newest-first discovery ordering is golden too. Event shapes are cribbed from the
raw envelopes in ``tests/support.py`` (``fixture_entries``) and the field names the
parser requires (``rust/src/parse.rs`` / ``cc_transcript/models.py``): a coherent mix
of real user prompts, assistant text/thinking/tool-use turns, matching tool-result
turns (Bash/Read/Edit/Write/Grep/Glob/Task), corrections and denials that feed the
mining detectors, sidechains, meta fields, and system events.

Run: ``uv run --no-sync python scripts/gen_corpus.py``
"""

from __future__ import annotations

import argparse
import os
import shutil
from datetime import UTC, datetime, timedelta
from pathlib import Path
from random import Random

import orjson

from cc_transcript.mining import DENIAL_PREFIX

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = REPO_ROOT / ".fixtures" / "corpus"
DEFAULT_SEED = 1417
BASE_TIME = datetime(2026, 1, 6, 9, 0, 0, tzinfo=UTC)
BASE_MTIME = 1_700_000_000  # 2023-11-14T22:13:20Z; +1h per file so ordering is stable.

# (project directory, target bytes). Newest-first discovery keys off pinned mtimes,
# not size; the smallest file is the golden `show`/`tools`/`commands` fixture.
FILE_PLAN: tuple[tuple[str, int], ...] = (
    ("-Users-dev-Code-web-app", 8_000_000),
    ("-Users-dev-Code-web-app", 5_500_000),
    ("-Users-dev-Code-rust-core", 6_500_000),
    ("-Users-dev-Code-rust-core", 2_200_000),
    ("-Users-dev-Code-rust-core", 1_800_000),
    ("-Users-dev-Code-data-pipeline", 2_400_000),
    ("-Users-dev-Code-data-pipeline", 1_500_000),
    ("-Users-dev-Code-data-pipeline", 900_000),
    ("-Users-dev-Code-infra", 1_200_000),
    ("-Users-dev-Code-infra", 700_000),
    ("-Users-dev-Code-infra", 400_000),
    ("-Users-dev-Code-docs", 500_000),
    ("-Users-dev-Code-docs", 300_000),
    ("-Users-dev-Code-scratch", 250_000),
    ("-Users-dev-Code-scratch", 180_000),
    ("-Users-dev-Code-scratch", 120_000),
    ("-Users-dev-Code-scratch", 60_000),
    ("-Users-dev-Code-scratch", 30_000),
    ("-Users-dev-Code-scratch", 12_000),
    ("-Users-dev-Code-scratch", 6_000),
    ("-Users-dev-Code-scratch", 4_000),
)

PROMPTS = (
    "fix the flaky parser test in tests/test_parser.py",
    "add a --json flag to the stats command",
    "why does the mining detector miss short corrections?",
    "refactor the discovery module to cache mtimes",
    "the build is broken on main, can you look into it",
    "implement the context window re-hydration path",
    "write a benchmark for the rust filter fast path",
    "our CI is timing out, profile the docs build",
    "port the sentiment score spec to rust",
    "the CLI grep is slow over large transcripts, speed it up",
    "wire up the corrections ledger to the gate writer",
    "add golden tests for the show command output",
    "investigate the token blowup in the compaction path",
    "make the activity probe handle async tool launches",
)
FOLLOWUPS = (
    "no, that's not right — use pathlib instead of os.path here",
    "that broke the tests, revert the last change",
    "don't hardcode the fixture path, read it from the env",
    "you missed the sidechain case, handle it too",
    "actually keep the old behavior for empty specs",
    "the regex is over-matching, anchor it to the head",
    "stop reformatting unrelated lines, minimal diff please",
    "this still leaks the interpreter path, close the handle",
)
ACKS = ("ok", "go ahead", "yes please", "thanks", "lgtm", "sounds good")
INTERRUPTS = ("[Request interrupted by user]", "[request interrupted by user for tool use]")
ASSISTANT_TEXTS = (
    "Let me read the relevant files to understand the current behavior.",
    "I'll fix that now.",
    "Found the issue — the predicate was applied to the wrong kind.",
    "Running the tests to confirm the change is green.",
    "The root cause is a stale mtime cache; here is the fix.",
    "I'll add a focused test that reproduces the failure first.",
    "Refactoring the helper to take the parent object directly.",
    "That path never re-parses raw events; it consumes the lifted layer.",
)
THINKING_TEXTS = (
    "The filter clause negates KindIs, so non-user events drop first.",
    "I should reproduce the smallest failing case before editing.",
    "This is the pyo3 boundary; the Rust side owns the parse.",
    "Careful — this looks safe to change but the parity suite pins it.",
)
FILE_PATHS = (
    "cc_transcript/parser.py",
    "cc_transcript/filterspec.py",
    "cc_transcript/mining/engine.py",
    "rust/src/parse.rs",
    "rust/src/filter.rs",
    "cc_transcript/discovery.py",
    "tests/test_parser.py",
    "cc_transcript/cli.py",
    "cc_transcript/activity.py",
)
BASH_COMMANDS = (
    ("uv run pytest tests/test_parser.py -x", "run the parser tests"),
    ("cargo test --workspace", "run the rust suite"),
    ("git status --short", "check the working tree"),
    ("uv run ruff check cc_transcript", "lint the package"),
    ("rg -n 'parse_bytes' rust/src", "find the parse entry"),
    ("ls -la cc_transcript/mining", "list the mining module"),
    ("git diff --stat", "review the pending change"),
    ("uv build", "build the wheel"),
    ("cargo bench --no-run", "compile the benches"),
    ("uv run python -m cc_transcript stats", "summarize the transcripts"),
)
GREP_PATTERNS = ("parse_bytes", "session_activity", "MiningSpec", "compile_spec", "TranscriptParser")
SUBAGENT_PROMPTS = (
    "explore how the filter spec is compiled on the rust side",
    "find every call site of collect_stats",
    "map the mining detector pipeline end to end",
)


def make_uuid(rng: Random) -> str:
    bits = rng.getrandbits(128)
    hex_ = f"{bits:032x}"
    return f"{hex_[:8]}-{hex_[8:12]}-4{hex_[13:16]}-{hex_[16:20]}-{hex_[20:]}"


def iso(dt: datetime) -> str:
    return f"{dt:%Y-%m-%dT%H:%M:%S}.{dt.microsecond // 1000:03d}Z"


class Session:
    def __init__(self, rng: Random, session_id: str, start: datetime) -> None:
        self.rng = rng
        self.session_id = session_id
        self.clock = start
        self.parent: str | None = None
        self.model = rng.choice(("claude-opus-4-8", "claude-sonnet-4", "claude-opus-4-7"))

    def tick(self, lo: int = 1, hi: int = 90) -> datetime:
        self.clock += timedelta(seconds=self.rng.randint(lo, hi), milliseconds=self.rng.randint(0, 999))
        return self.clock

    def envelope(self, kind: str, *, sidechain: bool = False, meta: bool = False) -> dict[str, object]:
        uuid = make_uuid(self.rng)
        env: dict[str, object] = {
            "type": kind,
            "uuid": uuid,
            "parentUuid": self.parent,
            "sessionId": self.session_id,
            "timestamp": iso(self.tick()),
            "cwd": "/Users/dev/Code/repo",
            "gitBranch": self.rng.choice(("main", "feature/parser", "fix/mining")),
            "version": "2.1.7",
            "entrypoint": "cli",
        }
        if sidechain:
            env["isSidechain"] = True
        if meta:
            env["isMeta"] = True
        self.parent = uuid
        return env

    def user_text(self, text: str, **fields: object) -> dict[str, object]:
        return self.envelope("user") | {"message": {"role": "user", "content": text}} | fields

    def assistant(
        self, blocks: list[dict[str, object]], *, stop_reason: str, usage: dict[str, object] | None = None
    ) -> dict[str, object]:
        message: dict[str, object] = {
            "role": "assistant",
            "model": self.model,
            "stop_reason": stop_reason,
            "content": blocks,
        }
        if usage is not None:
            message["usage"] = usage
        return self.envelope("assistant") | {"message": message}

    def tool_result(
        self,
        tool_use_id: str,
        content: str,
        *,
        is_error: bool = False,
        payload: object = None,
        denial: str | None = None,
    ) -> dict[str, object]:
        env = self.envelope("user")
        if payload is not None:
            env["toolUseResult"] = payload
        if denial is not None:
            env["toolDenialKind"] = denial
        block = {"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}
        env["message"] = {"role": "user", "content": [block]}
        return env


def tool_call(rng: Random, tool_use_id: str) -> tuple[dict[str, object], str, object]:
    """Returns an assistant tool_use block plus the matching result content and toolUseResult."""
    match rng.choice(("Bash", "Read", "Edit", "Write", "Grep", "Glob", "Task")):
        case "Bash":
            cmd, desc = rng.choice(BASH_COMMANDS)
            out = rng.choice(("ok", "1 passed", "no changes", "42 files", "done in 3.1s"))
            payload = {
                "stdout": out,
                "stderr": "",
                "interrupted": False,
                "isImage": False,
                "returnCodeInterpretation": "Command exited with code 0",
            }
            return (
                {"type": "tool_use", "id": tool_use_id, "name": "Bash", "input": {"command": cmd, "description": desc}},
                out,
                payload,
            )
        case "Read":
            path = rng.choice(FILE_PATHS)
            content = "\n".join(f"{n:>4}\tline {n} of {path}" for n in range(1, rng.randint(8, 40)))
            return (
                {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Read",
                    "input": {"file_path": path, "limit": rng.randint(50, 400)},
                },
                content,
                None,
            )
        case "Edit":
            path = rng.choice(FILE_PATHS)
            old, new = "return None", "return result"
            payload = {
                "filePath": path,
                "oldString": old,
                "newString": new,
                "replaceAll": False,
                "userModified": False,
                "structuredPatch": [
                    {"oldStart": 12, "oldLines": 1, "newStart": 12, "newLines": 1, "lines": [f"-{old}", f"+{new}"]}
                ],
                "originalFile": f"{old}\n",
            }
            return (
                {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Edit",
                    "input": {"file_path": path, "old_string": old, "new_string": new},
                },
                "ok",
                payload,
            )
        case "Write":
            path = rng.choice(FILE_PATHS)
            body = "\n".join(f"# generated line {n}" for n in range(rng.randint(4, 20)))
            return (
                {"type": "tool_use", "id": tool_use_id, "name": "Write", "input": {"file_path": path, "content": body}},
                f"wrote {path}",
                None,
            )
        case "Grep":
            pat = rng.choice(GREP_PATTERNS)
            hits = "\n".join(f"{rng.choice(FILE_PATHS)}:{rng.randint(1, 300)}:{pat}" for _ in range(rng.randint(1, 6)))
            return (
                {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Grep",
                    "input": {"pattern": pat, "output_mode": "content"},
                },
                hits,
                None,
            )
        case "Glob":
            return (
                {"type": "tool_use", "id": tool_use_id, "name": "Glob", "input": {"pattern": "**/*.py"}},
                "\n".join(FILE_PATHS),
                None,
            )
        case _:
            prompt = rng.choice(SUBAGENT_PROMPTS)
            text = "Investigated and found the answer in the parse layer."
            payload = {
                "agentId": f"agent-{tool_use_id[-4:]}",
                "agentType": "Explore",
                "status": "completed",
                "totalDurationMs": rng.randint(2000, 40000),
                "totalTokens": rng.randint(500, 9000),
                "totalToolUseCount": rng.randint(2, 12),
                "toolStats": {"Read": rng.randint(1, 8), "Grep": rng.randint(1, 5)},
                "usage": {"input_tokens": rng.randint(100, 5000), "output_tokens": rng.randint(50, 900)},
                "content": [{"type": "text", "text": text}],
                "prompt": prompt,
                "resolvedModel": "claude-opus-4-8",
            }
            return (
                {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Task",
                    "input": {"description": "explore", "prompt": prompt, "subagent_type": "Explore"},
                },
                text,
                payload,
            )


def usage_block(rng: Random) -> dict[str, object]:
    return {
        "input_tokens": rng.randint(200, 8000),
        "output_tokens": rng.randint(40, 1200),
        "cache_read_input_tokens": rng.randint(0, 60000),
        "cache_creation_input_tokens": rng.randint(0, 30000),
        "service_tier": "standard",
    }


def turn(session: Session, rng: Random) -> list[dict[str, object]]:
    """One assistant turn plus its follow-on user/tool/system events."""
    blocks: list[dict[str, object]] = [{"type": "text", "text": rng.choice(ASSISTANT_TEXTS)}]
    if rng.random() < 0.5:
        blocks.append({"type": "thinking", "thinking": rng.choice(THINKING_TEXTS)})
    calls: list[tuple[str, dict[str, object], str, object]] = []
    for _ in range(rng.randint(1, 2)):
        tid = f"toolu_{make_uuid(rng).replace('-', '')[:20]}"
        block, content, payload = tool_call(rng, tid)
        calls.append((tid, block, content, payload))
    blocks.extend(block for _, block, _, _ in calls)
    events = [session.assistant(blocks, stop_reason="tool_use")]
    for tid, _, content, payload in calls:
        if rng.random() < 0.08:
            reason = f"{DENIAL_PREFIX}. {rng.choice(FOLLOWUPS)}"
            events.append(session.tool_result(tid, reason, is_error=True, payload=reason, denial="user-rejected"))
        else:
            events.append(session.tool_result(tid, content, payload=payload))
    return events


def session_events(session: Session, rng: Random) -> list[dict[str, object]]:
    events: list[dict[str, object]] = [session.user_text(rng.choice(PROMPTS))]
    events.extend(turn(session, rng))
    tail_choice = rng.random()
    if tail_choice < 0.35:
        events.append(session.user_text(rng.choice(FOLLOWUPS)))
        events.extend(turn(session, rng))
    elif tail_choice < 0.5:
        events.append(session.user_text(rng.choice(INTERRUPTS)))
    if rng.random() < 0.25:
        events.append(
            session.envelope("system")
            | {"subtype": "turn_duration", "durationMs": rng.randint(2000, 60000), "messageCount": rng.randint(3, 30)}
        )
    if rng.random() < 0.12:
        sc = session.envelope("assistant", sidechain=True)
        sc["message"] = {
            "role": "assistant",
            "model": session.model,
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "subagent progress note"}],
        }
        events.append(sc)
    if rng.random() < 0.15:
        events.append(session.user_text(rng.choice(ACKS)))
    events.append(
        session.assistant(
            [{"type": "text", "text": "All done — the change is in and the tests are green."}],
            stop_reason="end_turn",
            usage=usage_block(rng),
        )
    )
    return events


def generate_file(rng: Random, session_id: str, start: datetime, target_bytes: int) -> bytes:
    lines: list[bytes] = []
    size = 0
    while size < target_bytes:
        session = Session(rng, session_id, start)
        for event in session_events(session, rng):
            line = orjson.dumps(event)
            lines.append(line)
            size += len(line) + 1
        start = session.clock + timedelta(minutes=rng.randint(5, 240))
    return b"\n".join(lines) + b"\n"


def generate(out: Path, seed: int) -> tuple[int, int]:
    if out.exists():
        shutil.rmtree(out)
    rng = Random(seed)
    total_bytes = 0
    for index, (project, target) in enumerate(FILE_PLAN):
        session_id = make_uuid(rng)
        start = BASE_TIME + timedelta(days=index)
        data = generate_file(rng, session_id, start, target)
        path = out / project / f"{session_id}.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        mtime = BASE_MTIME + index * 3600
        os.utime(path, (mtime, mtime))
        total_bytes += len(data)
    return len(FILE_PLAN), total_bytes


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate the deterministic baseline transcript corpus.")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="Corpus output directory.")
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED, help="RNG seed.")
    args = parser.parse_args()
    files, total = generate(args.out, args.seed)
    print(f"wrote {files} transcripts, {total} bytes ({total / 1024 / 1024:.1f} MiB) to {args.out} (seed={args.seed})")


if __name__ == "__main__":
    main()
