"""Parity + drift guards for the generated Rust protocol literals.

- Single source: `_parser_rs.embedded_literals()` equals the generator's `literals()`
  manifest (the same manifest the Rust files render from). A constant added to the
  manifest but missing from `python.rs`'s accessor fails here automatically. Skips
  when the `_parser_rs` extension isn't built.
- Freshness: the committed `rust/src/generated/*` files still match the generator's
  `render()` byte-for-byte, so a stale checkout fails loudly.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType

from tests.support import requires_rust

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
        key: list(value) if isinstance(value, tuple) else value for key, value in load_generator().literals().items()
    }


def test_generated_files_match_render() -> None:
    for relpath, content in load_generator().render().items():
        assert (ROOT / relpath).read_text(encoding="utf-8") == content, f"{relpath} is stale; regenerate"
