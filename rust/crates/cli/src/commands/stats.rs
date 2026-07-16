//! cli.py `stats`.

use std::path::PathBuf;

use cc_transcript_core::render::{
    collect_stats, render_stats, stats_json, transcript_header, Json,
};

use crate::output::{CliExit, Out};
use crate::target::{parse_transcripts, py_path, require_files, resolve_targets, scope_note};
use crate::DiscoveryOpts;

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
        &discovery.root(),
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
        discovery.effective_limit(),
    )?;
    let transcripts = parse_transcripts(&targets.paths);
    let mut out = Out::new();
    match (per_file, json) {
        (true, true) => {
            for parsed in &transcripts {
                let stats = collect_stats(std::slice::from_ref(&parsed.entries));
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
                let stats = collect_stats(std::slice::from_ref(&parsed.entries));
                out.line(&transcript_header(&parsed.path.to_string_lossy()))?;
                out.line(&render_stats(&stats))?;
                out.line("")?;
            }
            if let Some(note) = scope_note(&targets) {
                out.line(&note)?;
            }
        }
        (false, true) => {
            let entries: Vec<_> = transcripts.into_iter().map(|p| p.entries).collect();
            out.line(&stats_json(&collect_stats(&entries)).dumps())?;
        }
        (false, false) => {
            let entries: Vec<_> = transcripts.into_iter().map(|p| p.entries).collect();
            out.line(&render_stats(&collect_stats(&entries)))?;
            if let Some(note) = scope_note(&targets) {
                out.line(&note)?;
            }
        }
    }
    out.finish()
}
