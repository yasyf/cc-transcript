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

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    fn utime_ns(path: &Path, ns: i64) {
        let spec = libc::timespec {
            tv_sec: ns / 1_000_000_000,
            tv_nsec: ns % 1_000_000_000,
        };
        let times = [spec, spec];
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
            0
        );
    }

    #[test]
    fn formula_fallback_when_the_glob_leg_cannot_scan() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/Users/yasyf/Code/cc-skills");
        let claude_dir = tmp.path().join("claude-501");
        let expected = claude_dir
            .join("-Users-yasyf-Code-cc-skills")
            .join("fallback-session")
            .join("scratchpad");
        std::fs::create_dir_all(&expected).unwrap();
        // Execute-only claude dir: read_dir (the glob leg) fails, path traversal
        // (the formula leg) still works — the Python suite forced this split by
        // monkeypatching Path.glob to yield nothing.
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o311)).unwrap();
        let resolved =
            resolve_scratchpad(&[tmp.path().to_path_buf()], 501, cwd, "fallback-session");
        std::fs::set_permissions(&claude_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn traversal_and_dot_sessions_resolve_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("claude-501/x/scratchpad")).unwrap();
        std::fs::create_dir_all(tmp.path().join("claude-501/scratchpad")).unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let cwd = Path::new("/cwd");
        for session in ["../x", "..", ".", ""] {
            assert_eq!(
                resolve_scratchpad(&roots, 501, cwd, session),
                None,
                "{session:?}"
            );
        }
        let absolute = tmp.path().join("absolute-session");
        std::fs::create_dir_all(absolute.join("scratchpad")).unwrap();
        assert_eq!(
            resolve_scratchpad(&roots, 501, cwd, absolute.to_str().unwrap()),
            None
        );
    }

    #[test]
    fn uid_is_respected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            tmp.path()
                .join(format!("claude-502/slug/{SESSION}/scratchpad")),
        )
        .unwrap();
        assert_eq!(
            resolve_scratchpad(&[tmp.path().to_path_buf()], 501, Path::new("/cwd"), SESSION),
            None
        );
    }

    #[test]
    fn nanosecond_mtime_breaks_ties() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp
            .path()
            .join(format!("claude-501/old-slug/{SESSION}/scratchpad"));
        let new = tmp
            .path()
            .join(format!("claude-501/new-slug/{SESSION}/scratchpad"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let base_ns: i64 = 1_700_000_000_000_000_000;
        utime_ns(&old, base_ns);
        utime_ns(&new, base_ns + 1);
        assert_eq!(
            resolve_scratchpad(&[tmp.path().to_path_buf()], 501, Path::new("/cwd"), SESSION),
            Some(new)
        );
    }

    #[test]
    fn dangling_candidate_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let surviving = tmp
            .path()
            .join(format!("claude-501/b-slug/{SESSION}/scratchpad"));
        std::fs::create_dir_all(tmp.path().join("claude-501")).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("nowhere"),
            tmp.path().join("claude-501/a-slug"),
        )
        .unwrap();
        std::fs::create_dir_all(&surviving).unwrap();
        assert_eq!(
            resolve_scratchpad(&[tmp.path().to_path_buf()], 501, Path::new("/cwd"), SESSION),
            Some(surviving)
        );
    }

    #[test]
    fn file_candidate_is_not_a_scratchpad() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(format!("claude-501/slug/{SESSION}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("scratchpad"), b"not a dir").unwrap();
        assert_eq!(
            resolve_scratchpad(&[tmp.path().to_path_buf()], 501, Path::new("/cwd"), SESSION),
            None
        );
    }

    #[test]
    fn slug_matches_the_observed_pair() {
        assert_eq!(
            scratchpad_slug(Path::new("/Users/yasyf/Code/cc-skills")),
            "-Users-yasyf-Code-cc-skills"
        );
        assert_eq!(scratchpad_slug(Path::new("a__é")), "a---");
    }
}
