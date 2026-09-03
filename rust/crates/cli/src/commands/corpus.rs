//! `corpus`: flatten a whole sweep into one deduped file of character windows, so the
//! questions that follow grep the extract instead of re-reading every transcript.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use cc_transcript_core::render::haystack;
use regex::Regex;

use crate::commands::grep::{compile_pattern, where_flags};
use crate::output::{click_error, eline, CliExit};
use crate::target::{map_transcripts, py_path, require_files, resolve_targets};
use crate::DiscoveryOpts;

const USAGE: &str = "cc-transcript corpus [OPTIONS] --out <OUT> PATTERN [PATHS]...";
const HELP_PATH: &str = "cc-transcript corpus";

pub struct CorpusArgs {
    pub pattern: String,
    pub paths: Vec<PathBuf>,
    pub discovery: DiscoveryOpts,
    pub wheres: Vec<String>,
    pub ignore_case: bool,
    pub window: usize,
    pub out: PathBuf,
}

/// Byte offset of the `window`th character before `at`, or the start of `text`.
fn window_start(text: &str, at: usize, window: usize) -> usize {
    match window.checked_sub(1) {
        None => at,
        Some(back) => text[..at]
            .char_indices()
            .rev()
            .nth(back)
            .map_or(0, |(index, _)| index),
    }
}

/// Byte offset just past the `window`th character after `at`, or the end of `text`.
fn window_end(text: &str, at: usize, window: usize) -> usize {
    text[at..]
        .char_indices()
        .nth(window)
        .map_or(text.len(), |(index, _)| at + index)
}

/// One window per line: line breaks inside a window become their two-character escapes,
/// the shape `grep --corpus` then reads back.
fn flatten(window: &str) -> String {
    window.replace('\n', "\\n").replace('\r', "\\r")
}

fn windows_in(text: &str, regex: &Regex, window: usize) -> Vec<String> {
    regex
        .find_iter(text)
        .map(|hit| {
            flatten(
                &text[window_start(text, hit.start(), window)..window_end(text, hit.end(), window)],
            )
        })
        .collect()
}

pub fn run(args: CorpusArgs) -> Result<(), CliExit> {
    let regex = compile_pattern(&args.pattern, args.ignore_case, USAGE, HELP_PATH)?;
    let (w_text, w_thinking, w_tools) = where_flags(&args.wheres);
    let paths: Vec<PathBuf> = args
        .paths
        .iter()
        .map(|p| py_path(&p.to_string_lossy()))
        .collect();
    require_files(&paths, "'[PATHS]...'", USAGE, HELP_PATH)?;
    let targets = resolve_targets(
        &paths,
        &args.discovery.validated_root(USAGE, HELP_PATH)?,
        args.discovery.project.as_deref(),
        args.discovery.contains.as_deref(),
        args.discovery.sweep_limit(),
        None,
    )?;
    let per_file = map_transcripts(&targets.paths, |parsed| {
        parsed
            .entries
            .iter()
            .flat_map(|event| {
                windows_in(
                    &haystack(event, w_text, w_thinking, w_tools),
                    &regex,
                    args.window,
                )
            })
            .collect::<Vec<String>>()
    });
    let file = File::create(&args.out)
        .map_err(|e| click_error(&format!("{}: {e}", args.out.display())))?;
    let mut out = BufWriter::new(file);
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates = 0usize;
    let mut bytes = 0usize;
    for line in per_file.into_iter().flatten() {
        if !seen.insert(line.clone()) {
            duplicates += 1;
            continue;
        }
        out.write_all(line.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .map_err(|e| click_error(&format!("{}: {e}", args.out.display())))?;
        bytes += line.len() + 1;
    }
    out.flush()
        .map_err(|e| click_error(&format!("{}: {e}", args.out.display())))?;
    eline(&format!(
        "{} files scanned, {} windows kept, {duplicates} duplicates dropped, {bytes} bytes written",
        targets.paths.len(),
        seen.len()
    ));
    if seen.is_empty() {
        return Err(CliExit(1));
    }
    Ok(())
}
