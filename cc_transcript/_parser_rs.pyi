from __future__ import annotations

from typing import Any

from cc_transcript.models import PrintResult, TranscriptEvent

class ParseStream:
    """A streaming handle over a rayon-parsed batch of transcript files."""

    def recv(self) -> tuple[str, float, list[TranscriptEvent]] | None:
        """Blocks for the next parsed file, or returns None when drained."""

    def recv_many(self, max: int, /) -> list[tuple[str, float, list[TranscriptEvent]]]:
        """Blocks for at least one parsed file, then drains up to ``max``."""

def stream_parse(paths: list[tuple[str, float]], prefetch: int, spec_json: str | None = ..., /) -> ParseStream:
    """Spawns a rayon pool parsing ``paths``, buffering ``prefetch`` results.

    When ``spec_json`` is the JSON of a portable filter spec, events failing it
    are dropped during parsing, before any Python object is built.
    """

def parse_print_result(raw: bytes, /) -> PrintResult:
    """Parses a 'claude -p --output-format json' result from raw JSON bytes."""

def lexicon_available() -> bool:
    """Whether the UDPipe English model loaded (downloading + caching on first call)."""

def lexicon_polarity(lemma: str, /) -> int:
    """Polarity of a single lemma: domain override, else AFINN floored at MIN_MAGNITUDE."""

def lexicon_has_hit(text: str, floor: int, want_negative: bool, /) -> bool:
    """Whether any token's lemma polarity crosses ``floor`` (``<= -floor`` when ``want_negative``)."""

def lexicon_overrides() -> list[tuple[str, int]]:
    """The embedded domain-override entries (for the single-source drift guard)."""

def embedded_literals() -> dict[str, str | float]:
    """The generated protocol and mining literals keyed ``module.NAME`` (for the single-source drift guard)."""

def score_short_circuit(spec_json: str, buckets: list[list[str]], /) -> list[int | None]:
    """Per bucket, the first short-circuit stage's score over its user texts, else None."""

def score_post_process(spec_json: str, buckets: list[list[str]], raw: list[int], /) -> list[int]:
    """Folds each bucket's raw score through the post-process stages in order."""

def command_prefixes(commands: list[str], /) -> list[list[str]]:
    """Permission-style prefixes per command line, parsed in parallel off the GIL."""

def mine_signals(raw: bytes, spec_json: str, /) -> list[dict[str, Any]]:
    """Parses raw transcript bytes and mines signal dicts per the portable mining spec."""

def session_activity_probe(
    path: str, waiting_tools: list[str] | None = ..., human_facing_tools: list[str] | None = ..., /
) -> dict[str, Any]:
    """Parses the transcript at ``path`` and returns the session-activity verdict dict.

    The dict carries ``is_waiting: bool``, ``mid_tool: bool``, ``last_event_epoch: int | None``,
    and ``pending: list[dict]`` with ``tool_use_id``/``name``/``kind`` per contributing call.
    """
