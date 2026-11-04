//! cli.py `stats`.

use std::path::PathBuf;

use cc_transcript_core::facts::{tool_facts, ToolFact};
use cc_transcript_core::render::{
    collect_stats, keep_slowest, render_stats, slowest_tools, stats_json, transcript_header, Json,
    SlowTool, Stats, StatsFold,
};

use crate::output::{CliExit, Out};
use crate::target::{
    py_path, require_files, resolve_targets, scope_note, stream_transcripts, Parsed,
};
use crate::DiscoveryOpts;

fn slowest(parsed: &Parsed) -> Vec<SlowTool> {
    let facts: Vec<ToolFact> = match &parsed.session_id {
        Some(session) => tool_facts(session, &parsed.path.to_string_lossy(), &parsed.entries),
        None => Vec::new(),
    };
    slowest_tools(&facts)
}

fn file_stats(parsed: &Parsed) -> Stats {
    let mut stats = collect_stats(std::slice::from_ref(&parsed.entries));
    stats.slowest_tools = slowest(parsed);
    stats
}

pub fn run(
    paths: &[PathBuf],
    discovery: &DiscoveryOpts,
    per_file: bool,
    json: bool,
) -> Result<(), CliExit> {
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|p| py_path(&p.to_string_lossy()))
        .collect();
    require_files(
        &paths,
        "'[PATHS]...'",
        "cc-transcript stats [OPTIONS] [PATHS]...",
        "cc-transcript stats",
    )?;
    let targets = resolve_targets(
        &paths,
        &discovery.validated_root(
            "cc-transcript stats [OPTIONS] [PATHS]...",
            "cc-transcript stats",
        )?,
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
        discovery.effective_limit(),
        None,
        None,
    )?;
    let mut out = Out::new();
    if per_file {
        stream_transcripts(
            &targets.paths,
            |parsed| (parsed.path.clone(), file_stats(&parsed)),
            |(path, stats)| {
                if json {
                    let Json::Obj(mut pairs) = stats_json(&stats) else {
                        unreachable!("stats_json returns an object")
                    };
                    pairs.insert(
                        0,
                        (
                            "path".into(),
                            Json::Str(path.to_string_lossy().into_owned()),
                        ),
                    );
                    out.line(&Json::Obj(pairs).dumps())?;
                } else {
                    out.line(&transcript_header(&path.to_string_lossy()))?;
                    out.line(&render_stats(&stats))?;
                    out.line("")?;
                }
                out.finish()
            },
        )?;
    } else {
        let mut fold = StatsFold::default();
        let mut slowest_seen = Vec::new();
        stream_transcripts(
            &targets.paths,
            |parsed| (StatsFold::of(&parsed.entries), slowest(&parsed)),
            |(file, tools)| {
                fold.merge(file);
                slowest_seen.extend(tools);
                keep_slowest(&mut slowest_seen);
                Ok(())
            },
        )?;
        let mut stats = fold.finish();
        stats.slowest_tools = slowest_seen;
        out.line(&match json {
            true => stats_json(&stats).dumps(),
            false => render_stats(&stats),
        })?;
    }
    if !json {
        if let Some(note) = scope_note(&targets) {
            out.line(&note)?;
        }
    }
    out.finish()
}
