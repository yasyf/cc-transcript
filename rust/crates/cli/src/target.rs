//! Target discovery and parsing shared by every transcript command (cli.py's
//! discover / resolve_targets / parse_transcripts / scope_note plumbing).

use std::path::{Path, PathBuf};

use cc_transcript_core::discovery::{find_in, mtime_secs};
use cc_transcript_core::gateway::parse_transcript_bytes;
use cc_transcript_core::render::display_path;
use cc_transcript_core::types::Entry;
use rayon::prelude::*;

use crate::output::{click_error, eline, CliExit};

// Python Path.home(): HOME env, then the pwd database (std::env::home_dir does both).
pub fn home_dir() -> PathBuf {
    std::env::home_dir().expect("a home directory resolves via HOME or the pwd database")
}

/// discovery.py CLAUDE_PROJECTS_DIR.
pub fn claude_projects_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

pub struct Targets {
    pub paths: Vec<(PathBuf, f64)>,
    pub total: usize,
}

fn project_matches(path: &Path, root: &Path, project: Option<&str>) -> bool {
    let Some(project) = project else { return true };
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = rel.components().collect();
    parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|part| part.as_os_str().to_string_lossy().contains(project))
}

/// Newest-first discovery under `root` (cli.py discover): find_in order, project
/// filtered, stable-sorted by mtime descending.
pub fn discover(root: &Path, project: Option<&str>, contains: Option<&str>) -> Vec<(PathBuf, f64)> {
    let mut found: Vec<(PathBuf, f64)> = find_in(root, contains, None, None)
        .into_iter()
        .filter(|(path, _)| project_matches(path, root, project))
        .collect();
    found.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("mtimes are finite"));
    found
}

pub fn resolve_targets(
    paths: &[PathBuf],
    root: &Path,
    project: Option<&str>,
    contains: Option<&str>,
    limit: Option<usize>,
    min_mtime: Option<f64>,
) -> Result<Targets, CliExit> {
    if !paths.is_empty() {
        let mut pairs = Vec::with_capacity(paths.len());
        for path in paths {
            let meta = std::fs::metadata(path)
                .map_err(|e| click_error(&format!("{}: {e}", path.display())))?;
            pairs.push((path.clone(), mtime_secs(&meta)));
        }
        let total = pairs.len();
        return Ok(Targets {
            paths: pairs,
            total,
        });
    }
    let matched = discover(root, project, contains);
    let matched: Vec<(PathBuf, f64)> = match min_mtime {
        Some(floor) => matched
            .into_iter()
            .filter(|(_, mtime)| *mtime >= floor)
            .collect(),
        None => matched,
    };
    let total = matched.len();
    let paths = match limit {
        Some(n) => matched.into_iter().take(n).collect(),
        None => matched,
    };
    Ok(Targets { paths, total })
}

pub fn scope_note(targets: &Targets) -> Option<String> {
    (targets.paths.len() < targets.total).then(|| {
        format!(
            "searched {} of {} transcripts — use --all",
            targets.paths.len(),
            targets.total
        )
    })
}

pub struct Parsed {
    pub path: PathBuf,
    pub session_id: Option<String>,
    pub entries: Vec<Entry>,
}

// filterspec.session_id_of: the first meta-bearing entry names the session; the
// facts pipeline skips transcripts without one (facts.tool_facts).
fn session_id_of(entries: &[Entry]) -> Option<String> {
    entries
        .iter()
        .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()))
}

/// Parse every target in parallel, keeping target order and warning once about
/// unparseable files (cli.py parse_transcripts).
pub fn parse_transcripts(targets: &[(PathBuf, f64)]) -> Vec<Parsed> {
    let results: Vec<Option<Parsed>> = targets
        .par_iter()
        .map(|(path, _)| {
            let bytes = std::fs::read(path).ok()?;
            let entries = parse_transcript_bytes(&bytes).ok()?.entries;
            Some(Parsed {
                path: path.clone(),
                session_id: session_id_of(&entries),
                entries,
            })
        })
        .collect();
    let missing: Vec<String> = targets
        .iter()
        .zip(&results)
        .filter(|(_, parsed)| parsed.is_none())
        .map(|((path, _), _)| display_path(&path.to_string_lossy()))
        .collect();
    if !missing.is_empty() {
        eline(&format!(
            "warning: skipped {} unparseable transcript(s): {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    results.into_iter().flatten().collect()
}

/// cli.py parse_single: one path or a click-style `Error:` exit 1.
pub fn parse_single(path: &Path) -> Result<Parsed, CliExit> {
    let mtime = std::fs::metadata(path)
        .map(|m| mtime_secs(&m))
        .unwrap_or(0.0);
    parse_transcripts(&[(path.to_path_buf(), mtime)])
        .into_iter()
        .next()
        .ok_or_else(|| {
            click_error(&format!(
                "failed to parse {}",
                display_path(&path.to_string_lossy())
            ))
        })
}

/// click `Path(file_okay=False)`: an existing non-directory root exits 2.
pub fn require_dir(path: &Path, hint: &str, usage: &str, help_path: &str) -> Result<(), CliExit> {
    if path.exists() && !path.is_dir() {
        return Err(crate::output::usage_error(
            usage,
            help_path,
            &format!(
                "Invalid value for {hint}: Directory {} is a file.",
                crate::output::py_repr(&path.to_string_lossy())
            ),
        ));
    }
    Ok(())
}

/// click `Path(exists=True, dir_okay=False)`: missing or directory args exit 2 with a
/// click-shaped invalid-value message.
pub fn require_files(
    paths: &[PathBuf],
    hint: &str,
    usage: &str,
    help_path: &str,
) -> Result<(), CliExit> {
    for path in paths {
        let display = path.to_string_lossy();
        let message = if !path.exists() {
            format!(
                "Invalid value for {hint}: File {} does not exist.",
                crate::output::py_repr(&display)
            )
        } else if path.is_dir() {
            format!(
                "Invalid value for {hint}: File {} is a directory.",
                crate::output::py_repr(&display)
            )
        } else {
            continue;
        };
        return Err(crate::output::usage_error(usage, help_path, &message));
    }
    Ok(())
}

/// filterspec.py tool_names: every ToolUseBlock's id → name across `entries`.
pub fn tool_names(entries: &[Entry]) -> std::collections::HashMap<&str, &str> {
    entries
        .iter()
        .flat_map(|entry| entry.tool_uses())
        .map(|tu| (tu.id.as_str(), tu.name.as_str()))
        .collect()
}

/// Python `str(Path(arg))`: collapse duplicate separators and `.` segments.
pub fn py_path(arg: &str) -> PathBuf {
    if arg.is_empty() {
        return PathBuf::from(".");
    }
    let absolute = arg.starts_with('/');
    let parts: Vec<&str> = arg
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let joined = parts.join("/");
    let text = match (absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    };
    PathBuf::from(text)
}
