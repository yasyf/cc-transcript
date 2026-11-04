//! `corpus`: flatten a whole sweep into one deduped file of character windows, so the
//! questions that follow grep the extract instead of re-reading every transcript.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use cc_transcript_core::render::haystack;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::commands::grep::{compile_pattern, where_flags};
use crate::output::{click_error, eline, CliExit};
use crate::target::{py_path, require_files, resolve_targets, stream_transcripts};
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

fn line_key(line: &str) -> [u8; 16] {
    Sha256::digest(line.as_bytes())[..16]
        .try_into()
        .expect("a sha-256 digest holds 16 bytes")
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
        None,
    )?;
    let mut staged = args.out.clone().into_os_string();
    staged.push(format!(".{}.tmp", std::process::id()));
    let staged = PathBuf::from(staged);
    let out_error = |e: std::io::Error| click_error(&format!("{}: {e}", args.out.display()));
    let mut out = BufWriter::new(File::create(&staged).map_err(out_error)?);
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    let mut duplicates = 0usize;
    let mut bytes = 0usize;
    let swept = stream_transcripts(
        &targets.paths,
        |parsed| {
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
        },
        |lines| {
            for line in lines {
                if !seen.insert(line_key(&line)) {
                    duplicates += 1;
                    continue;
                }
                out.write_all(line.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .map_err(out_error)?;
                bytes += line.len() + 1;
            }
            Ok(())
        },
    )
    .and_then(|()| {
        out.flush()
            .and_then(|()| std::fs::rename(&staged, &args.out))
            .map_err(out_error)
    });
    if swept.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    swept?;
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
