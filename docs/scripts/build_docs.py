"""`great-docs build` wrapper that embeds explicit sidebar labels.

great-docs lists the API-reference sidebar as bare ``reference/<symbol>.qmd``
paths whose labels carry Pandoc span markup (``[Name]{.doc-label ...}``). Quarto
re-parses that markup for all ~260 entries on *every* page render, so the cost
grows O(pages x entries) — ~106 min for this 277-page site. Replacing each entry
with a plain ``{text, href}`` label (see :mod:`sidebar_text`) makes Quarto skip
the per-page re-parse, collapsing the render to a few minutes. We hook the last
config write before render so the rewrite always lands; everything else about
``great-docs build`` is unchanged.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import great_docs.core as gdcore
from great_docs.cli import main

from sidebar_text import embed_sidebar_titles

_normalize_freeze_shorthand = gdcore.GreatDocs._normalize_freeze_shorthand


def _normalize_freeze_shorthand_then_embed(self, *args, **kwargs):
    result = _normalize_freeze_shorthand(self, *args, **kwargs)
    n = embed_sidebar_titles(self.project_path / "_quarto.yml")
    print(f"      Embedded {n} explicit sidebar labels (skip per-page title re-parse)")
    return result


gdcore.GreatDocs._normalize_freeze_shorthand = _normalize_freeze_shorthand_then_embed


if __name__ == "__main__":
    main()
