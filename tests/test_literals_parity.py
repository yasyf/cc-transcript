r"""Drift guard for the hand-owned shared literals now that Rust owns them.

``_native.embedded_literals()`` is the single source of truth for the constants the
Rust core and Python both need (protocol markers, mining ids and floors, command
tables, the corrections DDL). The old ``scripts/build_rust_literals.py`` generator
is gone; these tests prove Python never carries its own copy:

- every Python constant that mirrors a manifest entry equals the native value, and
  the manifest is fully covered (a new native literal without a Python mirror fails);
- the three derived filter patterns serialize to a byte-identical *source string* on
  both sides. This is a TEXTUAL pin — the same regex text reaches the engine — not a
  semantic one: it does not prove Python ``re`` and Rust ``regex`` *evaluate* that text
  alike. They diverge (e.g. ``\w`` matches combining marks in Rust but not Python), and
  that behavioral divergence is pinned in ``tests/test_buckets_parity.py``;
- no ``cc_transcript`` module re-declares a distinctive literal value as a source
  string.

The ``command.*`` tables are consumed only by the native command parser after the P6
facade flip — no Python module mirrors them — so they count toward manifest coverage
here while ``tests/test_command_parity.py`` gates their parser behaviour.
"""

from __future__ import annotations

import ast
from pathlib import Path

from tests.support import requires_rust

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "cc_transcript"


@requires_rust
def test_python_mirrors_read_from_native() -> None:
    from cc_transcript import _native, filterspec
    from cc_transcript.corrections import CORRECTIONS_DDL
    from cc_transcript.filterspec import AGENT_INJECTION_GROUPS, INTERRUPT_MARKER_GROUPS, SENTIMENT_JUNK_GROUPS, group_pattern
    from cc_transcript.mining import confidence, signals, sourcekind, spec

    literals = _native.embedded_literals()

    scalars = {
        "protocol.DENIAL_PREFIX": filterspec.DENIAL_PREFIX,
        "protocol.DENIAL_KIND_USER_REJECTED": filterspec.DENIAL_KIND_USER_REJECTED,
        "protocol.DENIAL_KIND_PERMISSION_RULE": filterspec.DENIAL_KIND_PERMISSION_RULE,
        "protocol.USER_SAID_MARKER": filterspec.USER_SAID_MARKER,
        "protocol.USER_SAID_TRAILER": filterspec.USER_SAID_TRAILER,
        "protocol.ANSWERED_PREFIX": filterspec.ANSWERED_PREFIX,
        "protocol.ANSWERED_TRAILER": filterspec.ANSWERED_TRAILER,
        "protocol.TASK_NOTIFICATION_MARKER": filterspec.TASK_NOTIFICATION_MARKER,
        "protocol.TOOL_USE_ID_PREFIX": filterspec.TOOL_USE_ID_PREFIX,
        "protocol.TOOL_USE_ID_SUFFIX": filterspec.TOOL_USE_ID_SUFFIX,
        "mining.TRANSCRIPT_MESSAGE": sourcekind.TRANSCRIPT_MESSAGE,
        "mining.PLAN_REVIEW": sourcekind.PLAN_REVIEW,
        "mining.INTERRUPT_REJECTION": sourcekind.INTERRUPT_REJECTION,
        "mining.REVIEW_COMMENT": sourcekind.REVIEW_COMMENT,
        "mining.QUESTION_ANSWER": sourcekind.QUESTION_ANSWER,
        "mining.DETECTOR_TRANSCRIPT_MESSAGE": spec.TRANSCRIPT_MESSAGE_DETECTOR,
        "mining.DETECTOR_EXIT_PLAN_REJECTION": spec.EXIT_PLAN_REJECTION_DETECTOR,
        "mining.DETECTOR_PLAN_REENTRY": spec.PLAN_REENTRY_DETECTOR,
        "mining.DETECTOR_DENIAL": spec.DENIAL_DETECTOR,
        "mining.DETECTOR_INTERRUPT": spec.INTERRUPT_DETECTOR,
        "mining.DETECTOR_REVIEW_COMMENT": spec.REVIEW_COMMENT_DETECTOR,
        "mining.DETECTOR_ASK_USER_QUESTION": spec.ASK_USER_QUESTION_DETECTOR,
        "mining.ANSWER_PREVIEW_SEP": signals.ANSWER_PREVIEW_SEP,
        "mining.ANSWER_NOTES_SEP": signals.ANSWER_NOTES_SEP,
        "mining.NO_OPTION_SELECTED": signals.NO_OPTION_SELECTED,
        "mining.NONE": confidence.NONE,
        "mining.LOW": confidence.LOW,
        "corrections.DDL": CORRECTIONS_DDL,
    }
    for key, value in scalars.items():
        assert value == literals[key], key

    # TEXTUAL pin: the serialized pattern STRING matches byte-for-byte. Not a semantic pin — it
    # does not assert re and regex evaluate it alike (\w divergence: test_buckets_parity.py).
    patterns = {
        "protocol.INTERRUPT_MARKER_PATTERN": INTERRUPT_MARKER_GROUPS,
        "protocol.AGENT_INJECTION_PATTERN": AGENT_INJECTION_GROUPS,
        "protocol.SENTIMENT_JUNK_PATTERN": SENTIMENT_JUNK_GROUPS,
    }
    for key, groups in patterns.items():
        assert group_pattern(groups) == literals[key], key

    # Native-only literals: the command parser reads these in Rust and no Python module
    # mirrors them, so they carry no equality check here — only manifest coverage.
    command_native_only = {
        "command.WRAPPER_COMMANDS",
        "command.MULTI_LEVEL_TOOLS",
        "command.COMPOUND_OPS",
        "command.ASSIGNMENT_PATTERN",
    }

    manifest = set(scalars) | set(patterns) | command_native_only
    assert manifest == set(literals), set(literals) ^ manifest


@requires_rust
def test_no_python_redeclaration() -> None:
    from cc_transcript import _native

    # Scan only module-level statements: a redeclaration is a top-level constant, not a
    # coincidental in-function reuse. Short tokens are covered by the equality check above.
    distinctive = {value for value in _native.embedded_literals().values() if isinstance(value, str) and len(value) >= 10}
    offenders = [
        f"{path.relative_to(ROOT)}:{node.lineno} re-declares an inverted literal {node.value!r}"
        for path in sorted(PACKAGE.rglob("*.py"))
        for stmt in ast.parse(path.read_text(encoding="utf-8")).body
        if not isinstance(stmt, ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef)
        for node in ast.walk(stmt)
        if isinstance(node, ast.Constant) and isinstance(node.value, str) and node.value in distinctive
    ]
    assert not offenders, "\n".join(offenders)


def test_generator_removed() -> None:
    assert not (ROOT / "scripts" / "build_rust_literals.py").exists(), "the literals generator is gone; Rust is the source of truth"
