"""The ``cc-transcript corrections`` CLI: the ledger's write/read surface for
non-Python consumers (cc-review's Go) and humans.

The ledger's location is owned here — callers never name the database file. Most
work goes through the structured ``add``/``query`` commands; ``sql`` is the
escape hatch for the rare ad-hoc statement.
"""

from __future__ import annotations

import json
import time
from dataclasses import asdict
from typing import cast

import click
import orjson

from cc_transcript.corrections import Correction, CorrectionLog, Origin
from cc_transcript.ids import EventUuid, SessionId, ToolDigest


def emit_rows(rows: tuple[Correction, ...]) -> None:
    for row in rows:
        click.echo(orjson.dumps(asdict(row)))


@click.group()
def corrections() -> None:
    """Read and write the shared code-correction ledger."""


@corrections.command("add")
@click.option("--session", required=True, help="The Claude session UUID the edit fired in.")
@click.option("--source", required=True, help="The writing system, e.g. cc-review.")
@click.option("--anchor", required=True, help="The feedback anchor uuid, or review:<reviewID>:<commentID>.")
@click.option("--incorrect-file", required=True, help="The file the incorrect edit targeted.")
@click.option("--ts-ms", type=int, default=None, help="Edit timestamp in ms [default: now].")
@click.option("--origin", type=click.Choice(["session", "git", "review"]), default=None)
@click.option("--incorrect-old", default="", help="Content the incorrect edit replaced.")
@click.option("--incorrect-new", default="", help="Content the incorrect edit wrote.")
@click.option("--incorrect-digest", default=None, help="Cross-language tool digest; omit for review rows.")
@click.option("--correction-file", default=None)
@click.option("--correction-old", default=None)
@click.option("--correction-new", default=None)
@click.option("--correction-commit", default=None)
@click.option("--correction-text", default=None, help="The reviewer's verbatim note, for review rows.")
@click.option("--overlap", type=float, default=0.0)
@click.option("--repo", default=None, help="Repo key, stamped into detail.repo.")
@click.option("--detail", "detail_json", default=None, help="Extra JSON object merged into detail.")
def add(
    session: str,
    source: str,
    anchor: str,
    incorrect_file: str,
    ts_ms: int | None,
    origin: str | None,
    incorrect_old: str,
    incorrect_new: str,
    incorrect_digest: str | None,
    correction_file: str | None,
    correction_old: str | None,
    correction_new: str | None,
    correction_commit: str | None,
    correction_text: str | None,
    overlap: float,
    repo: str | None,
    detail_json: str | None,
) -> None:
    """Append one correction to the ledger (idempotent)."""
    detail = (json.loads(detail_json) if detail_json else {}) | ({"repo": repo} if repo else {})
    CorrectionLog.open().append(
        Correction(
            ts_ms=ts_ms if ts_ms is not None else int(time.time() * 1000),
            session_id=SessionId(session),
            source=source,
            anchor_uuid=EventUuid(anchor),
            incorrect_digest=ToolDigest(incorrect_digest) if incorrect_digest else None,
            incorrect_file=incorrect_file,
            incorrect_old=incorrect_old,
            incorrect_new=incorrect_new,
            correction_origin=cast("Origin | None", origin),
            correction_file=correction_file,
            correction_old=correction_old,
            correction_new=correction_new,
            correction_commit=correction_commit,
            correction_text=correction_text,
            overlap=overlap,
            detail=detail,
        )
    )


@corrections.command("query")
@click.option("--session", default=None)
@click.option("--repo", default=None)
@click.option("--digest", default=None, help="Requires --session.")
@click.option("--since", type=int, default=None, help="Corrections with ts_ms greater than this.")
@click.option("--source", default=None, help="Scopes --since to one producer.")
def query(session: str | None, repo: str | None, digest: str | None, since: int | None, source: str | None) -> None:
    """Query corrections as one JSON object per line."""
    log = CorrectionLog.open()
    match (session, digest, repo, since):
        case (str(s), str(d), _, _):
            emit_rows(log.by_digest(SessionId(s), incorrect_digest=ToolDigest(d)))
        case (str(s), None, _, _):
            emit_rows(log.for_session(SessionId(s)))
        case (None, _, str(r), _):
            emit_rows(log.for_repo(r))
        case (None, _, None, int(ts)):
            emit_rows(log.since(ts, source=source))
        case _:
            raise click.UsageError("pass one of --session, --repo, or --since (and --digest only with --session)")


@corrections.command("sql")
@click.argument("statement")
def sql(statement: str) -> None:
    """Run a raw SQL statement against the ledger — the escape hatch."""
    rows = CorrectionLog.open().conn.execute(statement).fetchall()
    for row in rows:
        click.echo(orjson.dumps(dict(row)))
