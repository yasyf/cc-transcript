//! cli.py `grep`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use cc_transcript_core::facts::{tool_facts, ToolFact};
use cc_transcript_core::filter::event_kind;
use cc_transcript_core::render::{
    compact_line, display_path, event_json, haystack, tool_haystack, transcript_header, Json,
};
use cc_transcript_core::toolcall::tool_name_matches;
use cc_transcript_core::types::{ContentBlock, Entry};
use regex::{Regex, RegexBuilder};

use crate::output::{click_error, eline, usage_error, CliExit, Out};
use crate::target::{parse_transcripts, py_path, resolve_targets, scope_note, tool_names, Parsed};
use crate::DiscoveryOpts;

const USAGE: &str = "cc-transcript grep [OPTIONS] PATTERN [PATHS]...";
const HELP_PATH: &str = "cc-transcript grep";
const DEFAULT_MAX_MATCHES: usize = 20;

pub struct GrepArgs {
    pub pattern: String,
    pub paths: Vec<PathBuf>,
    pub discovery: DiscoveryOpts,
    pub corpus: Option<PathBuf>,
    pub kinds: Vec<String>,
    pub tool: Option<String>,
    pub errors: bool,
    pub ignore_case: bool,
    pub wheres: Vec<String>,
    pub context: usize,
    pub max_matches: Option<usize>,
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

fn error_call_matches(
    event: &Entry,
    facts: &HashMap<String, ToolFact>,
    regex: &Regex,
    tool: Option<&str>,
    where_tools: bool,
) -> bool {
    where_tools
        && event.blocks().iter().any(|block| {
            let fact = match block {
                ContentBlock::ToolUse(tool_use) => facts.get(&tool_use.id),
                ContentBlock::ToolResult(result) => facts.get(&result.tool_use_id),
                _ => None,
            };
            fact.is_some_and(|fact| {
                fact.is_error
                    && tool.is_none_or(|name| tool_name_matches(&fact.tool, name))
                    && regex.is_match(&tool_haystack(block))
            })
        })
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

pub fn compile_pattern(
    pattern: &str,
    ignore_case: bool,
    usage: &str,
    help_path: &str,
) -> Result<Regex, CliExit> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| usage_error(usage, help_path, &format!("invalid pattern: {e}")))
}

/// An empty `--where` searches everywhere (cli.py's default).
pub fn where_flags(wheres: &[String]) -> (bool, bool, bool) {
    if wheres.is_empty() {
        return (true, true, true);
    }
    (
        wheres.iter().any(|w| w == "text"),
        wheres.iter().any(|w| w == "thinking"),
        wheres.iter().any(|w| w == "tools"),
    )
}

/// `--max-matches 0` lifts the cap and so does leaving it off in corpus mode, where each
/// match is one bounded window; anything else is a budget that must announce itself when
/// it bites, so a whole-corpus sweep never looks complete at 20 rows.
fn match_cap(max_matches: Option<usize>, default: Option<usize>) -> Option<usize> {
    max_matches.map_or(default, |cap| (cap > 0).then_some(cap))
}

fn warn_truncated(cap: usize) {
    eline(&format!(
        "warning: stopped at --max-matches {cap}; more matches may exist — raise it, or pass --max-matches 0 for no cap"
    ));
}

/// `grep --corpus`: one `corpus` window per line, matched verbatim. There is no event
/// structure left to filter on, so only the pattern, `-i`, and `--max-matches` apply.
fn run_over_corpus(args: &GrepArgs, corpus: &Path) -> Result<(), CliExit> {
    let regex = compile_pattern(&args.pattern, args.ignore_case, USAGE, HELP_PATH)?;
    let file =
        File::open(corpus).map_err(|e| click_error(&format!("{}: {e}", corpus.display())))?;
    let cap = match_cap(args.max_matches, None);
    let mut out_lines: Vec<String> = Vec::new();
    let mut truncated = false;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| click_error(&format!("{}: {e}", corpus.display())))?;
        if !regex.is_match(&line) {
            continue;
        }
        if cap.is_some_and(|cap| out_lines.len() == cap) {
            truncated = true;
            break;
        }
        out_lines.push(line);
    }
    let matched = out_lines.len();
    out_lines.push(format!(
        "{matched} matches in {}",
        display_path(&corpus.to_string_lossy())
    ));
    let mut out = Out::new();
    out.lines(out_lines)?;
    out.finish()?;
    if let Some(cap) = cap.filter(|_| truncated) {
        warn_truncated(cap);
    }
    if matched == 0 {
        return Err(CliExit(1));
    }
    Ok(())
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
    if let Some(corpus) = &args.corpus {
        return run_over_corpus(&args, corpus);
    }
    let regex = compile_pattern(&args.pattern, args.ignore_case, USAGE, HELP_PATH)?;
    let (w_text, w_thinking, w_tools) = where_flags(&args.wheres);
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
    let cap = match_cap(args.max_matches, Some(DEFAULT_MAX_MATCHES));
    let mut truncated = false;
    for parsed in parse_transcripts(&targets.paths) {
        if cap.is_some_and(|cap| matched == cap) {
            truncated = true;
            break;
        }
        let names = tool_names(&parsed.entries);
        let facts = if args.with_result || args.errors {
            fact_index(&parsed)
        } else {
            HashMap::new()
        };
        let mut hits: Vec<usize> = parsed
            .entries
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                args.kinds.is_empty() || args.kinds.iter().any(|k| k == event_kind(event))
            })
            .filter(|(_, event)| {
                args.errors
                    || args
                        .tool
                        .as_deref()
                        .is_none_or(|tool| uses_tool(event, tool, &names))
            })
            .filter(|(_, event)| {
                if args.errors {
                    error_call_matches(event, &facts, &regex, args.tool.as_deref(), w_tools)
                } else {
                    regex.is_match(&haystack(event, w_text, w_thinking, w_tools))
                }
            })
            .map(|(index, _)| index)
            .collect();
        if let Some(cap) = cap {
            if matched + hits.len() > cap {
                truncated = true;
                hits.truncate(cap - matched);
            }
        }
        if hits.is_empty() {
            continue;
        }
        files_matched += 1;
        matched += hits.len();
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
    if let Some(cap) = cap.filter(|_| truncated) {
        warn_truncated(cap);
    }
    if matched == 0 {
        return Err(CliExit(1));
    }
    Ok(())
}
