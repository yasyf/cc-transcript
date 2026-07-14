"""Freeze the Python ``cost_of`` breakdown into ``tests/testdata/cost_golden.json``.

Serializes the per-component USD cost for a battery of ``(usage, model)`` cases —
every distinct assistant ``message.usage`` block in the deterministic bench corpus
(``.fixtures/corpus``, regenerated via ``scripts/gen_corpus.py``) plus hand-built
edge cases (each pricing family with a per-TTL cache split, the flat cache-write
fallback, zero usage, a non-standard service tier, substring model resolution). A
later run plus ``git diff`` shows Python-side drift, and
``tests/test_cost_parity.py`` asserts the Rust ``cost_of_json`` port reproduces the
same breakdown and that the frozen golden still matches the Python reference.

Run: ``uv run --no-sync python scripts/gen_cost_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
from dataclasses import asdict
from typing import TYPE_CHECKING

import orjson

from cc_transcript.cost import cost_of
from cc_transcript.models import AssistantEvent
from cc_transcript.parser import parse_events_from_bytes
from scripts.gen_corpus import DEFAULT_OUT as CORPUS
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from cc_transcript.models import Usage

GOLDEN = REPO_ROOT / "tests" / "testdata" / "cost_golden.json"

# The corpus mints a fresh random usage block per turn, so cap the distinct sample —
# a bounded fuzz over realistic magnitudes; the edge cases pin every rate exactly.
CORPUS_SAMPLE = 100

# Distinct nonzero components per family reveal every rate; the doc comment lists the shapes.
EDGE_CASES: tuple[tuple[str, dict[str, object], str], ...] = (
    (
        "fable-per-ttl",
        {
            "input_tokens": 1_000_000,
            "output_tokens": 3_000_000,
            "cache_read_input_tokens": 7_000_000,
            "cache_creation_input_tokens": 99,
            "cache_creation": {"ephemeral_5m_input_tokens": 11_000_000, "ephemeral_1h_input_tokens": 13_000_000},
        },
        "claude-fable-5",
    ),
    (
        "opus-per-ttl",
        {
            "input_tokens": 2_000_000,
            "output_tokens": 4_000_000,
            "cache_read_input_tokens": 6_000_000,
            "cache_creation_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 8_000_000, "ephemeral_1h_input_tokens": 5_000_000},
        },
        "claude-opus-4-8",
    ),
    (
        "sonnet-per-ttl",
        {
            "input_tokens": 3_000_000,
            "output_tokens": 5_000_000,
            "cache_read_input_tokens": 9_000_000,
            "cache_creation_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 12_000_000, "ephemeral_1h_input_tokens": 7_000_000},
        },
        "claude-sonnet-4-5",
    ),
    (
        "haiku-per-ttl",
        {
            "input_tokens": 4_000_000,
            "output_tokens": 6_000_000,
            "cache_read_input_tokens": 10_000_000,
            "cache_creation_input_tokens": 0,
            "cache_creation": {"ephemeral_5m_input_tokens": 14_000_000, "ephemeral_1h_input_tokens": 9_000_000},
        },
        "claude-haiku-4-5-20251001",
    ),
    (
        "flat-cache-fallback",
        {
            "input_tokens": 500_000,
            "output_tokens": 250_000,
            "cache_read_input_tokens": 1_000_000,
            "cache_creation_input_tokens": 8_000_000,
        },
        "claude-opus-4-8",
    ),
    (
        "zero-usage",
        {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        },
        "claude-opus-4-8",
    ),
    (
        "service-tier-ignored",
        {
            "input_tokens": 1_234_567,
            "output_tokens": 89_012,
            "cache_read_input_tokens": 3_456,
            "cache_creation_input_tokens": 7_890,
            "service_tier": "batch",
        },
        "claude-sonnet-4-5",
    ),
    (
        "haiku-substring",
        {
            "input_tokens": 1_000_000,
            "output_tokens": 1_000_000,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        },
        "claude-haiku-4-5-20251001",
    ),
)


def usage_from_dict(usage: dict[str, object]) -> Usage:
    line = orjson.dumps(
        {
            "type": "assistant",
            "uuid": "u",
            "parentUuid": None,
            "sessionId": "s",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "/repo",
            "gitBranch": "main",
            "version": "1",
            "entrypoint": "cli",
            "message": {"role": "assistant", "model": "m", "content": [], "usage": usage},
        }
    )
    (event,) = parse_events_from_bytes(line)
    assert isinstance(event, AssistantEvent) and event.usage is not None
    return event.usage


def corpus_usages() -> list[tuple[str, dict[str, object], str]]:
    seen: dict[tuple[bytes, str], tuple[dict[str, object], str]] = {}
    for path in sorted(CORPUS.rglob("*.jsonl")):
        for raw in path.read_bytes().splitlines():
            message = orjson.loads(raw).get("message") if raw else None
            usage = message.get("usage") if isinstance(message, dict) else None
            model = message.get("model") if isinstance(message, dict) else None
            if isinstance(usage, dict) and isinstance(model, str):
                seen.setdefault((orjson.dumps(usage, option=orjson.OPT_SORT_KEYS), model), (usage, model))
    return [
        (f"corpus-{index:03d}", usage, model)
        for index, (usage, model) in enumerate(list(seen.values())[:CORPUS_SAMPLE])
    ]


def collect() -> list[tuple[str, dict[str, object], str]]:
    return corpus_usages() + [(f"edge-{name}", usage, model) for name, usage, model in EDGE_CASES]


def main() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)
    data = [
        {
            "id": cid,
            "usage": usage,
            "model": model,
            "breakdown": asdict(cost_of(usage_from_dict(usage), model)),
        }
        for cid, usage, model in collect()
    ]
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} golden cost breakdowns to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
