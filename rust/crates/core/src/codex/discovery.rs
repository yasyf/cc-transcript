use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

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
        .or_else(|| {
            found
                .iter()
                .find(|rollout| rollout.session_id == session_id)
        })
        .map(|rollout| rollout.path.clone())
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
            Some(twin.clone())
        );
        fs::remove_file(twin).unwrap();
        assert_eq!(
            resolve("019b7000-0000-7000-8000-000000000002", &root),
            Some(twin_zst)
        );
        assert_eq!(
            resolve("019b7000-0000-7000-8000-000000000001", &root),
            Some(oldest)
        );
        assert_eq!(resolve("019b7000-0000-7000-8000-000000000099", &root), None);
        assert_eq!(sessions_root(Some(&root)), root);
        assert!(sessions_root(None).ends_with(".codex/sessions"));
        fs::remove_dir_all(&root).ok();
    }
}
