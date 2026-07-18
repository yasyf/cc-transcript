"""Typed accessors over the hand-owned shared literals in the native extension.

:func:`~cc_transcript._native.embedded_literals` is the single source of truth for
the constants the Rust core owns and Python mirrors — protocol markers, mining ids
and floors, command tables, the corrections DDL. These helpers narrow one entry to
its concrete type so a module binds a constant without re-declaring the value.
``tests/test_literals_parity.py`` guards against a Python-side copy drifting from
the native table.
"""

from __future__ import annotations

from cc_transcript import _native

LITERALS: dict[str, str | float | list[str]] = _native.embedded_literals()


def literal_str(key: str) -> str:
    value = LITERALS[key]
    assert isinstance(value, str), key
    return value


def literal_float(key: str) -> float:
    value = LITERALS[key]
    assert isinstance(value, float), key
    return value
