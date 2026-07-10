"""Parity + drift guards for the generated Rust protocol literals.

- Single source: the Rust-embedded literals equal the `cc_transcript.filterspec`
  constants (and their `group_pattern` alternations). Skips when the `_parser_rs`
  extension isn't built.
- Freshness: the committed `rust/src/generated/*` files still match the generator's
  `render()` byte-for-byte, so a stale checkout fails loudly.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType

from cc_transcript.filterspec import (
    AGENT_INJECTION_GROUPS,
    ANSWERED_PREFIX,
    ANSWERED_TRAILER,
    DENIAL_PREFIX,
    INTERRUPT_MARKER_GROUPS,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
    group_pattern,
)
from tests.test_backend_parity import requires_rust

ROOT = Path(__file__).resolve().parent.parent
GENERATOR = ROOT / "scripts" / "build_rust_literals.py"


def load_generator() -> ModuleType:
    spec = importlib.util.spec_from_file_location("build_rust_literals", GENERATOR)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@requires_rust
def test_embedded_literals_match_python_source() -> None:
    from cc_transcript import _parser_rs

    assert _parser_rs.embedded_literals() == {
        "protocol.DENIAL_PREFIX": DENIAL_PREFIX,
        "protocol.USER_SAID_MARKER": USER_SAID_MARKER,
        "protocol.USER_SAID_TRAILER": USER_SAID_TRAILER,
        "protocol.ANSWERED_PREFIX": ANSWERED_PREFIX,
        "protocol.ANSWERED_TRAILER": ANSWERED_TRAILER,
        "protocol.INTERRUPT_MARKER_PATTERN": group_pattern(INTERRUPT_MARKER_GROUPS),
        "protocol.AGENT_INJECTION_PATTERN": group_pattern(AGENT_INJECTION_GROUPS),
    }


def test_generated_files_match_render() -> None:
    for relpath, content in load_generator().render().items():
        assert (ROOT / relpath).read_text(encoding="utf-8") == content, f"{relpath} is stale; regenerate"
