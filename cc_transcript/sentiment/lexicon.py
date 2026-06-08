# spaCy + afinn are optional ([lexicon]) and ship no/partial type info; their
# unknown-type and missing-import noise is suppressed here, not project-wide.
# pyright: reportUnknownMemberType=false, reportUnknownVariableType=false, reportUnknownParameterType=false, reportUnknownArgumentType=false, reportMissingImports=false, reportMissingModuleSource=false
from __future__ import annotations

import asyncio
import contextlib
import subprocess
import sys
import warnings
from pathlib import Path
from typing import TYPE_CHECKING, ClassVar

import anyio.to_thread

if TYPE_CHECKING:
    import spacy.language
    from afinn import Afinn

MODEL_NAME = "en_core_web_sm"
DISABLED_PIPES = ["parser"]
SPACY_CACHE_DIR = Path.home() / ".cache" / "spacy"
SPACY_MODEL_VERSION = "3.8.0"


class Lexicon:
    DOMAIN_OVERRIDES: ClassVar[dict[str, int]] = {
        "stop": -3, "halt": -3, "quit": -3, "cease": -3,
        "guess": -2, "guessing": -2,
        "continue": 2, "proceed": 2, "resume": 2,
        "break": -2, "nope": -2, "broken": -3, "garbage": -3,
        "nightmare": -3, "absurd": -2, "bug": -2, "hang": -2,
        "freeze": -2, "slow": -2, "trash": -3, "regression": -2,
        "flaky": -2, "impossible": -2, "incorrect": -2,
        "exactly": 2, "finally": 2, "incredible": 3, "smooth": 2,
        "neat": 2, "magic": 2, "work": 2, "correct": 2, "solve": 2,
        "fix": 2, "done": 2, "ship": 2, "crisp": 2, "tight": 2,
    }
    MIN_MAGNITUDE: ClassVar[int] = 2
    afinn: ClassVar[Afinn | None] = None
    locks_by_loop: ClassVar[dict[int, asyncio.Lock]] = {}

    @classmethod
    async def ensure_ready(cls) -> None:
        if cls.afinn is not None:
            return
        loop_id = id(asyncio.get_running_loop())
        lock = cls.locks_by_loop.setdefault(loop_id, asyncio.Lock())
        async with lock:
            if cls.afinn is None:
                cls.afinn = await anyio.to_thread.run_sync(cls.build)

    @staticmethod
    def build() -> Afinn:
        # Lazy + warning-suppressed: afinn is an optional ([lexicon]) dependency and
        # emits a SyntaxWarning on import, so it must not load at module import time.
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", SyntaxWarning)
            from afinn import Afinn

        return Afinn(language="en", emoticons=False)

    @classmethod
    def polarity(cls, lemma: str) -> int:
        lower = lemma.lower()
        if (override := cls.DOMAIN_OVERRIDES.get(lower)) is not None:
            return override
        assert cls.afinn is not None, "Lexicon.ensure_ready() must be awaited at startup"
        score = int(cls.afinn.score(lower))
        return score if abs(score) >= cls.MIN_MAGNITUDE else 0


class NLP:
    model: ClassVar[spacy.language.Language | None] = None
    failed: ClassVar[bool] = False
    last_download_output: ClassVar[str | None] = None
    locks_by_loop: ClassVar[dict[int, asyncio.Lock]] = {}

    @classmethod
    def get(cls) -> spacy.language.Language | None:
        return cls.model

    @classmethod
    async def ensure_ready(cls) -> spacy.language.Language | None:
        if cls.model is not None:
            return cls.model
        if cls.failed:
            return None
        loop_id = id(asyncio.get_running_loop())
        lock = cls.locks_by_loop.setdefault(loop_id, asyncio.Lock())
        async with lock:
            if cls.model is None and not cls.failed:
                try:
                    cls.model = await anyio.to_thread.run_sync(cls.load_or_download)
                except (OSError, subprocess.CalledProcessError, ImportError, RuntimeError) as exc:
                    cls.failed = True
                    cls.last_download_output = cls.format_failure(exc)
        return cls.model

    @staticmethod
    def format_failure(exc: BaseException) -> str:
        match exc:
            case subprocess.CalledProcessError():
                out = (exc.stdout or "").strip()
                err = (exc.stderr or "").strip()
                return f"exit={exc.returncode} stdout={out!r} stderr={err!r}"
            case _:
                return f"{exc.__class__.__name__}: {exc}"

    @staticmethod
    def load_or_download() -> spacy.language.Language:
        import spacy

        cache_str = str(SPACY_CACHE_DIR)
        if (SPACY_CACHE_DIR / MODEL_NAME).is_dir() and cache_str not in sys.path:
            sys.path.insert(0, cache_str)

        with contextlib.suppress(OSError):
            return spacy.load(MODEL_NAME, disable=DISABLED_PIPES)

        SPACY_CACHE_DIR.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["uvx", "spacy", "download", f"{MODEL_NAME}-{SPACY_MODEL_VERSION}", "--direct", "--target", cache_str],
            check=True,
            capture_output=True,
            text=True,
        )
        if cache_str not in sys.path:
            sys.path.insert(0, cache_str)
        return spacy.load(MODEL_NAME, disable=DISABLED_PIPES)


__all__ = ["NLP", "Lexicon"]
