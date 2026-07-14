"""The package root's two export ledgers must agree, name for name.

``EXPORTS`` is the runtime surface (PEP 562 lazy resolution); the
``TYPE_CHECKING`` ``import X as X`` alias block is the static surface — type
checkers resolve through it, and great-docs enumerates the documented API from
griffe's static read of it. A name in one but not the other is either
invisible to checkers/docs or a runtime AttributeError; this pin makes the
drift a test failure instead.
"""

from __future__ import annotations

import ast
from pathlib import Path

import cc_transcript

INIT = Path(cc_transcript.__file__)


def literal_exports(tree: ast.Module) -> set[tuple[str, str]]:
    for node in ast.walk(tree):
        match node:
            case ast.AnnAssign(target=ast.Name(id="EXPORTS"), value=ast.DictComp(generators=[comp, _])):
                source = comp.iter.func.value  # {...}.items() -> the dict literal
                return {
                    (module.value, name.value)
                    for module, names in zip(source.keys, source.values, strict=True)
                    for name in names.elts
                }
    raise AssertionError("EXPORTS dict comprehension not found in cc_transcript/__init__.py")


def alias_imports(tree: ast.Module) -> set[tuple[str, str]]:
    guarded = next(
        node.body
        for node in tree.body
        if isinstance(node, ast.If) and isinstance(node.test, ast.Name) and node.test.id == "TYPE_CHECKING"
    )
    pairs: set[tuple[str, str]] = set()
    for node in guarded:
        assert isinstance(node, ast.ImportFrom), f"non-import statement in the alias block: {ast.dump(node)}"
        for alias in node.names:
            assert alias.asname == alias.name, f"alias must be redundant (X as X): {alias.name} as {alias.asname}"
            pairs.add((node.module or "", alias.name))
    return pairs


def test_alias_block_matches_exports() -> None:
    tree = ast.parse(INIT.read_text())
    exports = literal_exports(tree)
    aliases = alias_imports(tree)
    assert exports == aliases, (
        f"EXPORTS-only: {sorted(exports - aliases)}\nalias-block-only: {sorted(aliases - exports)}"
    )


def test_runtime_surface_resolves_every_export() -> None:
    for name in cc_transcript.EXPORTS:
        assert getattr(cc_transcript, name) is not None
