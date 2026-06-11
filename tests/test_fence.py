"""The core⟂domains architectural fence, enforced as a release contract.

- core modules never import ``cc_transcript.domains.*``;
- domains never import each other (``sentiment`` ⟂ ``mining``);
- the top-level package re-exports core only;
- importing core (and each domain package) needs no domain extra installed.

The ``cc_transcript.sentiment`` package is the deprecation shim, not core — it is
allowed to re-export from ``cc_transcript.domains.sentiment``, so it is excluded
from the core checks below.
"""

from __future__ import annotations

import ast
import importlib
import subprocess
import sys
from pathlib import Path

PACKAGE = Path(__file__).resolve().parent.parent / "cc_transcript"
SHIM_PACKAGES = {"sentiment"}
LLM_DEPS = {"pydantic", "spawnllm"}


def imported_modules(path: Path) -> set[str]:
    names: set[str] = set()
    for node in ast.walk(ast.parse(path.read_text())):
        match node:
            case ast.Import(names=aliases):
                names.update(alias.name for alias in aliases)
            case ast.ImportFrom(module=module) if module:
                names.add(module)
    return names


def eager_imports(path: Path) -> set[str]:
    # Module body only: imports inside functions or `if TYPE_CHECKING:` blocks
    # are deferred by construction and do not count as eager.
    names: set[str] = set()
    for node in ast.parse(path.read_text()).body:
        match node:
            case ast.Import(names=aliases):
                names.update(alias.name for alias in aliases)
            case ast.ImportFrom(module=module) if module:
                names.add(module)
    return names


def py_files(root: Path) -> list[Path]:
    return sorted(root.rglob("*.py"))


def core_files() -> list[Path]:
    return [
        path
        for path in py_files(PACKAGE)
        if "domains" not in (parts := path.relative_to(PACKAGE).parts)
        if parts[0] not in SHIM_PACKAGES
    ]


def test_core_never_imports_domains() -> None:
    offenders = {
        str(path.relative_to(PACKAGE)): sorted(mod for mod in imported_modules(path) if mod.startswith("cc_transcript.domains"))
        for path in core_files()
    }
    assert not {path: mods for path, mods in offenders.items() if mods}


def test_domains_never_import_each_other() -> None:
    for this, other in (("sentiment", "mining"), ("mining", "sentiment")):
        for path in py_files(PACKAGE / "domains" / this):
            crossed = [mod for mod in imported_modules(path) if mod.startswith(f"cc_transcript.domains.{other}")]
            assert not crossed, f"{path.relative_to(PACKAGE)} imports {crossed}"


def test_top_level_init_is_core_only() -> None:
    domain = [mod for mod in imported_modules(PACKAGE / "__init__.py") if mod.startswith("cc_transcript.domains")]
    assert not domain, f"package root re-exports domain modules: {domain}"


def test_core_imports_without_domain_extras() -> None:
    for name in ("cc_transcript", "cc_transcript.messages", "cc_transcript.parser", "cc_transcript.store", "cc_transcript.filterspec"):
        importlib.import_module(name)


def test_domains_import_without_extras() -> None:
    # Heavy deps (spaCy/afinn) are lazy, so the domain packages import even when
    # the [sentiment] extra is absent (as it is in the test environment).
    importlib.import_module("cc_transcript.domains.sentiment")
    importlib.import_module("cc_transcript.domains.mining")


def test_mining_never_imports_llm_deps_eagerly() -> None:
    for path in py_files(PACKAGE / "domains" / "mining"):
        offenders = sorted(mod for mod in eager_imports(path) if mod.partition(".")[0] in LLM_DEPS)
        assert not offenders, f"{path.relative_to(PACKAGE)} eagerly imports {offenders}"


def test_mining_import_leaves_llm_deps_unloaded() -> None:
    code = (
        "import sys, cc_transcript.domains.mining, cc_transcript.domains.mining.llm; "
        f"assert not {LLM_DEPS!r} & sys.modules.keys(), sorted({LLM_DEPS!r} & sys.modules.keys())"
    )
    subprocess.run([sys.executable, "-c", code], check=True)
