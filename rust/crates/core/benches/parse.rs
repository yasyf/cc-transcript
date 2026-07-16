//! v14 P0 baseline benches: parse, parse+filter, session-activity over the corpus at
//! `CC_BENCH_CORPUS` (default `../../../.fixtures/corpus`; build via scripts/gen_corpus.py).

use std::fs;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use cc_transcript_core::activity::{lift_session, session_activity, ActivityOpts};
use cc_transcript_core::filter::{compile_spec, spec_keep, SIGNAL_SPEC_JSON};
use cc_transcript_core::parse::parse_bytes;

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

fn largest_corpus_file() -> Vec<u8> {
    let dir = corpus_dir();
    let mut files = Vec::new();
    collect_jsonl(&dir, &mut files);
    let largest = files
        .into_iter()
        .max_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .unwrap_or_else(|| {
            panic!(
                "no .jsonl transcripts under {} — run: uv run --no-sync python scripts/gen_corpus.py",
                dir.display()
            )
        });
    fs::read(&largest)
        .unwrap_or_else(|e| panic!("failed to read corpus file {}: {e}", largest.display()))
}

fn bench_parse(c: &mut Criterion) {
    let bytes = largest_corpus_file();
    let spec = compile_spec(SIGNAL_SPEC_JSON).expect("filter spec compiles");
    let mut group = c.benchmark_group("parse");
    group.sample_size(20);
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("parse_corpus", |b| {
        b.iter(|| parse_bytes(&bytes, |_| true).expect("corpus parses"))
    });
    group.bench_function("filter_spec", |b| {
        b.iter(|| parse_bytes(&bytes, |entry| spec_keep(&spec, entry)).expect("corpus parses"))
    });
    group.finish();
}

fn bench_activity(c: &mut Criterion) {
    let bytes = largest_corpus_file();
    let entries = parse_bytes(&bytes, |_| true).expect("corpus parses");
    let opts = ActivityOpts::default();
    let session_id = entries
        .first()
        .and_then(|e| e.session_id())
        .unwrap_or("")
        .to_string();
    let mut group = c.benchmark_group("activity");
    group.sample_size(20);
    group.throughput(Throughput::Elements(entries.len() as u64));
    group.bench_function("session_activity", |b| {
        b.iter(|| session_activity(&entries, &opts))
    });
    group.bench_function("full_lift", |b| {
        b.iter(|| lift_session(&session_id, &entries))
    });
    group.finish();
}

criterion_group!(benches, bench_parse, bench_activity);
criterion_main!(benches);
