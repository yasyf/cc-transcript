"""Generate the Rust-embedded lexicon data from the canonical sources.

Rust can't call the Python `afinn` library at compile time, so it embeds a copy of
the AFINN single-word scores plus the `DOMAIN_OVERRIDES`, generated here from the
installed `afinn` package and `cc_transcript.sentiment.lexicon.Lexicon`. The
`afinn` PyPI package stays the canonical source for the Python path; this only
mirrors it for the Rust `include_str!`. `tests/test_lexicon_parity.py` asserts the
embedded copy still matches the installed sources (drift guard).

Run: ``uv run --extra sentiment python scripts/build_lexicon_data.py``
"""

from __future__ import annotations

import warnings
from pathlib import Path

from cc_transcript.sentiment.lexicon import Lexicon

DATA = Path(__file__).resolve().parent.parent / "rust" / "data"


def afinn_single_word_scores() -> dict[str, int]:
    warnings.simplefilter("ignore", SyntaxWarning)
    from afinn import Afinn

    afinn = Afinn(language="en", emoticons=False)
    return {word: int(score) for word, score in afinn._dict.items() if " " not in word}  # noqa: SLF001


def write_tsv(path: Path, data: dict[str, int]) -> None:
    path.write_text("".join(f"{word}\t{data[word]}\n" for word in sorted(data)))


def main() -> None:
    DATA.mkdir(parents=True, exist_ok=True)
    write_tsv(DATA / "afinn-en-165.tsv", afinn_single_word_scores())
    write_tsv(DATA / "domain_overrides.tsv", dict(Lexicon.DOMAIN_OVERRIDES))
    print(f"wrote {DATA}/afinn-en-165.tsv + domain_overrides.tsv")


if __name__ == "__main__":
    main()
