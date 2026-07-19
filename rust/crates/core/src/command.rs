use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use once_cell::sync::Lazy;
use regex::Regex;
use tree_sitter::{Node, Parser};

use crate::literals::command::{
    ASSIGNMENT_PATTERN, COMPOUND_OPS, MULTI_LEVEL_TOOLS, PAYLOAD_DEPTH_LIMIT, POSIX_QUOTING_SHELLS,
    SHELL_COMMANDS, WRAPPER_COMMANDS,
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

// A quote layer enclosing an occurrence's bytes in the outer raw, stacked outermost-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteLayer {
    Bare,
    Single,
    Double,
}

impl QuoteLayer {
    pub fn symbol(self) -> &'static str {
        match self {
            QuoteLayer::Bare => "",
            QuoteLayer::Single => "'",
            QuoteLayer::Double => "\"",
        }
    }
}

#[derive(Clone)]
pub struct Word {
    pub raw: String,
    pub value: Option<String>,
    pub span: Option<(usize, usize)>,
    pub expandable: bool,
    // Native-only: quote shape + the offset where `value` sits verbatim in the owning raw
    // (None when quoting or escapes reshape it); excluded from PartialEq, Debug, and PyO3.
    #[doc(hidden)]
    pub layer: QuoteLayer,
    #[doc(hidden)]
    pub content_offset: Option<usize>,
}

impl PartialEq for Word {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
            && self.value == other.value
            && self.span == other.span
            && self.expandable == other.expandable
    }
}

impl Eq for Word {}

impl fmt::Debug for Word {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Word")
            .field("raw", &self.raw)
            .field("value", &self.value)
            .field("span", &self.span)
            .field("expandable", &self.expandable)
            .finish()
    }
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
    // words[0] is the executable word, words[1..] parallel args; empty for synthetic commands.
    // Excluded from PartialEq and Debug, like span.
    pub words: Vec<Word>,
    // Substitution/payload hops from top level; top-level selectors filter on 0.
    #[doc(hidden)]
    pub nesting: u8,
    // Parts-vec back-offset to the hosting command; survives parts concatenation.
    #[doc(hidden)]
    pub host_delta: Option<usize>,
    // Enclosing payload quote layers, outermost-first; empty at top level.
    #[doc(hidden)]
    pub contexts: Vec<QuoteLayer>,
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
    NotEmbeddable {
        index: isize,
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
            SpliceError::NotEmbeddable { index } => write!(
                f,
                "replacement at index {index} does not survive its enclosing quote layers"
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
            words: if self.words.is_empty() {
                Vec::new()
            } else {
                self.words[argv.len() - stripped.len()..].to_vec()
            },
            nesting: self.nesting,
            host_delta: self.host_delta,
            contexts: self.contexts.clone(),
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
        CommandLine::parse_at_depth(raw, 0)
    }

    fn parse_at_depth(raw: &str, depth: u8) -> CommandLine {
        // The borrow must drop before walking: payload enumeration re-enters parse_at_depth.
        let tree = BASH_PARSER.with(|parser| parser.borrow_mut().parse(raw, None));
        let mut parts = match tree {
            Some(tree) => walk_node(tree.root_node(), raw.as_bytes(), depth),
            None => Vec::new(),
        };
        // Nested inside an enumerated host's span: visible but span-less, like an absorbed word.
        // Payload parts (non-empty contexts) keep mapped spans; splice guards them by embeddability.
        let spans: Vec<Option<(usize, usize)>> = parts.iter().map(|(cmd, _)| cmd.span).collect();
        for (i, (cmd, _)) in parts.iter_mut().enumerate() {
            if !cmd.contexts.is_empty() {
                continue;
            }
            let Some((start, end)) = cmd.span else {
                continue;
            };
            let enclosed = spans.iter().enumerate().any(|(j, other)| {
                j != i && matches!(other, Some((os, oe)) if *os <= start && end <= *oe && (*os, *oe) != (start, end))
            });
            if enclosed {
                cmd.span = None;
            }
        }
        CommandLine {
            raw: raw.to_string(),
            parts,
        }
    }

    // Parity: command.py CommandLine.commands.
    pub fn commands(&self) -> Vec<&Command> {
        self.parts.iter().map(|(cmd, _)| cmd).collect()
    }

    // Parity: command.py CommandLine.primary — the final top-level command, or None.
    pub fn primary(&self) -> Option<&Command> {
        self.parts
            .iter()
            .rev()
            .find_map(|(cmd, _)| (cmd.nesting == 0).then_some(cmd))
    }

    // Parity: command.py CommandLine.head — the first top-level command, or None.
    pub fn head(&self) -> Option<&Command> {
        self.parts
            .iter()
            .find_map(|(cmd, _)| (cmd.nesting == 0).then_some(cmd))
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
            let part = &self.parts[index as usize].0;
            let span = part.span.ok_or(SpliceError::NoSpan { index: key })?;
            if !part.contexts.is_empty() && !embeddable(&part.contexts, text) {
                return Err(SpliceError::NotEmbeddable { index: key });
            }
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

// Python shlex.quote's safe charset: ASCII alphanumerics plus `_@%+=:,./-`.
fn shlex_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte)
}

// Python shlex.quote semantics: safe-charset passthrough, else `'...'` with `'` -> `'\''`.
pub fn shell_quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(shlex_safe) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn survives(layer: QuoteLayer, text: &str) -> bool {
    match layer {
        QuoteLayer::Single => !text.contains('\''),
        QuoteLayer::Bare => !text.is_empty() && text.bytes().all(shlex_safe),
        QuoteLayer::Double => {
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => match bytes.get(i + 1) {
                        Some(b'\\' | b'"' | b'$' | b'`') => i += 2,
                        _ => return false,
                    },
                    b'"' | b'$' | b'`' => return false,
                    _ => i += 1,
                }
            }
            true
        }
    }
}

pub fn embeddable(contexts: &[QuoteLayer], text: &str) -> bool {
    contexts.iter().all(|layer| survives(*layer, text))
}

// One shell word spelling `text` that survives every enclosing layer: bare, then single-quoted,
// then double-quoted with `\ $ ` "` escaped; None when no spelling embeds cleanly.
pub fn quote_for(contexts: &[QuoteLayer], text: &str) -> Option<String> {
    let bare = (!text.is_empty() && text.bytes().all(shlex_safe)).then(|| text.to_string());
    let single = (!text.contains('\'')).then(|| format!("'{text}'"));
    let double = format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`")
    );
    [bare, single, Some(double)]
        .into_iter()
        .flatten()
        .find(|candidate| embeddable(contexts, candidate))
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

// An unquoted glob (`* ? [`), a `{a,b}`/`{a..b}` brace expansion, or a leading tilde.
fn word_expandable(raw: &str) -> bool {
    raw.starts_with('~') || glob_or_brace(raw)
}

fn glob_or_brace(raw: &str) -> bool {
    let mut chars = raw.chars();
    let mut in_brace = false;
    let mut brace_sep = false;
    let mut prev_dot = false;
    while let Some(c) = chars.next() {
        let dotted = prev_dot;
        prev_dot = false;
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            '{' => {
                in_brace = true;
                brace_sep = false;
            }
            ',' if in_brace => brace_sep = true,
            '.' if in_brace => {
                brace_sep = brace_sep || dotted;
                prev_dot = true;
            }
            '}' if in_brace && brace_sep => return true,
            _ => {}
        }
    }
    false
}

fn unescaped(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().unwrap_or('\\')),
            c => out.push(c),
        }
    }
    out
}

// Double-quote escape resolution; returns whether any escape reshaped the bytes. Tree-sitter
// keeps `\"`-style escapes inside string_content, so this runs over content text too.
fn resolve_double_quoted(text: &str, out: &mut String) -> bool {
    let mut escaped = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some(&next @ ('$' | '`' | '"' | '\\')) => {
                escaped = true;
                chars.next();
                out.push(next);
            }
            Some('\n') => {
                escaped = true;
                chars.next();
            }
            _ => out.push('\\'),
        }
    }
    escaped
}

// Per-kind structural word resolution: value strips every quote layer and escape (None when an
// unresolved expansion taints it), content_offset marks a verbatim contiguous source slice.
fn analyze_word(node: Node, src: &[u8]) -> Word {
    let node = match node.kind() {
        "command_name" => node.child(0).unwrap_or(node),
        _ => node,
    };
    let raw = node_text(node, src);
    let span = (node.start_byte(), node.end_byte());
    let word = |value: Option<String>, expandable, layer, content_offset| Word {
        raw: raw.clone(),
        value,
        span: Some(span),
        expandable,
        layer,
        content_offset,
    };
    match node.kind() {
        "raw_string" => match raw.as_bytes() {
            [b'\'', .., b'\''] => word(
                Some(raw[1..raw.len() - 1].to_string()),
                false,
                QuoteLayer::Single,
                Some(span.0 + 1),
            ),
            _ => word(None, false, QuoteLayer::Single, None),
        },
        "string" => {
            let mut value = Some(String::new());
            let mut content: Vec<Node> = Vec::new();
            let mut escaped = false;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match (child.kind(), value.as_mut()) {
                    ("\"", _) | (_, None) => {}
                    ("string_content" | "escape_sequence", Some(v)) => {
                        escaped |= resolve_double_quoted(&node_text(child, src), v);
                        content.push(child);
                    }
                    _ => value = None,
                }
            }
            let offset = (value.is_some() && !escaped && content.len() == 1)
                .then(|| content[0].start_byte());
            word(value, false, QuoteLayer::Double, offset)
        }
        "word" => {
            let value = unescaped(&raw);
            let offset = (value == raw).then_some(span.0);
            word(Some(value), word_expandable(&raw), QuoteLayer::Bare, offset)
        }
        "number" => word(Some(raw.clone()), false, QuoteLayer::Bare, Some(span.0)),
        "concatenation" => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            let value = children
                .iter()
                .map(|child| analyze_word(*child, src).value)
                .collect::<Option<Vec<String>>>()
                .map(|values| values.concat());
            // Glob and brace syntax splits across the unquoted pieces (`{a,b}` parses as three
            // words), so expandability reads over their joined text; a tilde only counts when
            // it leads the whole word.
            let bare: String = children
                .iter()
                .filter(|child| child.kind() == "word")
                .map(|child| node_text(*child, src))
                .collect();
            let expandable = raw.starts_with('~') || glob_or_brace(&bare);
            word(value, expandable, QuoteLayer::Bare, None)
        }
        _ => word(None, false, QuoteLayer::Bare, None),
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
    let mut words: Vec<Word> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                executable = word_text(child, src);
                words.push(analyze_word(child, src));
            }
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
                words.push(analyze_word(child, src));
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
        words,
        ..Command::default()
    }
}

// Parity: command.py CommandLine.collect_parts — an operator child attaches as the last
// part's op; every other child recurses and its parts are appended in order.
fn collect_parts(
    node: Node,
    src: &[u8],
    ops: &[&str],
    depth: u8,
) -> Vec<(Command, Option<String>)> {
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
        parts.extend(walk_node(child, src, depth));
    }
    parts
}

// Parity: command.py CommandLine.walk_redirected — statement redirects append to every
// inner command; an empty inner yields one empty-executable command carrying them.
fn walk_redirected(node: Node, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut inner: Vec<(Command, Option<String>)> = Vec::new();
    let mut broken = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "file_redirect" {
            redirects.push(extract_redirect(child, src));
            broken = broken || redirect_absorbed_word(child);
        } else {
            inner.extend(walk_node(child, src, depth));
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
// command extracts plus its substitution and payload parts, redirected_statement unwraps,
// and everything else recurses in order.
fn walk_node(node: Node, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    match node.kind() {
        "program" => walk_program(node, src, depth),
        "list" => collect_parts(node, src, COMPOUND_OPS, depth),
        "pipeline" => collect_parts(node, src, &["|"], depth),
        "command" => {
            let mut parts = vec![(extract_command(node, src), None)];
            parts.extend(substitution_parts(node, src, depth));
            let payload = payload_parts(&parts[0].0, src, depth);
            parts.extend(payload);
            for index in 1..parts.len() {
                if parts[index].0.nesting == 1 && parts[index].0.host_delta.is_none() {
                    parts[index].0.host_delta = Some(index);
                }
            }
            parts
        }
        "command_substitution" => walk_substitution(node, src, depth),
        "redirected_statement" => walk_redirected(node, src, depth),
        _ => {
            let mut parts = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                parts.extend(walk_node(child, src, depth));
            }
            parts
        }
    }
}

fn walk_substitution(node: Node, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        parts.extend(walk_node(child, src, depth));
    }
    for (cmd, _) in &mut parts {
        cmd.nesting = cmd.nesting.saturating_add(1);
    }
    parts
}

// Word/argument-position `$(…)`/backtick substitutions under a command node, enumerated as parts
// mirroring assignment position: document order (host first), redirect targets excluded.
fn substitution_parts(node: Node, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "command_substitution" => parts.extend(walk_node(child, src, depth)),
            "file_redirect" => {}
            _ => parts.extend(substitution_parts(child, src, depth)),
        }
    }
    parts
}

fn payload_flag(arg: &str) -> bool {
    arg.strip_prefix('-').is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase()) && rest.ends_with('c')
    })
}

// The `-c` cluster must sit in the leading option run: options end at the first operand
// (the script file) or `--`, and a `-o`/`-O` cluster consumes the next word as its argument.
fn shell_payload(words: &[Word]) -> Option<(String, QuoteLayer, Option<usize>)> {
    let mut index = 1;
    while let Some(value) = words.get(index).and_then(|word| word.value.as_deref()) {
        match value {
            "--" => return None,
            _ if !value.starts_with('-') => return None,
            _ if payload_flag(value) => {
                let word = words.get(index + 1)?;
                return word
                    .value
                    .clone()
                    .map(|value| (value, word.layer, word.content_offset));
            }
            _ if value.ends_with('o') || value.ends_with('O') => index += 2,
            _ => index += 1,
        }
    }
    None
}

fn eval_payload(words: &[Word], src: &[u8]) -> Option<(String, QuoteLayer, Option<usize>)> {
    match words {
        [] => None,
        [word] => word
            .value
            .clone()
            .map(|value| (value, word.layer, word.content_offset)),
        parts => {
            let joined = parts
                .iter()
                .map(|word| word.value.as_deref())
                .collect::<Option<Vec<&str>>>()?
                .join(" ");
            let offset = match (parts[0].span, parts[parts.len() - 1].span) {
                (Some((start, _)), Some((_, end)))
                    if src.get(start..end) == Some(joined.as_bytes()) =>
                {
                    Some(start)
                }
                _ => None,
            };
            Some((joined, QuoteLayer::Bare, offset))
        }
    }
}

// Shell `-c` / `eval` payloads enumerated as first-class nested parts, mirroring
// substitution_parts: the payload re-parses depth-capped and hoists behind the host. Span
// mapping is structural — a verbatim contiguous payload keeps outer-raw spans; escapes,
// taint, and non-POSIX quoting go span-less. A tainted payload emits nothing.
fn payload_parts(host: &Command, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    if depth >= PAYLOAD_DEPTH_LIMIT {
        return Vec::new();
    }
    let unwrapped = host.unwrapped();
    let words = &unwrapped.words;
    let Some(exe) = words.first().and_then(|word| word.value.as_deref()) else {
        return Vec::new();
    };
    let exe = exe.rsplit('/').next().unwrap_or(exe);
    let payload = match exe {
        "eval" => eval_payload(&words[1..], src),
        _ if SHELL_COMMANDS.contains(&exe) => shell_payload(words),
        _ => return Vec::new(),
    };
    let Some((value, layer, offset)) = payload else {
        return Vec::new();
    };
    let offset = match exe == "eval" || POSIX_QUOTING_SHELLS.contains(&exe) {
        true => offset,
        false => None,
    };
    let mut parts = CommandLine::parse_at_depth(&value, depth + 1).parts;
    for (cmd, _) in &mut parts {
        cmd.nesting = cmd.nesting.saturating_add(1);
        cmd.contexts.insert(0, layer);
        cmd.span = offset.and_then(|offset| cmd.span.map(|(s, e)| (s + offset, e + offset)));
        for word in &mut cmd.words {
            word.span = offset.and_then(|offset| word.span.map(|(s, e)| (s + offset, e + offset)));
            word.content_offset = offset.and_then(|offset| word.content_offset.map(|c| c + offset));
        }
    }
    parts
}

// Parity: command.py CommandLine.walk_program — collect_parts over `;`, dropping the heredoc body
// and delimiter lines that tree-sitter's multi-heredoc ERROR recovery re-parses as sibling commands.
fn walk_program(node: Node, src: &[u8], depth: u8) -> Vec<(Command, Option<String>)> {
    let mut parts: Vec<(Command, Option<String>)> = Vec::new();
    let mut suppress_until = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() < suppress_until {
            continue;
        }
        let text = node_text(child, src);
        if child.kind() == ";" || text == ";" {
            if let Some(last) = parts.last_mut() {
                last.1 = Some(text);
            }
            continue;
        }
        // A degraded multi-heredoc: emit its command(s) span-less (splice must never rewrite heredoc
        // bytes), then suppress the sibling nodes re-parsed from its heredoc text.
        if let Some(range_end) = heredoc_suppression(child, src) {
            let mut inner = walk_node(child, src, depth);
            for (cmd, _) in inner.iter_mut() {
                cmd.span = None;
            }
            parts.extend(inner);
            suppress_until = range_end;
            continue;
        }
        parts.extend(walk_node(child, src, depth));
    }
    parts
}

// Parity: command.py CommandLine.heredoc_suppression — suppressed byte range of a
// redirected_statement whose heredoc degraded (a heredoc_redirect carrying an ERROR), else None.
fn heredoc_suppression(node: Node, src: &[u8]) -> Option<usize> {
    if node.kind() != "redirected_statement" {
        return None;
    }
    let degraded = find_degraded_heredoc(node)?;
    let delimiters = fabricated_delimiters(degraded, src);
    if delimiters.is_empty() {
        return None;
    }
    // Each unconsumed delimiter extends the range through its matching line, or to EOF when never
    // matched (bash reads an unmatched heredoc to EOF too).
    let raw = std::str::from_utf8(src).expect("valid utf-8 source");
    let mut cursor = node.end_byte();
    for (delimiter, dash) in &delimiters {
        cursor = scan_delimiter_line(raw, cursor, delimiter, *dash);
    }
    Some(cursor)
}

// Parity: command.py CommandLine.find_degraded_heredoc — first heredoc_redirect under `node` whose
// subtree carries an ERROR node.
fn find_degraded_heredoc(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "heredoc_redirect" && subtree_has_error(child) {
            return Some(child);
        }
        if let Some(found) = find_degraded_heredoc(child) {
            return Some(found);
        }
    }
    None
}

fn subtree_has_error(node: Node) -> bool {
    node.is_error() || {
        let mut cursor = node.walk();
        let has_error = node
            .children(&mut cursor)
            .any(|child| subtree_has_error(child));
        has_error
    }
}

// Parity: command.py CommandLine.fabricated_delimiters — the degraded heredoc's unconsumed
// delimiters in byte order; an unquoted leading `-` is the `<<-` tab-strip marker, split off.
fn fabricated_delimiters(hr: Node, src: &[u8]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    collect_fabricated(hr, src, &mut out);
    out
}

fn collect_fabricated(node: Node, src: &[u8], out: &mut Vec<(String, bool)>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "file_redirect" && redirect_is_input(child) {
            if let Some(delimiter) = redirect_delimiter(child, src) {
                out.push(delimiter);
            }
        } else {
            collect_fabricated(child, src, out);
        }
    }
}

fn redirect_is_input(node: Node) -> bool {
    let mut cursor = node.walk();
    let is_input = node.children(&mut cursor).any(|child| child.kind() == "<");
    is_input
}

fn redirect_delimiter(node: Node, src: &[u8]) -> Option<(String, bool)> {
    let mut cursor = node.walk();
    let target = node
        .children(&mut cursor)
        .find(|child| !REDIRECT_OP_TYPES.contains(&child.kind()))?;
    let text = word_text(target, src);
    if !matches!(target.kind(), "string" | "raw_string") {
        if let Some(rest) = text.strip_prefix('-') {
            return Some((rest.to_string(), true));
        }
    }
    Some((text, false))
}

// Parity: command.py CommandLine.scan_delimiter_line — byte offset just past the next line at or
// after `from` equal to `delimiter` (leading tabs stripped when `dash`), or the end of `raw`.
fn scan_delimiter_line(raw: &str, from: usize, delimiter: &str, dash: bool) -> usize {
    let end = raw.len();
    let mut pos = from;
    while pos < end {
        let line_end = raw[pos..].find('\n').map_or(end, |i| pos + i);
        let line = &raw[pos..line_end];
        let candidate = if dash {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if candidate == delimiter {
            return if line_end < end { line_end + 1 } else { end };
        }
        if line_end >= end {
            break;
        }
        pos = line_end + 1;
    }
    end
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

    // A degraded multi-heredoc drops its fabricated sibling parts, keeps the real trailing command,
    // and goes span-less so splice can't rewrite heredoc bytes.
    #[test]
    fn multi_heredoc_drops_fabricated_parts_and_keeps_trailing_command() {
        let line = CommandLine::parse("cat <<A <<B\none\nA\ntwo\nB\necho done");
        assert_eq!(
            line.commands()
                .iter()
                .map(|c| c.executable.as_str())
                .collect::<Vec<_>>(),
            ["cat", "echo"]
        );
        assert_eq!(line.parts[1].0.args, ["done"]);
        // The degraded heredoc command is unspliceable; the real trailing command keeps its span.
        assert_eq!(line.parts[0].0.span, None);
        assert!(line.parts[1].0.span.is_some());
        assert_eq!(
            line.splice(&BTreeMap::from([(0, "X".to_string())])),
            Err(SpliceError::NoSpan { index: 0 })
        );
        // Splicing the real trailing command leaves every heredoc byte verbatim.
        assert_eq!(
            line.splice(&BTreeMap::from([(1, "echo DONE".to_string())]))
                .unwrap(),
            "cat <<A <<B\none\nA\ntwo\nB\necho DONE"
        );
    }

    #[test]
    fn multi_heredoc_dash_strips_tabs_and_unmatched_reads_to_eof() {
        // `<<-` matches its delimiter ignoring leading tabs; the trailing command still survives.
        let dash = CommandLine::parse("cat <<-A <<-B\n\tone\n\tA\n\ttwo\n\tB\necho done");
        assert_eq!(
            dash.commands()
                .iter()
                .map(|c| c.executable.as_str())
                .collect::<Vec<_>>(),
            ["cat", "echo"]
        );
        assert_eq!(dash.parts[0].0.span, None);
        // An unmatched second delimiter reads to EOF (bash semantics), so no fabricated trailing part.
        let eof = CommandLine::parse("cat <<A <<B\none\nA\ntwo\necho done");
        assert_eq!(
            eof.commands()
                .iter()
                .map(|c| c.executable.as_str())
                .collect::<Vec<_>>(),
            ["cat"]
        );
        // A single unterminated heredoc is not degraded: its body is absorbed, no fabricated parts.
        let single = CommandLine::parse("cat <<EOF\nline one\nline two");
        assert_eq!(
            single
                .commands()
                .iter()
                .map(|c| c.executable.as_str())
                .collect::<Vec<_>>(),
            ["cat"]
        );
        assert!(single.parts[0].0.span.is_some());
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

    fn execs(line: &CommandLine) -> Vec<&str> {
        line.parts
            .iter()
            .map(|(cmd, _)| cmd.executable.as_str())
            .collect()
    }

    // Both substitution positions get one treatment: the nested command is a first-class part.
    #[test]
    fn substitutions_enumerate_in_word_position_like_assignment_position() {
        assert_eq!(
            execs(&CommandLine::parse("x=$(ccx repo overview)")),
            ["ccx"]
        );
        let word = CommandLine::parse("echo $(ccx repo overview)");
        assert_eq!(execs(&word), ["echo", "ccx"]);
        assert_eq!(word.parts[1].0.args, ["repo", "overview"]);
        assert_eq!(
            execs(&CommandLine::parse("echo `ccx repo overview`")),
            ["echo", "ccx"]
        );
        // command_name, quoted, concatenated, and env-prefix-value positions all enumerate.
        assert_eq!(
            execs(&CommandLine::parse("$(which python) --version")),
            ["$(which python)", "which"]
        );
        assert_eq!(execs(&CommandLine::parse("echo \"$(a)\"")), ["echo", "a"]);
        assert_eq!(
            execs(&CommandLine::parse("tag=v$(git rev-parse HEAD) make")),
            ["make", "git"]
        );
        // Redirect targets stay out, matching statement-level redirects.
        assert_eq!(execs(&CommandLine::parse("echo hi > $(target)")), ["echo"]);
    }

    #[test]
    fn primary_skips_argument_position_substitution() {
        let line = CommandLine::parse(r#"gh pr list --json number --search "$(cat q.txt)""#);
        assert_eq!(execs(&line), ["gh", "cat"]);
        assert_eq!(line.primary().unwrap().executable, "gh");
    }

    #[test]
    fn primary_keeps_word_position_host_command() {
        let line = CommandLine::parse(r#""$(get-cmd)" --flag"#);
        assert_eq!(execs(&line), ["\"$(get-cmd)\"", "get-cmd"]);
        assert_eq!(line.primary().unwrap().executable, "\"$(get-cmd)\"");
    }

    #[test]
    fn head_skips_leading_assignment_position_substitution() {
        let line = CommandLine::parse("x=$(get-cmd); host --flag");
        assert_eq!(execs(&line), ["get-cmd", "host"]);
        assert_eq!(line.head().unwrap().executable, "host");
    }

    // Document order: the host command first, then each outermost substitution left to right,
    // recursing so nested substitutions follow their host.
    #[test]
    fn substitutions_enumerate_nested_in_document_order() {
        assert_eq!(
            execs(&CommandLine::parse("diff $(sort a) $(b $(c))")),
            ["diff", "sort", "b", "c"]
        );
        assert_eq!(
            execs(&CommandLine::parse("echo $(a | b; c)")),
            ["echo", "a", "b", "c"]
        );
    }

    // Operator attachment mirrors assignment position: the op hangs off the last enumerated part.
    #[test]
    fn substitution_parts_carry_operators_and_spans_like_assignment_position() {
        let assign = CommandLine::parse("x=$(a) && foo");
        assert_eq!(execs(&assign), ["a", "foo"]);
        assert_eq!(assign.parts[0].1.as_deref(), Some("&&"));
        let word = CommandLine::parse("echo $(a) && foo");
        assert_eq!(execs(&word), ["echo", "a", "foo"]);
        assert_eq!(word.parts[0].1, None);
        assert_eq!(word.parts[1].1.as_deref(), Some("&&"));
        assert_eq!(word.next_op(1), Some("&&"));
        assert_eq!(word.prev_op(2), Some("&&"));
        assert!(!word.piped(0) && !word.piped(1));
        // Nested inside the host's span: visible but span-less (splice's non-overlap invariant);
        // assignment position has no host, so its span survives and splices in place.
        let line = CommandLine::parse("echo $(ccx repo overview)");
        assert_eq!(line.parts[0].0.span, Some((0, 25)));
        assert_eq!(line.parts[1].0.span, None);
        assert!(matches!(
            line.splice(&BTreeMap::from([(1, "ls".to_string())])),
            Err(SpliceError::NoSpan { index: 1 })
        ));
        let assign_only = CommandLine::parse("x=$(ccx repo overview)");
        assert_eq!(assign_only.parts[0].0.span, Some((4, 21)));
        assert_eq!(
            assign_only
                .splice(&BTreeMap::from([(0, "ls".to_string())]))
                .unwrap(),
            "x=$(ls)"
        );
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

    // The v14.3 word layer: per-argument structural resolution parallel to args.
    #[test]
    fn words_resolve_per_quote_shape() {
        let cmd =
            CommandLine::parse("echo plain 'sq x' \"dq y\" a\\ b pre'mid'post $V ~/f *.rs {a,b}")
                .head()
                .cloned()
                .unwrap();
        assert_eq!(cmd.words.len(), cmd.args.len() + 1);
        let values: Vec<Option<&str>> = cmd.words.iter().map(|w| w.value.as_deref()).collect();
        assert_eq!(
            values,
            [
                Some("echo"),
                Some("plain"),
                Some("sq x"),
                Some("dq y"),
                Some("a b"),
                Some("premidpost"),
                None,
                Some("~/f"),
                Some("*.rs"),
                Some("{a,b}"),
            ]
        );
        let expandable: Vec<bool> = cmd.words.iter().map(|w| w.expandable).collect();
        assert_eq!(
            expandable,
            [false, false, false, false, false, false, false, true, true, true]
        );
    }

    #[test]
    fn word_double_quote_escapes_resolve_and_expansions_taint() {
        let word = |raw: &str| {
            CommandLine::parse(&format!("echo {raw}"))
                .head()
                .cloned()
                .unwrap()
                .words[1]
                .clone()
        };
        assert_eq!(word("\"a\\\"b\"").value.as_deref(), Some("a\"b"));
        assert_eq!(word("\"\\$HOME\"").value.as_deref(), Some("$HOME"));
        assert_eq!(word("\"a$(b)c\"").value, None);
        assert_eq!(word("\"a${V}b\"").value, None);
        assert_eq!(word("\"*\"").expandable, false);
        // Escapes reshape the content, so the resolved value has no verbatim source slice.
        let line = CommandLine::parse("echo \"a\\\"b\"");
        let escaped = &line.head().unwrap().words[1];
        assert_eq!(escaped.span, Some((5, 11)));
        // A clean double-quoted word keeps a span over its raw, quotes included.
        let clean = CommandLine::parse("echo \"ab\"");
        assert_eq!(clean.head().unwrap().words[1].span, Some((5, 9)));
        assert_eq!(&clean.raw[5..9], "\"ab\"");
    }

    #[test]
    fn words_slice_through_unwrapped() {
        let cmd = CommandLine::parse("sudo env X=1 git push")
            .primary()
            .cloned()
            .unwrap();
        assert_eq!(cmd.words[0].value.as_deref(), Some("sudo"));
        let unwrapped = cmd.unwrapped();
        assert_eq!(unwrapped.argv(), ["git", "push"]);
        assert_eq!(
            unwrapped
                .words
                .iter()
                .map(|w| w.value.as_deref())
                .collect::<Vec<_>>(),
            [Some("git"), Some("push")]
        );
        // Synthetic commands carry no words.
        assert!(CommandLine::parse("> out").parts[0].0.words.is_empty());
    }

    // Shell -c payloads enumerate as first-class nested parts, mirroring substitutions.
    #[test]
    fn payload_parts_enumerate_shell_dash_c() {
        let line = CommandLine::parse("bash -c 'rm -rf /tmp/x'");
        assert_eq!(execs(&line), ["bash", "rm"]);
        let payload = &line.parts[1].0;
        assert_eq!(payload.args, ["-rf", "/tmp/x"]);
        assert_eq!(payload.nesting, 1);
        assert_eq!(payload.host_delta, Some(1));
        assert_eq!(payload.contexts, [super::QuoteLayer::Single]);
        let (start, end) = payload.span.unwrap();
        assert_eq!(&line.raw[start..end], "rm -rf /tmp/x");
        let (ws, we) = payload.words[0].span.unwrap();
        assert_eq!(&line.raw[ws..we], "rm");
        // Top-level selectors skip nested parts.
        assert_eq!(line.primary().unwrap().executable, "bash");
        assert_eq!(line.head().unwrap().executable, "bash");
        // Double-quoted and bare payloads map spans too; flag clusters count.
        let double = CommandLine::parse("sh -c \"rm x\"");
        assert_eq!(execs(&double), ["sh", "rm"]);
        assert_eq!(double.parts[1].0.contexts, [super::QuoteLayer::Double]);
        let (start, end) = double.parts[1].0.span.unwrap();
        assert_eq!(&double.raw[start..end], "rm x");
        let bare = CommandLine::parse("bash -c ls");
        assert_eq!(execs(&bare), ["bash", "ls"]);
        assert_eq!(bare.parts[1].0.contexts, [super::QuoteLayer::Bare]);
        assert_eq!(bare.parts[1].0.span, Some((8, 10)));
        assert_eq!(execs(&CommandLine::parse("bash -lc 'ls'")), ["bash", "ls"]);
        assert_eq!(execs(&CommandLine::parse("zsh -xc 'ls'")), ["zsh", "ls"]);
        // Wrappers unwrap first; absolute shell paths resolve by basename.
        assert_eq!(
            execs(&CommandLine::parse("sudo bash -c 'ls'")),
            ["sudo", "ls"]
        );
        assert_eq!(
            execs(&CommandLine::parse("env X=1 /bin/sh -c 'ls'")),
            ["env", "ls"]
        );
    }

    #[test]
    fn payload_eval_joins_words() {
        let single = CommandLine::parse("eval 'rm x'");
        assert_eq!(execs(&single), ["eval", "rm"]);
        let (start, end) = single.parts[1].0.span.unwrap();
        assert_eq!(&single.raw[start..end], "rm x");
        // Bare multi-word eval maps only when the join is the verbatim source slice.
        let bare = CommandLine::parse("eval rm x");
        assert_eq!(execs(&bare), ["eval", "rm"]);
        assert_eq!(bare.parts[1].0.span, Some((5, 9)));
        let quoted = CommandLine::parse("eval \"rm\" \"x\"");
        assert_eq!(execs(&quoted), ["eval", "rm"]);
        assert_eq!(quoted.parts[1].0.span, None);
    }

    #[test]
    fn payload_tainted_or_missing_emits_no_parts() {
        assert_eq!(execs(&CommandLine::parse("bash -c \"rm $X\"")), ["bash"]);
        assert_eq!(execs(&CommandLine::parse("bash -c \"$CMD\"")), ["bash"]);
        assert_eq!(execs(&CommandLine::parse("bash -c")), ["bash"]);
        assert_eq!(execs(&CommandLine::parse("bash script.sh")), ["bash"]);
        assert_eq!(execs(&CommandLine::parse("eval \"$CMD\"")), ["eval"]);
    }

    // Operand-terminates-options: a `-c` after the script operand or `--` is a positional
    // argument the shell passes through, never the command flag.
    #[test]
    fn payload_options_end_at_first_operand() {
        assert_eq!(
            execs(&CommandLine::parse("bash script.sh -c 'rm x'")),
            ["bash"]
        );
        assert_eq!(
            execs(&CommandLine::parse("bash -- s.sh -c 'rm x'")),
            ["bash"]
        );
        assert_eq!(execs(&CommandLine::parse("bash -s x -c 'rm x'")), ["bash"]);
        // `-o`/`-O` clusters consume their argument without ending the option run.
        assert_eq!(
            execs(&CommandLine::parse("bash -euo pipefail -c 'rm x'")),
            ["bash", "rm"]
        );
        assert_eq!(
            execs(&CommandLine::parse("bash -O extglob -c 'ls'")),
            ["bash", "ls"]
        );
    }

    // bash only tilde-expands a word-leading `~`; one buried mid-concatenation stays literal.
    #[test]
    fn concatenation_tilde_must_lead_the_word() {
        let word = |raw: &str| {
            CommandLine::parse(&format!("echo {raw}"))
                .head()
                .cloned()
                .unwrap()
                .words[1]
                .clone()
        };
        assert!(!word("\"foo\"~bar").expandable);
        assert!(word("~/\"x\"").expandable);
        assert!(word("'a'*").expandable);
    }

    #[test]
    fn payload_depth_caps_enumeration() {
        // Four shell levels: the third's payload still enumerates, the fourth's does not.
        let line = CommandLine::parse(r#"bash -c 'bash -c "bash -c \"bash -c ls\""'"#);
        assert_eq!(execs(&line), ["bash", "bash", "bash", "bash"]);
        assert_eq!(
            line.parts
                .iter()
                .map(|(cmd, _)| cmd.nesting)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn payload_non_posix_shell_enumerates_span_less() {
        let line = CommandLine::parse("fish -c 'rm x'");
        assert_eq!(execs(&line), ["fish", "rm"]);
        assert_eq!(line.parts[1].0.span, None);
        assert_eq!(line.parts[1].0.contexts, [super::QuoteLayer::Single]);
    }

    #[test]
    fn payload_splice_embeds_or_refuses() {
        let line = CommandLine::parse("bash -c 'rm -rf /tmp/x'");
        assert_eq!(
            line.splice(&BTreeMap::from([(1, "trash /tmp/x".to_string())]))
                .unwrap(),
            "bash -c 'trash /tmp/x'"
        );
        // A single quote cannot survive the single-quote layer.
        assert_eq!(
            line.splice(&BTreeMap::from([(1, "echo 'hi'".to_string())])),
            Err(SpliceError::NotEmbeddable { index: 1 })
        );
        // Host and nested payload cannot rewrite simultaneously.
        assert!(matches!(
            line.splice(&BTreeMap::from([
                (0, "X".to_string()),
                (1, "Y".to_string())
            ])),
            Err(SpliceError::Overlap { .. })
        ));
        // The double-quote layer refuses expansions the outer shell would evaluate.
        let double = CommandLine::parse("bash -c \"rm x\"");
        assert_eq!(
            double
                .splice(&BTreeMap::from([(1, "trash x".to_string())]))
                .unwrap(),
            "bash -c \"trash x\""
        );
        assert_eq!(
            double.splice(&BTreeMap::from([(1, "echo $HOME".to_string())])),
            Err(SpliceError::NotEmbeddable { index: 1 })
        );
        // A bare payload keeps single-word replacements only.
        let bare = CommandLine::parse("bash -c rm");
        assert_eq!(
            bare.splice(&BTreeMap::from([(1, "trash".to_string())]))
                .unwrap(),
            "bash -c trash"
        );
        assert_eq!(
            bare.splice(&BTreeMap::from([(1, "trash x".to_string())])),
            Err(SpliceError::NotEmbeddable { index: 1 })
        );
    }

    #[test]
    fn payload_parts_carry_operators_like_substitutions() {
        let line = CommandLine::parse("bash -c 'ls' && foo");
        assert_eq!(execs(&line), ["bash", "ls", "foo"]);
        assert_eq!(line.parts[0].1, None);
        assert_eq!(line.parts[1].1.as_deref(), Some("&&"));
        assert_eq!(line.primary().unwrap().executable, "foo");
    }

    #[test]
    fn nested_hosts_resolve_through_host_delta() {
        let line = CommandLine::parse("diff $(b $(c))");
        assert_eq!(execs(&line), ["diff", "b", "c"]);
        assert_eq!(line.parts[1].0.host_delta, Some(1));
        assert_eq!(line.parts[2].0.host_delta, Some(1));
        // An assignment-position substitution has no hosting command part.
        let assign = CommandLine::parse("x=$(a)");
        assert_eq!(line.parts[1].0.nesting, 1);
        assert_eq!(assign.parts[0].0.host_delta, None);
        // Substitutions inside a payload nest below it, carrying its quote layer.
        let nested = CommandLine::parse("bash -c 'echo $(a)'");
        assert_eq!(execs(&nested), ["bash", "echo", "a"]);
        assert_eq!(nested.parts[1].0.nesting, 1);
        assert_eq!(nested.parts[1].0.host_delta, Some(1));
        assert_eq!(nested.parts[2].0.nesting, 2);
        assert_eq!(nested.parts[2].0.host_delta, Some(1));
        assert_eq!(nested.parts[2].0.contexts, [super::QuoteLayer::Single]);
        assert_eq!(nested.parts[2].0.span, None);
    }

    #[test]
    fn nested_payload_stacks_contexts_and_splices() {
        let line = CommandLine::parse("bash -c 'sh -c \"rm x\"'");
        assert_eq!(execs(&line), ["bash", "sh", "rm"]);
        assert_eq!(
            line.parts[2].0.contexts,
            [super::QuoteLayer::Single, super::QuoteLayer::Double]
        );
        let (start, end) = line.parts[2].0.span.unwrap();
        assert_eq!(&line.raw[start..end], "rm x");
        assert_eq!(
            line.splice(&BTreeMap::from([(2, "trash x".to_string())]))
                .unwrap(),
            "bash -c 'sh -c \"trash x\"'"
        );
    }

    #[test]
    fn shell_quote_matches_shlex_semantics() {
        assert_eq!(super::shell_quote("abc"), "abc");
        assert_eq!(super::shell_quote("a b"), "'a b'");
        assert_eq!(super::shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(super::shell_quote(""), "''");
    }

    #[test]
    fn quote_for_picks_the_first_surviving_spelling() {
        use super::QuoteLayer::{Bare, Double, Single};
        assert_eq!(super::quote_for(&[], "ab").as_deref(), Some("ab"));
        assert_eq!(super::quote_for(&[], "a b").as_deref(), Some("'a b'"));
        assert_eq!(super::quote_for(&[Single], "ab").as_deref(), Some("ab"));
        assert_eq!(
            super::quote_for(&[Single], "a b").as_deref(),
            Some("\"a b\"")
        );
        assert_eq!(super::quote_for(&[Double], "ab").as_deref(), Some("ab"));
        // Single quotes transport verbatim through a double layer; the inner shell strips them.
        assert_eq!(super::quote_for(&[Double], "a b").as_deref(), Some("'a b'"));
        assert_eq!(super::quote_for(&[Bare], "ab").as_deref(), Some("ab"));
        assert_eq!(super::quote_for(&[Bare], "a b"), None);
        assert_eq!(
            super::quote_for(&[Single, Double], "ab").as_deref(),
            Some("ab")
        );
        assert_eq!(super::quote_for(&[Single, Double], "a b"), None);
        assert!(super::embeddable(&[], "anything ' at all"));
        assert!(super::embeddable(&[Double], "trash \\$HOME"));
        assert!(!super::embeddable(&[Double], "trash $HOME"));
    }
}
