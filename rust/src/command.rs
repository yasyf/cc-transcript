use std::cell::RefCell;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{Node, Parser};

// Parity: command.py WRAPPER_COMMANDS
const WRAPPER_COMMANDS: &[&str] = &[
    "sudo", "env", "time", "timeout", "nice", "nohup", "doas", "command", "exec", "xargs",
];

// Parity: command.py MULTI_LEVEL_TOOLS
const MULTI_LEVEL_TOOLS: &[&str] = &[
    "git", "gh", "uv", "uvx", "npx", "docker", "jj", "go", "cargo", "npm", "pnpm", "yarn",
    "kubectl", "pip", "brew", "aws", "gcloud", "terraform",
];

// Parity: command.py COMPOUND_OPS
const COMPOUND_OPS: &[&str] = &["&&", "||", ";", "|", "&"];

// Parity: command.py ASSIGNMENT_RE
static ASSIGNMENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\w+=").expect("assignment regex"));

thread_local! {
    static BASH_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .expect("load bash grammar");
        parser
    });
}

struct RawCommand {
    executable: String,
    args: Vec<String>,
}

// Parity: command.py CommandLine.dequote — strip exactly one layer of matching
// outer quotes; anything else (lone quotes, unmatched quotes) is left untouched.
fn dequote(raw: &str) -> &str {
    match raw.as_bytes() {
        [first @ (b'\'' | b'"'), .., last] if first == last => &raw[1..raw.len() - 1],
        _ => raw,
    }
}

fn word_text(node: Node, src: &[u8]) -> String {
    let raw = node.utf8_text(src).unwrap_or("");
    match node.kind() {
        // Parity: command.py CommandLine.word_text — dequote string/raw_string only.
        "string" | "raw_string" => dequote(raw).to_string(),
        _ => raw.to_string(),
    }
}

// Parity: command.py CommandLine.extract_command — command_name is the executable,
// variable_assignment/file_redirect are skipped, and word-like nodes fill the
// executable (first) then args.
fn extract_command(node: Node, src: &[u8]) -> RawCommand {
    let mut executable = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "command_name" => executable = word_text(child, src),
            "variable_assignment" | "file_redirect" => {}
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
    RawCommand { executable, args }
}

// Parity: command.py CommandLine.collect_parts — operator children are skipped so
// only the surrounding commands remain, in document order.
fn collect_parts(node: Node, src: &[u8], ops: &[&str], out: &mut Vec<RawCommand>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if ops.contains(&child.kind()) || ops.contains(&child.utf8_text(src).unwrap_or("")) {
            continue;
        }
        walk_node(child, src, out);
    }
}

// Parity: command.py CommandLine.walk_redirected — the inner statement is unwrapped
// past its redirects; an empty inner yields a single empty-executable command
// (which contributes no prefix).
fn walk_redirected(node: Node, src: &[u8], out: &mut Vec<RawCommand>) {
    let start = out.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "file_redirect" {
            continue;
        }
        walk_node(child, src, out);
    }
    if out.len() == start {
        out.push(RawCommand {
            executable: String::new(),
            args: Vec::new(),
        });
    }
}

// Parity: command.py CommandLine.walk_node — program/list/pipeline split at their
// operators, command extracts, redirected_statement unwraps, everything else
// recurses (loops, conditionals, subshells) so nested commands surface in order.
fn walk_node(node: Node, src: &[u8], out: &mut Vec<RawCommand>) {
    match node.kind() {
        "program" => collect_parts(node, src, &[";"], out),
        "list" => collect_parts(node, src, COMPOUND_OPS, out),
        "pipeline" => collect_parts(node, src, &["|"], out),
        "command" => out.push(extract_command(node, src)),
        "redirected_statement" => walk_redirected(node, src, out),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_node(child, src, out);
            }
        }
    }
}

// Parity: command.py Command.unwrapped dropwhile predicate — flags, bare
// ASCII-integer arguments (both sides test ASCII digits only), and VAR=val
// assignments trailing a wrapper are shifted past.
fn is_wrapper_skip(arg: &str) -> bool {
    arg.starts_with('-')
        || (!arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()))
        || ASSIGNMENT_RE.is_match(arg)
}

// Parity: command.py Command.unwrapped + Command.prefix. Unwrap leading wrappers,
// then a multi-level tool keeps its first non-flag argument as the subcommand.
// The empty-executable guard tests the unwrapped command, and an empty first
// non-flag pick falls back to the bare tool (walrus truthiness in Command.prefix).
fn command_prefix(cmd: &RawCommand) -> Option<String> {
    if cmd.executable.is_empty() {
        return None;
    }
    let mut argv: Vec<&str> = Vec::with_capacity(cmd.args.len() + 1);
    argv.push(cmd.executable.as_str());
    argv.extend(cmd.args.iter().map(String::as_str));

    while !argv.is_empty() && WRAPPER_COMMANDS.contains(&argv[0]) {
        let skip = argv[1..].iter().take_while(|a| is_wrapper_skip(a)).count();
        argv = argv[1 + skip..].to_vec();
    }

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

fn parse_prefixes(parser: &mut Parser, command: &str) -> Vec<String> {
    let Some(tree) = parser.parse(command, None) else {
        return Vec::new();
    };
    let mut cmds: Vec<RawCommand> = Vec::new();
    walk_node(tree.root_node(), command.as_bytes(), &mut cmds);
    cmds.iter().filter_map(command_prefix).collect()
}

pub fn prefixes(command: &str) -> Vec<String> {
    BASH_PARSER.with(|parser| parse_prefixes(&mut parser.borrow_mut(), command))
}

#[cfg(test)]
mod tests {
    use super::prefixes;

    fn pins() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            // command.py TestCommandPrefixes.test_command_prefixes
            ("VAR=1 sudo docker compose up -d", vec!["docker compose"]),
            (
                "git add . && git commit -m 'x; y'",
                vec!["git add", "git commit"],
            ),
            ("git --version", vec!["git"]),
            ("cat a && grep b", vec!["cat", "grep"]),
            ("ls -la", vec!["ls"]),
            ("echo \"unterminated", vec!["echo"]),
            ("for f in *.py; do python $f; done", vec!["python"]),
            ("while true; do sleep 1; done", vec!["true", "sleep"]),
            (
                "if grep -q x f; then echo y; else echo z; fi",
                vec!["grep", "echo", "echo"],
            ),
            ("", vec![]),
            ("timeout 30 git push", vec!["git push"]),
            // command.py TestPrefix.test_prefix (single command)
            ("git commit -m x", vec!["git commit"]),
            ("docker compose up -d", vec!["docker compose"]),
            ("sudo docker compose up", vec!["docker compose"]),
            ("nice -n 10 cargo build", vec!["cargo build"]),
            ("sudo", vec![]),
            // command.py TestUnwrapped nested wrappers + env skip
            ("env -i FOO=bar make test", vec!["make"]),
            ("sudo env FOO=1 timeout 5 ls -la", vec!["ls"]),
            // command.py TestCommandLine.test_prefixes / drops empty executable
            ("sudo git push -f && echo hi", vec!["git push", "echo"]),
            ("> out.txt", vec![]),
            // command.py TestCommandPrefixes — empty argv tokens
            ("sudo ''", vec![]),
            ("git '' status", vec!["git"]),
            // command.py TestCommandPrefixes — digit skip is ASCII-only both sides
            ("timeout ٣ git push", vec!["٣"]),
            // command.py TestCommandPrefixes — one-layer dequoting keeps inner quotes
            ("git \"'commit'\"", vec!["git 'commit'"]),
            // command.py TestCommandPrefixes — substitutions, functions, multiline
            ("diff <(sort a.txt) <(sort b.txt)", vec!["diff"]),
            ("cat <(git log) | head -3", vec!["cat", "head"]),
            ("echo $((1 + 2))", vec!["echo"]),
            ("x=$((COUNT + 1)) make build", vec!["make"]),
            ("echo `git rev-parse HEAD`", vec!["echo"]),
            ("foo() { git status; }; foo", vec!["git status", "foo"]),
            ("git commit -m \"line1\nline2\"", vec!["git commit"]),
            ("git add .\ngit commit -m x", vec!["git add", "git commit"]),
        ]
    }

    #[test]
    fn prefix_battery_matches_python_pins() {
        for (command, expected) in pins() {
            assert_eq!(
                prefixes(command),
                expected,
                "prefixes mismatch for {command:?}"
            );
        }
    }

    // command.py TestDequote — one layer of matching outer quotes, lone and
    // unmatched quotes untouched.
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
}
