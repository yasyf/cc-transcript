"""The core⟂domains architectural fence, enforced as a release contract.

- core modules never import the domain packages (``sentiment``, ``mining``,
  ``judge``, ``extract``);
- ``sentiment`` and ``mining`` never import each other, and neither imports
  ``judge``; ``judge`` layers on ``mining`` only, never ``sentiment``;
  ``extract`` layers on ``judge``, and the lower domains never import it;
- the top-level package re-exports core only;
- importing core (and each domain package) needs no domain extra installed.
"""

from __future__ import annotations

import ast
import importlib
import subprocess
import sys
from pathlib import Path

PACKAGE = Path(__file__).resolve().parent.parent / "cc_transcript"
DOMAIN_PACKAGES = ("sentiment", "mining", "judge", "extract")
FORBIDDEN_EDGES = (
    ("sentiment", "mining"),
    ("mining", "sentiment"),
    ("sentiment", "judge"),
    ("mining", "judge"),
    ("judge", "sentiment"),
    ("sentiment", "extract"),
    ("mining", "extract"),
    ("judge", "extract"),
)
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
    return [path for path in py_files(PACKAGE) if path.relative_to(PACKAGE).parts[0] not in DOMAIN_PACKAGES]


def domain_imports(path: Path, *packages: str) -> list[str]:
    prefixes = tuple(f"cc_transcript.{package}" for package in packages)
    return sorted(mod for mod in imported_modules(path) if mod.startswith(prefixes))


def test_core_never_imports_domains() -> None:
    offenders = {str(path.relative_to(PACKAGE)): domain_imports(path, *DOMAIN_PACKAGES) for path in core_files()}
    assert not {path: mods for path, mods in offenders.items() if mods}


def test_domains_never_cross_forbidden_edges() -> None:
    for this, other in FORBIDDEN_EDGES:
        for path in py_files(PACKAGE / this):
            crossed = domain_imports(path, other)
            assert not crossed, f"{path.relative_to(PACKAGE)} imports {crossed}"


def test_top_level_init_is_core_only() -> None:
    domain = domain_imports(PACKAGE / "__init__.py", *DOMAIN_PACKAGES)
    assert not domain, f"package root re-exports domain modules: {domain}"


def test_core_imports_without_domain_extras() -> None:
    for module in ("facts", "parser", "store", "filterspec"):
        importlib.import_module(f"cc_transcript.{module}")
    importlib.import_module("cc_transcript")


def test_domains_import_without_extras() -> None:
    # Heavy deps (spaCy/afinn/spawnllm) are lazy, so the domain packages import
    # even when no extra is installed (as in the test environment).
    for package in DOMAIN_PACKAGES:
        importlib.import_module(f"cc_transcript.{package}")


def test_mining_and_judge_never_import_llm_deps_eagerly() -> None:
    for package in ("mining", "judge"):
        for path in py_files(PACKAGE / package):
            offenders = sorted(mod for mod in eager_imports(path) if mod.partition(".")[0] in LLM_DEPS)
            assert not offenders, f"{path.relative_to(PACKAGE)} eagerly imports {offenders}"


def test_judge_import_leaves_llm_deps_unloaded() -> None:
    code = (
        "import sys, cc_transcript.mining, cc_transcript.judge, cc_transcript.judge.llm; "
        f"assert not {LLM_DEPS!r} & sys.modules.keys(), sorted({LLM_DEPS!r} & sys.modules.keys())"
    )
    subprocess.run([sys.executable, "-c", code], check=True)
