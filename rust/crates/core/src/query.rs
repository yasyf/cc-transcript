//! Session-level queries over the lifted activity (query.py).
//!
//! A [`Session`] is an immutable windowed view of a [`LiftedSession`]'s turns; every
//! slice returns another [`Session`]. A window turn is a `(turn, lo, hi)` view over the
//! lifted turn's events — slicing never clones a `ToolCall`, and the prompt survives
//! only when the window opens at the turn's first event (query.py `trim_turn`).
//!
//! Core does no IO, so query.py's filesystem surface (`from_path`, `from_id`,
//! `.subagents`, `sidechain_sessions`, `.notifications`) is out of scope; the `has_*`
//! methods port the self-session branch only (sidechain recursion is `subagents=False`).

use std::collections::HashMap;

use regex::Regex;
use sonic_rs::Value;

use crate::activity::{LiftedSession, ToolUse, Turn};
use crate::pystr;
use crate::render::py_str;
use crate::toolcall::{expand_tool_names, tool_name_matches, ToolCall};
use crate::types::{joined_text, matches_names, Entry};
use crate::value::field;

#[cfg(feature = "command")]
use crate::command::CommandLine;

/// query.py `FileRef.TEST_PATTERNS`.
const TEST_PATTERNS: [&str; 3] = ["**/test_*.py", "**/conftest.py", "**/tests/**/*.py"];

/// query.py `is_failure`.
fn is_failure(use_: &ToolUse) -> bool {
    use_.result.is_some_and(|r| r.is_error)
}

/// query.py `carries_token`.
fn carries_token(event: &Entry, token: &str) -> bool {
    match event {
        Entry::User(user) => {
            user.content.text().contains(token)
                || user.tool_results().any(|tr| tr.content.contains(token))
        }
        Entry::Assistant(assistant) => joined_text(&assistant.blocks).contains(token),
        Entry::System(system) => system.content.as_deref().is_some_and(|c| c.contains(token)),
        _ => false,
    }
}

/// The last component of a POSIX path (`PurePosixPath.name`): the final non-empty
/// `/`-separated segment, or `""`.
fn pure_name(path: &str) -> &str {
    path.rsplit('/').find(|c| !c.is_empty()).unwrap_or("")
}

/// The extension of a name including the leading dot (`PurePosixPath.suffix`), or `""`
/// when the last dot is leading or trailing.
fn pure_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// The first `n` code points of `s` (Python `s[:n]`).
fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Python `fnmatch.translate`: a glob compiled to an anchored, DOTALL regex. `*`
/// matches any run (including `/` and newlines), `?` one character, `[seq]` a class;
/// every other character is escaped literally.
fn glob_to_regex(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let n = chars.len();
    let mut body = String::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => body.push_str(".*"),
            '?' => body.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    body.push_str("\\[");
                } else {
                    let stuff: String =
                        chars[i..j].iter().collect::<String>().replace('\\', "\\\\");
                    i = j + 1;
                    body.push('[');
                    if let Some(rest) = stuff.strip_prefix('!') {
                        body.push('^');
                        body.push_str(rest);
                    } else if let Some(rest) = stuff.strip_prefix('^') {
                        body.push_str("\\^");
                        body.push_str(rest);
                    } else {
                        body.push_str(&stuff);
                    }
                    body.push(']');
                }
            }
            other => body.push_str(&regex::escape(&other.to_string())),
        }
    }
    format!("(?s)\\A(?:{body})\\z")
}

/// Python `fnmatch.fnmatch` on a POSIX path (no case-folding).
fn fnmatch(name: &str, pat: &str) -> bool {
    Regex::new(&glob_to_regex(pat))
        .expect("translated glob is valid regex")
        .is_match(name)
}

/// A file path carried by a tool call, with glob and prefix matching (query.py `FileRef`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub path: String,
}

impl FileRef {
    pub fn new(path: &str) -> FileRef {
        FileRef {
            path: path.to_string(),
        }
    }

    /// Whether the path names a Python test file.
    pub fn is_test(&self) -> bool {
        self.matches(&TEST_PATTERNS)
    }

    /// The file extension including the leading dot (e.g. `.py`), or `""`.
    pub fn suffix(&self) -> &str {
        pure_suffix(pure_name(&self.path))
    }

    /// Whether the full path or the basename matches any glob.
    pub fn matches(&self, globs: &[&str]) -> bool {
        let name = pure_name(&self.path);
        globs
            .iter()
            .any(|glob| fnmatch(&self.path, glob) || fnmatch(name, glob))
    }

    /// Whether the path starts with, or contains a `/`-anchored, prefix.
    pub fn under(&self, prefixes: &[&str]) -> bool {
        prefixes.iter().any(|prefix| {
            self.path.starts_with(prefix) || self.path.contains(&format!("/{prefix}"))
        })
    }
}

/// A `where_input` rule (query.py `input_rule_matches`): a regex searched against
/// `str(value)`, a predicate over the raw value, or a value compared for equality.
pub enum InputRule<'r> {
    Regex(&'r Regex),
    Predicate(&'r dyn Fn(&Value) -> bool),
    Equals(Value),
}

fn input_rule_matches(rule: &InputRule, value: &Value) -> bool {
    match rule {
        InputRule::Regex(re) => re.is_match(&py_str(value)),
        InputRule::Predicate(pred) => pred(value),
        InputRule::Equals(expected) => value == expected,
    }
}

/// A chainable filter over a window's tool calls (query.py `ToolCallQuery`).
///
/// Calls whose result errored are hidden by default; [`ToolCallQuery::with_errors`]
/// widens the view and [`ToolCallQuery::failed`] inverts it.
pub struct ToolCallQuery<'a> {
    all_items: Vec<&'a ToolUse<'a>>,
    include_errors: bool,
}

impl<'a> ToolCallQuery<'a> {
    /// The effective view: every call, or only those that did not error.
    pub fn items(&self) -> Vec<&'a ToolUse<'a>> {
        if self.include_errors {
            self.all_items.clone()
        } else {
            self.all_items
                .iter()
                .copied()
                .filter(|use_| !is_failure(use_))
                .collect()
        }
    }

    /// The same query with errored calls included.
    pub fn with_errors(&self) -> ToolCallQuery<'a> {
        ToolCallQuery {
            all_items: self.all_items.clone(),
            include_errors: true,
        }
    }

    fn filtered(&self, pred: impl Fn(&ToolUse<'a>) -> bool) -> ToolCallQuery<'a> {
        ToolCallQuery {
            all_items: self
                .all_items
                .iter()
                .copied()
                .filter(|&use_| pred(use_))
                .collect(),
            include_errors: self.include_errors,
        }
    }

    /// Calls whose tool name matches a pipe spec, honoring aliases and MCP suffixes.
    pub fn named(&self, spec: &str) -> ToolCallQuery<'a> {
        self.filtered(|use_| tool_name_matches(use_.call.name(), spec))
    }

    /// Calls targeting a file that matches any glob.
    pub fn touching(&self, globs: &[&str]) -> ToolCallQuery<'a> {
        self.filtered(|use_| {
            use_.call
                .file_path()
                .is_some_and(|p| FileRef::new(p).matches(globs))
        })
    }

    /// Calls targeting a file under any prefix.
    pub fn under(&self, prefixes: &[&str]) -> ToolCallQuery<'a> {
        self.filtered(|use_| {
            use_.call
                .file_path()
                .is_some_and(|p| FileRef::new(p).under(prefixes))
        })
    }

    /// Only the calls whose result errored.
    pub fn failed(&self) -> ToolCallQuery<'a> {
        ToolCallQuery {
            all_items: self
                .all_items
                .iter()
                .copied()
                .filter(|use_| is_failure(use_))
                .collect(),
            include_errors: true,
        }
    }

    /// Calls fired in any of the given session turn indices.
    pub fn in_turns(&self, indices: &[usize]) -> ToolCallQuery<'a> {
        self.filtered(|use_| indices.contains(&use_.turn_index))
    }

    /// Calls satisfying `pred`.
    pub fn where_(&self, pred: impl Fn(&ToolUse<'a>) -> bool) -> ToolCallQuery<'a> {
        self.filtered(pred)
    }

    /// Calls whose raw input carries every key, each matching its rule.
    pub fn where_input(&self, rules: &[(&str, InputRule)]) -> ToolCallQuery<'a> {
        self.filtered(|use_| {
            rules.iter().all(|(key, rule)| {
                field(use_.call.raw(), key).is_some_and(|v| input_rule_matches(rule, v))
            })
        })
    }

    /// The number of matching calls.
    pub fn count(&self) -> usize {
        self.items().len()
    }

    /// Whether any call matches.
    pub fn any(&self) -> bool {
        !self.items().is_empty()
    }

    /// The earliest matching call, or None.
    pub fn first(&self) -> Option<&'a ToolUse<'a>> {
        self.items().into_iter().next()
    }

    /// The latest matching call, or None.
    pub fn last(&self) -> Option<&'a ToolUse<'a>> {
        self.items().into_iter().last()
    }

    /// The matching calls as a list.
    pub fn list(&self) -> Vec<&'a ToolUse<'a>> {
        self.items()
    }

    /// The files the matching calls target, one entry per call, in order.
    pub fn files(&self) -> Vec<FileRef> {
        self.items()
            .into_iter()
            .filter_map(|use_| use_.call.file_path().map(FileRef::new))
            .collect()
    }
}

/// A `(turn, lo, hi)` view over a lifted turn's events. The prompt survives only when
/// the window opens at the turn's first event (query.py `trim_turn`).
#[derive(Clone, Copy)]
struct WindowTurn<'a> {
    turn: &'a Turn<'a>,
    lo: usize,
    hi: usize,
}

impl<'a> WindowTurn<'a> {
    fn full(turn: &'a Turn<'a>) -> WindowTurn<'a> {
        WindowTurn {
            turn,
            lo: 0,
            hi: turn.events.len(),
        }
    }

    fn events(&self) -> &'a [Entry] {
        &self.turn.events[self.lo..self.hi]
    }

    fn size(&self) -> usize {
        self.hi - self.lo
    }

    fn prompt(&self) -> &'a str {
        if self.lo == 0 {
            &self.turn.prompt
        } else {
            ""
        }
    }

    fn tool_uses(&self) -> Vec<&'a ToolUse<'a>> {
        if self.lo == 0 && self.hi == self.turn.events.len() {
            return self.turn.tool_uses.iter().collect();
        }
        let positions = turn_positions(self.turn);
        self.turn
            .tool_uses
            .iter()
            .filter(|use_| {
                let pos = positions[use_.event_uuid];
                self.lo <= pos && pos < self.hi
            })
            .collect()
    }
}

/// `{meta.uuid: index}` over a turn's events (query.py `trim_turn` positions): every
/// event advances the index, only meta-bearing ones are keyed.
fn turn_positions<'a>(turn: &'a Turn<'a>) -> HashMap<&'a str, usize> {
    turn.events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.meta().map(|m| (m.uuid.as_str(), i)))
        .collect()
}

/// An immutable windowed view of a session's turns (query.py `Session`).
#[derive(Clone)]
pub struct Session<'a> {
    turns: Vec<WindowTurn<'a>>,
}

impl<'a> Session<'a> {
    /// Views a lifted session's full turn range as a session (query.py `from_activity`).
    pub fn from_lift(lift: &'a LiftedSession<'a>) -> Session<'a> {
        Session {
            turns: lift.turns.iter().map(WindowTurn::full).collect(),
        }
    }

    fn events_iter(&self) -> impl Iterator<Item = &'a Entry> + '_ {
        self.turns.iter().flat_map(|wt| wt.events().iter())
    }

    /// Every event in the window, in order.
    pub fn events(&self) -> Vec<&'a Entry> {
        self.events_iter().collect()
    }

    /// The number of events in the window (Python `len`).
    pub fn len(&self) -> usize {
        self.turns.iter().map(WindowTurn::size).sum()
    }

    /// Whether the window carries any event (Python `bool`).
    pub fn non_empty(&self) -> bool {
        self.turns.iter().any(|wt| wt.size() > 0)
    }

    /// `{meta.uuid: index}` over the window's flattened events (query.py `event_positions`).
    fn event_positions(&self) -> HashMap<&'a str, usize> {
        self.events_iter()
            .enumerate()
            .filter_map(|(i, e)| e.meta().map(|m| (m.uuid.as_str(), i)))
            .collect()
    }

    /// The window's tool calls as a chainable query.
    pub fn tool_calls(&self) -> ToolCallQuery<'a> {
        ToolCallQuery {
            all_items: self.turns.iter().flat_map(|wt| wt.tool_uses()).collect(),
            include_errors: false,
        }
    }

    /// The sub-window over the flattened event index range `[start, stop)` (query.py `windowed`).
    fn windowed(&self, start: usize, stop: usize) -> Session<'a> {
        let mut turns = Vec::new();
        let mut base = 0usize;
        for wt in &self.turns {
            let size = wt.size();
            let lo = (start as isize - base as isize).max(0);
            let hi = (stop as isize - base as isize).min(size as isize);
            if lo < hi {
                turns.push(WindowTurn {
                    turn: wt.turn,
                    lo: wt.lo + lo as usize,
                    hi: wt.lo + hi as usize,
                });
            }
            base += size;
        }
        Session { turns }
    }

    /// The window strictly after the last call matching `tool` (query.py `after`).
    pub fn after(&self, tool: &str, file: Option<&str>) -> Session<'a> {
        let positions = self.event_positions();
        let last = self
            .tool_calls()
            .all_items
            .iter()
            .filter(|use_| {
                tool_name_matches(use_.call.name(), tool)
                    && file.is_none_or(|f| use_.call.file_path().is_some_and(|p| p.contains(f)))
            })
            .map(|use_| positions[use_.event_uuid])
            .max();
        match last {
            Some(mx) => self.windowed(mx + 1, self.len()),
            None => self.windowed(0, 0),
        }
    }

    /// The window strictly before the last call matching `tool` (query.py `before`).
    pub fn before(&self, tool: &str) -> Session<'a> {
        let positions = self.event_positions();
        let last = self
            .tool_calls()
            .all_items
            .iter()
            .filter(|use_| tool_name_matches(use_.call.name(), tool))
            .map(|use_| positions[use_.event_uuid])
            .max();
        match last {
            Some(mx) => self.windowed(0, mx),
            None => self.clone(),
        }
    }

    /// The window without its last user or assistant event (query.py `prior`).
    pub fn prior(&self) -> Session<'a> {
        let last = self
            .events_iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Entry::User(_) | Entry::Assistant(_)))
            .map(|(i, _)| i)
            .max();
        match last {
            Some(l) => self.windowed(0, l),
            None => self.windowed(0, 0),
        }
    }

    /// The window's last `n` events (query.py `recent`).
    pub fn recent(&self, n: usize) -> Session<'a> {
        let len = self.len();
        self.windowed(len.saturating_sub(n), len)
    }

    /// The one-turn view of the window's last turn (query.py `current_turn`).
    pub fn current_turn(&self) -> Session<'a> {
        Session {
            turns: self.turns.last().copied().into_iter().collect(),
        }
    }

    /// The prompt that opened the window's last turn (query.py `user_text`).
    pub fn user_text(&self) -> &'a str {
        self.turns.last().map_or("", |wt| wt.prompt())
    }

    /// The first non-empty prompt in the window, or None (query.py `first_prompt`).
    pub fn first_prompt(&self) -> Option<&'a str> {
        self.turns
            .iter()
            .map(|wt| wt.prompt())
            .find(|p| !p.is_empty())
    }

    /// The files targeted by any tool call in the window, one entry per call.
    pub fn files_touched(&self) -> Vec<FileRef> {
        self.tool_calls().files()
    }

    /// The files modified by edit-shaped calls in the window, one entry per call.
    pub fn edited_files(&self) -> Vec<FileRef> {
        self.tool_calls()
            .where_(|use_| !use_.call.hunks().is_empty())
            .files()
    }

    /// Whether any call in the window matches the pipe spec `name` (self-session only).
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_calls().named(name).any()
    }

    /// Whether any edit-shaped call in the window targets a file matching any glob.
    pub fn has_edit_to(&self, globs: &[&str]) -> bool {
        self.edited_files().iter().any(|file| file.matches(globs))
    }

    /// Whether any Read in the window targets a path containing `pattern`.
    pub fn has_read(&self, pattern: &str) -> bool {
        self.tool_calls()
            .named("Read")
            .files()
            .iter()
            .any(|file| file.path.contains(pattern))
    }

    /// Whether any Skill invocation in the window names one of `names`.
    pub fn has_skill(&self, names: &[&str]) -> bool {
        self.tool_calls().named("Skill").items().iter().any(|use_| {
            matches!(&use_.call, ToolCall::Skill(call) if names.contains(&call.skill.as_str()))
        })
    }

    /// Whether `token` appears in the window without a later invalidating call (query.py `has_override`).
    pub fn has_override(&self, token: &str, invalidated_by: &[&str]) -> bool {
        let last = self
            .events_iter()
            .enumerate()
            .filter(|(_, e)| carries_token(e, token))
            .map(|(i, _)| i)
            .max();
        let last = match last {
            Some(l) => l,
            None => return false,
        };
        let positions = self.event_positions();
        let expanded = expand_tool_names(&invalidated_by.join("|"));
        !self.tool_calls().all_items.iter().any(|use_| {
            positions[use_.event_uuid] > last && matches_names(use_.call.name(), &expanded)
        })
    }

    /// The number of calls in the window whose result errored.
    pub fn count_failures(&self) -> usize {
        self.tool_calls().failed().count()
    }

    /// The window's last `n` assistant texts, each capped at `max_per_msg` code points.
    pub fn assistant_text(&self, n: usize, max_per_msg: usize) -> String {
        let texts: Vec<String> = self
            .events_iter()
            .filter_map(|e| match e {
                Entry::Assistant(a) => Some(pystr::strip(&joined_text(&a.blocks)).to_string()),
                _ => None,
            })
            .collect();
        let start = texts.len().saturating_sub(n);
        texts[start..]
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| char_prefix(t, max_per_msg))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    /// Whether any prompt in the window contains any keyword, case-insensitively.
    pub fn user_said(&self, keywords: &[&str]) -> bool {
        self.turns.iter().any(|wt| {
            let prompt = wt.prompt().to_lowercase();
            keywords
                .iter()
                .any(|kw| prompt.contains(&kw.to_lowercase()))
        })
    }

    /// The shell command strings of the window's Bash calls (query.py `commands`).
    pub fn commands(&self) -> Vec<&'a str> {
        self.tool_calls()
            .named("Bash")
            .items()
            .into_iter()
            .filter_map(|use_| match &use_.call {
                ToolCall::Bash(call) => Some(call.command.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The window's Bash commands parsed into `CommandLine` objects (query.py `command_lines`).
    #[cfg(feature = "command")]
    pub fn command_lines(&self) -> Vec<CommandLine> {
        self.commands()
            .iter()
            .map(|c| CommandLine::parse(c))
            .collect()
    }

    /// Whether any Bash command in the window runs `argv` (query.py `has_command`, self-session only).
    #[cfg(feature = "command")]
    pub fn has_command(&self, argv: &[&str]) -> bool {
        self.commands().iter().any(|c| {
            CommandLine::parse(c)
                .commands()
                .iter()
                .any(|cmd| cmd.runs(argv))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::lift_session;
    use crate::parse::parse_bytes;

    fn parse(raw: &str) -> Vec<Entry> {
        parse_bytes(raw.as_bytes(), |_| true).expect("parses")
    }

    #[test]
    fn pure_name_and_suffix_track_pureposixpath() {
        assert_eq!(pure_name("/a/b/c.py"), "c.py");
        assert_eq!(pure_name("/a/b/"), "b");
        assert_eq!(pure_name("/"), "");
        assert_eq!(pure_suffix("c.py"), ".py");
        assert_eq!(pure_suffix("a.tar.gz"), ".gz");
        assert_eq!(pure_suffix(".bashrc"), "");
        assert_eq!(pure_suffix("trailing."), "");
        assert_eq!(pure_suffix("α.β"), ".β");
    }

    #[test]
    fn fnmatch_star_crosses_slashes() {
        assert!(fnmatch("/repo/tests/test_app.py", "**/test_*.py"));
        assert!(fnmatch("test_app.py", "*.py"));
        assert!(!fnmatch("test_app.py", "**/test_*.py"));
        assert!(fnmatch("/x/conftest.py", "**/conftest.py"));
        assert!(!fnmatch("main.rs", "*.py"));
    }

    #[test]
    fn fileref_is_test_and_under() {
        assert!(FileRef::new("/repo/tests/test_app.py").is_test());
        assert!(FileRef::new("/repo/conftest.py").is_test());
        assert!(!FileRef::new("/repo/src/app.py").is_test());
        assert!(FileRef::new("/repo/src/app.py").under(&["src"]));
        assert!(FileRef::new("src/app.py").under(&["src"]));
        assert!(!FileRef::new("/repo/lib/app.py").under(&["src"]));
    }

    #[test]
    fn where_input_matches_regex_equals_and_predicate() {
        let entries = parse(concat!(
            r#"{"type":"user","uuid":"u0","sessionId":"s","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"go"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a0","sessionId":"s","timestamp":"2026-01-01T00:00:01.000Z","message":{"model":"m","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","sessionId":"s","timestamp":"2026-01-01T00:00:02.000Z","message":{"model":"m","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git push"}}]}}"#,
            "\n",
        ));
        let lift = lift_session("s", &entries);
        let session = Session::from_lift(&lift);
        let re = Regex::new("ls").unwrap();
        assert_eq!(
            session
                .tool_calls()
                .where_input(&[("command", InputRule::Regex(&re))])
                .count(),
            1
        );
        assert_eq!(
            session
                .tool_calls()
                .where_input(&[(
                    "command",
                    InputRule::Equals(sonic_rs::from_str("\"git push\"").unwrap())
                )])
                .count(),
            1
        );
        use sonic_rs::JsonValueTrait;
        let pred = |v: &Value| v.as_str().is_some_and(|s| s.contains("push"));
        assert_eq!(
            session
                .tool_calls()
                .where_input(&[("command", InputRule::Predicate(&pred))])
                .count(),
            1
        );
        assert!(session.has_command(&["git", "push"]));
        assert!(!session.has_command(&["git", "commit"]));
    }
}
