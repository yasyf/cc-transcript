//! Locating Claude Code transcript files on disk. Ported from
//! `cc_transcript/discovery.py`: transcripts live as `*.jsonl` files under a
//! projects root (one directory per project plus `subagents/` sidechains), and
//! this module is the sync walk + session-id resolution the Python facade's
//! async wrappers delegate to. The `TRANSCRIPT_MEMO` positive-hit cache stays a
//! facade concern.

use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Every transcript under `root`, sorted by path. Mirrors
/// `TranscriptDiscovery.find_transcripts`: a recursive `*.jsonl` glob with no
/// resource-fork filter (Python `rglob` yields `._*` forks too).
pub fn find_transcripts(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut found = Vec::new();
    walk(root, &mut |path| {
        if is_jsonl(path) {
            found.push(path.to_path_buf());
        }
    });
    sort_by_path(&mut found);
    found
}

/// Transcripts under `directory` whose name contains `name_contains` (when set)
/// and that are newer than `known_mtimes` (when set), as `(path, mtime)` pairs
/// sorted by path and capped at `limit`. Mirrors `TranscriptDiscovery.find_in`.
pub fn find_in(
    directory: &Path,
    name_contains: Option<&str>,
    limit: Option<usize>,
    known_mtimes: Option<&HashMap<String, f64>>,
) -> Vec<(PathBuf, f64)> {
    if !directory.exists() {
        return Vec::new();
    }
    let mut found: Vec<(PathBuf, f64)> = Vec::new();
    walk(directory, &mut |path| {
        if !is_jsonl(path) {
            return;
        }
        if let Some(needle) = name_contains.filter(|n| !n.is_empty()) {
            if !file_name(path).contains(needle) {
                return;
            }
        }
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        let mtime = mtime_secs(&meta);
        if let Some(prev) =
            known_mtimes.and_then(|known| known.get(&path.to_string_lossy().into_owned()))
        {
            if *prev >= mtime {
                return;
            }
        }
        found.push((path.to_path_buf(), mtime));
    });
    found.sort_by(|a, b| {
        a.0.as_os_str()
            .as_encoded_bytes()
            .cmp(b.0.as_os_str().as_encoded_bytes())
    });
    match limit {
        Some(n) => found.into_iter().take(n).collect(),
        None => found,
    }
}

/// `session_id`'s transcript on disk: the newest-mtime real path, or `None`.
/// Mirrors `find_transcript_sync` (without its `TRANSCRIPT_MEMO`): glob
/// `<root>/**/<session_id>.jsonl`, resolve symlinks, dedupe by real path, and
/// pick the highest mtime — first-wins on a tie, as Python `max` returns.
pub fn find_transcript(root: &Path, session_id: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    let target = format!("{session_id}.jsonl");
    let mut candidates: Vec<(PathBuf, f64)> = Vec::new();
    walk(root, &mut |path| {
        if file_name(path) != target {
            return;
        }
        let Ok(real) = path.canonicalize() else {
            return;
        };
        if candidates.iter().any(|(seen, _)| *seen == real) {
            return;
        }
        if let Ok(meta) = fs::metadata(&real) {
            candidates.push((real, mtime_secs(&meta)));
        }
    });
    candidates
        .into_iter()
        .fold(None, |best: Option<(PathBuf, f64)>, next| match best {
            Some((_, mtime)) if mtime >= next.1 => best,
            _ => Some(next),
        })
        .map(|(path, _)| path)
}

/// Whether `path` names a subagent sidechain transcript: the
/// `agent-<tool_use_id>.jsonl` naming convention `subagent_paths` discovers.
pub fn is_subagent_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("agent-"))
}

/// Sidechain transcript files under `<parent>/<stem>/subagents/`, sorted by
/// path, skipping macOS resource forks (`._*`). Mirrors `subagent_paths`: a
/// non-recursive glob, so `._*` is the only name filtered.
pub fn subagent_paths(path: &Path) -> Vec<PathBuf> {
    let directory = match (path.parent(), path.file_stem()) {
        (Some(parent), Some(stem)) => parent.join(stem).join("subagents"),
        _ => return Vec::new(),
    };
    if !directory.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_jsonl(path) && !file_name(path).starts_with("._"))
        .collect();
    sort_by_path(&mut found);
    found
}

/// Sidechain transcripts keyed by the tool-use id that spawned each — the
/// `agent-<id>` stem, prefix stripped. Mirrors `subagent_transcripts`.
pub fn subagent_transcripts(path: &Path) -> Vec<(String, PathBuf)> {
    subagent_paths(path)
        .into_iter()
        .map(|entry| {
            let stem = entry.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            (
                stem.strip_prefix("agent-").unwrap_or(stem).to_string(),
                entry,
            )
        })
        .collect()
}

/// A file's modification time as CPython's `os.stat().st_mtime` computes it:
/// `sec + 1e-9 * nsec`, so the float compares bit-identically to the Python
/// side — the `find_transcript` newest-mtime pick and the `watch` size/mtime
/// skip both turn on it.
pub fn mtime_secs(meta: &Metadata) -> f64 {
    let modified = meta
        .modified()
        .expect("filesystem records a modification time");
    match modified.duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_secs() as f64 + 1e-9 * delta.subsec_nanos() as f64,
        Err(err) => {
            -(err.duration().as_secs() as f64 + 1e-9 * err.duration().subsec_nanos() as f64)
        }
    }
}

// Recursive walk mirroring pathlib `rglob`: real directories (hidden included)
// are descended, symlinked directories are not, and every non-directory entry
// (regular or symlink file) is visited.
fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    file_name(path).ends_with(".jsonl")
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn sort_by_path(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(&path).unwrap();
    }

    #[test]
    fn find_transcripts_includes_forks_sorts_by_path() {
        let dir = std::env::temp_dir().join(format!("disc-find-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        for rel in [
            "projA/s2.jsonl",
            "projA/s1.jsonl",
            "projA/._fork.jsonl",
            "projA/sub/s3.jsonl",
            "projA/notes.txt",
        ] {
            touch(&dir, rel);
        }
        let got: Vec<String> = find_transcripts(&dir)
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            [
                "projA/._fork.jsonl",
                "projA/s1.jsonl",
                "projA/s2.jsonl",
                "projA/sub/s3.jsonl"
            ]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subagent_paths_skips_only_underscore_forks() {
        let dir = std::env::temp_dir().join(format!("disc-sub-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        touch(&dir, "projA/sess.jsonl");
        for rel in ["agent-t2", "agent-t1", "._fork", ".hidden"] {
            touch(&dir, &format!("projA/sess/subagents/{rel}.jsonl"));
        }
        let got: Vec<String> = subagent_paths(&dir.join("projA/sess.jsonl"))
            .iter()
            .map(|p| file_name(p).to_string())
            .collect();
        assert_eq!(got, [".hidden.jsonl", "agent-t1.jsonl", "agent-t2.jsonl"]);
        assert!(
            is_subagent_path(Path::new("agent-x.jsonl"))
                && !is_subagent_path(Path::new("sess.jsonl"))
        );
        fs::remove_dir_all(&dir).ok();
    }
}
