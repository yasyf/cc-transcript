//! Bench for `collect_stats` over the corpus — the CLI `stats` hot path, one aggregate
//! pass over every parsed event. Corpus at `CC_BENCH_CORPUS` (default
//! `../../../.fixtures/corpus`; build via scripts/gen_corpus.py).

use std::fs;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use cc_transcript_core::parse::parse_bytes;
use cc_transcript_core::render::collect_stats;
use cc_transcript_core::types::Entry;

fn corpus_dir() -> PathBuf {
    std::env::var_os("CC_BENCH_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../../.fixtures/corpus"))
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn load_corpus() -> Vec<Vec<Entry>> {
    let dir = corpus_dir();
    let mut files = Vec::new();
    collect_jsonl(&dir, &mut files);
    files.sort();
    files
        .iter()
        .filter_map(|path| parse_bytes(&fs::read(path).ok()?, |_| true).ok())
        .collect()
}

fn bench_render_stats(c: &mut Criterion) {
    let transcripts = load_corpus();
    let events: usize = transcripts.iter().map(Vec::len).sum();
    assert!(
        events > 0,
        "no events under corpus — run: uv run --no-sync python scripts/gen_corpus.py"
    );
    eprintln!(
        "render_stats bench: {} transcripts, {events} events",
        transcripts.len()
    );
    let mut group = c.benchmark_group("render");
    group.sample_size(20);
    group.throughput(Throughput::Elements(events as u64));
    group.bench_function("render_stats", |b| {
        b.iter(|| std::hint::black_box(collect_stats(&transcripts)))
    });
    group.finish();
}

criterion_group!(benches, bench_render_stats);
criterion_main!(benches);
