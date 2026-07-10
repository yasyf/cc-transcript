"""Canonical-key retrieval over verdict evidence, via a sqlite-vec companion store.

When a verdict assigns a ``canonical_key``, :func:`record_evidence` embeds the
judged event's feedback with a static text model (``potion-retrieval-32M``) and
upserts the vector into a sqlite-vec table that lives in the verdict store's own
database — created on first use, never part of the base schema. Two read paths
sit on top: :func:`suggest_canonical_keys` ranks stored keys by evidence
similarity to a new correction, and :func:`near_duplicate_keys` flags distinct
keys whose evidence centroids nearly coincide (split detection; nothing merges).

The vector deps (``sqlite-vec``, ``model2vec``, ``numpy``) live behind the
``cc-transcript[judge]`` extra and load lazily, so importing this module needs
none of them installed; an app that assigns canonical keys without the extra
fails loud with an :class:`ImportError` naming it.
"""

from __future__ import annotations

from functools import cache
from importlib.util import find_spec
from typing import TYPE_CHECKING, NamedTuple
from weakref import WeakSet

import anyio.to_thread

if TYPE_CHECKING:
    from collections.abc import Callable

    import aiosqlite
    import numpy as np

    from cc_transcript.mining.candidates import DedupKey
    from cc_transcript.mining.store import FeedbackStore
    from cc_transcript.store import FileStateStore

EMBED_MODEL = "minishlab/potion-retrieval-32M"
EMBED_DIM = 512
JUDGE_EXTRA = "cc-transcript[judge]"
JUDGE_DEPS = ("model2vec", "numpy", "sqlite_vec")

VECTOR_SCHEMA = f"""
CREATE VIRTUAL TABLE IF NOT EXISTS verdict_vectors USING vec0(
  vector_id TEXT PRIMARY KEY,
  embedding float[{EMBED_DIM}] distance_metric=cosine
);
CREATE TABLE IF NOT EXISTS verdict_evidence (
  vector_id TEXT PRIMARY KEY,
  dedup_key TEXT NOT NULL,
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  canonical_key TEXT NOT NULL,
  evidence_text TEXT NOT NULL
);
"""

type Embedder = Callable[[str], np.ndarray]

PREPARED_CONNECTIONS: WeakSet[aiosqlite.Connection] = WeakSet()


class Suggestion(NamedTuple):
    """One canonical-key suggestion ranked by evidence similarity.

    Attributes:
        canonical_key: The suggested durable-rule key.
        score: Cosine similarity of the query to the key's nearest evidence
            vector, in ``[-1, 1]``; higher is closer.
        sentences: Up to three evidence texts backing the key, most similar first.
    """

    canonical_key: str
    score: float
    sentences: tuple[str, ...]


class Evidence(NamedTuple):
    """A judged event's embedded feedback, ready to upsert inside the verdict transaction.

    Attributes:
        vector: The serialized 512-dim embedding of the feedback plus the verdict summary.
        text: The event's feedback text, stored verbatim as the evidence sentence.
        canonical_key: The durable-rule key the verdict assigned.
    """

    vector: bytes
    text: str
    canonical_key: str


class KeyOverlap(NamedTuple):
    """Two distinct canonical keys whose evidence centroids nearly coincide.

    Attributes:
        key_a: The lexicographically smaller key.
        key_b: The lexicographically larger key.
        similarity: Cosine similarity of the two evidence centroids.
    """

    key_a: str
    key_b: str
    similarity: float


def require_judge_extra() -> None:
    """Raises a clear :class:`ImportError` naming the extra when a vector dep is missing."""
    if any(find_spec(name) is None for name in JUDGE_DEPS):
        raise ImportError(f"canonical-key retrieval needs the vector-store deps; install {JUDGE_EXTRA}")


@cache
def default_embedder() -> Embedder:
    """The ``potion-retrieval-32M`` static embedder, loaded once and cached per process.

    Returns a callable mapping one text to its L2-normalized 512-dim ``float32``
    embedding. The model downloads from the Hugging Face hub on first use and is
    cached there; inference is numpy-only. Tests inject a deterministic stand-in
    by monkeypatching this loader.

    Raises:
        ImportError: When the ``cc-transcript[judge]`` extra is not installed.
    """
    require_judge_extra()
    import numpy as np
    from model2vec import StaticModel

    model = StaticModel.from_pretrained(EMBED_MODEL)

    def embed(text: str) -> np.ndarray:
        vector = model.encode([text])[0].astype(np.float32)
        return vector / np.linalg.norm(vector)

    return embed


async def prepare_connection(store: FileStateStore) -> None:
    """Loads the sqlite-vec extension and creates the companion tables, once per connection.

    Holds ``store.lock`` around the check-and-prepare so the schema-creating
    ``executescript`` — which implicitly commits the connection — can never fire
    while another task holds an open :meth:`~cc_transcript.store.FileStateStore.transaction`,
    and so the "already prepared" test-and-set stays atomic under concurrent
    :meth:`~cc_transcript.judge.verdicts.VerdictStoreMixin.record_verdict` fan-out.
    """
    import sqlite_vec

    async with store.lock:
        if store.conn in PREPARED_CONNECTIONS:
            return
        await store.conn.enable_load_extension(True)
        await store.conn.load_extension(sqlite_vec.loadable_path())
        await store.conn.enable_load_extension(False)
        await store.conn.executescript(VECTOR_SCHEMA)
        PREPARED_CONNECTIONS.add(store.conn)


def serialize_vector(vector: np.ndarray) -> bytes:
    import sqlite_vec

    return sqlite_vec.serialize_float32(vector.tolist())


async def embed_evidence(store: FileStateStore, *, dedup_key: DedupKey, canonical_key: str, summary: str) -> Evidence:
    """Prepares the vector store and embeds a judged event's feedback for upsert.

    Fetches the event's feedback text from ``feedback_events`` and embeds it
    together with ``summary``, off the event loop. Runs the schema-creating
    :func:`prepare_connection` and the feedback read under ``store.lock``, both
    outside the caller's transaction: ``prepare_connection``'s ``executescript``
    implicitly commits an open transaction, so the caller calls this before
    ``BEGIN`` and then upserts the returned :class:`Evidence` atomically via
    :func:`record_evidence`. Called from
    :meth:`~cc_transcript.judge.verdicts.VerdictStoreMixin.record_verdict`
    whenever a verdict assigns a ``canonical_key``.

    Args:
        store: The verdict store; the vectors live in its database.
        dedup_key: The judged event's dedup key.
        canonical_key: The durable-rule key the verdict assigned.
        summary: The verdict's one-sentence summary, embedded with the feedback.

    Returns:
        The serialized vector, evidence text, and canonical key, ready to upsert.

    Raises:
        ImportError: When the ``cc-transcript[judge]`` extra is not installed.
    """
    require_judge_extra()
    await prepare_connection(store)
    async with store.lock:
        cursor = await store.conn.execute("SELECT text FROM feedback_events WHERE dedup_key = ?", (dedup_key,))
        row = await cursor.fetchone()
    assert row is not None, "verdict dedup keys always resolve to a stored feedback event"
    text = row["text"]
    embedder = await anyio.to_thread.run_sync(default_embedder)
    vector = await anyio.to_thread.run_sync(embedder, f"{text}\n{summary}")
    return Evidence(serialize_vector(vector), text, canonical_key)


def evidence_vector_id(dedup_key: DedupKey, role: str, prompt_version: int) -> str:
    return f"{dedup_key}::{role}::{prompt_version}"


async def clear_evidence(conn: aiosqlite.Connection, *, dedup_key: DedupKey, role: str, prompt_version: int) -> None:
    """Deletes any evidence vector for one verdict identity, on the caller's transaction.

    Both the delete pair :func:`record_evidence` runs before it re-inserts, and
    the sole cleanup when a verdict is upgraded to name no ``canonical_key``: the
    removal commits atomically with the verdict row that dropped the key, so a
    dropped rule never leaves stale evidence for :func:`suggest_canonical_keys`
    to surface.

    Args:
        conn: The verdict store's connection, inside the caller's write transaction.
        dedup_key: The judged event's dedup key.
        role: Who produced the verdict, e.g. ``judge`` or ``auditor``.
        prompt_version: The prompt version that produced the verdict.
    """
    vector_id = evidence_vector_id(dedup_key, role, prompt_version)
    await conn.execute("DELETE FROM verdict_vectors WHERE vector_id = ?", (vector_id,))
    await conn.execute("DELETE FROM verdict_evidence WHERE vector_id = ?", (vector_id,))


async def record_evidence(
    conn: aiosqlite.Connection, *, dedup_key: DedupKey, role: str, prompt_version: int, evidence: Evidence
) -> None:
    """Upserts a judged event's evidence vector inside the caller's open transaction.

    Replaces any prior vector keyed ``(dedup_key, role, prompt_version)`` — one
    vector per event per role per prompt version, mirroring the verdict's own
    identity so a ``judge`` and an ``auditor`` verdict on the same event never
    overwrite each other's evidence. The delete/insert pairs run on the caller's
    already-begun transaction, so a failure rolls the verdict row back with them;
    the failure-prone prepare and embed happen earlier in :func:`embed_evidence`.

    Args:
        conn: The verdict store's connection, inside the caller's write transaction.
        dedup_key: The judged event's dedup key.
        role: Who produced the verdict, e.g. ``judge`` or ``auditor``.
        prompt_version: The prompt version that produced the verdict.
        evidence: The embedded feedback from :func:`embed_evidence`.
    """
    await clear_evidence(conn, dedup_key=dedup_key, role=role, prompt_version=prompt_version)
    vector_id = evidence_vector_id(dedup_key, role, prompt_version)
    await conn.execute("INSERT INTO verdict_vectors(vector_id, embedding) VALUES (?, ?)", (vector_id, evidence.vector))
    await conn.execute(
        "INSERT INTO verdict_evidence(vector_id, dedup_key, role, prompt_version, canonical_key, evidence_text) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (vector_id, dedup_key, role, prompt_version, evidence.canonical_key, evidence.text),
    )


async def prepare_evidence_removal(store: FileStateStore) -> bool:
    """Readies the vec companion so an in-transaction :func:`clear_evidence` can reach it.

    Returns False — skip the removal — whenever no evidence could exist: ``sqlite_vec``
    is absent, or the companion tables were never created. The removal path embeds
    nothing, so it gates on ``sqlite_vec`` alone — the ``model2vec``/``numpy``
    embedder deps that :func:`require_judge_extra` guards belong to the insert side,
    and a partial install (``sqlite_vec`` present, embedder absent) must still clear
    stranded evidence. Otherwise loads the extension onto the connection (a no-op
    once the connection is prepared) so a later :func:`clear_evidence` on the
    caller's transaction can delete from the ``vec0`` virtual table, and returns
    True. Called before the verdict transaction opens, mirroring
    :func:`embed_evidence`, because :func:`prepare_connection`'s ``executescript``
    commits.

    Args:
        store: The verdict store; the vectors live in its database.
    """
    if store.conn in PREPARED_CONNECTIONS:
        return True
    if find_spec("sqlite_vec") is None:
        return False
    async with store.lock:
        cursor = await store.conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'verdict_evidence'"
        )
        if await cursor.fetchone() is None:
            return False
    await prepare_connection(store)
    return True


async def suggest_canonical_keys(
    store: FeedbackStore, text: str, *, prompt_version: int, k: int = 5
) -> list[Suggestion]:
    """Ranks stored canonical keys by evidence similarity to ``text``.

    Embeds ``text`` and scans every evidence vector recorded at ``prompt_version``,
    ranking each key by its single closest evidence vector (cosine similarity).
    Returns the top ``k`` keys, each with up to three backing evidence sentences
    ordered most-similar first.

    Args:
        store: The verdict store whose database holds the evidence vectors.
        text: The free text to find canonical keys for, e.g. a new correction.
        prompt_version: The prompt version whose evidence to search.
        k: The maximum number of distinct canonical keys to return.

    Returns:
        Up to ``k`` :class:`Suggestion`s, highest score first.

    Raises:
        ImportError: When the ``cc-transcript[judge]`` extra is not installed.
    """
    require_judge_extra()
    conn = store.store.conn
    await prepare_connection(store.store)
    embedder = await anyio.to_thread.run_sync(default_embedder)
    query = serialize_vector(await anyio.to_thread.run_sync(embedder, text))
    cur = await conn.execute(
        "SELECT e.canonical_key AS ck, e.evidence_text AS ev, vec_distance_cosine(v.embedding, ?) AS dist "
        "FROM verdict_vectors v JOIN verdict_evidence e ON e.vector_id = v.vector_id "
        "WHERE e.prompt_version = ? ORDER BY dist",
        (query, prompt_version),
    )
    ranked: dict[str, list[tuple[float, str]]] = {}
    async for row in cur:
        ranked.setdefault(row["ck"], []).append((1.0 - row["dist"], row["ev"]))
    return sorted(
        (Suggestion(ck, hits[0][0], tuple(ev for _, ev in hits[:3])) for ck, hits in ranked.items()),
        key=lambda suggestion: suggestion.score,
        reverse=True,
    )[:k]


async def near_duplicate_keys(store: FeedbackStore, *, prompt_version: int, threshold: float) -> list[KeyOverlap]:
    """Finds distinct canonical keys whose evidence centroids nearly coincide.

    Groups the evidence vectors recorded at ``prompt_version`` by canonical key,
    reduces each group to a normalized centroid, and returns every pair of
    distinct keys whose centroid cosine similarity exceeds ``threshold`` — a
    split-detection signal that two keys may name the same rule. Nothing merges;
    the caller decides.

    Args:
        store: The verdict store whose database holds the evidence vectors.
        prompt_version: The prompt version whose evidence to compare.
        threshold: The exclusive cosine-similarity floor a pair must clear.

    Returns:
        Overlapping key pairs, highest similarity first, each pair's keys ordered
        lexicographically.

    Raises:
        ImportError: When the ``cc-transcript[judge]`` extra is not installed.
    """
    require_judge_extra()
    import numpy as np

    conn = store.store.conn
    await prepare_connection(store.store)
    cur = await conn.execute(
        "SELECT e.canonical_key AS ck, v.embedding AS emb "
        "FROM verdict_vectors v JOIN verdict_evidence e ON e.vector_id = v.vector_id "
        "WHERE e.prompt_version = ?",
        (prompt_version,),
    )
    groups: dict[str, list[np.ndarray]] = {}
    async for row in cur:
        groups.setdefault(row["ck"], []).append(np.frombuffer(row["emb"], dtype=np.float32))
    centroids = {ck: (mean := np.mean(vectors, axis=0)) / np.linalg.norm(mean) for ck, vectors in groups.items()}
    keys = sorted(centroids)
    return sorted(
        (
            KeyOverlap(key_a, key_b, similarity)
            for i, key_a in enumerate(keys)
            for key_b in keys[i + 1 :]
            if (similarity := float(np.dot(centroids[key_a], centroids[key_b]))) > threshold
        ),
        key=lambda overlap: overlap.similarity,
        reverse=True,
    )
