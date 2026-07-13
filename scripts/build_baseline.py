"""Assemble ``BASELINE.json`` — the v14 P0 performance baseline, recorded before any
code moves so the rewrite has a number to beat.

Regenerates the corpus, runs ``cargo bench`` and reads the criterion medians, runs
``scripts/bench_e2e.py``, times CLI cold start (``hyperfine`` when present, else the
subprocess fallback baked into ``bench_e2e.py``), and writes ``BASELINE.json`` at the
repo root with machine and corpus descriptors.

Run: ``env PATH=$HOME/.cargo/bin:$PATH uv run --no-sync python scripts/build_baseline.py``
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import orjson

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gen_corpus  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS = REPO_ROOT / ".fixtures" / "corpus"
CLI = Path(".venv") / "bin" / "cc-transcript"
BENCHES = (("parse", "parse_corpus"), ("parse", "filter_spec"), ("activity", "session_activity"))
MINING_RATIONALE = (
    "mining has no pure-Rust entry pre-v14; baselined e2e through pyo3, which is "
    "exactly the number the v14 native-handle mining path must beat."
)


def cargo_env() -> dict[str, str]:
    cargo_bin = str(Path.home() / ".cargo" / "bin")
    return os.environ | {
        "PATH": f"{cargo_bin}:{os.environ['PATH']}",
        "PYO3_PYTHON": str(REPO_ROOT / ".venv" / "bin" / "python"),
        "CC_BENCH_CORPUS": str(CORPUS),
    }


def run_cargo_bench() -> None:
    subprocess.run(
        ["cargo", "bench", "-p", "cc_transcript_parser"],
        cwd=REPO_ROOT,
        env=cargo_env(),
        check=True,
    )


def criterion_dir() -> Path:
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "-q"],
        cwd=REPO_ROOT,
        env=cargo_env(),
        capture_output=True,
        check=True,
    )
    return Path(orjson.loads(proc.stdout)["target_directory"]) / "criterion"


def largest_file_bytes() -> int:
    return max(path.stat().st_size for path in CORPUS.rglob("*.jsonl"))


def criterion_medians() -> dict[str, dict[str, float]]:
    big = largest_file_bytes()
    root = criterion_dir()
    out: dict[str, dict[str, float]] = {}
    for group, name in BENCHES:
        estimates = root / group / name / "new" / "estimates.json"
        median_ns = orjson.loads(estimates.read_bytes())["median"]["point_estimate"]
        seconds = median_ns / 1e9
        entry = {"median_ns": median_ns, "median_ms": round(median_ns / 1e6, 4)}
        if group == "parse":
            entry |= {"mib_per_s": round(big / seconds / 1024 / 1024, 2), "mb_per_s": round(big / seconds / 1e6, 2)}
        out[f"{group}/{name}"] = entry
    return out


def run_bench_e2e() -> dict[str, object]:
    proc = subprocess.run(
        [sys.executable, str(REPO_ROOT / "scripts" / "bench_e2e.py")],
        cwd=REPO_ROOT,
        capture_output=True,
        check=True,
    )
    return orjson.loads(proc.stdout)


def hyperfine_cold_start() -> dict[str, object] | None:
    if shutil.which("hyperfine") is None:
        return None
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as handle:
        export = Path(handle.name)
    subprocess.run(
        [
            "hyperfine",
            "--warmup",
            "2",
            "--export-json",
            str(export),
            f"{CLI} --help",
            f"{CLI} stats --root .fixtures/corpus --all",
        ],
        cwd=REPO_ROOT,
        env=os.environ | {"TZ": "UTC"},
        check=True,
    )
    results = orjson.loads(export.read_bytes())["results"]
    export.unlink()
    labels = ("help", "stats")
    return {
        "method": "hyperfine --warmup 2",
        **{
            label: {"cmd": r["command"], "mean_s": round(r["mean"], 6), "min_s": round(r["min"], 6)}
            for label, r in zip(labels, results, strict=True)
        },
    }


def machine() -> dict[str, object]:
    cpu = platform.processor()
    if sys.platform == "darwin":
        brand = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True)
        cpu = brand.stdout.strip() or cpu
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu": cpu,
        "cores": os.cpu_count(),
        "python": platform.python_version(),
    }


def build(*, skip_bench: bool) -> dict[str, object]:
    gen_corpus.generate(CORPUS, gen_corpus.DEFAULT_SEED)
    if not skip_bench:
        run_cargo_bench()
    files = sorted(CORPUS.rglob("*.jsonl"))
    e2e = run_bench_e2e()
    cold = hyperfine_cold_start() or e2e["cli_cold_start"]
    return {
        "machine": machine(),
        "corpus": {
            "files": len(files),
            "total_bytes": sum(path.stat().st_size for path in files),
            "largest_file_bytes": largest_file_bytes(),
            "seed": gen_corpus.DEFAULT_SEED,
        },
        "criterion_medians": criterion_medians(),
        "bench_e2e": e2e["e2e"],
        "cli_cold_start": cold,
        "mining_rationale": MINING_RATIONALE,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Assemble BASELINE.json.")
    parser.add_argument("--skip-bench", action="store_true", help="Reuse existing criterion results (skip cargo bench).")
    args = parser.parse_args()
    baseline = build(skip_bench=args.skip_bench)
    (REPO_ROOT / "BASELINE.json").write_bytes(orjson.dumps(baseline, option=orjson.OPT_INDENT_2))
    print(f"wrote BASELINE.json ({(REPO_ROOT / 'BASELINE.json').stat().st_size} bytes)")


if __name__ == "__main__":
    main()
