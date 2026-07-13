use std::cell::RefCell;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{Node, Parser};

use cc_transcript_core::generated::command::{
    ASSIGNMENT_PATTERN, COMPOUND_OPS, MULTI_LEVEL_TOOLS, WRAPPER_COMMANDS,
};

static ASSIGNMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(ASSIGNMENT_PATTERN).expect("assignment regex"));

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

    const PIN_DELIM: char = '|';
    const PINS_TSV: &str = include_str!("../data/command_prefix_pins.tsv");

    fn decode_pin(field: &str) -> String {
        let mut out = String::with_capacity(field.len());
        let mut chars = field.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        out
    }

    fn pins() -> Vec<(String, Vec<String>)> {
        PINS_TSV
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let _id = fields.next().unwrap();
                let command = decode_pin(fields.next().unwrap());
                let expected = match fields.next().unwrap() {
                    "" => Vec::new(),
                    field => field.split(PIN_DELIM).map(str::to_string).collect(),
                };
                (command, expected)
            })
            .collect()
    }

    #[test]
    fn prefix_battery_matches_python_pins() {
        for (command, expected) in pins() {
            assert_eq!(
                prefixes(&command),
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
