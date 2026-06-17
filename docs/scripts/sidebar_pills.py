"""Restore kind-color pills onto sidebar labels as static HTML (post-render).

Paired with :mod:`sidebar_text`, which renders the sidebar with plain-text labels
so Quarto never re-parses label markup per page. This pass wraps each rendered
sidebar label in its symbol's kind span (e.g. ``doc-label-dataclass``), read once
from each reference page's title. The pills look identical to a native great-docs
build but cost nothing at render time.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from sidebar_text import label_and_classes, title_raw

LINK_RE = re.compile(
    r'(?P<open><a href="(?:\.\./)*reference/(?P<path>[^"]+\.html)"[^>]*'
    r'class="[^"]*sidebar-item-text[^"]*"[^>]*>\s*<span class="menu-text">)'
    r'(?P<label>[^<]*)(?P<close></span>)'
)


def kind_classes(project_path: Path) -> dict[str, str]:
    return {
        str(qmd.relative_to(project_path).with_suffix(".html")): classes
        for qmd in (project_path / "reference").rglob("*.qmd")
        if (raw := title_raw(qmd)) and (classes := label_and_classes(raw)[1])
    }


def wrap_pills(html: str, classes_by_href: dict[str, str]) -> tuple[str, int]:
    count = 0

    def repl(m: re.Match) -> str:
        nonlocal count
        if not (classes := classes_by_href.get(f"reference/{m.group('path')}")):
            return m.group(0)
        count += 1
        return f'{m.group("open")}<span class="{classes}">{m.group("label")}</span>{m.group("close")}'

    return LINK_RE.sub(repl, html), count


def decorate_site(project_path: Path) -> int:
    classes_by_href = kind_classes(project_path)
    total = 0
    for page in (project_path / "_site").rglob("*.html"):
        new, count = wrap_pills(page.read_text(encoding="utf-8"), classes_by_href)
        if count:
            page.write_text(new, encoding="utf-8")
            total += count
    return total


if __name__ == "__main__":
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("great-docs")
    print(f"Wrapped {decorate_site(root)} sidebar labels in kind pills")
