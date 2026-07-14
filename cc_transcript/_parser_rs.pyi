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

def lexicon_tokenize(text: str, /) -> list[str]:
    """Split ``text`` into lowercased maximal runs of alphabetic characters (the shared tokenizer)."""

def lexicon_polarity(token: str, /) -> int:
    """Polarity of a single token surface: domain override, else AFINN floored at MIN_MAGNITUDE."""

def lexicon_has_hit(text: str, want_negative: bool, /) -> bool:
    """Whether any content token's effective (negation-flipped) polarity crosses the fixed ±3 floor."""

def lexicon_overrides() -> list[tuple[str, int]]:
    """The embedded domain-override entries (for the single-source drift guard)."""

def nlp_analyze(text: str, /) -> list[tuple[str, str, str, str, int, int, int, bool]]:
    """Analyze ``text`` with the embedded UDPipe model.

    Per token: ``(form, lower, lemma, upos, char_start, char_end, polarity, negated)``,
    where offsets are codepoint indices and polarity is the surface-keyed lexicon score.
    """

def embedded_literals() -> dict[str, str | float | list[str]]:
    """The generated protocol, mining, and command literals keyed ``module.NAME`` (for the single-source drift guard)."""

def score_short_circuit(spec_json: str, buckets: list[list[str]], /) -> list[int | None]:
    """Per bucket, the first short-circuit stage's score over its user texts, else None."""

def score_post_process(spec_json: str, buckets: list[list[str]], raw: list[int], /) -> list[int]:
    """Folds each bucket's raw score through the post-process stages in order."""

def command_prefixes(commands: list[str], /) -> list[list[str]]:
    """Permission-style prefixes per command line, parsed in parallel off the GIL."""

def command_parse(command: str, /) -> dict[str, Any]:
    """Parses one bash command line into a serializable ``CommandLine`` structure.

    The dict carries ``raw``, ``prefixes: list[str]``, and ``parts: list[dict]`` where
    each part has ``op: str | None`` and a ``command`` dict of ``raw``/``executable``/
    ``args``/``env``/``redirects``/``program``/``unwrapped_argv``/``prefix``.
    """

def mine_events(
    events: list[TranscriptEvent], spec_json: str, callable_formats: list[tuple[str, Any, Any]], /
) -> list[dict[str, Any]]:
    """Mines signal dicts from parsed transcript events per the portable mining spec."""

def ids_canonical_json(value_json: str, /) -> str:
    """RFC 8785 canonical JSON of the value parsed from ``value_json`` (the Rust ids port)."""

def ids_tool_digest(name: str, input_json: str, /) -> str:
    """The cross-language tool-content digest for ``name`` over the input parsed from ``input_json``."""

def session_activity_probe(
    path: str, waiting_tools: list[str] | None = ..., human_facing_tools: list[str] | None = ..., /
) -> dict[str, Any]:
    """Parses the transcript at ``path`` and returns the session-activity verdict dict.

    The dict carries ``is_waiting: bool``, ``mid_tool: bool``, ``last_event_epoch: int | None``,
    and ``pending: list[dict]`` with ``tool_use_id``/``name``/``kind`` per contributing call.
    """

def toolcall_parse(name: str, input_json: str, on_error: str | None = ..., /) -> dict[str, Any]:
    """Parses a tool ``name`` and JSON-encoded input into the projected typed-call dict.

    Mirrors ``cc_transcript.tools.parse_tool_call``: ``on_error`` is ``"raise"`` (default)
    or ``"other"``. The dict carries ``cls`` (the Python dataclass name) plus each public
    field; it is the parity projection, not a live ``ToolCall``.
    """

def toolresult_parse(name: str, payload_json: str, /) -> dict[str, Any]:
    """Parses a tool ``name`` and JSON-encoded ``toolUseResult`` into the projected result dict.

    Mirrors ``cc_transcript.tools.parse_tool_result``: a JSON string payload is a
    ``TextResult``, an object dispatches by tool name, anything else is an ``OtherResult``.
    """

def activity_lift(path: str, max_events: int, /) -> dict[str, Any]:
    """Lifts the transcript at ``path`` into the ``SessionActivity.from_events`` projection.

    Lifts over the first ``max_events`` parsed entries and returns ``turn_count`` plus
    ``turns`` (each ``index``/``prompt``/``started_at_ms``/``ended_at_ms``/``event_count``
    with ``tool_uses`` and ``edits``), a whole-window ``result_index``, and the same-file
    ``hunk_overlaps`` — every timestamp an epoch-millisecond int. The lift-parity substrate.
    """

def activity_hunk_overlap(a_old: str, a_new: str, b_old: str, b_new: str, /) -> float:
    """The activity.py ``hunk_overlap`` of two ``Hunk``s: the fraction of ``a_new``'s
    non-empty normalized lines present in ``b_old``."""
