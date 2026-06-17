"""Embed plain-text sidebar labels into a great-docs `_quarto.yml`.

great-docs emits the API-reference sidebar as bare ``reference/<symbol>.qmd``
paths whose labels carry Pandoc span markup (``[Name]{.doc-label ...}``). Quarto
re-parses that markup for every entry on *every* page render, so the cost grows
O(pages x entries) — ~106 min for this 277-page site. Replacing each entry with a
plain-text ``{text, href}`` label makes Quarto skip the per-page re-parse; the
kind-color pills are restored afterward as static HTML by :mod:`sidebar_pills`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

from yaml12 import read_yaml, write_yaml

TITLE_RE = re.compile(r"^title:\s*(.+?)\s*$", re.MULTILINE)
SPAN_RE = re.compile(r"^\[(?P<text>[^\]]*)\]\{(?P<attrs>[^}]*)\}\s*$")
SIDEBAR_HREF_PREFIX = "reference/"


def title_raw(qmd: Path) -> str | None:
    if not qmd.is_file():
        return None
    parts = qmd.read_text(encoding="utf-8").split("---", 2)
    return None if len(parts) < 3 else (m := TITLE_RE.search(parts[1])) and m.group(1).strip().strip('"')


def label_and_classes(raw: str) -> tuple[str, str | None]:
    """Split a doc title `[Name]{.a .b}` into its plain label and class string."""
    if not (m := SPAN_RE.match(raw)):
        return raw, None
    return m.group("text"), " ".join(c.lstrip(".") for c in m.group("attrs").split())


def embed(contents: list, project_path: Path) -> int:
    n = 0
    for i, item in enumerate(contents):
        match item:
            case str() if item.startswith(SIDEBAR_HREF_PREFIX) and item.endswith(".qmd"):
                if raw := title_raw(project_path / item):
                    contents[i] = {"text": label_and_classes(raw)[0], "href": item}
                    n += 1
            case dict() if "contents" in item:
                n += embed(item["contents"], project_path)
    return n


def embed_sidebar_titles(quarto_yml: Path) -> int:
    with open(quarto_yml) as f:
        config = read_yaml(f) or {}
    project_path = quarto_yml.parent
    total = sum(
        embed(sidebar["contents"], project_path)
        for sidebar in config.get("website", {}).get("sidebar", [])
        if isinstance(sidebar, dict) and "contents" in sidebar
    )
    with open(quarto_yml, "w") as f:
        write_yaml(config, f)
    return total


if __name__ == "__main__":
    target = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("great-docs/_quarto.yml")
    print(f"Embedded {embed_sidebar_titles(target)} sidebar labels into {target}")
