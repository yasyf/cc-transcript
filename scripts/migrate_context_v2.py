"""Convert a feedback-store SQLite DB in place to the cc-transcript 3.0 context schema.

cc-transcript 3.0 removes the window-model migration affordance: the context wire
schema bumps from ``cc-transcript.context/1`` to ``cc-transcript.context/2`` (the
``origin`` key is gone, ``anchor`` must be non-null, ``trigger`` may still be
null), and persisted candidate payloads must always carry a ``signal``. ``/1``
rows with a null ``anchor`` are unconvertible and are left fully untouched unless
``--delete-unconvertible`` is passed, which removes them together with their rows
in any table referencing ``feedback_events`` (judge verdict tables); foreign-format
rows need cc-pushback's own ``migrate-corpus`` first and are left fully untouched.
Exits 0 only when no unconvertible or foreign-format rows remain.

Run: ``python3 scripts/migrate_context_v2.py <db-path> [--delete-unconvertible]``
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from pathlib import Path

SCHEMA_V2 = "cc-transcript.context/2"
DEFAULT_SIGNAL = {"confidence": 0.5, "reasons": [], "durable": True}
REPORT_KEYS = ("converted", "already", "signal_backfilled", "unconvertible", "foreign", "deleted")


def dump_context(context: dict[str, object]) -> str:
    return json.dumps(context, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def referencing_tables(conn: sqlite3.Connection) -> tuple[str, ...]:
    return tuple(
        name
        for (name,) in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND sql LIKE '%REFERENCES feedback_events%'"
        )
    )


def migrate(conn: sqlite3.Connection, *, delete_unconvertible: bool) -> dict[str, int]:
    counts = dict.fromkeys(REPORT_KEYS, 0)
    verdict_tables = referencing_tables(conn)
    for row_id, dedup_key, payload_json, context_json in conn.execute(
        "SELECT id, dedup_key, payload_json, context_json FROM feedback_events"
    ).fetchall():
        match context := json.loads(context_json):
            case {"schema": "cc-transcript.context/2"}:
                counts["already"] += 1
            case {"schema": "cc-transcript.context/1"} if context.get("anchor") is None:
                counts["unconvertible"] += 1
                if not delete_unconvertible:
                    continue
                for table in verdict_tables:
                    conn.execute(f'DELETE FROM "{table}" WHERE dedup_key = ?', (dedup_key,))
                conn.execute("DELETE FROM feedback_events WHERE id = ?", (row_id,))
                counts["deleted"] += 1
                continue
            case {"schema": "cc-transcript.context/1"}:
                converted = {key: value for key, value in context.items() if key != "origin"} | {
                    "schema": SCHEMA_V2
                }
                conn.execute(
                    "UPDATE feedback_events SET context_json = ? WHERE id = ?",
                    (dump_context(converted), row_id),
                )
                counts["converted"] += 1
            case _:
                counts["foreign"] += 1
                continue
        payload = json.loads(payload_json) if payload_json is not None else {}
        if "signal" not in payload:
            payload["signal"] = DEFAULT_SIGNAL
            conn.execute(
                "UPDATE feedback_events SET payload_json = ? WHERE id = ?",
                (json.dumps(payload), row_id),
            )
            counts["signal_backfilled"] += 1
    return counts


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Convert a feedback-store SQLite DB in place to the cc-transcript 3.0 context schema."
    )
    parser.add_argument("db_path", type=Path, help="feedback-store SQLite database to convert in place")
    parser.add_argument(
        "--delete-unconvertible",
        action="store_true",
        help="delete /1 rows whose anchor is null instead of leaving them behind",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not args.db_path.exists():
        sys.exit(f"{args.db_path}: no such file")
    conn = sqlite3.connect(args.db_path)
    try:
        if (
            conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'feedback_events'"
            ).fetchone()
            is None
        ):
            sys.exit(f"{args.db_path}: no feedback_events table")
        counts = migrate(conn, delete_unconvertible=args.delete_unconvertible)
        conn.commit()
    except BaseException:
        conn.rollback()
        raise
    finally:
        conn.close()
    print(" ".join(f"{key}={value}" for key, value in counts.items()))
    if (remaining := counts["unconvertible"] - counts["deleted"]) or counts["foreign"]:
        sys.exit(f"rows still blocking 3.0: unconvertible={remaining} foreign={counts['foreign']}")


if __name__ == "__main__":
    main()
