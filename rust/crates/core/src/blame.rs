//! Blame and attribute engines. [`session_writes`]/[`blame`] find who wrote a file;
//! [`attribute`] classifies how it came to exist. Ungated: no `command`-gated deps.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

use crate::activity::lift_session;
use crate::toolcall::ToolCall;
use crate::types::Entry;

/// The path segment (no leading slash) marking a `.claude/worktrees/<name>` worktree root.
pub const WORKTREES_SEGMENT: &str = ".claude/worktrees/";
const BASH_SUSPECTS: usize = 5;

/// A canonical absolute repository root (no trailing slash) and the lexical path logic
/// that maps a cwd or file into it.
#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub root: String,
}

/// Which working tree a path lives in: the main checkout or a named
/// `.claude/worktrees/<name>` worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tree {
    Main,
    Worktree(String),
}

impl Tree {
    /// The one-token label: `main` or `worktree:<name>`.
    pub fn label(&self) -> String {
        match self {
            Tree::Main => "main".to_string(),
            Tree::Worktree(name) => format!("worktree:{name}"),
        }
    }
}

impl RepoPaths {
    /// The tree a cwd belongs to, or None when it falls outside the repo root — the
    /// `root + "/"` boundary drops prefix-collided sibling repos (`<root>-rust`).
    pub fn tree_of(&self, cwd: &str) -> Option<Tree> {
        if cwd == self.root {
            return Some(Tree::Main);
        }
        let rest = cwd.strip_prefix(&self.root)?.strip_prefix('/')?;
        match rest.strip_prefix(WORKTREES_SEGMENT) {
            Some(tail) => tail
                .split('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(|name| Tree::Worktree(name.to_string())),
            None => Some(Tree::Main),
        }
    }

    /// The on-disk root of a tree within this repo.
    fn tree_root(&self, tree: &Tree) -> String {
        match tree {
            Tree::Main => self.root.clone(),
            Tree::Worktree(name) => format!("{}/{WORKTREES_SEGMENT}{name}", self.root),
        }
    }

    /// The `(tree, repo-relative path)` a target resolves to, or None outside the repo.
    /// `file_path` is used as-is when absolute, else joined onto `cwd` (root when absent);
    /// either way the path is lexically normalized (`.`/`..` folded) and the tree is read
    /// from where the NORMALIZED path lands, so a relative ref crossing a tree boundary
    /// resolves to the tree it actually lands in, not `cwd`'s.
    pub fn relative(&self, file_path: &str, cwd: Option<&str>) -> Option<(Tree, String)> {
        let normalized = normalize(&if file_path.starts_with('/') {
            file_path.to_string()
        } else {
            format!("{}/{file_path}", cwd.unwrap_or(&self.root))
        });
        let tree = self.tree_of(&normalized)?;
        let rel = normalized
            .strip_prefix(&format!("{}/", self.tree_root(&tree)))?
            .to_string();
        Some((tree, rel))
    }
}

/// Lexically folds `.` and `..` segments and collapses redundant separators over an
/// absolute path — pure string logic, no filesystem access.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Whether `ts` falls in the half-open window `[since, until)`, each bound optional.
fn within(
    ts: DateTime<FixedOffset>,
    since: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
) -> bool {
    since.is_none_or(|start| start <= ts) && until.is_none_or(|end| ts < end)
}

/// One session's writes to a target file within a single tree; newest-first ordering is
/// applied by [`blame`].
#[derive(Debug, Clone)]
pub struct SessionWrites {
    pub session_id: String,
    pub transcript_path: String,
    pub tree: String,
    pub first_write_ts: DateTime<FixedOffset>,
    pub last_write_ts: DateTime<FixedOffset>,
    pub writes: usize,
    pub tools: Vec<String>,
    pub first_prompt: Option<String>,
}

/// The running span, count, and distinct tools of one `(session, tree)`'s writes.
struct WriteGroup {
    first: DateTime<FixedOffset>,
    last: DateTime<FixedOffset>,
    writes: usize,
    tools: Vec<String>,
}

impl WriteGroup {
    fn new(ts: DateTime<FixedOffset>, tool: &str) -> WriteGroup {
        WriteGroup {
            first: ts,
            last: ts,
            writes: 1,
            tools: vec![tool.to_string()],
        }
    }

    fn add(&mut self, ts: DateTime<FixedOffset>, tool: &str) {
        self.first = self.first.min(ts);
        self.last = self.last.max(ts);
        self.writes += 1;
        if !self.tools.iter().any(|seen| seen == tool) {
            self.tools.push(tool.to_string());
        }
    }
}

/// Every write to `rel_target` in `entries`, grouped one record per tree, within the
/// optional `[since, until)` window; tool names are distinct in first-seen order and
/// `first_prompt` is the session's first non-empty turn prompt.
pub fn session_writes(
    session_id: &str,
    transcript_path: &str,
    entries: &[Entry],
    repo: &RepoPaths,
    rel_target: &str,
    since: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
) -> Vec<SessionWrites> {
    let lifted = lift_session(session_id, entries);
    let first_prompt = lifted
        .turns
        .iter()
        .map(|turn| turn.prompt.as_str())
        .find(|prompt| !prompt.is_empty())
        .map(str::to_string);
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, WriteGroup> = HashMap::new();
    for use_ in lifted.turns.iter().flat_map(|turn| turn.tool_uses.iter()) {
        for (path, _hunks) in &use_.edits {
            let Some((tree, rel)) = repo.relative(path, use_.cwd) else {
                continue;
            };
            if rel != rel_target || !within(use_.ts, since, until) {
                continue;
            }
            match groups.get_mut(&tree.label()) {
                Some(group) => group.add(use_.ts, use_.name),
                None => {
                    let label = tree.label();
                    order.push(label.clone());
                    groups.insert(label, WriteGroup::new(use_.ts, use_.name));
                }
            }
        }
    }
    order
        .into_iter()
        .map(|label| {
            let group = groups
                .remove(&label)
                .expect("group exists for ordered label");
            SessionWrites {
                session_id: session_id.to_string(),
                transcript_path: transcript_path.to_string(),
                tree: label,
                first_write_ts: group.first,
                last_write_ts: group.last,
                writes: group.writes,
                tools: group.tools,
                first_prompt: first_prompt.clone(),
            }
        })
        .collect()
}

/// Every [`SessionWrites`] sorted newest write first, ties broken by ascending session
/// id.
pub fn blame(mut all: Vec<SessionWrites>) -> Vec<SessionWrites> {
    all.sort_by(|a, b| {
        b.last_write_ts
            .cmp(&a.last_write_ts)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    all
}

/// The winning edit-shaped call behind a `claude` verdict.
#[derive(Debug, Clone)]
pub struct WriteEvidence {
    pub transcript_path: String,
    pub tree: String,
    pub ts: DateTime<FixedOffset>,
    pub tool: String,
    pub tool_use_id: String,
}

/// A session active over a file's mtime with Bash activity — a `generated` suspect.
#[derive(Debug, Clone)]
pub struct GeneratedCandidate {
    pub session_id: String,
    pub transcript_path: String,
    pub window_start: DateTime<FixedOffset>,
    pub window_end: DateTime<FixedOffset>,
    pub bash: Vec<(DateTime<FixedOffset>, String)>,
}

/// How a file came to exist: written by a Claude session, generated by a session's
/// tooling, or external to any session.
#[derive(Debug, Clone)]
pub enum Verdict {
    Claude {
        session_id: String,
        evidence: WriteEvidence,
    },
    Generated {
        candidates: Vec<GeneratedCandidate>,
    },
    External,
}

/// Classifies `rel_target` against its `mtime`: an edit-shaped write to it (newest wins,
/// ties by ascending session id) is `claude`; else every repo-member session with Bash
/// activity whose active window contains `mtime` is a `generated` candidate (newest
/// window first); else `external`.
pub fn attribute(
    sessions: &[(&str, &str, &[Entry])],
    repo: &RepoPaths,
    rel_target: &str,
    mtime: DateTime<FixedOffset>,
) -> Verdict {
    if let Some(verdict) = claude_verdict(sessions, repo, rel_target) {
        return verdict;
    }
    match generated_candidates(sessions, repo, mtime) {
        candidates if !candidates.is_empty() => Verdict::Generated { candidates },
        _ => Verdict::External,
    }
}

/// The newest edit-shaped write to `rel_target` across all sessions, if any.
fn claude_verdict(
    sessions: &[(&str, &str, &[Entry])],
    repo: &RepoPaths,
    rel_target: &str,
) -> Option<Verdict> {
    let mut writes: Vec<(String, WriteEvidence)> = Vec::new();
    for &(session_id, path, entries) in sessions {
        let lifted = lift_session(session_id, entries);
        for use_ in lifted.turns.iter().flat_map(|turn| turn.tool_uses.iter()) {
            for (file, _hunks) in &use_.edits {
                match repo.relative(file, use_.cwd) {
                    Some((tree, rel)) if rel == rel_target => writes.push((
                        session_id.to_string(),
                        WriteEvidence {
                            transcript_path: path.to_string(),
                            tree: tree.label(),
                            ts: use_.ts,
                            tool: use_.name.to_string(),
                            tool_use_id: use_.tool_use_id.to_string(),
                        },
                    )),
                    _ => {}
                }
            }
        }
    }
    writes
        .into_iter()
        .max_by(|a, b| a.1.ts.cmp(&b.1.ts).then_with(|| b.0.cmp(&a.0)))
        .map(|(session_id, evidence)| Verdict::Claude {
            session_id,
            evidence,
        })
}

/// Every repo-member session with Bash activity whose active window contains `mtime`,
/// newest window first; each carries its last up-to-five Bash calls at or before `mtime`.
fn generated_candidates(
    sessions: &[(&str, &str, &[Entry])],
    repo: &RepoPaths,
    mtime: DateTime<FixedOffset>,
) -> Vec<GeneratedCandidate> {
    let mut candidates: Vec<GeneratedCandidate> = sessions
        .iter()
        .filter_map(|&(session_id, path, entries)| {
            let stamps: Vec<DateTime<FixedOffset>> = entries
                .iter()
                .filter_map(Entry::meta)
                .map(|meta| meta.timestamp)
                .collect();
            let window_start = stamps.iter().min().copied()?;
            let window_end = stamps.iter().max().copied()?;
            let member = entries
                .iter()
                .filter_map(Entry::meta)
                .filter_map(|meta| meta.cwd.as_deref())
                .any(|cwd| repo.tree_of(cwd).is_some());
            if !member || !(window_start <= mtime && mtime <= window_end) {
                return None;
            }
            let bash: Vec<(DateTime<FixedOffset>, String)> = lift_session(session_id, entries)
                .turns
                .iter()
                .flat_map(|turn| turn.tool_uses.iter())
                .filter_map(|use_| match &use_.call {
                    ToolCall::Bash(call) => Some((use_.ts, call.command.clone())),
                    _ => None,
                })
                .collect();
            if bash.is_empty() {
                return None;
            }
            let mut before: Vec<(DateTime<FixedOffset>, String)> =
                bash.into_iter().filter(|(ts, _)| *ts <= mtime).collect();
            Some(GeneratedCandidate {
                session_id: session_id.to_string(),
                transcript_path: path.to_string(),
                window_start,
                window_end,
                bash: before.split_off(before.len().saturating_sub(BASH_SUSPECTS)),
            })
        })
        .collect();
    candidates.sort_by(|a, b| b.window_end.cmp(&a.window_end));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_entry;

    fn repo() -> RepoPaths {
        RepoPaths {
            root: "/a/repo".to_string(),
        }
    }

    fn ts(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    fn parse(raw: &str) -> Entry {
        parse_entry(sonic_rs::from_str(raw).unwrap()).unwrap()
    }

    fn user(text: &str, at: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"user","uuid":"u-{at}","sessionId":"s","timestamp":"{at}","message":{{"role":"user","content":{}}}}}"#,
            sonic_rs::to_string(&text).unwrap()
        ))
    }

    fn tool_use(name: &str, id: &str, cwd: &str, at: &str, input: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"assistant","uuid":"a-{id}","sessionId":"s","timestamp":"{at}","cwd":{},"message":{{"model":"m","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#,
            sonic_rs::to_string(&cwd).unwrap()
        ))
    }

    fn edit(id: &str, cwd: &str, at: &str, file_path: &str) -> Entry {
        tool_use(
            "Edit",
            id,
            cwd,
            at,
            &format!(
                r#"{{"file_path":{},"old_string":"x","new_string":"y"}}"#,
                sonic_rs::to_string(&file_path).unwrap()
            ),
        )
    }

    fn bash(id: &str, cwd: &str, at: &str, command: &str) -> Entry {
        tool_use(
            "Bash",
            id,
            cwd,
            at,
            &format!(
                r#"{{"command":{}}}"#,
                sonic_rs::to_string(&command).unwrap()
            ),
        )
    }

    fn apply_patch(id: &str, cwd: &str, at: &str, files: &[&str]) -> Entry {
        let body: String = files
            .iter()
            .map(|file| format!("*** Update File: {file}\n ctx\n"))
            .collect();
        let envelope = format!("*** Begin Patch\n{body}*** End Patch");
        tool_use(
            "apply_patch",
            id,
            cwd,
            at,
            &sonic_rs::to_string(&envelope).unwrap(),
        )
    }

    #[test]
    fn tree_of_places_main_worktree_and_rejects_outsiders() {
        let repo = repo();
        assert_eq!(repo.tree_of("/a/repo"), Some(Tree::Main));
        assert_eq!(repo.tree_of("/a/repo/src"), Some(Tree::Main));
        assert_eq!(
            repo.tree_of("/a/repo/.claude/worktrees/wt1/src"),
            Some(Tree::Worktree("wt1".to_string()))
        );
        assert_eq!(repo.tree_of("/a/repo-rust"), None);
        assert_eq!(repo.tree_of("/elsewhere"), None);
    }

    #[test]
    fn relative_resolves_absolute_relative_and_normalizes() {
        let repo = repo();
        assert_eq!(
            repo.relative("/a/repo/src/app.py", Some("/a/repo")),
            Some((Tree::Main, "src/app.py".to_string()))
        );
        // Absolute worktree file edited from a main cwd is still a worktree hit.
        assert_eq!(
            repo.relative("/a/repo/.claude/worktrees/wt1/src/app.py", Some("/a/repo")),
            Some((Tree::Worktree("wt1".to_string()), "src/app.py".to_string()))
        );
        assert_eq!(
            repo.relative("src/app.py", Some("/a/repo")),
            Some((Tree::Main, "src/app.py".to_string()))
        );
        // No cwd falls back to the repo root.
        assert_eq!(
            repo.relative("src/app.py", None),
            Some((Tree::Main, "src/app.py".to_string()))
        );
        assert_eq!(
            repo.relative("/a/repo/src/../lib/app.py", Some("/a/repo")),
            Some((Tree::Main, "lib/app.py".to_string()))
        );
        assert_eq!(repo.relative("/elsewhere/app.py", Some("/elsewhere")), None);
        // A worktree cwd's relative ref that climbs back out into the main tree resolves
        // to Main, not to the worktree the cwd sits in.
        assert_eq!(
            repo.relative("../../../src/app.py", Some("/a/repo/.claude/worktrees/wt1")),
            Some((Tree::Main, "src/app.py".to_string()))
        );
        // Mirror: a main cwd's relative ref that descends into a worktree resolves there.
        assert_eq!(
            repo.relative(".claude/worktrees/wt1/src/app.py", Some("/a/repo")),
            Some((Tree::Worktree("wt1".to_string()), "src/app.py".to_string()))
        );
    }

    #[test]
    fn session_writes_counts_edits_ignoring_reads_and_bash() {
        let entries = vec![
            user("do it", "2026-01-01T10:00:00Z"),
            edit(
                "e1",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                "/a/repo/src/app.py",
            ),
            tool_use(
                "Read",
                "r1",
                "/a/repo",
                "2026-01-01T10:00:02Z",
                r#"{"file_path":"/a/repo/src/app.py"}"#,
            ),
            bash("b1", "/a/repo", "2026-01-01T10:00:03Z", "cat src/app.py"),
        ];
        let records = session_writes("s", "/t.jsonl", &entries, &repo(), "src/app.py", None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tree, "main");
        assert_eq!(records[0].writes, 1);
        assert_eq!(records[0].tools, vec!["Edit".to_string()]);
        assert_eq!(records[0].first_prompt.as_deref(), Some("do it"));
        assert_eq!(records[0].first_write_ts, ts("2026-01-01T10:00:01Z"));
    }

    #[test]
    fn session_writes_counts_apply_patch_secondary_file() {
        let entries = vec![
            user("patch", "2026-01-01T10:00:00Z"),
            apply_patch(
                "p1",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                &["/a/repo/src/other.py", "/a/repo/src/app.py"],
            ),
        ];
        let records = session_writes("s", "/t.jsonl", &entries, &repo(), "src/app.py", None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].writes, 1);
        assert_eq!(records[0].tools, vec!["apply_patch".to_string()]);
    }

    #[test]
    fn session_writes_applies_the_half_open_window() {
        let entries = vec![
            user("do it", "2026-01-01T10:00:00Z"),
            edit(
                "e1",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                "/a/repo/src/app.py",
            ),
        ];
        let hit = session_writes(
            "s",
            "/t.jsonl",
            &entries,
            &repo(),
            "src/app.py",
            Some(ts("2026-01-01T10:00:00Z")),
            Some(ts("2026-01-01T10:00:02Z")),
        );
        assert_eq!(hit.len(), 1);
        // since after the write excludes it.
        assert!(session_writes(
            "s",
            "/t.jsonl",
            &entries,
            &repo(),
            "src/app.py",
            Some(ts("2026-01-01T10:00:02Z")),
            None,
        )
        .is_empty());
        // until equal to the write ts excludes it (upper bound is exclusive).
        assert!(session_writes(
            "s",
            "/t.jsonl",
            &entries,
            &repo(),
            "src/app.py",
            None,
            Some(ts("2026-01-01T10:00:01Z")),
        )
        .is_empty());
    }

    #[test]
    fn session_writes_separates_main_and_worktree_records() {
        let entries = vec![
            user("do it", "2026-01-01T10:00:00Z"),
            edit(
                "e1",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                "/a/repo/src/app.py",
            ),
            edit(
                "e2",
                "/a/repo/.claude/worktrees/wt1",
                "2026-01-01T10:00:02Z",
                "/a/repo/.claude/worktrees/wt1/src/app.py",
            ),
        ];
        let records = session_writes("s", "/t.jsonl", &entries, &repo(), "src/app.py", None, None);
        let trees: Vec<&str> = records.iter().map(|r| r.tree.as_str()).collect();
        assert_eq!(records.len(), 2);
        assert!(trees.contains(&"main"));
        assert!(trees.contains(&"worktree:wt1"));
    }

    #[test]
    fn session_writes_resolves_a_relative_edit_crossing_into_the_main_tree() {
        let entries = vec![
            user("do it", "2026-01-01T10:00:00Z"),
            edit(
                "e1",
                "/a/repo/.claude/worktrees/wt1",
                "2026-01-01T10:00:01Z",
                "../../../src/app.py",
            ),
        ];
        let records = session_writes("s", "/t.jsonl", &entries, &repo(), "src/app.py", None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tree, "main");
    }

    #[test]
    fn session_writes_tracks_distinct_tools_first_seen() {
        let entries = vec![
            user("do it", "2026-01-01T10:00:00Z"),
            edit(
                "e1",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                "/a/repo/src/app.py",
            ),
            apply_patch(
                "p1",
                "/a/repo",
                "2026-01-01T10:00:02Z",
                &["/a/repo/src/app.py"],
            ),
            edit(
                "e2",
                "/a/repo",
                "2026-01-01T10:00:03Z",
                "/a/repo/src/app.py",
            ),
        ];
        let records = session_writes("s", "/t.jsonl", &entries, &repo(), "src/app.py", None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].writes, 3);
        assert_eq!(
            records[0].tools,
            vec!["Edit".to_string(), "apply_patch".to_string()]
        );
        assert_eq!(records[0].last_write_ts, ts("2026-01-01T10:00:03Z"));
    }

    #[test]
    fn blame_sorts_newest_first_with_session_tiebreak() {
        let make = |session_id: &str, last: &str| SessionWrites {
            session_id: session_id.to_string(),
            transcript_path: "/t.jsonl".to_string(),
            tree: "main".to_string(),
            first_write_ts: ts("2026-01-01T09:00:00Z"),
            last_write_ts: ts(last),
            writes: 1,
            tools: vec!["Edit".to_string()],
            first_prompt: None,
        };
        let sorted = blame(vec![
            make("bbb", "2026-01-01T10:00:00Z"),
            make("older", "2026-01-01T09:30:00Z"),
            make("aaa", "2026-01-01T10:00:00Z"),
        ]);
        let ids: Vec<&str> = sorted.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "bbb", "older"]);
    }

    #[test]
    fn attribute_claude_picks_the_newest_write() {
        let old = vec![
            user("first", "2026-01-01T09:00:00Z"),
            edit(
                "e1",
                "/a/repo",
                "2026-01-01T09:00:01Z",
                "/a/repo/src/app.py",
            ),
        ];
        let new = vec![
            user("second", "2026-01-01T10:00:00Z"),
            edit(
                "e2",
                "/a/repo",
                "2026-01-01T10:00:01Z",
                "/a/repo/src/app.py",
            ),
        ];
        let sessions = [
            ("old", "/old.jsonl", old.as_slice()),
            ("new", "/new.jsonl", new.as_slice()),
        ];
        match attribute(&sessions, &repo(), "src/app.py", ts("2026-01-01T11:00:00Z")) {
            Verdict::Claude {
                session_id,
                evidence,
            } => {
                assert_eq!(session_id, "new");
                assert_eq!(evidence.tool, "Edit");
                assert_eq!(evidence.tree, "main");
                assert_eq!(evidence.tool_use_id, "e2");
                assert_eq!(evidence.ts, ts("2026-01-01T10:00:01Z"));
            }
            other => panic!("expected claude, got {other:?}"),
        }
    }

    #[test]
    fn attribute_generated_keeps_last_five_bash_before_mtime() {
        let entries = vec![
            user("build", "2026-01-01T10:00:00Z"),
            bash("b1", "/a/repo", "2026-01-01T10:00:01Z", "step one"),
            bash("b2", "/a/repo", "2026-01-01T10:00:02Z", "step two"),
            bash("b3", "/a/repo", "2026-01-01T10:00:03Z", "step three"),
            bash("b4", "/a/repo", "2026-01-01T10:00:04Z", "step four"),
            bash("b5", "/a/repo", "2026-01-01T10:00:05Z", "step five"),
            bash("b6", "/a/repo", "2026-01-01T10:00:06Z", "step six"),
            bash("b7", "/a/repo", "2026-01-01T10:00:30Z", "after mtime"),
        ];
        let sessions = [("gen", "/gen.jsonl", entries.as_slice())];
        match attribute(&sessions, &repo(), "src/app.py", ts("2026-01-01T10:00:10Z")) {
            Verdict::Generated { candidates } => {
                assert_eq!(candidates.len(), 1);
                let candidate = &candidates[0];
                assert_eq!(candidate.session_id, "gen");
                assert_eq!(candidate.window_start, ts("2026-01-01T10:00:00Z"));
                assert_eq!(candidate.window_end, ts("2026-01-01T10:00:30Z"));
                let commands: Vec<&str> = candidate.bash.iter().map(|(_, c)| c.as_str()).collect();
                assert_eq!(
                    commands,
                    vec![
                        "step two",
                        "step three",
                        "step four",
                        "step five",
                        "step six"
                    ]
                );
            }
            other => panic!("expected generated, got {other:?}"),
        }
    }

    #[test]
    fn attribute_external_when_mtime_outside_every_window() {
        let entries = vec![
            user("build", "2026-01-01T10:00:00Z"),
            bash("b1", "/a/repo", "2026-01-01T10:00:01Z", "make"),
        ];
        let sessions = [("gen", "/gen.jsonl", entries.as_slice())];
        assert!(matches!(
            attribute(&sessions, &repo(), "src/app.py", ts("2026-01-02T00:00:00Z")),
            Verdict::External
        ));
    }

    #[test]
    fn attribute_ignores_sessions_outside_the_repo() {
        let entries = vec![
            user("build", "2026-01-01T10:00:00Z"),
            bash("b1", "/elsewhere", "2026-01-01T10:00:01Z", "make"),
        ];
        let sessions = [("foreign", "/foreign.jsonl", entries.as_slice())];
        assert!(matches!(
            attribute(&sessions, &repo(), "src/app.py", ts("2026-01-01T10:00:00Z")),
            Verdict::External
        ));
    }
}
