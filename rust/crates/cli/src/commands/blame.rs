//! The file-centric `blame` and `attribute` verbs — native-only, with no click twin.
//! `blame` lists the sessions that wrote a working-tree file, newest first; `attribute`
//! classifies how the file came to exist. Both resolve the target's repo root from a
//! `.git`/`.jj` ancestor and scan only that repo's project dirs unless `--all-projects`.

use std::fs;
use std::path::{Path, PathBuf};

use cc_transcript_core::blame::{attribute as classify, blame as rank, session_writes, RepoPaths};
use cc_transcript_core::discovery::{find_in, mtime_secs, project_dir_name};
use cc_transcript_core::render::{attribute_json, attribute_lines, blame_json, blame_line};
use cc_transcript_core::types::Entry;
use chrono::{DateTime, FixedOffset};

use crate::output::{click_error, eline, py_repr, usage_error, CliExit, Out};
use crate::target::{claude_projects_dir, parse_transcripts, require_dir, Parsed};

const BLAME_USAGE: &str = "cc-transcript blame [OPTIONS] <PATH>";
const BLAME_HELP: &str = "cc-transcript blame";
const ATTRIBUTE_USAGE: &str = "cc-transcript attribute [OPTIONS] <PATH>";
const ATTRIBUTE_HELP: &str = "cc-transcript attribute";
const WORKTREE_MARKER: &str = "/.claude/worktrees/";

pub struct BlameArgs {
    pub path: PathBuf,
    pub root: Option<PathBuf>,
    pub all_projects: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    pub json: bool,
}

pub struct AttributeArgs {
    pub path: PathBuf,
    pub root: Option<PathBuf>,
    pub all_projects: bool,
    pub json: bool,
}

/// A resolved target: the repo it belongs to, its repo-relative path, and its canonical
/// on-disk location.
struct Target {
    repo: RepoPaths,
    rel_target: String,
    canonical: PathBuf,
}

/// The nearest ancestor directory carrying a `.git` or `.jj` entry (file or dir — a
/// worktree's `.git` is a file), starting from the target's own parent.
fn marker_root(canonical: &Path) -> Option<&Path> {
    canonical
        .ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists() || dir.join(".jj").exists())
}

/// A worktree's marker root lifts to the main checkout: everything before the
/// `/.claude/worktrees/` segment. Any other marker root is the repo root as found.
fn repo_root(marker: &Path) -> String {
    let text = marker.to_string_lossy();
    match text.find(WORKTREE_MARKER) {
        Some(cut) => text[..cut].to_string(),
        None => text.into_owned(),
    }
}

fn resolve_target(path: &Path, usage: &str, help_path: &str) -> Result<Target, CliExit> {
    let invalid = |what: &str| {
        usage_error(
            usage,
            help_path,
            &format!(
                "Invalid value for 'PATH': File {} {what}.",
                py_repr(&path.to_string_lossy())
            ),
        )
    };
    let canonical = fs::canonicalize(path).map_err(|_| invalid("does not exist"))?;
    if canonical.is_dir() {
        return Err(invalid("is a directory"));
    }
    let marker = marker_root(&canonical).ok_or_else(|| {
        click_error(&format!(
            "{} is not inside a repository (no .git or .jj ancestor)",
            path.display()
        ))
    })?;
    let rel_target = canonical
        .strip_prefix(marker)
        .expect("canonical target sits under its marker root")
        .to_string_lossy()
        .into_owned();
    Ok(Target {
        repo: RepoPaths {
            root: repo_root(marker),
        },
        rel_target,
        canonical,
    })
}

/// Parse every transcript under a project dir belonging to `repo_root` (or every dir
/// under `--all-projects`), dropping files older than `min_mtime` (blame's `--since`).
/// The `{encoded}-` prefix admits prefix-collided siblings (`<root>-rust`); the engine's
/// per-call cwd check drops their writes.
fn select_transcripts(
    projects_root: &Path,
    repo_root: &str,
    all_projects: bool,
    min_mtime: Option<f64>,
    usage: &str,
    help_path: &str,
) -> Result<Vec<Parsed>, CliExit> {
    require_dir(projects_root, "'--root'", usage, help_path)?;
    let encoded = project_dir_name(repo_root);
    let prefix = format!("{encoded}-");
    let pairs: Vec<(PathBuf, f64)> = fs::read_dir(projects_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            all_projects || name == encoded || name.starts_with(&prefix)
        })
        .flat_map(|entry| find_in(&entry.path(), None, None, None))
        .filter(|(_, mtime)| min_mtime.is_none_or(|floor| *mtime >= floor))
        .collect();
    Ok(parse_transcripts(&pairs))
}

/// A file mtime (`os.stat` seconds) as a UTC-offset datetime, for the attribute window.
fn mtime_datetime(mtime: f64) -> DateTime<FixedOffset> {
    let secs = mtime.floor();
    let nanos = ((mtime - secs) * 1e9) as u32;
    DateTime::from_timestamp(secs as i64, nanos)
        .expect("file mtime is in range")
        .fixed_offset()
}

pub fn blame(args: BlameArgs) -> Result<(), CliExit> {
    let target = resolve_target(&args.path, BLAME_USAGE, BLAME_HELP)?;
    let now = chrono::Local::now();
    let since = args
        .since
        .as_deref()
        .map(|value| crate::timearg::parse_time("--since", value, now, BLAME_USAGE, BLAME_HELP))
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(|value| crate::timearg::parse_time("--until", value, now, BLAME_USAGE, BLAME_HELP))
        .transpose()?;
    let projects_root = args.root.unwrap_or_else(claude_projects_dir);
    let parsed = select_transcripts(
        &projects_root,
        &target.repo.root,
        args.all_projects,
        since.map(|ts| ts.timestamp() as f64),
        BLAME_USAGE,
        BLAME_HELP,
    )?;
    let mut records = rank(
        parsed
            .iter()
            .filter_map(|p| p.session_id.as_deref().map(|session| (session, p)))
            .flat_map(|(session, p)| {
                session_writes(
                    session,
                    &p.path.to_string_lossy(),
                    &p.entries,
                    &target.repo,
                    &target.rel_target,
                    since,
                    until,
                )
            })
            .collect(),
    );
    if let Some(limit) = args.limit {
        records.truncate(limit);
    }
    if records.is_empty() {
        eline(&format!("no sessions wrote {}", target.rel_target));
        return Err(CliExit(1));
    }
    let mut out = Out::new();
    for record in &records {
        out.line(&match args.json {
            true => blame_json(record).dumps(),
            false => blame_line(record),
        })?;
    }
    out.finish()
}

pub fn attribute(args: AttributeArgs) -> Result<(), CliExit> {
    let target = resolve_target(&args.path, ATTRIBUTE_USAGE, ATTRIBUTE_HELP)?;
    let meta = fs::metadata(&target.canonical)
        .map_err(|e| click_error(&format!("{}: {e}", args.path.display())))?;
    let mtime = mtime_datetime(mtime_secs(&meta));
    let projects_root = args.root.unwrap_or_else(claude_projects_dir);
    let parsed = select_transcripts(
        &projects_root,
        &target.repo.root,
        args.all_projects,
        None,
        ATTRIBUTE_USAGE,
        ATTRIBUTE_HELP,
    )?;
    let owned: Vec<(&str, String, &[Entry])> = parsed
        .iter()
        .filter_map(|p| {
            p.session_id.as_deref().map(|session| {
                (
                    session,
                    p.path.to_string_lossy().into_owned(),
                    p.entries.as_slice(),
                )
            })
        })
        .collect();
    let sessions: Vec<(&str, &str, &[Entry])> = owned
        .iter()
        .map(|(session, path, entries)| (*session, path.as_str(), *entries))
        .collect();
    let verdict = classify(&sessions, &target.repo, &target.rel_target, mtime);
    let mut out = Out::new();
    match args.json {
        true => out.line(&attribute_json(&verdict, &target.rel_target, mtime).dumps())?,
        false => out.lines(attribute_lines(&verdict, &target.rel_target))?,
    }
    out.finish()
}
