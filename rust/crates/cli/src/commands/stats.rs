//! cli.py `stats`.

use std::path::PathBuf;

use cc_transcript_core::facts::{tool_facts, ToolFact};
use cc_transcript_core::render::{
    collect_stats, render_stats, slowest_tools, stats_json, transcript_header, Json, SlowTool,
};

use crate::output::{CliExit, Out};
use crate::target::{
    parse_transcripts, py_path, require_files, resolve_targets, scope_note, Parsed,
};
use crate::DiscoveryOpts;

fn slowest(parsed: &[Parsed]) -> Vec<SlowTool> {
    let facts: Vec<ToolFact> = parsed
        .iter()
        .flat_map(|transcript| match &transcript.session_id {
            Some(session) => tool_facts(
                session,
                &transcript.path.to_string_lossy(),
                &transcript.entries,
            ),
            None => Vec::new(),
        })
        .collect();
    slowest_tools(&facts)
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
    )?;
    let transcripts = parse_transcripts(&targets.paths);
    let mut out = Out::new();
    match (per_file, json) {
        (true, true) => {
            for parsed in &transcripts {
                let mut stats = collect_stats(std::slice::from_ref(&parsed.entries));
                stats.slowest_tools = slowest(std::slice::from_ref(parsed));
                let Json::Obj(mut pairs) = stats_json(&stats) else {
                    unreachable!("stats_json returns an object")
                };
                pairs.insert(
                    0,
                    (
                        "path".into(),
                        Json::Str(parsed.path.to_string_lossy().into_owned()),
                    ),
                );
                out.line(&Json::Obj(pairs).dumps())?;
            }
        }
        (true, false) => {
            for parsed in &transcripts {
                let mut stats = collect_stats(std::slice::from_ref(&parsed.entries));
                stats.slowest_tools = slowest(std::slice::from_ref(parsed));
                out.line(&transcript_header(&parsed.path.to_string_lossy()))?;
                out.line(&render_stats(&stats))?;
                out.line("")?;
            }
            if let Some(note) = scope_note(&targets) {
                out.line(&note)?;
            }
        }
        (false, true) => {
            let slowest_tools = slowest(&transcripts);
            let entries: Vec<_> = transcripts.into_iter().map(|p| p.entries).collect();
            let mut stats = collect_stats(&entries);
            stats.slowest_tools = slowest_tools;
            out.line(&stats_json(&stats).dumps())?;
        }
        (false, false) => {
            let slowest_tools = slowest(&transcripts);
            let entries: Vec<_> = transcripts.into_iter().map(|p| p.entries).collect();
            let mut stats = collect_stats(&entries);
            stats.slowest_tools = slowest_tools;
            out.line(&render_stats(&stats))?;
            if let Some(note) = scope_note(&targets) {
                out.line(&note)?;
            }
        }
    }
    out.finish()
}
