"""End-to-end Python timing harness for the v14 P0 baseline.

Times the public parse/stream, activity-probe, mining, and post-parse-filter paths
over the synthetic corpus (``scripts/gen_corpus.py``), each ``--runs`` times, and
reports the minimum. Also times CLI cold start (``--help`` and a real ``stats`` run
over the corpus) with a ``--cold-runs`` subprocess timer — the fallback for when
``hyperfine`` is absent. The computation is deterministic and never touches the
network; only wall-clock timings vary between runs. Emits one JSON object to stdout.

Run: ``uv run --no-sync python scripts/bench_e2e.py``
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path
from time import perf_counter
from typing import Callable

import anyio
import orjson

from cc_transcript.activity_probe import session_activity_probe
from cc_transcript.filterspec import apply_spec
from cc_transcript.mining import MiningSpec, mine
from cc_transcript.parser import TranscriptParser
from cc_transcript.render import collect_stats

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CORPUS = REPO_ROOT / ".fixtures" / "corpus"


def corpus_files(corpus: Path) -> list[Path]:
    files = sorted(corpus.rglob("*.jsonl"))
    if not files:
        raise SystemExit(f"no transcripts under {corpus} — run: uv run --no-sync python scripts/gen_corpus.py")
    return files


def timed(fn: Callable[[], object], runs: int) -> dict[str, object]:
    runs_s: list[float] = []
    for _ in range(runs):
        start = perf_counter()
        fn()
        runs_s.append(round(perf_counter() - start, 6))
    return {"min_s": min(runs_s), "runs_s": runs_s}


def stream_stats(pairs: list[tuple[Path, float]]) -> int:
    async def collect() -> int:
        transcripts = [parsed async for parsed in TranscriptParser.stream_transcripts(pairs)]
        return collect_stats(transcripts).events

    return anyio.run(collect)


def bench_e2e(corpus: Path, runs: int) -> dict[str, object]:
    from cc_transcript.builders import build_spec, drop_junk, drop_synthetic, keep_only

    files = corpus_files(corpus)
    pairs = [(path, path.stat().st_mtime) for path in files]
    parsed = [TranscriptParser.parse_file(path) for path in files]
    spec = build_spec(keep_only("user", "assistant"), drop_junk("structural"), drop_synthetic())
    mining_spec = MiningSpec()
    return {
        "stream_stats": timed(lambda: stream_stats(pairs), runs),
        "probe_sweep": timed(lambda: [session_activity_probe(path) for path in files], runs),
        "mine": timed(lambda: [len(list(mine(events, mining_spec))) for events in parsed], runs),
        "post_filter": timed(lambda: [len(list(apply_spec(events, spec))) for events in parsed], runs),
    }


def cli_bin() -> str:
    return str(Path(sys.executable).parent / "cc-transcript")


def time_subprocess(argv: list[str], runs: int) -> dict[str, object]:
    def once() -> None:
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    result = timed(once, runs)
    return {"cmd": " ".join(argv), **result, "method": f"perf_counter subprocess x{runs}"}


def bench_cli_cold_start(corpus: Path, cold_runs: int) -> dict[str, object]:
    binary = cli_bin()
    return {
        "help": time_subprocess([binary, "--help"], cold_runs),
        "stats": time_subprocess([binary, "stats", "--root", str(corpus), "--all"], cold_runs),
    }


def run_all(corpus: Path, *, runs: int, cold_runs: int) -> dict[str, object]:
    files = corpus_files(corpus)
    return {
        "corpus": {
            "dir": str(corpus),
            "files": len(files),
            "total_bytes": sum(path.stat().st_size for path in files),
        },
        "e2e": bench_e2e(corpus, runs),
        "cli_cold_start": bench_cli_cold_start(corpus, cold_runs),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="End-to-end Python baseline timings over the corpus.")
    parser.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS, help="Corpus directory.")
    parser.add_argument("--runs", type=int, default=3, help="Timed runs per e2e bench (report min).")
    parser.add_argument("--cold-runs", type=int, default=10, help="Subprocess runs per CLI cold-start bench.")
    args = parser.parse_args()
    sys.stdout.buffer.write(orjson.dumps(run_all(args.corpus, runs=args.runs, cold_runs=args.cold_runs), option=orjson.OPT_INDENT_2))
    sys.stdout.buffer.write(b"\n")


if __name__ == "__main__":
    main()
