use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use super::parse::parse_codex_bytes;

const MAX_SESSION_META_LINE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutFile {
    pub path: PathBuf,
    pub session_id: String,
    pub compressed: bool,
}

pub fn sessions_root(root: Option<&Path>) -> PathBuf {
    root.map(Path::to_path_buf).unwrap_or_else(|| {
        std::env::home_dir()
            .expect("a home directory resolves via HOME or the pwd database")
            .join(".codex")
            .join("sessions")
    })
}

pub fn discover(root: &Path) -> Vec<RolloutFile> {
    if !root.exists() {
        return Vec::new();
    }
    let mut found = Vec::new();
    walk(root, &mut |path| {
        let Some((stamp, session_id, compressed)) = parse_file_name(path) else {
            return;
        };
        found.push((
            stamp,
            RolloutFile {
                path: path.to_path_buf(),
                session_id,
                compressed,
            },
        ));
    });
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    found.into_iter().map(|(_, rollout)| rollout).collect()
}

pub fn resolve(session_id: &str, root: &Path) -> Option<PathBuf> {
    let found = discover(root);
    found
        .iter()
        .find(|rollout| rollout.session_id == session_id && !rollout.compressed)
        .and_then(|rollout| fs::canonicalize(&rollout.path).ok())
}

pub fn children_of(session_id: &str, root: &Path) -> Vec<RolloutFile> {
    discover(root)
        .into_iter()
        .filter(|rollout| {
            !rollout.compressed
                && rollout.session_id != session_id
                && head_parent_thread_id(&rollout.path).as_deref() == Some(session_id)
        })
        .collect()
}

fn head_parent_thread_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file).take(MAX_SESSION_META_LINE_BYTES);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    if line.len() as u64 == MAX_SESSION_META_LINE_BYTES && !line.ends_with(b"\n") {
        return None;
    }
    parse_codex_bytes(&line).parent_thread_id
}

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

fn parse_file_name(path: &Path) -> Option<(String, String, bool)> {
    let name = path.file_name()?.to_str()?;
    let (stem, compressed) = match name.strip_suffix(".jsonl.zst") {
        Some(stem) => (stem, true),
        None => (name.strip_suffix(".jsonl")?, false),
    };
    let body = stem.strip_prefix("rollout-")?;
    let split = body.len().checked_sub(37)?;
    if body.as_bytes().get(split) != Some(&b'-') {
        return None;
    }
    let stamp = body.get(..split)?;
    let session_id = body.get(split + 1..)?;
    NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    if !is_uuid_v7(session_id) {
        return None;
    }
    Some((stamp.to_string(), session_id.to_string(), compressed))
}

fn is_uuid_v7(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        14 => byte == b'7',
        19 => matches!(byte, b'8' | b'9' | b'a' | b'b' | b'A' | b'B'),
        _ => byte.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn touch(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(&path).unwrap();
        path
    }

    fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn discovers_and_resolves_rollouts() {
        let root = std::env::temp_dir().join(format!(
            "codex-discovery-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&root).ok();
        let oldest = touch(
            &root,
            "2025/12/31/rollout-2025-12-31T23-59-59-019b7000-0000-7000-8000-000000000001.jsonl",
        );
        let twin = touch(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-01-019b7000-0000-7000-8000-000000000002.jsonl",
        );
        let twin_zst = touch(
            &root,
            "2026/01/03/rollout-2026-01-03T09-00-00-019b7000-0000-7000-8000-000000000002.jsonl.zst",
        );
        let newest = touch(
            &root,
            "2026/01/02/rollout-2026-01-02T03-04-05-019b7000-0000-7000-8000-000000000003.jsonl",
        );
        touch(&root, "2026/01/02/notes.jsonl");
        touch(
            &root,
            "2026/01/02/rollout-2026-01-02T03-04-06-not-a-uuid.jsonl",
        );

        let found = discover(&sessions_root(Some(&root)));
        assert_eq!(
            found,
            [
                RolloutFile {
                    path: twin_zst.clone(),
                    session_id: "019b7000-0000-7000-8000-000000000002".into(),
                    compressed: true,
                },
                RolloutFile {
                    path: newest,
                    session_id: "019b7000-0000-7000-8000-000000000003".into(),
                    compressed: false,
                },
                RolloutFile {
                    path: twin.clone(),
                    session_id: "019b7000-0000-7000-8000-000000000002".into(),
                    compressed: false,
                },
                RolloutFile {
                    path: oldest.clone(),
                    session_id: "019b7000-0000-7000-8000-000000000001".into(),
                    compressed: false,
                },
            ]
        );
        assert_eq!(
            resolve("019b7000-0000-7000-8000-000000000002", &root),
            Some(fs::canonicalize(&twin).unwrap())
        );
        fs::remove_file(twin).unwrap();
        assert_eq!(resolve("019b7000-0000-7000-8000-000000000002", &root), None);
        assert_eq!(
            resolve("019b7000-0000-7000-8000-000000000001", &root),
            Some(fs::canonicalize(oldest).unwrap())
        );
        assert_eq!(resolve("019b7000-0000-7000-8000-000000000099", &root), None);
        assert_eq!(sessions_root(Some(&root)), root);
        assert!(sessions_root(None).ends_with(".codex/sessions"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finds_children_from_session_meta_heads() {
        let root = std::env::temp_dir().join(format!(
            "codex-discovery-children-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&root).ok();
        let parent_id = "019b7000-0000-7000-8000-000000000001";
        let first_child = write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-02-019b7000-0000-7000-8000-000000000002.jsonl",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000002\",\"parent_thread_id\":\"{parent_id}\"}}}}\n{{not valid JSON"
            ),
        );
        let newest_child = write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-03-019b7000-0000-7000-8000-000000000003.jsonl",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000003\",\"parent_thread_id\":\"{parent_id}\"}}}}\n"
            ),
        );
        write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-01-019b7000-0000-7000-8000-000000000001.jsonl",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{parent_id}\",\"parent_thread_id\":\"{parent_id}\"}}}}\n"
            ),
        );
        write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-04-019b7000-0000-7000-8000-000000000004.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019b7000-0000-7000-8000-000000000004\",\"parent_thread_id\":\"019b7000-0000-7000-8000-000000000099\"}}\n",
        );
        write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-05-019b7000-0000-7000-8000-000000000005.jsonl.zst",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000005\",\"parent_thread_id\":\"{parent_id}\"}}}}\n"
            ),
        );

        assert_eq!(
            children_of(parent_id, &root),
            [
                RolloutFile {
                    path: newest_child,
                    session_id: "019b7000-0000-7000-8000-000000000003".into(),
                    compressed: false,
                },
                RolloutFile {
                    path: first_child,
                    session_id: "019b7000-0000-7000-8000-000000000002".into(),
                    compressed: false,
                },
            ]
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn head_scan_honors_the_line_cap_and_only_the_first_row() {
        let root = std::env::temp_dir().join(format!(
            "codex-discovery-head-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&root).ok();
        let parent_id = "019b7000-0000-7000-8000-000000000001";
        write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-06-019b7000-0000-7000-8000-000000000006.jsonl",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000006\",\"parent_thread_id\":\"{parent_id}\",\"pad\":\"{}\"}}}}\n",
                "x".repeat(MAX_SESSION_META_LINE_BYTES as usize)
            ),
        );
        write(
            &root,
            "2026/01/01/rollout-2026-01-01T00-00-07-019b7000-0000-7000-8000-000000000007.jsonl",
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000007\"}}}}\n{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019b7000-0000-7000-8000-000000000007\",\"parent_thread_id\":\"{parent_id}\"}}}}\n"
            ),
        );

        assert!(children_of(parent_id, &root).is_empty());

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_returns_canonical_path_through_symlinked_root() {
        let base = std::env::temp_dir().join(format!(
            "codex-discovery-symlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&base).ok();
        fs::create_dir_all(&base).unwrap();
        let root = base.join("sessions");
        let rollout = touch(
            &root,
            "2026/01/02/rollout-2026-01-02T03-04-05-019b7000-0000-7000-8000-000000000003.jsonl",
        );
        let linked_root = base.join("linked-sessions");
        std::os::unix::fs::symlink(&root, &linked_root).unwrap();

        assert_eq!(
            resolve("019b7000-0000-7000-8000-000000000003", &linked_root),
            Some(fs::canonicalize(rollout).unwrap())
        );

        fs::remove_dir_all(&base).ok();
    }
}
