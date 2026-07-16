use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{Node, Parser};

use crate::literals::command::{
    ASSIGNMENT_PATTERN, COMPOUND_OPS, MULTI_LEVEL_TOOLS, WRAPPER_COMMANDS,
};
use crate::pystr;

static ASSIGNMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(ASSIGNMENT_PATTERN).expect("assignment regex"));

const REDIRECT_OPS: &[&str] = &[">", ">>", "<", "<<", ">&", "<&", ">|"];

// Parity: command.py REDIRECT_OP_TYPES — the file_redirect operator/descriptor node kinds.
const REDIRECT_OP_TYPES: &[&str] = &["file_descriptor", ">", ">>", "<", "<<", ">&", "<&", ">|"];

thread_local! {
    static BASH_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .expect("load bash grammar");
        parser
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub op: String,
    pub target: String,
    pub fd: Option<i64>,
}

#[derive(Clone, Default)]
pub struct Command {
    pub raw: String,
    pub executable: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub redirects: Vec<Redirect>,
    // Parity: command.py Command.span — byte offsets (start, end), None when a redirect absorbs a
    // trailing word; carries compare=False, repr=False, so it is excluded from PartialEq and Debug.
    pub span: Option<(usize, usize)>,
}

impl PartialEq for Command {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
            && self.executable == other.executable
            && self.args == other.args
            && self.env == other.env
            && self.redirects == other.redirects
    }
}

impl Eq for Command {}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("raw", &self.raw)
            .field("executable", &self.executable)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("redirects", &self.redirects)
            .finish()
    }
}

// Parity: command.py CommandLine.splice error legs. NoSpan/Overlap carry the raw (maybe negative)
// key the Python message prints; IndexOutOfRange maps to IndexError at the py boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpliceError {
    NoSpan {
        index: isize,
    },
    Overlap {
        span: (usize, usize),
        index: isize,
        cursor: usize,
    },
    IndexOutOfRange,
}

impl fmt::Display for SpliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpliceError::NoSpan { index } => write!(f, "command at index {index} has no span"),
            SpliceError::Overlap {
                span,
                index,
                cursor,
            } => write!(
                f,
                "span ({}, {}) at index {index} overlaps or precedes cursor {cursor}",
                span.0, span.1
            ),
            SpliceError::IndexOutOfRange => write!(f, "tuple index out of range"),
        }
    }
}

impl Command {
    // Parity: command.py Command.argv — an empty executable collapses the vector to ().
    pub fn argv(&self) -> Vec<&str> {
        if self.executable.is_empty() {
            Vec::new()
        } else {
            std::iter::once(self.executable.as_str())
                .chain(self.args.iter().map(String::as_str))
                .collect()
        }
    }

    // Parity: command.py Command.program — `re.match(r"python3?$")` is exactly python/python3.
    pub fn program(&self) -> &str {
        if self.executable == "uv" && self.args.len() >= 2 && self.args[0] == "run" {
            return &self.args[1];
        }
        if matches!(self.executable.as_str(), "python" | "python3")
            && self.args.len() >= 2
            && self.args[0] == "-m"
        {
            return &self.args[1];
        }
        &self.executable
    }

    // Parity: command.py Command.unwrapped — returns self when nothing is stripped.
    pub fn unwrapped(&self) -> Command {
        let argv = self.argv();
        let stripped = strip_wrappers(&argv);
        if stripped.len() == argv.len() {
            return self.clone();
        }
        Command {
            raw: self.raw.clone(),
            executable: stripped.first().copied().unwrap_or("").to_string(),
            args: stripped.iter().skip(1).map(|s| s.to_string()).collect(),
            env: self.env.clone(),
            redirects: self.redirects.clone(),
            span: self.span,
        }
    }

    // Parity: command.py Command.prefix — over the unwrapped argv; empty pick falls back to the tool.
    pub fn prefix(&self) -> Option<String> {
        let argv = strip_wrappers(&self.argv());
        match argv.first() {
            None | Some(&"") => None,
            Some(&exe) if MULTI_LEVEL_TOOLS.contains(&exe) => Some(
                argv[1..]
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .filter(|sub| !sub.is_empty())
                    .map_or_else(|| exe.to_string(), |sub| format!("{exe} {sub}")),
            ),
            Some(&exe) => Some(exe.to_string()),
        }
    }

    // Parity: command.py Command.runs — argv is a non-empty prefix of the unwrapped argv.
    pub fn runs(&self, argv: &[&str]) -> bool {
        if argv.is_empty() {
            return false;
        }
        let unwrapped = strip_wrappers(&self.argv());
        unwrapped.len() >= argv.len() && unwrapped[..argv.len()] == *argv
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub raw: String,
    pub parts: Vec<(Command, Option<String>)>,
}

impl CommandLine {
    // Parity: command.py CommandLine.parse — blank/comment-only input yields empty parts.
    pub fn parse(raw: &str) -> CommandLine {
        let parts = BASH_PARSER.with(|parser| match parser.borrow_mut().parse(raw, None) {
            Some(tree) => walk_node(tree.root_node(), raw.as_bytes()),
            None => Vec::new(),
        });
        CommandLine {
            raw: raw.to_string(),
            parts,
        }
    }

    // Parity: command.py CommandLine.commands.
    pub fn commands(&self) -> Vec<&Command> {
        self.parts.iter().map(|(cmd, _)| cmd).collect()
    }

    // Parity: command.py CommandLine.primary — the final command, or None.
    pub fn primary(&self) -> Option<&Command> {
        self.parts.last().map(|(cmd, _)| cmd)
    }

    // Parity: command.py CommandLine.head — the first command, or None.
    pub fn head(&self) -> Option<&Command> {
        self.parts.first().map(|(cmd, _)| cmd)
    }

    // Parity: command.py CommandLine.prefixes — each command's prefix, None dropped.
    pub fn prefixes(&self) -> Vec<String> {
        self.parts
            .iter()
            .filter_map(|(cmd, _)| cmd.prefix())
            .collect()
    }

    pub fn q(&self) -> CommandLineQuery<'_> {
        CommandLineQuery { line: self }
    }

    // Parity: command.py Occurrence.prev_op — the operator joining the previous command, None at index 0.
    pub fn prev_op(&self, index: usize) -> Option<&str> {
        if index > 0 {
            self.parts[index - 1].1.as_deref()
        } else {
            None
        }
    }

    // Parity: command.py Occurrence.next_op — the operator joining this command to the next.
    pub fn next_op(&self, index: usize) -> Option<&str> {
        self.parts[index].1.as_deref()
    }

    // Parity: command.py Occurrence.piped — a neighboring `|`, or a raw source gap toward a
    // None-operator neighbor that is exactly one pipe token (PIPE_GAP_RE) surrounded by whitespace.
    pub fn piped(&self, index: usize) -> bool {
        if self.prev_op(index) == Some("|") || self.next_op(index) == Some("|") {
            return true;
        }
        let Some(span) = self.parts[index].0.span else {
            return false;
        };
        let source = self.raw.as_str();
        if self.next_op(index).is_none() && index + 1 < self.parts.len() {
            if let Some(next) = self.parts[index + 1].0.span {
                if span.1 <= next.0 && pipe_gap_full_match(&source[span.1..next.0]) {
                    return true;
                }
            }
        }
        if self.prev_op(index).is_none() && index > 0 {
            if let Some(prev) = self.parts[index - 1].0.span {
                if prev.1 <= span.0 && pipe_gap_full_match(&source[prev.1..span.0]) {
                    return true;
                }
            }
        }
        false
    }

    // Parity: command.py CommandLine.splice — keys apply in ascending order (BTreeMap = sorted());
    // a negative key resolves like tuple indexing, out-of-range raises IndexError.
    pub fn splice(&self, replacements: &BTreeMap<isize, String>) -> Result<String, SpliceError> {
        let source = self.raw.as_bytes();
        let len = self.parts.len() as isize;
        let mut out: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        for (&key, text) in replacements {
            let index = if key < 0 { key + len } else { key };
            if index < 0 || index >= len {
                return Err(SpliceError::IndexOutOfRange);
            }
            let span = self.parts[index as usize]
                .0
                .span
                .ok_or(SpliceError::NoSpan { index: key })?;
            let (start, end) = span;
            if start < cursor {
                return Err(SpliceError::Overlap {
                    span,
                    index: key,
                    cursor,
                });
            }
            out.extend_from_slice(&source[cursor..start]);
            out.extend_from_slice(text.as_bytes());
            cursor = end;
        }
        out.extend_from_slice(&source[cursor..]);
        Ok(String::from_utf8(out).expect("splice produced valid utf-8"))
    }

    // Parity: command.py CommandLine.rewrite_occurrences — splice in each occurrence's non-None
    // mapping, or None when nothing maps.
    pub fn rewrite_occurrences(
        &self,
        mut to: impl FnMut(usize) -> Option<String>,
    ) -> Result<Option<String>, SpliceError> {
        let replacements: BTreeMap<isize, String> = (0..self.parts.len())
            .filter_map(|index| to(index).map(|text| (index as isize, text)))
            .collect();
        if replacements.is_empty() {
            Ok(None)
        } else {
            self.splice(&replacements).map(Some)
        }
    }
}

// Parity: command.py PIPE_GAP_RE — `\s*\|&?\s*` fullmatch, with Python str whitespace via pystr.
fn pipe_gap_full_match(gap: &str) -> bool {
    let mut chars = gap.chars().peekable();
    while chars.peek().is_some_and(|c| pystr::is_space(*c)) {
        chars.next();
    }
    if chars.next() != Some('|') {
        return false;
    }
    if chars.peek() == Some(&'&') {
        chars.next();
    }
    while chars.peek().is_some_and(|c| pystr::is_space(*c)) {
        chars.next();
    }
    chars.next().is_none()
}

// Parity: command.py CommandLine.redirect_absorbed_word — a file_redirect that swallowed a command
// word past its target carries more than one non-operator child.
fn redirect_absorbed_word(node: Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| !REDIRECT_OP_TYPES.contains(&c.kind()))
        .count()
        > 1
}

// Parity: command.py CommandLineQuery — predicate helpers over a parsed line.
pub struct CommandLineQuery<'a> {
    pub line: &'a CommandLine,
}

impl CommandLineQuery<'_> {
    // Parity: command.py CommandLineQuery.runs — the primary command's unwrapped argv.
    pub fn runs(&self, argv: &[&str]) -> bool {
        self.line
            .primary()
            .is_some_and(|primary| primary.runs(argv))
    }

    // Parity: command.py CommandLineQuery.has_subcommand — name appears as an argument.
    pub fn has_subcommand(&self, name: &str) -> bool {
        self.line
            .parts
            .iter()
            .any(|(cmd, _)| cmd.args.iter().any(|a| a == name))
    }

    // Parity: command.py CommandLineQuery.any_command.
    pub fn any_command(&self, pred: impl Fn(&Command) -> bool) -> bool {
        self.line.parts.iter().any(|(cmd, _)| pred(cmd))
    }

    // Parity: command.py CommandLineQuery.uses_redirect — any file redirect, or a pipe op.
    pub fn uses_redirect(&self) -> bool {
        self.line
            .parts
            .iter()
            .any(|(cmd, op)| !cmd.redirects.is_empty() || op.as_deref() == Some("|"))
    }

    // Parity: command.py CommandLineQuery.contains_token — exact argv element match.
    pub fn contains_token(&self, token: &str) -> bool {
        self.line
            .parts
            .iter()
            .any(|(cmd, _)| cmd.argv().iter().any(|a| *a == token))
    }
}

// Parity: command.py CommandLine.dequote — strip exactly one layer of matching outer quotes.
pub fn dequote(raw: &str) -> &str {
    match raw.as_bytes() {
        [first @ (b'\'' | b'"'), .., last] if first == last => &raw[1..raw.len() - 1],
        _ => raw,
    }
}

fn node_text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

// Parity: command.py CommandLine.word_text — dequote string/raw_string only.
fn word_text(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "string" | "raw_string" => dequote(node.utf8_text(src).unwrap_or("")).to_string(),
        _ => node_text(node, src),
    }
}

// Parity: command.py Command.unwrapped dropwhile — flags, bare ASCII-integer args, VAR=val.
fn is_wrapper_skip(arg: &str) -> bool {
    arg.starts_with('-')
        || (!arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()))
        || ASSIGNMENT_RE.is_match(arg)
}

// Parity: command.py Command.unwrapped — drop each leading wrapper plus its skippable args.
fn strip_wrappers<'a>(argv: &[&'a str]) -> Vec<&'a str> {
    let mut argv: Vec<&str> = argv.to_vec();
    while !argv.is_empty() && WRAPPER_COMMANDS.contains(&argv[0]) {
        let skip = argv[1..].iter().take_while(|a| is_wrapper_skip(a)).count();
        argv = argv[1 + skip..].to_vec();
    }
    argv
}

// Parity: command.py CommandLine.extract_redirect — fd, then op (typed or textual), then target.
fn extract_redirect(node: Node, src: &[u8]) -> Redirect {
    let mut op = String::new();
    let mut target = String::new();
    let mut fd: Option<i64> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "file_descriptor" {
            let text = node_text(child, src);
            fd = (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
                .then(|| text.parse().ok())
                .flatten();
        } else if REDIRECT_OPS.contains(&kind) {
            op = kind.to_string();
        } else {
            let text = node_text(child, src);
            if op.is_empty() && REDIRECT_OPS.contains(&text.as_str()) {
                op = text;
            } else {
                target = text;
            }
        }
    }
    Redirect { op, target, fd }
}

// Parity: command.py CommandLine.extract_command — command_name/variable_assignment/
// file_redirect are typed; word-like nodes fill the executable (first) then args.
fn extract_command(node: Node, src: &[u8]) -> Command {
    let mut executable = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "command_name" => executable = word_text(child, src),
            "variable_assignment" => {
                let mut vc = child.walk();
                let children: Vec<Node> = child.children(&mut vc).collect();
                if let Some(name) = children.iter().find(|c| c.kind() == "variable_name") {
                    let value = match (children.len() >= 3).then(|| children[children.len() - 1]) {
                        Some(val) if val.kind() != "=" => word_text(val, src),
                        _ => String::new(),
                    };
                    env.push((node_text(*name, src), value));
                }
            }
            "file_redirect" => redirects.push(extract_redirect(child, src)),
            "word" | "string" | "raw_string" | "number" | "concatenation" | "simple_expansion"
            | "expansion" => {
                if executable.is_empty() {
                    executable = word_text(child, src);
                } else {
                    args.push(word_text(child, src));
                }
            }
            _ => {}
        }
    }
    // Parity: command.py CommandLine.extract_command — the span covers the command's non-redirect
    // children (redirect bytes stay outside), falling back to the whole node when there are none.
    let mut span_cursor = node.walk();
    let content: Vec<Node> = node
        .children(&mut span_cursor)
        .filter(|c| c.kind() != "file_redirect")
        .collect();
    let span = if content.is_empty() {
        (node.start_byte(), node.end_byte())
    } else {
        (
            content[0].start_byte(),
            content[content.len() - 1].end_byte(),
        )
    };
    Command {
        raw: node_text(node, src),
        executable,
        args,
        env,
        redirects,
        span: Some(span),
    }
}

// Parity: command.py CommandLine.collect_parts — an operator child attaches as the last
// part's op; every other child recurses and its parts are appended in order.
fn collect_parts(node: Node, src: &[u8], ops: &[&str]) -> Vec<(Command, Option<String>)> {
    let mut parts: Vec<(Command, Option<String>)> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let text = node_text(child, src);
        if ops.contains(&child.kind()) || ops.contains(&text.as_str()) {
            if let Some(last) = parts.last_mut() {
                last.1 = Some(text);
            }
            continue;
        }
        parts.extend(walk_node(child, src));
    }
    parts
}

// Parity: command.py CommandLine.walk_redirected — statement redirects append to every
// inner command; an empty inner yields one empty-executable command carrying them.
fn walk_redirected(node: Node, src: &[u8]) -> Vec<(Command, Option<String>)> {
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut inner: Vec<(Command, Option<String>)> = Vec::new();
    let mut broken = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "file_redirect" {
            redirects.push(extract_redirect(child, src));
            broken = broken || redirect_absorbed_word(child);
        } else {
            inner.extend(walk_node(child, src));
        }
    }
    if inner.is_empty() {
        return vec![(
            Command {
                raw: node_text(node, src),
                span: Some((node.start_byte(), node.end_byte())),
                redirects,
                ..Command::default()
            },
            None,
        )];
    }
    // Parity: command.py CommandLine.walk_redirected — statement redirects append to every inner
    // command, and an absorbed trailing word (broken) drops that command's contiguous span.
    if !redirects.is_empty() {
        for (cmd, _) in inner.iter_mut() {
            cmd.redirects.extend(redirects.iter().cloned());
            if broken {
                cmd.span = None;
            }
        }
    }
    inner
}

// Parity: command.py CommandLine.walk_node — program/list/pipeline split at their ops,
// command extracts, redirected_statement unwraps, everything else recurses in order.
fn walk_node(node: Node, src: &[u8]) -> Vec<(Command, Option<String>)> {
    match node.kind() {
        "program" => collect_parts(node, src, &[";"]),
        "list" => collect_parts(node, src, COMPOUND_OPS),
        "pipeline" => collect_parts(node, src, &["|"]),
        "command" => vec![(extract_command(node, src), None)],
        "redirected_statement" => walk_redirected(node, src),
        _ => {
            let mut parts = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                parts.extend(walk_node(child, src));
            }
            parts
        }
    }
}

// Parity: command.py command_prefixes — the permission-style prefix of each command.
pub fn prefixes(command: &str) -> Vec<String> {
    CommandLine::parse(command).prefixes()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{prefixes, Command, CommandLine, Redirect, SpliceError};

    // command.py TestDequote — one layer of matching outer quotes, others untouched.
    #[test]
    fn dequote_strips_exactly_one_matching_layer() {
        assert_eq!(super::dequote("\"'hello'\""), "'hello'");
        assert_eq!(super::dequote("'hello'"), "hello");
        assert_eq!(super::dequote("\"hello\""), "hello");
        assert_eq!(super::dequote("'"), "'");
        assert_eq!(super::dequote("\"a"), "\"a");
        assert_eq!(super::dequote("hello"), "hello");
        assert_eq!(super::dequote(""), "");
    }

    #[test]
    fn extracts_executable_args_env_and_redirects() {
        let line = CommandLine::parse("ENV=val uv run pytest > out.txt 2>&1");
        let cmd = line.primary().unwrap();
        assert_eq!(cmd.executable, "uv");
        assert_eq!(cmd.args, ["run", "pytest"]);
        assert_eq!(cmd.env, [("ENV".to_string(), "val".to_string())]);
        assert_eq!(
            cmd.redirects,
            [
                Redirect {
                    op: ">".to_string(),
                    target: "out.txt".to_string(),
                    fd: None
                },
                Redirect {
                    op: ">&".to_string(),
                    target: "1".to_string(),
                    fd: Some(2)
                },
            ]
        );
        assert_eq!(cmd.program(), "pytest");
    }

    #[test]
    fn segments_and_carries_operators() {
        let line = CommandLine::parse("cmd1; cmd2 && cmd3");
        assert_eq!(line.parts.len(), 3);
        assert_eq!(line.parts[0].1.as_deref(), Some(";"));
        assert_eq!(line.parts[1].1.as_deref(), Some("&&"));
        assert_eq!(line.primary().unwrap().executable, "cmd3");
        assert_eq!(line.head().unwrap().executable, "cmd1");
    }

    #[test]
    fn unwrapped_strips_wrappers_and_keeps_env_and_redirects() {
        let cmd = CommandLine::parse("VAR=1 sudo git push > log.txt")
            .primary()
            .unwrap()
            .unwrapped();
        assert_eq!(cmd.argv(), ["git", "push"]);
        assert_eq!(cmd.env, [("VAR".to_string(), "1".to_string())]);
        assert_eq!(
            cmd.redirects,
            [Redirect {
                op: ">".to_string(),
                target: "log.txt".to_string(),
                fd: None
            }]
        );
    }

    #[test]
    fn unwrapped_returns_self_when_no_wrapper() {
        let cmd = Command {
            raw: "ls -la".to_string(),
            executable: "ls".to_string(),
            args: vec!["-la".to_string()],
            ..Command::default()
        };
        assert_eq!(cmd.unwrapped(), cmd);
    }

    #[test]
    fn prefixes_unwrap_and_keep_subcommands() {
        assert_eq!(
            prefixes("sudo git push -f && echo hi"),
            ["git push", "echo"]
        );
        assert_eq!(prefixes("> out.txt"), Vec::<String>::new());
    }

    #[test]
    fn query_surface_answers_over_parts() {
        let line = CommandLine::parse("cd /x && sudo git push origin 2>&1 | head -3");
        let q = line.q();
        assert!(q.runs(&["head"]));
        assert!(q.has_subcommand("origin"));
        assert!(q.uses_redirect());
        assert!(q.contains_token("git"));
        assert!(!q.contains_token("orig"));
        assert!(q.any_command(|cmd| cmd.executable == "head"));
        assert!(!q.any_command(|cmd| cmd.executable == "sed"));
    }

    // command.py TestSplice / TestOccurrences — the v13.2 byte-span layer.
    #[test]
    fn span_excludes_redirect_bytes_and_ignores_span_in_eq() {
        let line = CommandLine::parse(">out echo hi");
        assert_eq!(line.parts[0].0.span, Some((5, 12)));
        // span carries compare=False: same content, different span, still equal.
        let mut other = line.parts[0].0.clone();
        other.span = Some((0, 0));
        assert_eq!(line.parts[0].0, other);
    }

    #[test]
    fn absorbed_trailing_word_has_no_span() {
        let line = CommandLine::parse("echo a >out b");
        assert_eq!(line.parts[0].0.span, None);
    }

    #[test]
    fn piped_covers_operator_and_raw_gap_fallback() {
        assert!(CommandLine::parse("foo | bar").piped(0));
        // `|&` records no operator token; the raw-gap fallback still reads it as piped.
        let amp = CommandLine::parse("a |& b");
        assert!(amp.piped(0) && amp.piped(1));
        // Newlines, `&&`, and a commented pipe are not pipes.
        assert!(!CommandLine::parse("a\nb").piped(0));
        assert!(!CommandLine::parse("a && b").piped(0));
        assert!(!CommandLine::parse("a # x|y\nb").piped(1));
    }

    #[test]
    fn splice_swaps_span_and_preserves_neighbors() {
        let line = CommandLine::parse("a; b; c");
        assert_eq!(
            line.splice(&BTreeMap::from([(1, "XX".to_string())]))
                .unwrap(),
            "a; XX; c"
        );
        // Byte offsets, not char offsets: the multibyte prefix is preserved verbatim.
        let uni = CommandLine::parse("echo café; rm x");
        assert_eq!(
            uni.splice(&BTreeMap::from([(1, "ls".to_string())]))
                .unwrap(),
            "echo café; ls"
        );
    }

    #[test]
    fn splice_rejects_span_less_and_overlapping() {
        let no_span = CommandLine::parse("echo a >out b");
        assert_eq!(
            no_span.splice(&BTreeMap::from([(0, "X".to_string())])),
            Err(SpliceError::NoSpan { index: 0 })
        );
        let overlap = CommandLine {
            raw: "abcdef".to_string(),
            parts: vec![
                (
                    Command {
                        raw: "ab".to_string(),
                        executable: "ab".to_string(),
                        span: Some((0, 4)),
                        ..Command::default()
                    },
                    None,
                ),
                (
                    Command {
                        raw: "cd".to_string(),
                        executable: "cd".to_string(),
                        span: Some((2, 6)),
                        ..Command::default()
                    },
                    None,
                ),
            ],
        };
        assert_eq!(
            overlap
                .splice(&BTreeMap::from([
                    (0, "X".to_string()),
                    (1, "Y".to_string())
                ]))
                .unwrap_err()
                .to_string(),
            "span (2, 6) at index 1 overlaps or precedes cursor 4"
        );
    }

    #[test]
    fn splice_resolves_negative_and_rejects_out_of_range() {
        let line = CommandLine::parse("a; b");
        // A negative key resolves like Python tuple indexing.
        assert_eq!(
            line.splice(&BTreeMap::from([(-1, "X".to_string())]))
                .unwrap(),
            "a; X"
        );
        // Out of range in either direction is IndexOutOfRange (→ IndexError at the boundary).
        assert_eq!(
            line.splice(&BTreeMap::from([(2, "X".to_string())])),
            Err(SpliceError::IndexOutOfRange)
        );
        assert_eq!(
            line.splice(&BTreeMap::from([(-3, "X".to_string())])),
            Err(SpliceError::IndexOutOfRange)
        );
    }

    // debcd87a divergences, pinned (not fixed): all three are unreachable invalid inputs and the
    // Rust behavior is the contract once the Python reference is deleted (see the commit body).
    #[test]
    fn documented_divergences_are_pinned() {
        // U+00B2 (²): the regex crate's \w rejects it, so `²=x` is not a VAR=val skip.
        assert_eq!(prefixes("env ²=x cmd"), ["²=x"]);
        // U+203F (‿): the regex crate's \w matches it, so `‿=x` is skipped.
        assert_eq!(prefixes("env ‿=x cmd"), ["cmd"]);
        // A file descriptor above i64::MAX overflows to None.
        let line = CommandLine::parse("9223372036854775808>out");
        assert_eq!(line.parts[0].0.redirects[0].fd, None);
    }

    #[test]
    fn rewrite_occurrences_maps_or_none() {
        let line = CommandLine::parse("git push; ls; git pull");
        let rewritten = line
            .rewrite_occurrences(|i| {
                (line.parts[i].0.executable == "git").then(|| "BLOCKED".to_string())
            })
            .unwrap();
        assert_eq!(rewritten.as_deref(), Some("BLOCKED; ls; BLOCKED"));
        assert_eq!(
            CommandLine::parse("ls -la").rewrite_occurrences(|_| None),
            Ok(None)
        );
    }
}
