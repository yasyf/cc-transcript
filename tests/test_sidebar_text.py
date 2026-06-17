from __future__ import annotations

import sys
from pathlib import Path

import pytest
from yaml12 import read_yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "docs" / "scripts"))

from sidebar_pills import wrap_pills
from sidebar_text import embed_sidebar_titles

SIDEBAR_YML = """\
website:
  sidebar:
    - id: reference
      contents:
        - text: API Index
          href: reference/index.qmd
        - section: Identity
          contents:
            - reference/SessionId.qmd
            - reference/EventRef.qmd
    - id: cli-reference
      contents:
        - reference/cli/list.qmd
"""

TITLES = {
    "reference/SessionId.qmd": "[SessionId]{.doc-object-name .doc-attribute .doc-label .doc-label-constant}",
    "reference/EventRef.qmd": "[EventRef]{.doc-object-name .doc-class .doc-label .doc-label-dataclass}",
    "reference/cli/list.qmd": "[cc-transcript list]{.doc-object-name .doc-label-cli}",
}


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "_quarto.yml").write_text(SIDEBAR_YML, encoding="utf-8")
    for href, title in TITLES.items():
        page = tmp_path / href
        page.parent.mkdir(parents=True, exist_ok=True)
        page.write_text(f'---\ntitle: "{title}"\n---\n\nbody\n', encoding="utf-8")
    return tmp_path


def _flatten(contents: list) -> dict[str, object]:
    out: dict[str, object] = {}
    for item in contents:
        match item:
            case {"href": href, "text": text}:
                out[href] = text
            case {"contents": nested}:
                out |= _flatten(nested)
            case str():
                out[item] = None
    return out


def test_embed_strips_markup_to_plain_labels(project: Path) -> None:
    quarto_yml = project / "_quarto.yml"
    assert embed_sidebar_titles(quarto_yml) == 3

    labels = _flatten(read_yaml(open(quarto_yml))["website"]["sidebar"][0]["contents"])
    labels |= _flatten(read_yaml(open(quarto_yml))["website"]["sidebar"][1]["contents"])
    assert labels["reference/SessionId.qmd"] == "SessionId"
    assert labels["reference/EventRef.qmd"] == "EventRef"
    assert labels["reference/cli/list.qmd"] == "cc-transcript list"
    # A pre-existing explicit entry is left untouched.
    assert labels["reference/index.qmd"] == "API Index"


def test_embed_leaves_missing_pages_as_bare_paths(tmp_path: Path) -> None:
    quarto_yml = tmp_path / "_quarto.yml"
    quarto_yml.write_text(
        "website:\n  sidebar:\n    - id: reference\n      contents:\n        - reference/Gone.qmd\n",
        encoding="utf-8",
    )
    assert embed_sidebar_titles(quarto_yml) == 0
    assert read_yaml(open(quarto_yml))["website"]["sidebar"][0]["contents"] == ["reference/Gone.qmd"]


SIDEBAR_HTML = (
    '<nav id="quarto-sidebar">'
    '<a href="../reference/EventRef.html" class="sidebar-item-text sidebar-link">\n'
    ' <span class="menu-text">EventRef</span></a>'
    '<a href="../reference/parse_event.html" class="sidebar-item-text sidebar-link active">\n'
    ' <span class="menu-text">parse_event()</span></a>'
    '<a href="../reference/index.html" class="sidebar-item-text sidebar-link">\n'
    ' <span class="menu-text">API Index</span></a></nav>'
    '<main><a href="../reference/EventRef.html" class="gdls-link">EventRef</a></main>'
)
CLASSES = {
    "reference/EventRef.html": "doc-object-name doc-class doc-label doc-label-dataclass",
    "reference/parse_event.html": "doc-object-name doc-function doc-label doc-label-function",
}


def test_wrap_pills_decorates_sidebar_labels_only() -> None:
    out, count = wrap_pills(SIDEBAR_HTML, CLASSES)
    assert count == 2
    assert (
        '<span class="menu-text"><span class="doc-object-name doc-class doc-label doc-label-dataclass">'
        "EventRef</span></span></a>" in out
    )
    assert "parse_event()</span></span></a>" in out
    # active state preserved; unmapped entry (API Index) left plain; body link untouched
    assert "sidebar-link active" in out
    assert '<span class="menu-text">API Index</span>' in out
    assert '<a href="../reference/EventRef.html" class="gdls-link">EventRef</a>' in out


def test_wrap_pills_is_idempotent() -> None:
    once, _ = wrap_pills(SIDEBAR_HTML, CLASSES)
    twice, count = wrap_pills(once, CLASSES)
    assert count == 0 and twice == once
