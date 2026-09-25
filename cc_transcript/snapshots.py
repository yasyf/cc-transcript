"""One owner for immutable transcript generations and bounded projections."""

from __future__ import annotations

from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import asdict
from math import isfinite
from typing import Any, Literal, TypedDict, overload

import orjson

from cc_transcript import _native
from cc_transcript.activity import SessionActivity, ToolUse, Turn
from cc_transcript.context import ContextWindow
from cc_transcript.ids import EventRef, SessionId
from cc_transcript.models import TranscriptEvent, UserEvent
from cc_transcript.query import FileRef, PredicateInputs


class UserAuthority(TypedDict):
    kind: Literal["user"]
    effective_uid: str


class RestrictedAuthority(TypedDict):
    kind: Literal["restricted_roots"]
    effective_uid: str
    roots: list[str]


class CallContext(TypedDict):
    claimant: str
    admission: Literal["hook", "review"]
    authority: UserAuthority | RestrictedAuthority
    registry_generation: str


class SourceFacts(TypedDict):
    """Source metadata and an exact match against the first user message."""

    cwds: list[str]
    first_user_contains: bool


class ProseRow(TypedDict):
    """Logical message text and flags without the event's tool payloads."""

    event_index: int
    role: Literal["user", "assistant"]
    text: str
    is_sidechain: bool
    is_meta: bool


class SnapshotIncomplete(RuntimeError):
    """A bounded operation did not establish a complete answer."""

    def __init__(
        self,
        status: str,
        reason: str,
        *,
        usage: Mapping[str, int] | None = None,
        work: Mapping[str, int] | None = None,
    ) -> None:
        super().__init__(status, reason)
        self.status = status
        self.reason = reason
        self.usage = dict(usage or {})
        self.work = dict(work or {})


def _wire(value: Any) -> Any:
    if isinstance(value, float):
        if not isfinite(value):
            raise ValueError("snapshot metadata requires finite numbers")
        return int(value) if value.is_integer() else value
    if isinstance(value, Mapping):
        return {key: _wire(item) for key, item in value.items()}
    if isinstance(value, list | tuple):
        return [_wire(item) for item in value]
    return value


def _json(value: object) -> str:
    return orjson.dumps(_wire(value)).decode()


def _call(function: Callable[..., Any], *args: Any) -> Any:
    try:
        return function(*args)
    except _native.SnapshotOperationError as error:
        if len(error.args) < 2:
            raise
        status, reason, *details = error.args
        usage = details[0] if details else {}
        work = details[1] if len(details) > 1 else {}
        if isinstance(usage, str):
            usage = orjson.loads(usage)
        if isinstance(work, str):
            work = orjson.loads(work)
        raise SnapshotIncomplete(status, reason, usage=usage, work=work) from error


class CancellationToken:
    """Cancellation belongs to one request's waiter, not its shared load."""

    def __init__(self) -> None:
        self._native = _native.SnapshotCancellation()

    def cancel(self) -> None:
        self._native.cancel()


def _tool(payload: Mapping[str, Any]) -> ToolUse:
    return ToolUse(
        ref=EventRef(**payload["ref"]),
        call=payload["call"],
        result=payload["result"],
        result_ts=payload["result_ts"],
        edits=payload["edits"],
        turn_index=payload["turn_index"],
        ts=payload["ts"],
    )


def _turn(payload: Mapping[str, Any]) -> Turn:
    return Turn(
        index=payload["index"],
        prompt=payload["prompt"],
        started_at=payload["started_at"],
        ended_at=payload["ended_at"],
        events=payload["events"],
        tool_uses=tuple(_tool(tool) for tool in payload["tool_uses"]),
    )


def decode_projection(
    record_schema: str,
    records_json: Sequence[str],
    *,
    max_bytes: int = 16 * 1024 * 1024,
    tool_registry: Sequence[Mapping[str, Any]] | None = None,
) -> list[Any]:
    """Decode bounded owned records without rereading source transcript bytes."""
    payloads = _call(
        _native.decode_snapshot_projection,
        record_schema,
        list(records_json),
        max_bytes,
        None if tool_registry is None else _json(tool_registry),
    )
    match record_schema:
        case "cc-transcript.event/1" | "cc-transcript.sidechain/1":
            return payloads
        case "cc-transcript.mining-signal/1":
            return payloads
        case "cc-transcript.tool-use/1":
            return [_tool(payload) for payload in payloads]
        case "cc-transcript.turn/1":
            return [_turn(payload) for payload in payloads]
        case "cc-transcript.file-ref/1":
            return [FileRef(**payload) for payload in payloads]
        case "cc-transcript.predicate-inputs/1":
            return [
                PredicateInputs(
                    calls=payload["calls"],
                    name_matchers=payload["name_matchers"],
                    commands=payload["commands"],
                    skills=payload["skills"],
                    edited_files=tuple(FileRef(**file) for file in payload["edited_files"]),
                )
                for payload in payloads
            ]
        case _:
            raise ValueError(f"unsupported snapshot record schema {record_schema}")


class _Events(Sequence[TranscriptEvent]):
    def __init__(self, scope: TranscriptSnapshot) -> None:
        self._scope = scope

    def __len__(self) -> int:
        return _call(self._scope._native.event_count)

    @overload
    def __getitem__(self, index: int) -> TranscriptEvent: ...

    @overload
    def __getitem__(self, index: slice) -> list[TranscriptEvent]: ...

    def __getitem__(self, index: int | slice) -> TranscriptEvent | list[TranscriptEvent]:
        if isinstance(index, slice):
            return [self[position] for position in range(*index.indices(len(self)))]
        if index < 0:
            index += len(self)
        if not 0 <= index < len(self):
            raise IndexError(index)
        return _call(self._scope._native.event, index)


class TranscriptSnapshot:
    """A scoped owner-only view; its lease must end before model work begins."""

    def __init__(self, native: Any) -> None:
        self._native = native
        self.events = _Events(self)

    @property
    def description(self) -> Mapping[str, Any]:
        return orjson.loads(_call(self._native.description))

    @property
    def usage(self) -> Mapping[str, int]:
        return orjson.loads(_call(self._native.usage))

    @property
    def work(self) -> Mapping[str, int]:
        return orjson.loads(_call(self._native.work))

    def checkpoint(self) -> None:
        _call(self._native.checkpoint)

    def consume(self, *, events: int = 0, items: int = 0, output_bytes: int = 0) -> None:
        _call(self._native.consume, events, items, output_bytes)

    def activity(
        self,
        classifier: Mapping[str, str],
        *,
        anchor: EventRef | None = None,
        lookback_turns: int = 40,
        lookahead_turns: int = 120,
    ) -> SessionActivity:
        payload = _call(
            self._native.activity,
            _json(classifier),
            None if anchor is None else _json(asdict(anchor)),
            lookback_turns,
            lookahead_turns,
        )
        return SessionActivity(SessionId(payload["session_id"]), tuple(_turn(turn) for turn in payload["turns"]))

    def classifier_facts(self, prefix: str, *, event_limit: int = 50) -> Mapping[str, bool]:
        return orjson.loads(_call(self._native.classifier_facts, prefix, event_limit))

    def source_facts(self, *, first_user_contains: str) -> SourceFacts:
        """Return distinct cwds in source order and a substring match in the first user text.

        User flags do not change which message is first. Missing user text and an
        empty token return false. Limits apply to metadata and logical text read.
        """
        return orjson.loads(_call(self._native.source_facts, first_user_contains))

    def prose_rows(self) -> Iterator[ProseRow]:
        """Yield exact user and assistant text with original positions and flags.

        Blank text is omitted. Each native page contains at most 256 source events,
        and every page consumes the borrow scope's cumulative limits.
        """
        position = 0
        while True:
            page = orjson.loads(_call(self._native.prose_page, position))
            yield from page["rows"]
            if page["next"] is None:
                return
            position = page["next"]

    def mine_json(
        self, spec_json: str, callable_formats: Sequence[tuple[str, Any, Any, bool]]
    ) -> list[Mapping[str, Any]]:
        return _call(self._native.mine, spec_json, list(callable_formats))

    def capture(
        self,
        anchors: Sequence[EventRef],
        *,
        before: int = 6,
        after: int = 2,
        preview_chars: int = 200,
    ) -> list[ContextWindow]:
        windows = orjson.loads(
            _call(self._native.capture, _json([asdict(anchor) for anchor in anchors]), before, after, preview_chars)
        )
        return [ContextWindow.from_json(window) for window in windows]

    def hydrate(self, windows: Sequence[ContextWindow], *, render: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        return orjson.loads(_call(self._native.hydrate, _json([window.to_json() for window in windows]), _json(render)))


class TranscriptStore:
    """Own shared parsed generations, leases, and explicitly registered classifiers."""

    def __init__(
        self,
        config: Mapping[str, Any],
        *,
        policies: Mapping[tuple[str, str], Callable[[TranscriptEvent], bool]] | None = None,
    ) -> None:
        self._native = _native.NativeSnapshotStore(_json(config))
        for (policy_id, version), predicate in (policies or {}).items():
            self.register_classifier(policy_id, version, predicate)

    def register_tool_registry(self, specs: Sequence[Mapping[str, Any]], *, context: CallContext) -> str:
        return _call(self._native.register_tool_registry, _json(specs), _json(context))

    def register_classifier(self, policy_id: str, version: str, predicate: Callable[[TranscriptEvent], bool]) -> None:
        def classify(events: Sequence[TranscriptEvent]) -> list[bool]:
            return [isinstance(event, UserEvent) and predicate(event) for event in events]

        _call(self._native.register_classifier, policy_id, version, classify)

    def register_mining_policy_json(
        self,
        policy_id: str,
        version: str,
        spec_json: str,
        callable_formats: Sequence[tuple[str, Any, Any, bool]],
    ) -> None:
        _call(self._native.register_mining_policy, policy_id, version, spec_json, list(callable_formats))

    @property
    def owner_epoch(self) -> str:
        return self._native.owner_epoch()

    def record_transport(self, byte_count: int) -> None:
        self._native.record_transport(byte_count)

    def request(
        self, request: Mapping[str, Any], *, cancellation: CancellationToken, context: CallContext
    ) -> Mapping[str, Any]:
        return orjson.loads(_call(self._native.request, _json(request), _json(context), cancellation._native))

    def prepare_classifier(
        self,
        handle: Mapping[str, str],
        classifier: Mapping[str, str],
        *,
        context: CallContext,
        cancellation: CancellationToken,
        limits: Mapping[str, int],
        deadline_unix_ms: int,
    ) -> Mapping[str, Any]:
        return orjson.loads(
            _call(
                self._native.prepare_classifier,
                _json(handle),
                _json(classifier),
                _json(context),
                cancellation._native,
                _json({"limits": limits, "deadline_unix_ms": deadline_unix_ms}),
            )
        )

    def submit_classifier(
        self,
        cursor: str,
        labels: Sequence[bool],
        *,
        context: CallContext,
        cancellation: CancellationToken,
    ) -> Mapping[str, Any]:
        return orjson.loads(
            _call(self._native.submit_classifier, cursor, list(labels), _json(context), cancellation._native)
        )

    def discard_response(self, response: Mapping[str, Any], *, context: CallContext) -> None:
        _call(self._native.discard_response, _json(response), _json(context))

    def publish_projection(
        self,
        request: Mapping[str, Any],
        *,
        context: CallContext,
        cancellation: CancellationToken,
        metadata: Mapping[str, Any],
        field: str,
        records_json: Sequence[str],
        usage: Mapping[str, int],
        work: Mapping[str, int],
    ) -> Mapping[str, Any]:
        return orjson.loads(
            _call(
                self._native.publish_projection,
                _json(
                    {
                        "id": request["id"],
                        "deadline_unix_ms": request["deadline_unix_ms"],
                        "limits": request["limits"],
                        "view": {"handle": request["view"]["handle"]},
                    }
                ),
                _json(context),
                cancellation._native,
                _json(metadata),
                field,
                list(records_json),
                _json(usage),
                _json(work),
            )
        )

    def resume_projection(
        self,
        cursor: str,
        *,
        context: CallContext,
        cancellation: CancellationToken,
    ) -> Mapping[str, Any] | None:
        result = _call(self._native.resume_projection, cursor, _json(context), cancellation._native)
        return None if result is None else orjson.loads(result)

    @contextmanager
    def borrow_snapshot(
        self,
        handle: Mapping[str, str],
        *,
        context: CallContext,
        cancellation: CancellationToken,
        limits: Mapping[str, int],
        deadline_unix_ms: int,
    ) -> Iterator[TranscriptSnapshot]:
        native = _call(
            self._native.borrow,
            _json(handle),
            _json(context),
            _json({"limits": limits, "deadline_unix_ms": deadline_unix_ms}),
            cancellation._native,
        )
        snapshot = TranscriptSnapshot(native)
        try:
            yield snapshot
        except SnapshotIncomplete as error:
            error.usage = dict(snapshot.usage)
            error.work = dict(snapshot.work)
            raise
        finally:
            _call(native.close)
