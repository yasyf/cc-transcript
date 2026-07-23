"""The SQLite feedback store, over the native engine that owns ``feedback.db``.

The verdict tier folds into this one facade: downstream apps compose a
:class:`StoreSchema` at open and hold a :class:`FeedbackStore`, adding their own
domain methods over its primitive and typed surface rather than subclassing it.
"""

from __future__ import annotations

import asyncio
import json
import sqlite3
import threading
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Self

from cc_transcript import _native
from cc_transcript.literals import literal_str
from cc_transcript.mining.confidence import to_payload

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Mapping, Sequence
    from pathlib import Path
    from types import TracebackType

    from cc_transcript.context import Fidelity
    from cc_transcript.judge.verdicts import VerdictLike
    from cc_transcript.mining.candidates import DedupKey, FeedbackCandidate
    from cc_transcript.mining.sourcekind import SourceKind

FILE_SCHEMA = literal_str("feedback.FILE_SCHEMA")
FEEDBACK_DDL = literal_str("feedback.FEEDBACK_DDL")
VERDICT_DDL_TEMPLATE = literal_str("feedback.VERDICT_DDL_TEMPLATE")
DEFAULT_SCHEMA_DDL = FILE_SCHEMA + FEEDBACK_DDL + VERDICT_DDL_TEMPLATE.format(
    table="verdicts", accepted="accepted", summary="summary"
)
REFRESH_PAGE_SIZE = 256


def now() -> str:
    return datetime.now(UTC).isoformat()


def current_owner() -> tuple[int, int | None]:
    try:
        task = asyncio.current_task()
    except RuntimeError:
        task = None
    return (threading.get_ident(), id(task) if task is not None else None)


def event_row(candidate: FeedbackCandidate, ingested_at: str) -> tuple[object, ...]:
    payload = dict(candidate.payload or {})
    payload["signal"] = to_payload(candidate.signal)
    return (
        candidate.dedup_key,
        candidate.source_kind,
        candidate.session_id,
        candidate.ref.event_uuid,
        candidate.occurred_at.isoformat(),
        candidate.text,
        json.dumps(payload),
        candidate.window.to_json(),
        candidate.cc_version,
        ingested_at,
    )


class TransactionConflictError(RuntimeError):
    """A write attempted while another task's transaction holds the connection.

    The store shares one SQLite connection, so a transaction is exclusive: a
    standalone write from a different task or thread must not silently join
    it (a rollback would take the bystander's committed-looking write with
    it). Callers hitting this serialize their writes or retry after the open
    transaction finishes.
    """


@dataclass(frozen=True, slots=True)
class StoreSchema:
    """One complete exact v1 feedback-store schema.

    Attributes:
        identity: The product-owned identity recorded in the exact v1 marker.
        ddl: Complete one-shot application DDL, excluding the platform marker.
        event_columns: Product column names appended to candidate insert rows.
        verdict_table: The physical verdict table name.
        accepted_column: The verdict table's accept column name.
        summary_column: The verdict table's summary column name.
        event_filter: A SQL predicate over alias ``e`` ANDed into ``unjudged``
            and ``judged``, e.g. ``"e.quarantined_reason IS NULL"``.
    """

    identity: str = "cc-transcript-feedback"
    ddl: str = DEFAULT_SCHEMA_DDL
    event_columns: tuple[str, ...] = ()
    verdict_table: str = "verdicts"
    accepted_column: str = "accepted"
    summary_column: str = "summary"
    event_filter: str | None = None


@dataclass(frozen=True, slots=True)
class Stats:
    """A snapshot of ingestion progress.

    Attributes:
        total: The total feedback events ingested.
        files: The number of scanned files recorded.
        by_source: Event counts keyed by source kind.
    """

    total: int
    files: int
    by_source: Mapping[str, int]


class FeedbackStore:
    """Persistent store for collected feedback over the native store engine.

    Owns the one connection to ``feedback.db`` — the ``feedback_events`` ledger,
    the scanned-file mtime table, and the verdict tier. Recording a scanned file
    and inserting its candidates commit in one transaction, so a scan is atomic.
    Apps hold a store and compose their own writes with :meth:`record_file` inside
    :meth:`transaction`.

    Example:
        >>> async with await FeedbackStore.open(Path("feedback.db")) as store:
        ...     await store.record_file_scan(str(path), mtime, candidates)
    """

    def __init__(self, engine: _native.RustFeedbackStore, schema: StoreSchema) -> None:
        self.engine = engine
        self.schema = schema
        self._txn_owner: tuple[int, int | None] | None = None
        self._txn_lock = threading.Lock()
        self._vec_prepared = False

    @classmethod
    async def open(
        cls,
        path: Path,
        schema: StoreSchema | None = None,
        *,
        readonly: bool = False,
        busy_timeout_ms: int | None = None,
        extensions: Sequence[str] = (),
    ) -> Self:
        """Opens (creating if needed) the feedback database at ``path``.

        Args:
            path: The database file path; its parent is created if absent.
            schema: The app's schema composition; the platform default when omitted.
            readonly: When True, open read-only after exact schema attestation.
            busy_timeout_ms: The SQLite busy timeout; the 5000ms default when omitted.
            extensions: Loadable SQLite extensions required by the exact schema.

        Returns:
            The opened store.
        """
        schema = schema if schema is not None else StoreSchema()
        engine = _native.RustFeedbackStore(
            str(path),
            schema_identity=schema.identity,
            schema_ddl=schema.ddl,
            event_columns=list(schema.event_columns),
            extension_paths=list(extensions),
            verdict_table=schema.verdict_table,
            accepted_column=schema.accepted_column,
            summary_column=schema.summary_column,
            event_filter=schema.event_filter,
            readonly=readonly,
            busy_timeout_ms=busy_timeout_ms,
        )
        try:
            await engine.open()
        except BaseException:
            await engine.close()
            raise
        return cls(engine, schema)

    async def close(self) -> None:
        """Closes the underlying connection; a second close is a no-op.

        Any later use of the store — or of a retained ``engine`` reference —
        raises ``sqlite3.ProgrammingError``, exactly like a closed
        ``sqlite3.Connection``.
        """
        await self.engine.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.close()

    # --- primitive surface ---------------------------------------------------
    async def sql(self, statement: str, params: Sequence[object] = ()) -> list[dict[str, object]]:
        """Runs one parameterized statement, returning rows as dicts."""
        return await self.engine.sql(statement, list(params))

    async def execute(self, statement: str, params: Sequence[object] = ()) -> int:
        """Runs one parameterized write statement, returning the modified-row count."""
        return await self.engine.execute(statement, list(params))

    async def executemany(self, statement: str, seq: Sequence[Sequence[object]]) -> int:
        """Runs ``statement`` once per parameter set, returning the total modified-row count."""
        return await self.engine.executemany(statement, [list(params) for params in seq])

    async def executescript(self, script: str) -> None:
        """Runs a multi-statement script — refused while a transaction is open."""
        await self.engine.executescript(script)

    async def last_insert_rowid(self) -> int:
        """Returns the rowid of the last inserted row on this connection."""
        return await self.engine.last_insert_rowid()

    @asynccontextmanager
    async def transaction(self) -> AsyncIterator[Self]:
        """Yields the store inside a single committed transaction.

        Composes consumer writes with :meth:`record_file` so they commit or roll
        back together. A standalone write by the same task joins this transaction;
        the transaction is exclusive, so opening another while one is in flight
        raises :class:`TransactionConflictError`.

        Yields:
            The store.

        Raises:
            TransactionConflictError: A transaction is already open.
        """
        with self._txn_lock:
            if self._txn_owner is not None:
                raise TransactionConflictError(f"a transaction opened by {self._txn_owner} is already in flight")
            self._txn_owner = current_owner()
        try:
            try:
                await self.engine.begin_immediate()
                yield self
            except BaseException:
                await self._rollback_dispatched()
                raise
            else:
                await self.engine.commit()
        finally:
            with self._txn_lock:
                self._txn_owner = None

    async def _rollback_dispatched(self) -> None:
        # A begin that never took effect, or a cancellation racing the dispatched begin,
        # leaves no active transaction; rolling one back is a benign no-op (native design §4).
        try:
            await self.engine.rollback()
        except sqlite3.OperationalError as error:
            if "cannot rollback - no transaction is active" not in str(error):
                raise

    # --- file-scan ledger ----------------------------------------------------
    async def file_mtimes(self) -> dict[str, float]:
        """Returns the recorded ``path`` to ``mtime`` map for incremental scans."""
        return {row["path"]: row["mtime"] for row in await self.engine.file_mtimes()}

    async def record_file(self, path: str, mtime: float) -> None:
        """Upserts the recorded mtime for ``path``.

        Called inside :meth:`transaction` by the owning task it joins that
        transaction; called on its own it commits immediately. A standalone call
        during another task's transaction raises :class:`TransactionConflictError`.

        Raises:
            TransactionConflictError: Another task's transaction is open.
        """
        if self._txn_owner == current_owner():
            await self.engine.record_file(path, mtime)
            return
        async with self.transaction():
            await self.engine.record_file(path, mtime)

    async def insert_candidates(
        self, rows: Sequence[Sequence[object]], extras: Sequence[Sequence[object]] | None = None
    ) -> list[str]:
        """``INSERT OR IGNORE``s candidate rows, returning the newly inserted dedup keys."""
        return await self.engine.insert_candidates(
            [list(row) for row in rows],
            [list(extra) for extra in extras] if extras is not None else None,
        )

    async def record_file_scan(self, path: str, mtime: float, candidates: Sequence[FeedbackCandidate]) -> int:
        """Records a scanned file and its candidates in one transaction.

        Inserts every candidate with ``INSERT OR IGNORE`` keyed by its dedup key
        and upserts the file's mtime, so re-scanning an unchanged file is a no-op.

        Args:
            path: The scanned file's path.
            mtime: The file's modification time at scan.
            candidates: The candidates extracted from the file.

        Returns:
            The number of newly inserted feedback events.
        """
        ingested_at = now()
        async with self.transaction():
            inserted = await self.engine.insert_candidates(
                [list(event_row(candidate, ingested_at)) for candidate in candidates]
            )
            await self.record_file(path, mtime)
            return len(inserted)

    # --- corpus reads --------------------------------------------------------
    async def stats(self) -> Stats:
        """Returns ingestion counts by source kind and the scanned-file count."""
        total, files, by_source = await self.engine.stats()
        return Stats(total=total, files=files, by_source={row["source_kind"]: row["n"] for row in by_source})

    async def recent(self, *, source_kind: SourceKind | None = None, limit: int = 20) -> list[dict[str, object]]:
        """Returns the most recent feedback events, newest first.

        Args:
            source_kind: When set, restrict to this source kind.
            limit: The maximum number of rows to return.

        Returns:
            One dict per event with its ``source_kind``, ``occurred_at``, and ``text``.
        """
        return await self.engine.recent(str(source_kind) if source_kind is not None else None, limit)

    async def events(self, *, source_kind: SourceKind | None = None) -> list[dict[str, object]]:
        """Returns every feedback event, newest first, with the columns needed to render it.

        Args:
            source_kind: When set, restrict to this source kind.

        Returns:
            One dict per event with its ``id``, ``source_kind``, ``occurred_at``,
            ``text``, ``payload_json``, ``context_json``, ``event_uuid``, and
            ``session_id``.
        """
        return await self.engine.events(str(source_kind) if source_kind is not None else None)

    async def dedup_keys(self) -> set[str]:
        """Returns every stored event's dedup key."""
        return set(await self.engine.dedup_keys())

    # --- verdict tier --------------------------------------------------------
    async def record_verdict(
        self, key: DedupKey, verdict: VerdictLike, *, role: str, prompt_version: int, model: str, fidelity: Fidelity
    ) -> None:
        """Records one verdict, idempotently, keyed by ``(dedup_key, role, prompt_version)``.

        ``model`` is provenance only, never part of the identity: re-recording is
        a no-op, except a ``'full'``-fidelity verdict replaces a ``'summary'`` one
        at the same key (any model), carrying the new model, content, and
        ``canonical_key`` across. When the engine reports a row changed, the
        judged event's sqlite-vec evidence is upserted (or cleared when the
        verdict names no ``canonical_key``) in the same transaction.

        Args:
            key: The judged event's dedup key.
            verdict: The structured verdict to persist.
            role: Who produced it, e.g. ``judge`` or ``auditor``.
            prompt_version: The prompt version that produced it.
            model: The resolved model name that produced it, kept as provenance.
            fidelity: Whether the judged window rendered at ``'full'`` fidelity
                or from ``'summary'`` previews.
        """
        from cc_transcript.judge import similar

        evidence = (
            await similar.embed_evidence(self, dedup_key=key, canonical_key=verdict.canonical_key, summary=verdict.summary)
            if verdict.canonical_key is not None
            else None
        )
        removable = evidence is None and await similar.prepare_evidence_removal(self)
        async with self.transaction():
            changed = await self.engine.record_verdict(
                str(key),
                role,
                prompt_version,
                model,
                verdict.category,
                verdict.accepted,
                verdict.summary,
                verdict.confidence,
                verdict.rationale,
                verdict.canonical_key,
                fidelity,
                now(),
            )
            if changed:
                if evidence is not None:
                    await similar.record_evidence(self, dedup_key=key, role=role, prompt_version=prompt_version, evidence=evidence)
                elif removable:
                    await similar.clear_evidence(self, dedup_key=key, role=role, prompt_version=prompt_version)

    async def unjudged(
        self,
        *,
        role: str,
        prompt_version: int,
        limit: int | None = None,
        refresh_summary: bool = False,
        probe_hydration: bool = True,
    ) -> list[dict[str, object]]:
        """Returns events lacking a verdict for ``(role, prompt_version)``, unjudged first.

        Truly-unjudged events sort ahead of summary-refresh rows, then by event id.
        With ``refresh_summary`` set, events whose verdict was recorded at
        ``fidelity='summary'`` re-yield for a full re-judge; a summary row whose
        context window no longer hydrates is dropped (unless ``probe_hydration``
        is False). An event filter, when configured, pages a bounded window past
        dead summary rows and short-circuits ``limit=0`` to empty; the unfiltered
        shape loads the candidates and returns its off-by-one first row for
        ``limit=0`` — the pinned observable divergence between the two shapes.

        Args:
            role: The verdict role to check.
            prompt_version: The prompt version the verdict must carry.
            limit: When set, the maximum number of rows to return.
            refresh_summary: When True, also re-yield summary-fidelity rows.
            probe_hydration: When True, drop summary-refresh rows whose window no
                longer hydrates; when False, skip the per-row transcript probe.

        Returns:
            One dict per event with the columns needed to build its prompt.
        """
        if not refresh_summary:
            return await self.engine.unjudged(role, prompt_version, False, limit, None)
        if self.schema.event_filter is not None:
            return await self._paged_refresh(role, prompt_version, limit, probe_hydration)
        return await self._loadall_refresh(role, prompt_version, limit, probe_hydration)

    async def _loadall_refresh(
        self, role: str, prompt_version: int, limit: int | None, probe_hydration: bool
    ) -> list[dict[str, object]]:
        from cc_transcript.judge.verdicts import hydratable

        kept: list[dict[str, object]] = []
        for row in await self.engine.unjudged(role, prompt_version, True, None, None):
            fresh = row.pop("verdict_id") is None
            if not probe_hydration or fresh or await asyncio.to_thread(hydratable, str(row["context_json"])):
                kept.append(row)
                if limit is not None and len(kept) >= limit:
                    break
        return kept

    async def _paged_refresh(
        self, role: str, prompt_version: int, limit: int | None, probe_hydration: bool
    ) -> list[dict[str, object]]:
        from cc_transcript.judge.verdicts import hydratable

        if limit == 0:
            return []
        page_size = limit if limit is not None else REFRESH_PAGE_SIZE
        kept: list[dict[str, object]] = []
        offset = 0
        while limit is None or len(kept) < limit:
            page = await self.engine.unjudged(role, prompt_version, True, page_size, offset)
            if not page:
                break
            offset += len(page)
            for row in page:
                fresh = row.pop("verdict_id") is None
                if not probe_hydration or fresh or await asyncio.to_thread(hydratable, str(row["context_json"])):
                    kept.append(row)
                    if limit is not None and len(kept) >= limit:
                        break
        return kept

    async def judged(self, *, role: str, prompt_version: int) -> list[dict[str, object]]:
        """Returns events joined with their ``(role, prompt_version)`` verdicts, oldest first.

        The physical accepted/summary columns are aliased to the generic
        ``accepted`` and ``summary`` keys, whatever the schema names them.

        Args:
            role: The verdict role to join.
            prompt_version: The prompt version to join.

        Returns:
            One dict per verdict-bearing event: the event columns plus the
            verdict's ``category``, ``accepted``, ``confidence``, ``summary``,
            ``rationale``, and ``model``.
        """
        return await self.engine.judged(role, prompt_version)
