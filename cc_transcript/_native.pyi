from __future__ import annotations

from pathlib import Path
from typing import Any

from cc_transcript.models import PrintResult, TranscriptEvent

class EventList:
    """A lazily-materializing sequence of transcript events over one parse output."""

    def __len__(self) -> int: ...
    def __getitem__(self, index: int | slice, /) -> TranscriptEvent | list[TranscriptEvent]: ...

class Transcript:
    """One parsed transcript file: its path, mtime, and lazily-materialized events."""

    path: Path | None
    mtime: float
    events: EventList

class ParseStream:
    """A streaming handle over a rayon-parsed batch of transcript files."""

    def recv(self) -> Transcript | None:
        """Blocks for the next parsed file, or returns None when drained."""

    def recv_many(self, max: int, /) -> list[Transcript]:
        """Blocks for at least one parsed file, then drains up to ``max``."""

def stream_parse(paths: list[tuple[str, float]], prefetch: int, spec_json: str | None = ..., /) -> ParseStream:
    """Spawns a rayon pool parsing ``paths``, buffering ``prefetch`` results.

    When ``spec_json`` is the JSON of a portable filter spec, events failing it
    are dropped during parsing, before any Python object is built.
    """

def parse_bytes(raw: bytes, spec_json: str | None = None) -> Transcript:
    """Parses raw JSONL transcript bytes into a :class:`Transcript` view.

    When ``spec_json`` is the JSON of a portable filter spec, events failing it
    are dropped during parsing, before any Python object is built.
    """

def parse_print_result(raw: bytes, /) -> PrintResult:
    """Parses a 'claude -p --output-format json' result from raw JSON bytes."""

def cost_of_json(usage_json: str, model: str, /) -> dict[str, float]:
    """Costs a turn's usage JSON under ``model``'s rates (the cost.py cost_of port).

    Returns the ``input_cost``/``output_cost``/``cache_read_cost``/``cache_write_cost``/
    ``total`` breakdown. Raises ``KeyError`` when no pricing family matches ``model``.
    """

def notifications_replay(raw: bytes, /) -> dict[str, list[str]]:
    """Replays the harness notification queue over a transcript's raw JSONL bytes.

    Returns the ``queued``/``delivered``/``enqueued`` string lists of the modeled
    :class:`~cc_transcript.notifications.Notifications` state (the notifications.py port).
    """

def bucket_events(raw: bytes, /) -> list[dict[str, Any]]:
    """Groups a transcript's raw JSONL bytes into scorable sentiment buckets.

    Each dict carries ``session_id``, ``bucket_index``, ``bucket_start_ms`` (epoch
    milliseconds), and ``uuids`` (the member events, in window order) — the
    ConversationBucketer.bucket_events port (cc_transcript.sentiment.buckets).
    """

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

def context_capture_window(
    raw: bytes,
    session_id: str,
    anchor_uuid: str,
    anchor_tool_use_id: str | None,
    before: int,
    after: int,
    preview_chars: int,
    /,
) -> str:
    """Captures the context window around an anchor and returns its ``to_json`` string.

    Mirrors ``cc_transcript.context.capture_window`` over the ``SessionActivity.from_events``
    lift of the transcript's raw JSONL: the anchor is ``EventRef(session_id, anchor_uuid,
    anchor_tool_use_id)``. Raises ``ValueError`` when the anchor does not resolve.
    """

def context_roundtrip(data: str, /) -> str:
    """Deserializes a persisted window and re-serializes it, byte-stably.

    Mirrors ``ContextWindow.from_json(data).to_json()``; raises ``ValueError`` for any
    payload not carrying the ``cc-transcript.context/2`` schema.
    """

def context_render_preview(data: str, turn_chars: int, /) -> str:
    """Renders a persisted window's previews under ``turn_chars``.

    Mirrors ``ContextWindow.from_json(data).render_preview(budget=Budget(turn_chars=...))``;
    summary-fidelity windows lead with the summary-fidelity label.
    """

def query_session(path: str, max_events: int, /) -> dict[str, Any]:
    """Projects the query.py ``Session`` battery over the transcript at ``path``.

    Lifts the first ``max_events`` parsed entries into a ``Session`` (parse, then the
    same year-zero materialization drop and cap as ``activity_lift``) and returns the
    deterministic battery ``scripts/gen_query_golden.py`` freezes: tool-call counts,
    touched/edited files, failures, commands, prompts, ``has_*`` predicates, window
    event-lengths, and per-``FileRef`` ``is_test``/``suffix``. The query-parity substrate.
    """

def tool_facts(paths: list[str], max_events: int, /) -> list[dict[str, Any]]:
    """Projects the facts.py analytics over each transcript in ``paths``.

    Per path (parse, then the same year-zero materialization drop and cap as
    ``activity_lift``, then ``tool_facts``): a dict of ``facts`` (one flattened tool call
    each — session, tool, command prefixes, MCP server/tool/access, file path, error and
    denial state, duration, ``ts_ms``), plus ``command_prefix_counts`` and ``mcp_summary``
    over that file's facts as ordered ``[{...}]`` lists. The facts-parity substrate.
    """

def render_tool_call(name: str, input_json: str, turn_chars: int, tool_chars: int, /) -> str:
    """Renders a tool ``name`` and JSON-encoded input under a budget.

    Mirrors ``cc_transcript.render.render_tool_call`` over ``parse_tool_call(..., on_error="other")``
    with a ``Budget(turn_chars, tool_chars)``.
    """

def render_compact_lines(raw: bytes, width: int, thinking: bool, uuids: bool, /) -> list[str]:
    """Renders one compact line per event of a transcript's raw JSONL bytes.

    Mirrors ``cc_transcript.render.compact_line`` over every parsed event in order, keyed by the
    transcript's own ``tool_names`` join.
    """

def render_haystacks(raw: bytes, wheres: list[str], /) -> list[str]:
    """Renders the search haystack of each event of a transcript's raw JSONL bytes.

    Mirrors ``cc_transcript.render.haystack``; ``wheres`` selects the ``text``/``thinking``/``tools``
    areas (an empty list searches none).
    """

def render_stats(raws: list[bytes], /) -> str:
    """Renders the aggregate statistics block over several transcripts' raw JSONL bytes.

    Mirrors ``cc_transcript.render.render_stats(collect_stats(...))`` over the parsed transcripts.
    """

def discovery_find_transcripts(root: str, /) -> list[str]:
    """Every ``*.jsonl`` transcript under ``root``, sorted by path (``._*`` forks included)."""

def discovery_find_transcript(root: str, session_id: str, /) -> str | None:
    """``session_id``'s newest-mtime real transcript under ``root``, or None (symlink-deduped)."""

def discovery_find_in(
    directory: str,
    name_contains: str | None = ...,
    limit: int | None = ...,
    known_mtimes: dict[str, float] | None = ...,
    /,
) -> list[tuple[str, float]]:
    """``(path, mtime)`` pairs under ``directory`` filtered by name/freshness, sorted by path, capped at ``limit``."""

def discovery_subagent_paths(path: str, /) -> list[str]:
    """The sidechain ``*.jsonl`` files under ``<parent>/<stem>/subagents/``, sorted, skipping ``._*`` forks."""

def discovery_subagent_transcripts(path: str, /) -> dict[str, str]:
    """Sidechain transcripts keyed by the ``agent-<id>`` tool-use id that spawned each."""

def discovery_is_subagent_path(path: str, /) -> bool:
    """Whether ``path`` names an ``agent-<id>.jsonl`` subagent sidechain transcript."""

class WatchTailer:
    """A stateful byte-offset tail over the transcript tree (the Rust ``watch.tick`` port).

    Holds one cursor per discovered ``*.jsonl`` file between calls; the Python facade
    wraps it in the async poll-forever loop.
    """
    def __init__(self) -> None: ...
    def tick(self, roots: list[str], from_start: bool = ..., /) -> list[tuple[str, str, bool, TranscriptEvent]]:
        """Run one poll step over ``roots``, returning ``(path, session_id, is_sidechain, event)``
        tuples for each freshly appended entry."""

    def snapshot(self) -> dict[str, Any]:
        """The whole cursor state: ``{"primed": bool, "cursors": {path: {offset, size, mtime,
        session_id, seen}}}`` — the Python ``TailState`` projection."""

class RustCorrectionLog:
    """The correction-ledger engine over a bundled SQLite (the Rust ``CorrectionLog`` port).

    Mirrors ``cc_transcript.corrections.CorrectionLog`` on the same on-disk format —
    schema, WAL journal mode, ``INSERT OR IGNORE`` append, and ``json.dumps``-parity
    ``detail_json`` — so both engines read and write one ledger file interchangeably.
    Query methods project each row to a ``dataclasses.asdict``-shaped ``Correction`` dict.
    """

    def __init__(self, path: str) -> None:
        """Opens (creating if needed) the ledger at ``path`` in WAL mode."""

    def append(
        self,
        ts_ms: int,
        session_id: str,
        source: str,
        anchor_uuid: str,
        incorrect_digest: str | None,
        incorrect_file: str,
        incorrect_old: str,
        incorrect_new: str,
        correction_origin: str | None,
        correction_file: str | None,
        correction_old: str | None,
        correction_new: str | None,
        correction_commit: str | None,
        correction_text: str | None,
        overlap: float,
        detail: Any,
    ) -> None:
        """Appends one correction (idempotent on the UNIQUE key). ``detail`` is normalized
        with ``dict(detail)`` (raising on a non-mapping) and stored as ``json.dumps``."""

    def for_session(self, session_id: str) -> list[dict[str, Any]]:
        """All records for ``session_id``, ordered by timestamp."""

    def for_repo(self, repo: str) -> list[dict[str, Any]]:
        """All corrections whose ``detail.repo`` is ``repo``, ordered by timestamp."""

    def since(self, ts_ms: int, source: str | None = ...) -> list[dict[str, Any]]:
        """Corrections with ``ts_ms`` strictly greater than ``ts_ms``, oldest first."""

    def for_anchor(self, session_id: str, anchor_uuid: str) -> list[dict[str, Any]]:
        """The corrections harvested around one feedback ``anchor_uuid``."""

    def by_digest(self, session_id: str, incorrect_digest: str) -> list[dict[str, Any]]:
        """Corrections of the tool call with ``incorrect_digest`` in ``session_id``."""

    def sql(self, statement: str) -> list[dict[str, Any]]:
        """Runs a raw SQL ``statement`` — the escape hatch behind ``corrections sql``."""
