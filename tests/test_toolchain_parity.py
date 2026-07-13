"""Toolchain parity: the tree-sitter grammar pins agree across both lockfiles.

The Rust extension and the Python wheel embed independent tree-sitter builds
that must parse bash identically (rationale at ``rust/crates/py/Cargo.toml``). This pins
their resolved versions: ``tree-sitter-bash`` exactly equal, ``tree-sitter``
core sharing a ``(major, minor)`` so a ``0.27`` bump on one side alone fails.
"""

from __future__ import annotations

import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO_LOCK = ROOT / "Cargo.lock"
UV_LOCK = ROOT / "uv.lock"


def resolved_version(lock: Path, name: str) -> str:
    versions = [pkg["version"] for pkg in tomllib.loads(lock.read_text())["package"] if pkg["name"] == name]
    assert len(versions) == 1, f"{name} resolves to {len(versions)} entries in {lock.name}: {versions}"
    return versions[0]


def minor_pair(version: str) -> tuple[int, int]:
    major, minor, *_ = version.split(".")
    return int(major), int(minor)


def test_tree_sitter_bash_versions_match_exactly() -> None:
    assert resolved_version(CARGO_LOCK, "tree-sitter-bash") == resolved_version(UV_LOCK, "tree-sitter-bash")


def test_tree_sitter_core_shares_major_minor() -> None:
    assert minor_pair(resolved_version(CARGO_LOCK, "tree-sitter")) == minor_pair(
        resolved_version(UV_LOCK, "tree-sitter")
    )
