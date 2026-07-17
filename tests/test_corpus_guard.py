from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from scripts.gen_corpus import DEFAULT_SEED, REPO_ROOT, generate, process_alive
from tests.corpus import CORPUS_MANIFEST, corpus_current


@pytest.fixture
def fresh_corpus(tmp_path: Path) -> Path:
    out = tmp_path / "corpus"
    generate(out, DEFAULT_SEED)
    return out


def test_manifest_matches_the_generator(fresh_corpus: Path) -> None:
    assert frozenset(str(p.relative_to(fresh_corpus)) for p in fresh_corpus.rglob("*.jsonl")) == CORPUS_MANIFEST


def test_guard_accepts_a_current_corpus(fresh_corpus: Path) -> None:
    assert corpus_current(fresh_corpus)


def test_guard_rebuilds_when_absent(tmp_path: Path) -> None:
    assert not corpus_current(tmp_path / "corpus")


def test_guard_rebuilds_on_a_missing_file(fresh_corpus: Path) -> None:
    next(iter(sorted(fresh_corpus.rglob("*.jsonl")))).unlink()
    assert not corpus_current(fresh_corpus)


def test_guard_rebuilds_on_a_count_preserving_stray(fresh_corpus: Path) -> None:
    sorted(fresh_corpus.rglob("*.jsonl"))[0].rename(fresh_corpus / "adversarial-stray.jsonl")
    assert len(list(fresh_corpus.rglob("*.jsonl"))) == len(CORPUS_MANIFEST)
    assert not corpus_current(fresh_corpus)


def test_guard_rebuilds_on_an_extra_file(fresh_corpus: Path) -> None:
    (fresh_corpus / "-Users-dev-Code-scratch" / "extra.jsonl").write_bytes(b"{}\n")
    assert not corpus_current(fresh_corpus)


def test_regenerate_prunes_a_stray_back_to_the_manifest(fresh_corpus: Path) -> None:
    sorted(fresh_corpus.rglob("*.jsonl"))[0].rename(fresh_corpus / "adversarial-stray.jsonl")
    generate(fresh_corpus, DEFAULT_SEED)
    assert corpus_current(fresh_corpus)


def test_regenerate_prunes_dead_pid_staging_and_keeps_live(fresh_corpus: Path) -> None:
    reaped = subprocess.Popen([sys.executable, "-c", "pass"])
    reaped.wait()
    assert not process_alive(reaped.pid)
    dead = fresh_corpus / f".gone.jsonl.{reaped.pid}.tmp"
    live = fresh_corpus / f".gone.jsonl.{os.getpid()}.tmp"
    dead.write_bytes(b"x")
    live.write_bytes(b"x")
    generate(fresh_corpus, DEFAULT_SEED)
    assert not dead.exists()
    assert live.exists()


def test_concurrent_same_seed_generation_is_stale_prune_safe(tmp_path: Path) -> None:
    out = tmp_path / "corpus"
    (out / "-Users-dev-Code-scratch").mkdir(parents=True)
    for i in range(300):
        (out / "-Users-dev-Code-scratch" / f"stale-{i}.jsonl").write_bytes(b"stale")
    cmd = [sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py"), "--out", str(out), "--seed", str(DEFAULT_SEED)]
    procs = [subprocess.Popen(cmd, cwd=REPO_ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE) for _ in range(2)]
    errors = [proc.communicate()[1].decode() for proc in procs]
    assert all(proc.returncode == 0 for proc in procs), errors
    assert corpus_current(out)
