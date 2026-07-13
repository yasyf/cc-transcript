//! Bench for the typed tool-call parse over every corpus tool_use input — the hot
//! path fired per tool-use across hooks, mining, and the activity lift. Corpus at
//! `CC_BENCH_CORPUS` (default `../../../.fixtures/corpus`; build via scripts/gen_corpus.py).

use std::fs;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use sonic_rs::Value;

use cc_transcript_core::parse::parse_bytes;
use cc_transcript_core::toolcall::parse_tool_call;

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

fn collect_tool_uses() -> Vec<(String, Value)> {
    let dir = corpus_dir();
    let mut files = Vec::new();
    collect_jsonl(&dir, &mut files);
    let mut out = Vec::new();
    for path in files {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(entries) = parse_bytes(&bytes, |_| true) else {
            continue;
        };
        for entry in &entries {
            for tu in entry.tool_uses() {
                out.push((tu.name.clone(), tu.input.clone()));
            }
        }
    }
    out
}

fn bench_toolcall(c: &mut Criterion) {
    let uses = collect_tool_uses();
    assert!(
        !uses.is_empty(),
        "no tool_use blocks under corpus — run: uv run --no-sync python scripts/gen_corpus.py"
    );
    eprintln!("toolcall bench: {} tool_use inputs", uses.len());
    let mut group = c.benchmark_group("toolcall");
    group.sample_size(20);
    group.throughput(Throughput::Elements(uses.len() as u64));
    group.bench_function("toolcall_parse", |b| {
        b.iter(|| {
            for (name, input) in &uses {
                std::hint::black_box(parse_tool_call(name, input));
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_toolcall);
criterion_main!(benches);
