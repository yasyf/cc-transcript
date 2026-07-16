//! cli.py `tools`, `commands`, `permissions`, and `mcp` — the fact-projection commands.

use std::path::PathBuf;

use cc_transcript_core::facts::{command_prefix_counts, mcp_summary, tool_facts, ToolFact};
use cc_transcript_core::render::{
    denial_json, denial_line, fact_json, fact_line, render_counts, render_mcp, Json,
};
use cc_transcript_core::toolcall::tool_name_matches;

use crate::output::{CliExit, Out};
use crate::target::{
    parse_transcripts, py_path, require_files, resolve_targets, scope_note, Targets,
};
use crate::DiscoveryOpts;

fn gather(
    paths: &[PathBuf],
    discovery: &DiscoveryOpts,
    name: &str,
) -> Result<(Targets, Vec<ToolFact>), CliExit> {
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|p| py_path(&p.to_string_lossy()))
        .collect();
    require_files(
        &paths,
        "'[PATHS]...'",
        &format!("cc-transcript {name} [OPTIONS] [PATHS]..."),
        &format!("cc-transcript {name}"),
    )?;
    let targets = resolve_targets(
        &paths,
        &discovery.root(),
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
        discovery.effective_limit(),
    )?;
    let facts: Vec<ToolFact> = parse_transcripts(&targets.paths)
        .iter()
        .flat_map(|parsed| {
            tool_facts(
                &parsed.session_id,
                &parsed.path.to_string_lossy(),
                &parsed.entries,
            )
        })
        .collect();
    Ok((targets, facts))
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
    json: bool,
) -> Result<(), CliExit> {
    let (targets, facts) = gather(paths, discovery, "tools")?;
    let facts: Vec<&ToolFact> = facts
        .iter()
        .filter(|fact| tool.is_none_or(|t| tool_name_matches(&fact.tool, t)))
        .collect();
    let mut out = Out::new();
    if json {
        for fact in facts {
            out.line(&fact_json(fact).dumps())?;
        }
        return out.finish();
    }
    for fact in facts {
        out.line(&fact_line(fact))?;
    }
    emit_note(&mut out, &targets)?;
    out.finish()
}

pub fn commands(paths: &[PathBuf], discovery: &DiscoveryOpts, json: bool) -> Result<(), CliExit> {
    let (targets, facts) = gather(paths, discovery, "commands")?;
    let counts = command_prefix_counts(facts.iter());
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
    let (targets, facts) = gather(paths, discovery, "permissions")?;
    let denials: Vec<&ToolFact> = facts
        .iter()
        .filter(|fact| fact.denied)
        .filter(|fact| !tool_name_matches(&fact.tool, "ExitPlanMode|AskUserQuestion"))
        .collect();
    let mut out = Out::new();
    if json {
        for fact in denials {
            out.line(&denial_json(fact).dumps())?;
        }
        return out.finish();
    }
    for fact in denials {
        out.line(&denial_line(fact))?;
    }
    emit_note(&mut out, &targets)?;
    out.finish()
}

pub fn mcp(paths: &[PathBuf], discovery: &DiscoveryOpts, json: bool) -> Result<(), CliExit> {
    let (targets, facts) = gather(paths, discovery, "mcp")?;
    let summary = mcp_summary(facts.iter());
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
