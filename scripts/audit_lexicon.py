"""Re-runnable audit of the negation upgrade to ``Lexicon.has_hit``.

Samples real conversation buckets from ``~/.claude/projects`` (bounded, fixed seed)
and reports how the negation-aware ``has_hit`` reclassifies buckets as hostile/positive
versus the pre-change surface-only rule. The tokenizer is held constant (the embedded
UDPipe model on both sides), so every delta reported here is attributable to the
negation sign-flip alone — there is no POS gate — not the tokenizer swap.

Run: ``uv run --no-sync python scripts/audit_lexicon.py [max_transcripts] [max_buckets]``
"""

from __future__ import annotations

import random
import sys
from collections import Counter
from dataclasses import dataclass

import anyio

from cc_transcript import parse_events_from_bytes
from cc_transcript.discovery import TranscriptDiscovery
from cc_transcript.models import UserEvent
from cc_transcript.sentiment import bucket_events
from cc_transcript.sentiment.lexicon import Lexicon

SEED = 20260712
FLOOR = Lexicon.FLOOR


@dataclass(frozen=True, slots=True)
class Axis:
    surface: bool
    signed: bool


def classify(text: str) -> tuple[Axis, Axis, list[str]]:
    from cc_transcript.nlp import analyze

    tokens = analyze(text)
    surf_neg = any(t.polarity <= -FLOOR for t in tokens)
    surf_pos = any(t.polarity >= FLOOR for t in tokens)
    effective = [(t, -t.polarity if t.negated else t.polarity) for t in tokens]
    signed_neg = any(e <= -FLOOR for _, e in effective)
    signed_pos = any(e >= FLOOR for _, e in effective)
    drivers = [f"{t.form}({t.upos},{'¬' if t.negated else ''}{t.polarity})" for t, e in effective if abs(e) >= FLOOR]
    return Axis(surf_neg, signed_neg), Axis(surf_pos, signed_pos), drivers


def bucket_texts(events: list) -> list[str]:  # noqa: ANN001
    return [e.text for e in events if isinstance(e, UserEvent) and e.text and len(e.text) >= 5]


async def collect(max_transcripts: int, max_buckets: int) -> list[list[str]]:
    paths = await TranscriptDiscovery.find_transcripts()
    random.Random(SEED).shuffle(paths)
    out: list[list[str]] = []
    for path in paths[:max_transcripts]:
        try:
            events = parse_events_from_bytes(path.read_bytes())
        except (OSError, ValueError):
            continue
        for bucket in bucket_events(events):
            if texts := bucket_texts(list(bucket.events)):
                out.append(texts)
                if len(out) >= max_buckets:
                    return out
    return out


def audit(buckets: list[list[str]]) -> None:
    flips: Counter[str] = Counter()
    examples: dict[str, list[str]] = {k: [] for k in ("neg_gained", "neg_lost", "pos_gained", "pos_lost")}
    for texts in buckets:
        classified = [classify(t) for t in texts]
        old_neg = any(neg.surface for neg, _pos, _d in classified)
        new_neg = any(neg.signed for neg, _pos, _d in classified)
        old_pos = any(pos.surface for _neg, pos, _d in classified)
        new_pos = any(pos.signed for _neg, pos, _d in classified)
        for label, changed in (
            ("neg_gained", not old_neg and new_neg),
            ("neg_lost", old_neg and not new_neg),
            ("pos_gained", not old_pos and new_pos),
            ("pos_lost", old_pos and not new_pos),
        ):
            if changed:
                flips[label] += 1
                if len(examples[label]) < 5:
                    text, drivers = next(
                        ((t, d) for t, (_n, _p, d) in zip(texts, classified, strict=True) if d), (texts[0], [])
                    )
                    examples[label].append(f"    {text[:90]!r}  drivers={drivers}")

    total = len(buckets)
    changed = sum(flips.values())
    print(f"buckets sampled: {total}")
    print(f"buckets with any hostile/positive reclassification: {changed} ({changed / max(total, 1):.1%})")
    for label in ("neg_lost", "neg_gained", "pos_lost", "pos_gained"):
        print(f"\n{label}: {flips[label]}")
        for line in examples[label]:
            print(line)


def main() -> None:
    max_transcripts = int(sys.argv[1]) if len(sys.argv) > 1 else 300
    max_buckets = int(sys.argv[2]) if len(sys.argv) > 2 else 5000
    audit(anyio.run(collect, max_transcripts, max_buckets))


if __name__ == "__main__":
    main()
