use std::collections::BTreeMap;
use std::fmt;

use once_cell::sync::Lazy;
use rable::{parse, ListOperator, Node, NodeKind, PipeSep, Span};
use regex::Regex;

use crate::literals::command::{
    ASSIGNMENT_PATTERN, MULTI_LEVEL_TOOLS, PAYLOAD_DEPTH_LIMIT, POSIX_QUOTING_SHELLS,
    SHELL_COMMANDS, WRAPPER_COMMANDS, WRAPPER_OPERAND_SKIP, WRAPPER_VALUE_FLAGS,
};
use crate::pystr;

static ASSIGNMENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(ASSIGNMENT_PATTERN).expect("assignment regex"));

// rable spans index characters; the contract's byte spans need a char->byte map (they diverge for
// multibyte UTF-8). `cmap[len]` = source length, so an end index one past the last char resolves.
struct Src<'a> {
    text: &'a str,
    cmap: Vec<usize>,
}

impl<'a> Src<'a> {
    fn new(text: &'a str) -> Self {
        let mut cmap: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
        cmap.push(text.len());
        Src { text, cmap }
    }

    fn bytes(&self, span: &Span) -> (usize, usize) {
        let at = |c: usize| self.cmap.get(c).copied().unwrap_or(self.text.len());
        (at(span.start), at(span.end))
    }

    fn slice(&self, span: &Span) -> &'a str {
        let (start, end) = self.bytes(span);
        self.text.get(start..end).unwrap_or("")
    }
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
        let stripped = strip_wrappers(&argv, &self.words);
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
        let argv = strip_wrappers(&self.argv(), &self.words);
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
        let unwrapped = strip_wrappers(&self.argv(), &self.words);
        unwrapped.len() >= argv.len() && unwrapped[..argv.len()] == *argv
    }

    // Split the argument words into (options, operands): `--` ends options (dropped), a lone `-` is
    // an operand, a `-`-led token is an option that pulls its next word when it is a listed value
    // flag. Classification reads the dequoted arg text; the returned Word handles carry each token's
    // raw spelling and span so a value flag can never match a duplicate operand back to the wrong word.
    pub fn split_options(&self, value_flags: &[&str]) -> (Vec<Word>, Vec<Word>) {
        let words = self.words.get(1..).unwrap_or_default();
        let mut options: Vec<Word> = Vec::new();
        let mut operands: Vec<Word> = Vec::new();
        let mut i = 0;
        while i < self.args.len() {
            match self.args[i].as_str() {
                "--" => {
                    operands.extend(words[i + 1..].iter().cloned());
                    break;
                }
                "-" => operands.push(words[i].clone()),
                flag if flag.starts_with('-') => {
                    options.push(words[i].clone());
                    if is_value_flag(flag, value_flags) && i + 1 < self.args.len() {
                        options.push(words[i + 1].clone());
                        i += 1;
                    }
                }
                _ => operands.push(words[i].clone()),
            }
            i += 1;
        }
        (options, operands)
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
        let src = Src::new(raw);
        // A syntax error (`|`, `&&`) yields no commands, matching the old empty-tree fallback.
        let mut parts = match parse(raw, false) {
            Ok(nodes) => nodes
                .iter()
                .flat_map(|node| walk(node, &src, depth))
                .collect(),
            Err(_) => Vec::new(),
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

// Double-quote escape resolution; returns whether any escape reshaped the bytes.
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

// A word part that leaves the word unresolvable (`value = None`): every expansion kind rable
// splits out of the literal text, plus ANSI-C (`$'…'`) and locale (`$"…"`) quoting — the raw-slice
// re-scan mishandles their leading `$`, so signalling None matches the base parser's honest taint.
fn is_expansion_part(part: &Node) -> bool {
    matches!(
        part.kind,
        NodeKind::ParamExpansion { .. }
            | NodeKind::ParamLength { .. }
            | NodeKind::ParamIndirect { .. }
            | NodeKind::CommandSubstitution { .. }
            | NodeKind::ProcessSubstitution { .. }
            | NodeKind::ArithmeticExpansion { .. }
            | NodeKind::AnsiCQuote { .. }
            | NodeKind::LocaleString { .. }
    )
}

// The outer quote layer of a raw word: `'…'` and `"…"` only when a single matching pair wraps the
// whole word (adjacent or mixed quoting is a bare concatenation). rable does not expose it.
fn wholly_quoted(raw: &str) -> QuoteLayer {
    let bytes = raw.as_bytes();
    match bytes.first() {
        Some(b'\'') => match raw[1..].find('\'') {
            Some(pos) if pos + 1 == raw.len() - 1 => QuoteLayer::Single,
            _ => QuoteLayer::Bare,
        },
        Some(b'"') => {
            let mut i = 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        return if i == bytes.len() - 1 {
                            QuoteLayer::Double
                        } else {
                            QuoteLayer::Bare
                        }
                    }
                    _ => i += 1,
                }
            }
            QuoteLayer::Bare
        }
        _ => QuoteLayer::Bare,
    }
}

// The executable/arg spelling: one wrapping quote layer stripped, else verbatim.
fn word_text(node: &Node, src: &Src) -> String {
    let raw = src.slice(&node.span);
    match wholly_quoted(raw) {
        QuoteLayer::Bare => raw.to_string(),
        _ => dequote(raw).to_string(),
    }
}

// Strip every quote layer and escape from a bare (concatenated) word: single quotes pass through
// literally, double quotes resolve `\ $ ` "` escapes, a bare backslash escapes the next char.
fn bare_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for ic in chars.by_ref() {
                    if ic == '\'' {
                        break;
                    }
                    out.push(ic);
                }
            }
            '"' => {
                while let Some(ic) = chars.next() {
                    if ic == '"' {
                        break;
                    }
                    match (ic, chars.peek()) {
                        ('\\', Some('"' | '\\' | '$' | '`')) => out.push(chars.next().unwrap()),
                        ('\\', Some('\n')) => {
                            chars.next();
                        }
                        _ => out.push(ic),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// The unquoted runs of a bare word, joined — glob/brace expandability reads only these (a `*`
// inside quotes is literal), and a tilde only expands when it leads the whole word.
fn bare_pieces(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for ic in chars.by_ref() {
                    if ic == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                while let Some(ic) = chars.next() {
                    match ic {
                        '"' => break,
                        '\\' => {
                            chars.next();
                        }
                        _ => {}
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

// Per-word structural resolution: `value` strips every quote layer and escape (None when an
// expansion taints it), `content_offset` marks a verbatim contiguous source slice, `layer` and
// `expandable` reconstruct what rable does not expose.
fn analyze_word(node: &Node, src: &Src) -> Word {
    let (start, end) = src.bytes(&node.span);
    let raw = src.text.get(start..end).unwrap_or("").to_string();
    let span = Some((start, end));
    let tainted = match &node.kind {
        NodeKind::Word { parts, .. } => parts.iter().any(is_expansion_part),
        _ => false,
    };
    match wholly_quoted(&raw) {
        QuoteLayer::Single => Word {
            value: Some(raw[1..raw.len() - 1].to_string()),
            span,
            expandable: false,
            layer: QuoteLayer::Single,
            content_offset: Some(start + 1),
            raw,
        },
        QuoteLayer::Double if tainted => Word {
            value: None,
            span,
            expandable: false,
            layer: QuoteLayer::Double,
            content_offset: None,
            raw,
        },
        QuoteLayer::Double => {
            let inner = &raw[1..raw.len() - 1];
            let mut value = String::new();
            let escaped = resolve_double_quoted(inner, &mut value);
            let content_offset = (!escaped && !inner.is_empty()).then_some(start + 1);
            Word {
                value: Some(value),
                span,
                expandable: false,
                layer: QuoteLayer::Double,
                content_offset,
                raw,
            }
        }
        QuoteLayer::Bare => {
            let value = (!tainted).then(|| bare_value(&raw));
            let content_offset = match &value {
                Some(v) if *v == raw => Some(start),
                _ => None,
            };
            let expandable = raw.starts_with('~') || glob_or_brace(&bare_pieces(&raw));
            Word {
                value,
                span,
                expandable,
                layer: QuoteLayer::Bare,
                content_offset,
                raw,
            }
        }
    }
}

// A standalone process/arithmetic expansion is never an executable, arg, or word; a standalone
// command substitution is one only in command-name (first) position. Everything else, and any
// concatenation, stays. This mirrors tree-sitter's separate node kinds for these expansions.
fn standalone_part(node: &Node) -> Option<&NodeKind> {
    match &node.kind {
        NodeKind::Word { parts, .. } if parts.len() == 1 => Some(&parts[0].kind),
        _ => None,
    }
}

fn excluded_word(node: &Node, first: bool) -> bool {
    match standalone_part(node) {
        Some(NodeKind::ProcessSubstitution { .. } | NodeKind::ArithmeticExpansion { .. }) => true,
        Some(NodeKind::CommandSubstitution { .. }) => !first,
        _ => false,
    }
}

// Byte offset within `raw` of the inner content of each top-level command substitution (`$(…)` or
// backticks), paired with that inner text. Single-quoted spans are literal; `${…}`/`$((…))` are not
// command substitutions; a command substitution inside double quotes still counts.
fn command_subs(raw: &str) -> Vec<(usize, String)> {
    let b = raw.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_double = false;
    while i < n {
        match b[i] {
            b'\\' => i += 2,
            b'\'' if !in_double => {
                i += 1;
                while i < n && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                in_double = !in_double;
                i += 1;
            }
            b'`' => {
                let start = i + 1;
                let mut j = start;
                while j < n && b[j] != b'`' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                out.push((start, raw.get(start..j.min(n)).unwrap_or("").to_string()));
                i = j + 1;
            }
            b'$' if i + 1 < n && b[i + 1] == b'(' => {
                if i + 2 < n && b[i + 2] == b'(' {
                    i = skip_matched(b, i + 3, b'(', b')');
                } else {
                    let start = i + 2;
                    let close = match_paren(b, start);
                    out.push((
                        start,
                        raw.get(start..close.min(n)).unwrap_or("").to_string(),
                    ));
                    i = close + 1;
                }
            }
            b'$' if i + 1 < n && b[i + 1] == b'{' => i = skip_matched(b, i + 2, b'{', b'}'),
            _ => i += 1,
        }
    }
    out
}

// Index of the `)` closing a `$(` opened just before `from`, respecting quotes and nested parens.
fn match_paren(b: &[u8], mut from: usize) -> usize {
    let n = b.len();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    while from < n {
        match b[from] {
            b'\\' if !in_single => from += 2,
            b'\'' if !in_double => {
                in_single = !in_single;
                from += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                from += 1;
            }
            b'(' if !in_single && !in_double => {
                depth += 1;
                from += 1;
            }
            b')' if !in_single && !in_double => {
                if depth == 0 {
                    return from;
                }
                depth -= 1;
                from += 1;
            }
            _ => from += 1,
        }
    }
    n
}

// Index just past the delimiter that closes `open`/`close` opened just before `from`.
fn skip_matched(b: &[u8], mut from: usize, open: u8, close: u8) -> usize {
    let n = b.len();
    let mut depth = 0usize;
    while from < n {
        let c = b[from];
        from += 1;
        if c == open {
            depth += 1;
        } else if c == close {
            match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return from,
            }
        }
    }
    n
}

// Background `&` records no operator token (like `|&` and newlines): the old parser attached
// nothing, and a backgrounded command is not "joined" to its neighbor.
fn list_op(op: ListOperator) -> Option<&'static str> {
    match op {
        ListOperator::And => Some("&&"),
        ListOperator::Or => Some("||"),
        ListOperator::Semi => Some(";"),
        ListOperator::Background => None,
    }
}

// name=value assignments become env pairs; the value strips one wrapping quote layer like word_text.
fn build_env(assignments: &[Node], src: &Src) -> Vec<(String, String)> {
    assignments
        .iter()
        .filter_map(|node| {
            let raw = src.slice(&node.span);
            let eq = raw.find('=')?;
            let value = &raw[eq + 1..];
            let value = match wholly_quoted(value) {
                QuoteLayer::Bare => value.to_string(),
                _ => dequote(value).to_string(),
            };
            Some((raw[..eq].to_string(), value))
        })
        .collect()
}

// File redirects only; heredocs stay out of the redirect list (their bytes are not spliceable), and
// an unspecified leading descriptor (rable's -1) becomes None.
fn build_redirects(redirects: &[Node], src: &Src) -> Vec<Redirect> {
    redirects
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Redirect { op, target, fd, .. } => Some(Redirect {
                op: op.clone(),
                target: match &target.kind {
                    NodeKind::Word { value, .. } => value.clone(),
                    _ => src.slice(&target.span).to_string(),
                },
                fd: (*fd >= 0).then_some(*fd as i64),
            }),
            _ => None,
        })
        .collect()
}

fn with_redirects(
    node: &Node,
    redirects: &[Node],
    mut parts: Vec<(Command, Option<String>)>,
    src: &Src,
) -> Vec<(Command, Option<String>)> {
    if redirects.is_empty() {
        return parts;
    }
    let redirects = build_redirects(redirects, src);
    if parts.is_empty() {
        return vec![(
            Command {
                raw: src.slice(&node.span).to_string(),
                redirects,
                span: Some(src.bytes(&node.span)),
                ..Command::default()
            },
            None,
        )];
    }
    for (cmd, _) in &mut parts {
        cmd.redirects.extend(redirects.iter().cloned());
    }
    parts
}

// List/pipeline split at their operators; a command extracts plus its substitution and payload
// parts; compound bodies recurse; everything else contributes nothing.
fn walk(node: &Node, src: &Src, depth: u8) -> Vec<(Command, Option<String>)> {
    match &node.kind {
        NodeKind::List { items } => {
            let mut parts: Vec<(Command, Option<String>)> = Vec::new();
            for item in items {
                let mut inner = walk(&item.command, src, depth);
                if let (Some(Some(op)), Some(last)) = (item.operator.map(list_op), inner.last_mut())
                {
                    last.1 = Some(op.to_string());
                }
                parts.extend(inner);
            }
            parts
        }
        NodeKind::Pipeline {
            commands,
            separators,
        } => {
            let mut parts: Vec<(Command, Option<String>)> = Vec::new();
            for (i, command) in commands.iter().enumerate() {
                let mut inner = walk(command, src, depth);
                // `|&` records no operator token (matching the old parser); the raw-gap fallback in
                // `piped` still reads it as a pipe. Only a plain `|` attaches an operator.
                if let (Some(PipeSep::Pipe), Some(last)) =
                    (separators.get(i).copied(), inner.last_mut())
                {
                    last.1 = Some("|".to_string());
                }
                parts.extend(inner);
            }
            parts
        }
        NodeKind::Command {
            assignments,
            words,
            redirects,
        } => walk_command(node, assignments, words, redirects, src, depth),
        NodeKind::Subshell {
            body, redirects, ..
        }
        | NodeKind::BraceGroup {
            body, redirects, ..
        } => with_redirects(node, redirects, walk(body, src, depth), src),
        NodeKind::If {
            condition,
            then_body,
            else_body,
            redirects,
        } => {
            let mut parts = walk(condition, src, depth);
            parts.extend(walk(then_body, src, depth));
            if let Some(else_body) = else_body {
                parts.extend(walk(else_body, src, depth));
            }
            with_redirects(node, redirects, parts, src)
        }
        NodeKind::While {
            condition,
            body,
            redirects,
        }
        | NodeKind::Until {
            condition,
            body,
            redirects,
        } => {
            let mut parts = walk(condition, src, depth);
            parts.extend(walk(body, src, depth));
            with_redirects(node, redirects, parts, src)
        }
        NodeKind::For {
            words,
            body,
            redirects,
            ..
        }
        | NodeKind::Select {
            words,
            body,
            redirects,
            ..
        } => {
            let mut parts = words.as_ref().map_or_else(Vec::new, |_| {
                header_substitution_parts(node, body, src, depth)
            });
            parts.extend(walk(body, src, depth));
            with_redirects(node, redirects, parts, src)
        }
        NodeKind::ForArith {
            body, redirects, ..
        } => {
            let mut parts = header_substitution_parts(node, body, src, depth);
            parts.extend(walk(body, src, depth));
            with_redirects(node, redirects, parts, src)
        }
        NodeKind::Case {
            word,
            patterns,
            redirects,
        } => {
            let (mut cursor, end) = src.bytes(&node.span);
            let mut parts = located_word_substitution_parts(word, src, &mut cursor, end, depth);
            for pattern in patterns {
                for word in &pattern.patterns {
                    parts.extend(located_word_substitution_parts(
                        word,
                        src,
                        &mut cursor,
                        end,
                        depth,
                    ));
                }
                if let Some(body) = &pattern.body {
                    parts.extend(walk(body, src, depth));
                    cursor = src.bytes(&body.span).1;
                }
            }
            with_redirects(node, redirects, parts, src)
        }
        NodeKind::Function { body, .. } => walk(body, src, depth),
        NodeKind::ConditionalExpr {
            body, redirects, ..
        } => with_redirects(
            node,
            redirects,
            substitution_parts(&[], std::slice::from_ref(body.as_ref()), None, src, depth),
            src,
        ),
        NodeKind::ArithmeticCommand { redirects, .. } => {
            with_redirects(node, redirects, Vec::new(), src)
        }
        NodeKind::Coproc { command, .. } => walk(command, src, depth),
        NodeKind::Negation { pipeline } => walk(pipeline, src, depth),
        NodeKind::Time { pipeline, .. } => walk(pipeline, src, depth),
        NodeKind::Word { .. }
        | NodeKind::WordLiteral { .. }
        | NodeKind::Redirect { .. }
        | NodeKind::HereDoc { .. }
        | NodeKind::ParamExpansion { .. }
        | NodeKind::ParamLength { .. }
        | NodeKind::ParamIndirect { .. }
        | NodeKind::CommandSubstitution { .. }
        | NodeKind::ProcessSubstitution { .. }
        | NodeKind::AnsiCQuote { .. }
        | NodeKind::LocaleString { .. }
        | NodeKind::BraceExpansion { .. }
        | NodeKind::ArithmeticExpansion { .. }
        | NodeKind::ArithNumber { .. }
        | NodeKind::ArithVar { .. }
        | NodeKind::ArithBinaryOp { .. }
        | NodeKind::ArithUnaryOp { .. }
        | NodeKind::ArithPreIncr { .. }
        | NodeKind::ArithPostIncr { .. }
        | NodeKind::ArithPreDecr { .. }
        | NodeKind::ArithPostDecr { .. }
        | NodeKind::ArithAssign { .. }
        | NodeKind::ArithTernary { .. }
        | NodeKind::ArithComma { .. }
        | NodeKind::ArithSubscript { .. }
        | NodeKind::ArithEmpty
        | NodeKind::ArithEscape { .. }
        | NodeKind::ArithDeprecated { .. }
        | NodeKind::ArithConcat { .. }
        | NodeKind::UnaryTest { .. }
        | NodeKind::BinaryTest { .. }
        | NodeKind::CondAnd { .. }
        | NodeKind::CondOr { .. }
        | NodeKind::CondNot { .. }
        | NodeKind::CondParen { .. }
        | NodeKind::CondTerm { .. }
        | NodeKind::Array { .. }
        | NodeKind::Empty
        | NodeKind::Comment { .. } => Vec::new(),
    }
}

// Word/argument-position `$(…)`/backtick substitutions under a command, enumerated as nested parts
// mirroring assignment position: document order (assignments then words), redirects excluded.
// Each inner command re-parses depth-unchanged, offsets its spans into the outer raw, and hoists a
// nesting level; the host's own span nulls their enclosed command spans back in parse_at_depth.
fn substitution_parts_in(raw: &str, start: usize, depth: u8) -> Vec<(Command, Option<String>)> {
    let mut parts: Vec<(Command, Option<String>)> = Vec::new();
    for (inner_offset, inner) in command_subs(raw) {
        let offset = start + inner_offset;
        let mut inner_parts = CommandLine::parse_at_depth(&inner, depth).parts;
        for (cmd, _) in inner_parts.iter_mut() {
            cmd.nesting = cmd.nesting.saturating_add(1);
            cmd.span = cmd.span.map(|(s, e)| (s + offset, e + offset));
            for w in &mut cmd.words {
                w.span = w.span.map(|(s, e)| (s + offset, e + offset));
                w.content_offset = w.content_offset.map(|c| c + offset);
            }
        }
        parts.extend(inner_parts);
    }
    parts
}

fn header_substitution_parts(
    node: &Node,
    body: &Node,
    src: &Src,
    depth: u8,
) -> Vec<(Command, Option<String>)> {
    let (start, _) = src.bytes(&node.span);
    let (body_start, _) = src.bytes(&body.span);
    substitution_parts_in(&src.text[start..body_start], start, depth)
}

fn located_word_substitution_parts(
    node: &Node,
    src: &Src,
    cursor: &mut usize,
    end: usize,
    depth: u8,
) -> Vec<(Command, Option<String>)> {
    let NodeKind::Word { value, .. } = &node.kind else {
        unreachable!("compound header word")
    };
    let relative = src.text[*cursor..end]
        .find(value)
        .expect("compound header word source");
    let start = *cursor + relative;
    *cursor = start + value.len();
    substitution_parts_in(value, start, depth)
}

fn substitution_parts(
    assignments: &[Node],
    words: &[Node],
    excluded_word_span: Option<(usize, usize)>,
    src: &Src,
    depth: u8,
) -> Vec<(Command, Option<String>)> {
    assignments
        .iter()
        .chain(words.iter())
        .filter(|word| Some(src.bytes(&word.span)) != excluded_word_span)
        .flat_map(|word| {
            let (start, _) = src.bytes(&word.span);
            substitution_parts_in(src.slice(&word.span), start, depth)
        })
        .collect()
}

// A simple command: words[0] is the executable (never dequoted), words[1..] the args, assignments
// the env, file redirects excluded from the span. A pure assignment host emits no command (its
// substitutions stand alone); a redirect-only command emits one synthetic empty part.
fn walk_command(
    node: &Node,
    assignments: &[Node],
    words: &[Node],
    redirects: &[Node],
    src: &Src,
    depth: u8,
) -> Vec<(Command, Option<String>)> {
    let content: Vec<&Node> = words
        .iter()
        .enumerate()
        .filter(|(i, w)| !excluded_word(w, *i == 0))
        .map(|(_, w)| w)
        .collect();

    if content.is_empty() {
        let subs = substitution_parts(assignments, words, None, src, depth);
        if !subs.is_empty() {
            return subs;
        }
        if redirects.is_empty() {
            return Vec::new();
        }
        return vec![(
            Command {
                raw: src.slice(&node.span).to_string(),
                env: build_env(assignments, src),
                redirects: build_redirects(redirects, src),
                span: Some(src.bytes(&node.span)),
                ..Command::default()
            },
            None,
        )];
    }

    // None when the content interval overlaps a redirect (splice must not overwrite redirect bytes)
    // or a degraded multi-heredoc; a trailing redirect stays outside, so `rm x >out` still rewrites.
    let heredocs = redirects
        .iter()
        .filter(|r| matches!(r.kind, NodeKind::HereDoc { .. }))
        .count();
    let content_span: Vec<&Node> = assignments.iter().chain(words.iter()).collect();
    let start = content_span
        .iter()
        .map(|n| src.bytes(&n.span).0)
        .min()
        .unwrap();
    let end = content_span
        .iter()
        .map(|n| src.bytes(&n.span).1)
        .max()
        .unwrap();
    let straddles_redirect = redirects.iter().any(|r| {
        let (rs, re) = src.bytes(&r.span);
        rs < end && re > start
    });
    let span = (heredocs < 2 && !straddles_redirect).then_some((start, end));

    let host = Command {
        raw: src.slice(&node.span).to_string(),
        executable: src.slice(&content[0].span).to_string(),
        args: content[1..].iter().map(|w| word_text(w, src)).collect(),
        env: build_env(assignments, src),
        redirects: build_redirects(redirects, src),
        span,
        words: content.iter().map(|w| analyze_word(w, src)).collect(),
        ..Command::default()
    };

    let (payloads, excluded_word_span) = payload_parts(&host, src, depth);
    let subs = substitution_parts(assignments, words, excluded_word_span, src, depth);
    let mut parts = vec![(host, None)];
    parts.extend(subs);
    parts.extend(payloads);
    for index in 1..parts.len() {
        if parts[index].0.nesting == 1 && parts[index].0.host_delta.is_none() {
            parts[index].0.host_delta = Some(index);
        }
    }
    parts
}

// Parity: command.py Command.unwrapped dropwhile — flags, bare ASCII-integer args, VAR=val.
fn is_wrapper_skip(arg: &str) -> bool {
    arg.starts_with('-')
        || (!arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()))
        || ASSIGNMENT_RE.is_match(arg)
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn wrapper_value_flags(wrapper: &str) -> &'static [&'static str] {
    WRAPPER_VALUE_FLAGS
        .iter()
        .find(|(name, _)| *name == wrapper)
        .map_or(&[], |(_, flags)| *flags)
}

fn wrapper_operand_skip(wrapper: &str) -> usize {
    WRAPPER_OPERAND_SKIP
        .iter()
        .find(|(name, _)| *name == wrapper)
        .map_or(0, |(_, count)| *count)
}

fn is_value_flag(flag: &str, value_flags: &[&str]) -> bool {
    !flag.contains('=') && value_flags.contains(&flag)
}

// A GNU duration operand: `[0-9]`-led or `.[0-9]`-led (`.5s`), never a bare command like `rm`.
fn duration_led(operand: &str) -> bool {
    match operand.as_bytes() {
        [b'.', d, ..] => d.is_ascii_digit(),
        [d, ..] => d.is_ascii_digit(),
        [] => false,
    }
}

// Tokens `wrapper` consumes before the real command: value flags swallow their argument, then the
// operand budget consumes a duration-led operand only (so a malformed `timeout rm` never hides the
// command), then today's bare-integer / VAR=val skips. Unknown flags keep flag-only skip.
fn wrapper_skip(wrapper: &str, tokens: &[&str]) -> usize {
    let value_flags = wrapper_value_flags(wrapper);
    let mut budget = wrapper_operand_skip(wrapper);
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            flag if flag.len() > 1 && flag.starts_with('-') => {
                i += 1 + usize::from(is_value_flag(flag, value_flags));
            }
            operand if budget > 0 && duration_led(operand) => {
                budget -= 1;
                i += 1;
            }
            arg if is_wrapper_skip(arg) => i += 1,
            _ => break,
        }
    }
    i.min(tokens.len())
}

// Drop each leading wrapper plus its skippable args. The head matches on the dequoted word value
// (`"sudo"` → `sudo`), basenamed (`/usr/bin/sudo` → `sudo`); argv and words slice in lockstep.
fn strip_wrappers<'a>(argv: &[&'a str], words: &[Word]) -> Vec<&'a str> {
    let mut argv: Vec<&str> = argv.to_vec();
    let mut words: &[Word] = words;
    while let Some(&raw_head) = argv.first() {
        let head = basename(
            words
                .first()
                .and_then(|w| w.value.as_deref())
                .unwrap_or(raw_head),
        );
        if !WRAPPER_COMMANDS.contains(&head) {
            break;
        }
        let skip = wrapper_skip(head, &argv[1..]);
        argv = argv[1 + skip..].to_vec();
        words = words.get(1 + skip..).unwrap_or_default();
    }
    argv
}

fn payload_flag(arg: &str) -> bool {
    arg.strip_prefix('-').is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase()) && rest.ends_with('c')
    })
}

// A shell payload word's re-parseable text: its resolved value when clean, or the dequoted raw when
// an expansion taints it. A tainted payload re-parses span-less (offset None) so its inner command
// enumerates but splice refuses to rewrite it.
fn payload_of(word: &Word) -> (String, QuoteLayer, Option<usize>) {
    match &word.value {
        Some(value) => (value.clone(), word.layer, word.content_offset),
        None => (dequote(&word.raw).to_string(), word.layer, None),
    }
}

// The `-c` cluster must sit in the leading option run: options end at the first operand
// (the script file) or `--`, and a `-o`/`-O` cluster consumes the next word as its argument.
fn shell_payload(words: &[Word]) -> Option<(String, QuoteLayer, Option<usize>)> {
    let mut index = 1;
    while let Some(value) = words.get(index).and_then(|word| word.value.as_deref()) {
        match value {
            "--" => return None,
            _ if !value.starts_with('-') => return None,
            _ if payload_flag(value) => return Some(payload_of(words.get(index + 1)?)),
            _ if value.ends_with('o') || value.ends_with('O') => index += 2,
            _ => index += 1,
        }
    }
    None
}

fn eval_payload(words: &[Word], src: &[u8]) -> Option<(String, QuoteLayer, Option<usize>)> {
    match words {
        [] => None,
        [word] => match &word.value {
            Some(value) => Some((value.clone(), word.layer, word.content_offset)),
            None => {
                let value = dequote(&word.raw).to_string();
                CommandLine::parse_at_depth(&value, PAYLOAD_DEPTH_LIMIT)
                    .parts
                    .iter()
                    .filter(|(cmd, _)| cmd.nesting == 0)
                    .any(|(cmd, _)| {
                        cmd.unwrapped()
                            .words
                            .first()
                            .and_then(|word| word.value.as_deref())
                            .is_some()
                    })
                    .then_some((value, word.layer, None))
            }
        },
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
// mapping is structural — a verbatim contiguous payload keeps outer-raw spans; shell taint and
// non-POSIX quoting go span-less. An unevaluable eval word emits no payload.
fn payload_parts(
    host: &Command,
    src: &Src,
    depth: u8,
) -> (Vec<(Command, Option<String>)>, Option<(usize, usize)>) {
    if depth >= PAYLOAD_DEPTH_LIMIT {
        return (Vec::new(), None);
    }
    let unwrapped = host.unwrapped();
    let words = &unwrapped.words;
    let Some(exe) = words.first().and_then(|word| word.value.as_deref()) else {
        return (Vec::new(), None);
    };
    let exe = exe.rsplit('/').next().unwrap_or(exe);
    let payload = match exe {
        "eval" => eval_payload(&words[1..], src.text.as_bytes()),
        _ if SHELL_COMMANDS.contains(&exe) => shell_payload(words),
        _ => return (Vec::new(), None),
    };
    let Some((value, layer, offset)) = payload else {
        return (Vec::new(), None);
    };
    let excluded_word_span = (exe == "eval" && words.len() == 2)
        .then(|| words[1].span)
        .flatten();
    let offset = match exe == "eval" || POSIX_QUOTING_SHELLS.contains(&exe) {
        true => offset,
        false => None,
    };
    let mut parts = CommandLine::parse_at_depth(&value, depth + 1).parts;
    for (cmd, _) in &mut parts {
        if exe == "eval" {
            if let Some(executable) = cmd.words.first().and_then(|word| word.value.as_ref()) {
                cmd.executable.clone_from(executable);
            }
        }
        cmd.nesting = cmd.nesting.saturating_add(1);
        cmd.contexts.insert(0, layer);
        cmd.span = offset.and_then(|offset| cmd.span.map(|(s, e)| (s + offset, e + offset)));
        for word in &mut cmd.words {
            word.span = offset.and_then(|offset| word.span.map(|(s, e)| (s + offset, e + offset)));
            word.content_offset = offset.and_then(|offset| word.content_offset.map(|c| c + offset));
        }
    }
    (parts, excluded_word_span)
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
    fn arity_aware_unwrap_reaches_the_real_command() {
        for raw in [
            "env -u HOME rm /x",
            "sudo -u root rm /x",
            "timeout 5s rm /x",
            "sudo -u root -g wheel rm /x",
            "env --unset=HOME rm /x",
            "nice -n 10 rm /x",
            "xargs -I{} rm {}",
            "sudo env -u HOME rm /x",
            "sudo -Z rm /x",
            "/usr/bin/sudo rm /x",
        ] {
            let cmd = CommandLine::parse(raw).primary().unwrap().unwrapped();
            assert_eq!(cmd.executable, "rm", "unwrapping {raw}");
        }
    }

    #[test]
    fn operand_skip_only_consumes_a_digit_led_duration() {
        // A non-digit token in the duration slot is the command, not the duration — never hidden.
        for (raw, executable) in [
            ("timeout rm -rf /", "rm"),
            ("timeout git push", "git"),
            ("timeout ٣ git push", "٣"),
        ] {
            let cmd = CommandLine::parse(raw).primary().unwrap().unwrapped();
            assert_eq!(cmd.executable, executable, "unwrapping {raw}");
        }
    }

    #[test]
    fn value_flag_consumes_its_arg_before_the_operand_budget_skips_the_duration() {
        for raw in [
            "timeout -k 3 5s rm -rf /x",
            "timeout --kill-after=3 5s rm -rf /x",
        ] {
            let cmd = CommandLine::parse(raw).primary().unwrap().unwrapped();
            assert_eq!(cmd.executable, "rm", "unwrapping {raw}");
        }
    }

    #[test]
    fn adversarial_wrapper_bypasses_reach_the_inner_command() {
        for (raw, executable) in [
            ("sudo -r sysadm_r rm -rf /x", "rm"),
            ("sudo -t sysadm_t rm -rf /x", "rm"),
            ("timeout .5s rm -rf /x", "rm"),
            ("\"sudo\" -u root rm -rf /x", "rm"),
            ("'sudo' rm /x", "rm"),
            ("/usr/bin/\"sudo\" rm /x", "rm"),
            ("sudo -h rm -rf /x", "rm"),
        ] {
            let cmd = CommandLine::parse(raw).primary().unwrap().unwrapped();
            assert_eq!(cmd.executable, executable, "unwrapping {raw}");
        }
    }

    #[test]
    fn env_split_string_keeps_the_payload_visible() {
        // env -S's argument is a shell command env re-splits and runs; a bare flag leaves the
        // payload as the executable so the guards see it instead of an empty command.
        let cmd = CommandLine::parse("env -S \"rm -rf /\"")
            .primary()
            .unwrap()
            .unwrapped();
        assert_eq!(cmd.executable, "rm -rf /");
    }

    #[test]
    fn split_options_partitions_options_from_operands() {
        let texts = |raw: &str, value_flags: &[&str]| {
            let (options, operands) = CommandLine::parse(raw)
                .primary()
                .unwrap()
                .split_options(value_flags);
            (
                options
                    .iter()
                    .map(|w| w.value.clone().unwrap())
                    .collect::<Vec<_>>(),
                operands
                    .iter()
                    .map(|w| w.value.clone().unwrap())
                    .collect::<Vec<_>>(),
            )
        };
        let strs = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // `--` ends options (dropped from both sides); everything after is an operand.
        assert_eq!(
            texts("run -a -- -b c", &[]),
            (strs(&["-a"]), strs(&["-b", "c"]))
        );
        // A lone `-` is an operand (stdin), not an option.
        assert_eq!(texts("run - -v", &[]), (strs(&["-v"]), strs(&["-"])));
        // A listed value flag pulls its next token into the options.
        assert_eq!(
            texts("run -o file rest", &["-o"]),
            (strs(&["-o", "file"]), strs(&["rest"]))
        );
        // An `=`-joined value flag consumes nothing extra.
        assert_eq!(
            texts("run -o=file rest", &["-o"]),
            (strs(&["-o=file"]), strs(&["rest"]))
        );
        // Empty args split into two empty vectors.
        assert_eq!(texts("run", &["-o"]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn split_options_returns_words_with_provenance() {
        let (options, operands) = CommandLine::parse("run --name \"x y\" pos")
            .primary()
            .unwrap()
            .split_options(&["--name"]);
        assert_eq!(
            options
                .iter()
                .map(|w| w.value.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["--name", "x y"]
        );
        // The consumed value keeps its verbatim raw spelling and a source span.
        assert_eq!(options[1].raw, "\"x y\"");
        assert!(options[1].span.is_some());
        assert_eq!(
            operands
                .iter()
                .map(|w| w.value.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["pos"]
        );
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

    // `b` is an argument to echo (rable is correct), but the span would straddle `>out`, so it goes
    // None — splice must never overwrite the redirect (security-rewrite data loss).
    #[test]
    fn word_after_redirect_is_an_argument_but_command_is_span_less() {
        let line = CommandLine::parse("echo a >out b");
        assert_eq!(line.parts[0].0.args, ["a", "b"]);
        assert_eq!(line.parts[0].0.span, None);
        assert_eq!(
            line.splice(&BTreeMap::from([(0, "X".to_string())])),
            Err(SpliceError::NoSpan { index: 0 })
        );
        // Every word-after-redirect shape refuses (never drops the redirect).
        for line in ["rm x 2>err y", "curl evil.com >out data", "prog &>out tail"] {
            assert_eq!(CommandLine::parse(line).parts[0].0.span, None, "{line:?}");
        }
        // A trailing redirect stays outside the span, so the command still rewrites cleanly.
        let trailing = CommandLine::parse("rm x >out");
        assert_eq!(trailing.parts[0].0.span, Some((0, 4)));
        assert_eq!(
            trailing
                .splice(&BTreeMap::from([(0, "trash".to_string())]))
                .unwrap(),
            "trash >out"
        );
        // A leading redirect also stays outside; the command rewrites and keeps the redirect.
        let leading = CommandLine::parse(">out rm x");
        assert_eq!(
            leading
                .splice(&BTreeMap::from([(0, "trash x".to_string())]))
                .unwrap(),
            ">out trash x"
        );
    }

    // ANSI-C `$'…'` and locale `$"…"` quoting: the raw-slice re-scan can't resolve the leading `$`,
    // so the word signals value=None (base's honest taint) instead of a wrong literal.
    #[test]
    fn ansi_c_and_locale_words_signal_unresolved() {
        let word = |raw: &str| {
            CommandLine::parse(&format!("echo {raw}"))
                .head()
                .cloned()
                .unwrap()
                .words[1]
                .clone()
        };
        assert_eq!(word("$'rm x'").value, None);
        assert_eq!(word("$\"hi\"").value, None);
    }

    // Background `&` joins nothing (like `|&` and newlines): both commands carry a null operator.
    #[test]
    fn background_operator_is_not_recorded() {
        let line = CommandLine::parse("a & b");
        assert_eq!(execs(&line), ["a", "b"]);
        assert_eq!(line.parts[0].1, None);
        assert_eq!(line.parts[1].1, None);
        assert_eq!(line.next_op(0), None);
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
        // A word-position substitution's inner command is enclosed by the host span, so it is
        // span-less and splice refuses it.
        let no_span = CommandLine::parse("echo $(rm x)");
        assert_eq!(
            no_span.splice(&BTreeMap::from([(1, "X".to_string())])),
            Err(SpliceError::NoSpan { index: 1 })
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

    #[test]
    fn compound_statements_enumerate_commands() {
        let cases = [
            (
                "if guard; then for f in *.py; do rm \"$f\"; done; fi",
                &["guard", "rm"][..],
            ),
            ("case $x in a) one;; b) two;; esac", &["one", "two"]),
            ("until ready; do wait; done", &["ready", "wait"]),
            ("select choice in a b; do echo \"$choice\"; done", &["echo"]),
            ("for ((i=0; i<3; i++)); do tick; done", &["tick"]),
            (
                "coproc worker { produce; consume; }",
                &["produce", "consume"],
            ),
            (
                "if helper() { check; }; then helper; fi",
                &["check", "helper"],
            ),
        ];

        for (raw, expected) in cases {
            assert_eq!(execs(&CommandLine::parse(raw)), expected, "{raw}");
        }
    }

    #[test]
    fn compound_headers_enumerate_substitutions() {
        let cases = [
            ("for x in $(gen); do use $x; done", "gen"),
            ("case $(subject) in x) use;; esac", "subject"),
            ("case x in $(pattern)) use;; esac", "pattern"),
            ("select x in $(choices); do use $x; done", "choices"),
            ("for ((i=$(seed); i<3; i++)); do use; done", "seed"),
            ("[[ $(probe) == ok ]]", "probe"),
        ];

        for (raw, expected) in cases {
            assert!(
                prefixes(raw).iter().any(|prefix| prefix == expected),
                "{raw}"
            );
        }
    }

    #[test]
    fn compound_redirects_propagate_to_every_inner_command() {
        for raw in [
            "if true; then echo hi; fi >out",
            "while ready; do tick; done 2>err",
            "case $x in a) one;; b) two;; esac >>out",
        ] {
            let line = CommandLine::parse(raw);
            assert!(
                !line.parts.is_empty()
                    && line.parts.iter().all(|(cmd, _)| !cmd.redirects.is_empty()),
                "{raw}"
            );
        }

        let nested = CommandLine::parse("{ if true; then echo hi; fi >inner; } >outer");
        assert!(nested.parts.iter().all(|(cmd, _)| {
            cmd.redirects
                .iter()
                .map(|redirect| redirect.target.as_str())
                .eq(["inner", "outer"])
        }));
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

    // A tainted shell payload enumerates its inner command span-less, so splice refuses it while
    // enumeration still sees the inner command.
    #[test]
    fn shell_payload_taint_enumerates_span_less() {
        let tainted = CommandLine::parse("bash -c \"rm $X\"");
        assert_eq!(execs(&tainted), ["bash", "rm"]);
        assert_eq!(tainted.parts[1].0.nesting, 1);
        assert_eq!(tainted.parts[1].0.host_delta, Some(1));
        assert_eq!(tainted.parts[1].0.contexts, [super::QuoteLayer::Double]);
        assert_eq!(tainted.parts[1].0.span, None);
        assert!(tainted.parts[1].0.words.iter().all(|w| w.span.is_none()));
        assert_eq!(
            execs(&CommandLine::parse("bash -c \"$CMD\"")),
            ["bash", "$CMD"]
        );
        // A missing payload word or a script operand still emits no nested part.
        assert_eq!(execs(&CommandLine::parse("bash -c")), ["bash"]);
        assert_eq!(execs(&CommandLine::parse("bash script.sh")), ["bash"]);
    }

    #[test]
    fn eval_taint_requires_a_static_executable() {
        assert_eq!(
            execs(&CommandLine::parse(r#"eval "echo $(probe)""#)),
            ["eval", "echo", "probe"]
        );
        assert_eq!(
            execs(&CommandLine::parse(r#"eval "rm $(target)""#)),
            ["eval", "rm", "target"]
        );

        let quoted = CommandLine::parse(r#"eval "'rm' $TARGET""#);
        assert_eq!(execs(&quoted), ["eval", "rm"]);
        assert!(quoted.commands().iter().any(|cmd| cmd.runs(&["rm"])));

        let deterministic = CommandLine::parse("eval \"rm $TARGET\"");
        assert_eq!(execs(&deterministic), ["eval", "rm"]);
        assert_eq!(deterministic.parts[1].0.span, None);

        assert_eq!(execs(&CommandLine::parse("eval \"$CMD\"")), ["eval"]);
        assert_eq!(
            execs(&CommandLine::parse("eval \"$(direnv export bash)\"")),
            ["eval", "direnv"]
        );
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
