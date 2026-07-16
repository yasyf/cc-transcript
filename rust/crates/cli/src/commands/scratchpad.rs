//! cli.py `scratchpad` — the per-session harness scratchpad resolver.

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::output::{eline, py_repr, usage_error, CliExit, Out};

const USAGE: &str = "cc-transcript scratchpad [OPTIONS]";
const HELP_PATH: &str = "cc-transcript scratchpad";

static SESSION_UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
        .expect("UUID pattern compiles")
});

fn scratchpad_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// tempfile.gettempdir(): first non-empty of TMPDIR/TEMP/TMP, else /tmp.
fn temp_dir() -> PathBuf {
    ["TMPDIR", "TEMP", "TMP"]
        .iter()
        .find_map(|var| std::env::var_os(var).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn realpath(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

pub fn resolve_scratchpad(
    roots: &[PathBuf],
    uid: u32,
    cwd: &Path,
    session: &str,
) -> Option<PathBuf> {
    if session.is_empty() || session == "." || session == ".." || session.contains('/') {
        return None;
    }
    let mut unique_roots: Vec<&PathBuf> = Vec::new();
    for root in roots {
        if !unique_roots.contains(&root) {
            unique_roots.push(root);
        }
    }
    let mut best: Option<(i128, PathBuf)> = None;
    for root in &unique_roots {
        let claude_dir = root.join(format!("claude-{uid}"));
        let Ok(entries) = std::fs::read_dir(&claude_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join(session).join("scratchpad");
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let ns = mtime_ns(&meta);
            match &best {
                Some((best_ns, _)) if *best_ns >= ns => {}
                _ => best = Some((ns, candidate)),
            }
        }
    }
    if let Some((_, path)) = best {
        return Some(path);
    }
    unique_roots.iter().find_map(|root| {
        let path = root
            .join(format!("claude-{uid}"))
            .join(scratchpad_slug(cwd))
            .join(session)
            .join("scratchpad");
        path.is_dir().then_some(path)
    })
}

pub fn run(session: &str) -> Result<(), CliExit> {
    if session.is_empty() {
        return Err(usage_error(USAGE, HELP_PATH, "Missing option '--session'."));
    }
    if !SESSION_UUID.is_match(session) {
        return Err(usage_error(
            USAGE,
            HELP_PATH,
            &format!("invalid --session {}; expected a UUID", py_repr(session)),
        ));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in [realpath(&temp_dir()), realpath(Path::new("/tmp"))] {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let uid = unsafe { libc::getuid() };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(path) = resolve_scratchpad(&roots, uid, &cwd, session) {
        let mut out = Out::new();
        out.line(&path.to_string_lossy())?;
        return out.finish();
    }
    eline(&format!("scratchpad not found for session {session}"));
    Err(CliExit(1))
}
