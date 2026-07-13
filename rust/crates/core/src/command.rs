use std::cell::RefCell;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{Node, Parser};

use crate::generated::command::{
    ASSIGNMENT_PATTERN, COMPOUND_OPS, MULTI_LEVEL_TOOLS, WRAPPER_COMMANDS,
};

static ASSIGNMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(ASSIGNMENT_PATTERN).expect("assignment regex"));

const REDIRECT_OPS: &[&str] = &[">", ">>", "<", "<<", ">&", "<&", ">|"];

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    pub raw: String,
    pub executable: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub redirects: Vec<Redirect>,
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
fn dequote(raw: &str) -> &str {
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
    Command {
        raw: node_text(node, src),
        executable,
        args,
        env,
        redirects,
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
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "file_redirect" {
            redirects.push(extract_redirect(child, src));
        } else {
            inner.extend(walk_node(child, src));
        }
    }
    if inner.is_empty() {
        return vec![(
            Command {
                raw: node_text(node, src),
                redirects,
                ..Command::default()
            },
            None,
        )];
    }
    if !redirects.is_empty() {
        for (cmd, _) in inner.iter_mut() {
            cmd.redirects.extend(redirects.iter().cloned());
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
    use super::{prefixes, Command, CommandLine, Redirect};

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
}
