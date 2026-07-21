//! cli.py `grep`.

use std::collections::HashMap;
use std::path::PathBuf;

use cc_transcript_core::facts::{tool_facts, ToolFact};
use cc_transcript_core::filter::event_kind;
use cc_transcript_core::render::{compact_line, event_json, haystack, transcript_header, Json};
use cc_transcript_core::toolcall::tool_name_matches;
use cc_transcript_core::types::{ContentBlock, Entry};
use regex::{Regex, RegexBuilder};

use crate::output::{usage_error, CliExit, Out};
use crate::target::{parse_transcripts, py_path, resolve_targets, scope_note, tool_names, Parsed};
use crate::DiscoveryOpts;

const USAGE: &str = "cc-transcript grep [OPTIONS] PATTERN [PATHS]...";
const HELP_PATH: &str = "cc-transcript grep";

pub struct GrepArgs {
    pub pattern: String,
    pub paths: Vec<PathBuf>,
    pub discovery: DiscoveryOpts,
    pub kinds: Vec<String>,
    pub tool: Option<String>,
    pub ignore_case: bool,
    pub wheres: Vec<String>,
    pub context: usize,
    pub max_matches: usize,
    pub width: usize,
    pub uuids: bool,
    pub with_result: bool,
    pub json: bool,
}

fn uses_tool(event: &Entry, tool: &str, names: &HashMap<&str, &str>) -> bool {
    match event {
        Entry::Assistant(a) => a
            .blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse(tu) if tool_name_matches(&tu.name, tool))),
        Entry::User(u) => u.blocks().iter().any(|b| {
            matches!(b, ContentBlock::ToolResult(tr)
                if names.get(tr.tool_use_id.as_str()).is_some_and(|name| tool_name_matches(name, tool)))
        }),
        _ => false,
    }
}

fn event_facts<'a>(event: &Entry, facts: &'a HashMap<String, ToolFact>) -> Vec<&'a ToolFact> {
    match event {
        Entry::Assistant(a) => a
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse(tu) => facts.get(tu.id.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn result_key(fact: &ToolFact) -> Json {
    Json::Obj(vec![
        ("is_error".into(), Json::Bool(fact.is_error)),
        ("denied".into(), Json::Bool(fact.denied)),
        (
            "duration_ms".into(),
            fact.duration_ms.map_or(Json::Null, Json::Int),
        ),
    ])
}

fn outcome_marker(fact: &ToolFact) -> String {
    let status = if fact.denied {
        "[denied]"
    } else if fact.is_error {
        "[err]"
    } else {
        ""
    };
    let dur = match fact.duration_ms {
        Some(ms) => format!("({ms}ms)"),
        None => String::new(),
    };
    [status.to_string(), dur]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn result_suffix(event: &Entry, facts: &HashMap<String, ToolFact>) -> String {
    let markers: Vec<String> = event_facts(event, facts)
        .into_iter()
        .map(outcome_marker)
        .filter(|m| !m.is_empty())
        .collect();
    if markers.is_empty() {
        String::new()
    } else {
        format!(" {}", markers.join(" "))
    }
}

fn merge_windows(hits: &[usize], context: usize, size: usize) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &i in hits {
        let (lo, hi) = (i.saturating_sub(context), size.min(i + context + 1));
        match merged.last_mut() {
            Some((_, last_hi)) if lo <= *last_hi => *last_hi = (*last_hi).max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

fn compile_pattern(pattern: &str, ignore_case: bool) -> Result<Regex, CliExit> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| usage_error(USAGE, HELP_PATH, &format!("invalid pattern: {e}")))
}

fn fact_index(parsed: &Parsed) -> HashMap<String, ToolFact> {
    let Some(session) = &parsed.session_id else {
        return HashMap::new();
    };
    tool_facts(session, &parsed.path.to_string_lossy(), &parsed.entries)
        .into_iter()
        .map(|fact| (fact.tool_use_id.clone(), fact))
        .collect()
}

pub fn run(args: GrepArgs) -> Result<(), CliExit> {
    let regex = compile_pattern(&args.pattern, args.ignore_case)?;
    let (w_text, w_thinking, w_tools) = if args.wheres.is_empty() {
        (true, true, true)
    } else {
        (
            args.wheres.iter().any(|w| w == "text"),
            args.wheres.iter().any(|w| w == "thinking"),
            args.wheres.iter().any(|w| w == "tools"),
        )
    };
    let paths: Vec<PathBuf> = args
        .paths
        .iter()
        .map(|p| py_path(&p.to_string_lossy()))
        .collect();
    crate::target::require_files(&paths, "'[PATHS]...'", USAGE, HELP_PATH)?;
    let targets = resolve_targets(
        &paths,
        &args.discovery.validated_root(USAGE, HELP_PATH)?,
        args.discovery.project.as_deref(),
        args.discovery.contains.as_deref(),
        args.discovery.effective_limit(),
        None,
    )?;
    let mut out_lines: Vec<String> = Vec::new();
    let mut files_matched = 0usize;
    let mut matched = 0usize;
    let mut budget = args.max_matches;
    for parsed in parse_transcripts(&targets.paths) {
        if budget == 0 {
            break;
        }
        let names = tool_names(&parsed.entries);
        let hits: Vec<usize> = parsed
            .entries
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                args.kinds.is_empty() || args.kinds.iter().any(|k| k == event_kind(event))
            })
            .filter(|(_, event)| {
                args.tool
                    .as_deref()
                    .is_none_or(|tool| uses_tool(event, tool, &names))
            })
            .filter(|(_, event)| regex.is_match(&haystack(event, w_text, w_thinking, w_tools)))
            .map(|(index, _)| index)
            .take(budget)
            .collect();
        if hits.is_empty() {
            continue;
        }
        let facts = if args.with_result {
            fact_index(&parsed)
        } else {
            HashMap::new()
        };
        files_matched += 1;
        matched += hits.len();
        budget -= hits.len();
        let windows = merge_windows(&hits, args.context, parsed.entries.len());
        if args.json {
            let hit_set: std::collections::HashSet<usize> = hits.iter().copied().collect();
            for (lo, hi) in windows {
                for i in lo..hi {
                    let event = &parsed.entries[i];
                    let Json::Obj(mut pairs) = event_json(i, event) else {
                        unreachable!("event_json returns an object")
                    };
                    pairs.insert(
                        0,
                        (
                            "path".into(),
                            Json::Str(parsed.path.to_string_lossy().into_owned()),
                        ),
                    );
                    if !hit_set.contains(&i) {
                        pairs.push(("context".into(), Json::Bool(true)));
                    }
                    if args.with_result {
                        let rk: Vec<(String, Json)> = event_facts(event, &facts)
                            .into_iter()
                            .map(|fact| (fact.tool_use_id.clone(), result_key(fact)))
                            .collect();
                        if !rk.is_empty() {
                            pairs.push(("results".into(), Json::Obj(rk)));
                        }
                    }
                    out_lines.push(Json::Obj(pairs).dumps());
                }
            }
            continue;
        }
        out_lines.push(transcript_header(&parsed.path.to_string_lossy()));
        for (n, (lo, hi)) in windows.iter().enumerate() {
            if args.context > 0 && n > 0 {
                out_lines.push("--".to_string());
            }
            for i in *lo..*hi {
                let mut line =
                    compact_line(i, &parsed.entries[i], &names, args.width, false, args.uuids);
                if args.with_result {
                    line.push_str(&result_suffix(&parsed.entries[i], &facts));
                }
                out_lines.push(line);
            }
        }
    }
    if !args.json {
        let note = scope_note(&targets)
            .map(|note| format!(" · {note}"))
            .unwrap_or_default();
        out_lines.push(format!("{files_matched} files, {matched} matches{note}"));
    }
    let mut out = Out::new();
    out.lines(out_lines)?;
    out.finish()?;
    if matched == 0 {
        return Err(CliExit(1));
    }
    Ok(())
}
