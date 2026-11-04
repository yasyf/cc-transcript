//! cli.py `tools`, `commands`, `permissions`, and `mcp` — the fact-projection commands.

use std::path::PathBuf;

use cc_transcript_core::facts::{tool_facts, McpTally, Tally, ToolFact};
use cc_transcript_core::query::FileRef;
use cc_transcript_core::render::{
    denial_json, denial_line, fact_json, fact_line, render_counts, render_mcp, Json,
};
use cc_transcript_core::toolcall::tool_name_matches;
use chrono::{DateTime, FixedOffset};

use crate::output::{CliExit, Out};
use crate::target::{
    py_path, require_files, resolve_targets, scope_note, stream_transcripts, Parsed, Targets,
};
use crate::DiscoveryOpts;

fn targets(
    paths: &[PathBuf],
    discovery: &DiscoveryOpts,
    name: &str,
    min_mtime: Option<f64>,
    until: Option<DateTime<FixedOffset>>,
) -> Result<Targets, CliExit> {
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|p| py_path(&p.to_string_lossy()))
        .collect();
    let usage = format!("cc-transcript {name} [OPTIONS] [PATHS]...");
    let help_path = format!("cc-transcript {name}");
    require_files(&paths, "'[PATHS]...'", &usage, &help_path)?;
    resolve_targets(
        &paths,
        &discovery.validated_root(&usage, &help_path)?,
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
        discovery.effective_limit(),
        min_mtime,
        until,
    )
}

fn facts_of(parsed: &Parsed) -> Vec<ToolFact> {
    match &parsed.session_id {
        Some(session) => tool_facts(session, &parsed.path.to_string_lossy(), &parsed.entries),
        None => Vec::new(),
    }
}

fn stream_lines(
    targets: &Targets,
    out: &mut Out,
    lines_of: impl Fn(Vec<ToolFact>) -> Vec<String> + Sync,
) -> Result<(), CliExit> {
    stream_transcripts(
        &targets.paths,
        |parsed| lines_of(facts_of(&parsed)),
        |lines| {
            out.lines(lines)?;
            out.finish()
        },
    )
}

fn emit_note(out: &mut Out, targets: &Targets) -> Result<(), CliExit> {
    if let Some(note) = scope_note(targets) {
        out.line(&note)?;
    }
    Ok(())
}

pub fn tools(
    paths: &[PathBuf],
    discovery: &DiscoveryOpts,
    tool: Option<&str>,
    file: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    json: bool,
) -> Result<(), CliExit> {
    let usage = "cc-transcript tools [OPTIONS] [PATHS]...";
    let help_path = "cc-transcript tools";
    let now = chrono::Local::now();
    let since_ts = since
        .map(|value| crate::timearg::parse_time("--since", value, now, usage, help_path))
        .transpose()?;
    let until_ts = until
        .map(|value| crate::timearg::parse_time("--until", value, now, usage, help_path))
        .transpose()?;
    let min_mtime = since_ts.map(|ts| ts.timestamp() as f64);
    let targets = targets(paths, discovery, "tools", min_mtime, until_ts)?;
    let mut out = Out::new();
    stream_lines(&targets, &mut out, |facts| {
        facts
            .iter()
            .filter(|fact| tool.is_none_or(|t| tool_name_matches(&fact.tool, t)))
            .filter(|fact| {
                file.is_none_or(|glob| {
                    fact.file_paths
                        .iter()
                        .any(|p| FileRef::new(p).matches(&[glob]))
                })
            })
            .filter(|fact| since_ts.is_none_or(|s| fact.ts >= s))
            .filter(|fact| until_ts.is_none_or(|u| fact.ts < u))
            .map(|fact| match json {
                true => fact_json(fact).dumps(),
                false => fact_line(fact),
            })
            .collect()
    })?;
    if !json {
        emit_note(&mut out, &targets)?;
    }
    out.finish()
}

pub fn commands(paths: &[PathBuf], discovery: &DiscoveryOpts, json: bool) -> Result<(), CliExit> {
    let targets = targets(paths, discovery, "commands", None, None)?;
    let mut tally = Tally::default();
    stream_transcripts(
        &targets.paths,
        |parsed| {
            facts_of(&parsed)
                .into_iter()
                .flat_map(|fact| fact.command_prefixes)
                .collect::<Vec<String>>()
        },
        |prefixes| {
            prefixes.into_iter().for_each(|prefix| tally.add(prefix));
            Ok(())
        },
    )?;
    let counts = tally.most_common();
    let mut out = Out::new();
    if json {
        for (prefix, count) in &counts {
            out.line(
                &Json::Obj(vec![
                    ("prefix".into(), Json::Str(prefix.clone())),
                    ("count".into(), Json::UInt(*count as u64)),
                ])
                .dumps(),
            )?;
        }
        return out.finish();
    }
    for line in render_counts(&counts) {
        out.line(&line)?;
    }
    emit_note(&mut out, &targets)?;
    out.finish()
}

pub fn permissions(
    paths: &[PathBuf],
    discovery: &DiscoveryOpts,
    json: bool,
) -> Result<(), CliExit> {
    let targets = targets(paths, discovery, "permissions", None, None)?;
    let mut out = Out::new();
    stream_lines(&targets, &mut out, |facts| {
        facts
            .iter()
            .filter(|fact| fact.denied)
            .filter(|fact| !tool_name_matches(&fact.tool, "ExitPlanMode|AskUserQuestion"))
            .map(|fact| match json {
                true => denial_json(fact).dumps(),
                false => denial_line(fact),
            })
            .collect()
    })?;
    if !json {
        emit_note(&mut out, &targets)?;
    }
    out.finish()
}

pub fn mcp(paths: &[PathBuf], discovery: &DiscoveryOpts, json: bool) -> Result<(), CliExit> {
    let targets = targets(paths, discovery, "mcp", None, None)?;
    let mut tally = McpTally::default();
    stream_transcripts(
        &targets.paths,
        |parsed| {
            let mut facts = facts_of(&parsed);
            facts.retain(|fact| fact.mcp_server.is_some());
            facts
        },
        |facts| {
            facts.iter().for_each(|fact| tally.add(fact));
            Ok(())
        },
    )?;
    let summary = tally.summary();
    let mut out = Out::new();
    if json {
        for (server, data) in &summary {
            out.line(
                &Json::Obj(vec![
                    ("server".into(), Json::Str(server.clone())),
                    ("read".into(), Json::UInt(data.read as u64)),
                    ("write".into(), Json::UInt(data.write as u64)),
                    ("total".into(), Json::UInt(data.total as u64)),
                    (
                        "tools".into(),
                        Json::Obj(
                            data.tools
                                .iter()
                                .map(|(tool, count)| (tool.clone(), Json::UInt(*count as u64)))
                                .collect(),
                        ),
                    ),
                ])
                .dumps(),
            )?;
        }
        return out.finish();
    }
    for line in render_mcp(&summary) {
        out.line(&line)?;
    }
    emit_note(&mut out, &targets)?;
    out.finish()
}
