//! The one renderer, ported from `cc_transcript/render.py` — every cut happens here,
//! under a [`Budget`]. Parity notes on the individual helpers.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::activity::Turn;
#[cfg(feature = "command")]
use crate::facts::{McpServerSummary, ToolFact};
use crate::filter::{entry_text, event_kind};
use crate::ids::encode_string;
use crate::pystr;
use crate::toolcall::{parse_tool_call, ToolCall};
use crate::types::{
    joined_text, ApiError, AssistantEntry, AttachmentDetail, Attribution, CacheCreation,
    CompactBoundary, ContentBlock, Entry, EntryMeta, HookInfo, ModelRefusalFallback,
    PreservedMessages, PreservedSegment, ServerToolUse, StopHookSummary, SystemDetail,
    ToolResultBlock, TurnDuration, Usage, UserEntry,
};
use crate::value::field;

pub const PRIMARY_KEYS: [&str; 8] = [
    "file_path",
    "path",
    "command",
    "pattern",
    "url",
    "prompt",
    "query",
    "description",
];
const SIZE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
const BLANK_TIME: &str = "        ";
const TIME_FMT: &str = "%H:%M:%S";
const SPAN_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Render-time character budgets — the only place the platform cuts content
/// (render.py Budget). `usize::MAX` stands in for Python's `sys.maxsize`.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub turn_chars: usize,
    pub tool_chars: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            turn_chars: 700,
            tool_chars: 1500,
        }
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

// The first `n` code points of `s` — Python `s[:n]`.
fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// Python `" ".join(text.split())`, over pystr's Python whitespace set.
fn collapse_ws(text: &str) -> String {
    pystr::split_whitespace(text).collect::<Vec<_>>().join(" ")
}

// Python str.splitlines(): its boundary set, dropping the terminator and yielding no
// trailing empty segment.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !matches!(
            c,
            '\n' | '\r'
                | '\u{0B}'
                | '\u{0C}'
                | '\u{1C}'
                | '\u{1D}'
                | '\u{1E}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            continue;
        }
        lines.push(&s[start..i]);
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                start = j + 1;
                continue;
            }
        }
        start = i + c.len_utf8();
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Collapse whitespace, cut to `width - 1` code points plus ellipsis; width == 0 means
/// no cut (render.py truncate).
pub fn truncate(text: &str, width: usize) -> String {
    let collapsed = collapse_ws(text);
    if width == 0 || char_len(&collapsed) <= width {
        return collapsed;
    }
    let mut out = char_prefix(&collapsed, width - 1).to_string();
    out.push('…');
    out
}

// render.py clip: cut to `limit` code points plus an `…(+Nch)` overflow marker.
pub(crate) fn clip(text: &str, limit: usize) -> String {
    let n = char_len(text);
    if n <= limit {
        return text.to_string();
    }
    format!("{}…(+{}ch)", char_prefix(text, limit), n - limit)
}

/// `1023` → `"1023B"`, `1024` → `"1.0KB"` (render.py human_size).
pub fn human_size(n: u64) -> String {
    for (i, unit) in SIZE_UNITS.iter().enumerate() {
        if i == SIZE_UNITS.len() - 1 || n < 1024u64.pow(i as u32 + 1) {
            return if i == 0 {
                format!("{n}{unit}")
            } else {
                format!("{:.1}{}", n as f64 / 1024f64.powi(i as i32), unit)
            };
        }
    }
    unreachable!("SIZE_UNITS ends open-ended")
}

// orjson.dumps mirror: compact, insertion-ordered keys, raw UTF-8, Python number format.
fn write_orjson(value: &Value, out: &mut String) {
    match value.get_type() {
        JsonType::Null => out.push_str("null"),
        JsonType::Boolean => out.push_str(if value.as_bool().unwrap() {
            "true"
        } else {
            "false"
        }),
        JsonType::Number => out.push_str(&format_number(value, false)),
        JsonType::String => encode_string(value.as_str().unwrap(), out),
        JsonType::Array => {
            out.push('[');
            for (i, item) in value.as_array().unwrap().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_orjson(item, out);
            }
            out.push(']');
        }
        JsonType::Object => {
            out.push('{');
            for (i, (key, item)) in crate::value::deduped_pairs(value.as_object().unwrap())
                .into_iter()
                .enumerate()
            {
                if i > 0 {
                    out.push(',');
                }
                encode_string(key, out);
                out.push(':');
                write_orjson(item, out);
            }
            out.push('}');
        }
    }
}

fn orjson_dumps(value: &Value) -> String {
    let mut out = String::new();
    write_orjson(value, &mut out);
    out
}

// str()/repr() of a parsed JSON number: int -> canonical decimal (signed zero -> "0"),
// float -> the layout `pad_exp` selects (true = Python repr, false = orjson).
fn format_number(value: &Value, pad_exp: bool) -> String {
    if let Some(raw) = value.as_raw_number() {
        let lexeme = raw.as_str();
        return if lexeme.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
            py_float_repr(
                lexeme.parse::<f64>().expect("JSON float lexeme parses"),
                pad_exp,
            )
        } else {
            py_int(lexeme)
        };
    }
    if let Some(int) = value.as_i64() {
        return int.to_string();
    }
    if let Some(uint) = value.as_u64() {
        return uint.to_string();
    }
    py_float_repr(
        value.as_f64().expect("a Number Value is i64, u64, or f64"),
        pad_exp,
    )
}

// str(int) over a JSON integer lexeme: JSON forbids leading zeros and '+', so only a
// signed zero ("-0") needs normalizing.
fn py_int(lexeme: &str) -> String {
    if lexeme.trim_start_matches('-').bytes().all(|b| b == b'0') {
        "0".to_string()
    } else {
        lexeme.to_string()
    }
}

// pad_exp=true: Python repr(float) layout. false: installed-orjson layout (fixed
// notation through exponent -5, bare exponent, nonfinite -> null; orjson 3.11.9).
fn py_float_repr(value: f64, pad_exp: bool) -> String {
    if !value.is_finite() {
        return match (pad_exp, value.is_nan(), value.is_sign_negative()) {
            (false, _, _) => "null".to_string(),
            (true, true, _) => "nan".to_string(),
            (true, false, false) => "inf".to_string(),
            (true, false, true) => "-inf".to_string(),
        };
    }
    let sci_floor = if pad_exp { -5 } else { -6 };
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let (digits, exp) = shortest_digits(value.abs());
    let n = digits.len() as i64;
    if exp <= sci_floor || exp >= 16 {
        let mant = if n > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.clone()
        };
        let exp_str = if pad_exp {
            format!("{exp:+03}")
        } else {
            format!("{exp:+}")
        };
        format!("{sign}{mant}e{exp_str}")
    } else {
        let decpt = exp + 1;
        let body = if decpt <= 0 {
            format!("0.{}{digits}", "0".repeat((-decpt) as usize))
        } else if decpt >= n {
            format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
        } else {
            format!(
                "{}.{}",
                &digits[..decpt as usize],
                &digits[decpt as usize..]
            )
        };
        format!("{sign}{body}")
    }
}

// Shortest round-trip digits of a non-negative finite f64 and its base-10 exponent E
// (value = d.ddd x 10^E), from ryu-js — which breaks shortest ties like Python, unlike std.
fn shortest_digits(value: f64) -> (String, i64) {
    let es = ryu_js::Buffer::new().format_finite(value).to_string();
    let (mantissa, exp) = match es.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().expect("exponent parses")),
        None => (es.as_str(), 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let raw = format!("{int_part}{frac_part}");
    let point = int_part.len() as i64 + exp;
    match raw.find(|c| c != '0') {
        None => ("0".to_string(), 0),
        Some(first) => {
            let sig = raw[first..].trim_end_matches('0');
            (
                if sig.is_empty() {
                    "0".to_string()
                } else {
                    sig.to_string()
                },
                point - 1 - first as i64,
            )
        }
    }
}

/// Python ``str()`` of a JSON value: a str verbatim, else its ``repr()``. The single
/// owner of JSON→Python-string coercion (notifications queue content reuses it).
pub(crate) fn py_str(value: &Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => py_repr(value),
    }
}

fn py_repr(value: &Value) -> String {
    match value.get_type() {
        JsonType::Null => "None".to_string(),
        JsonType::Boolean => if value.as_bool().unwrap() {
            "True"
        } else {
            "False"
        }
        .to_string(),
        JsonType::Number => format_number(value, true),
        JsonType::String => py_string_repr(value.as_str().unwrap()),
        JsonType::Array => {
            let inner = value
                .as_array()
                .unwrap()
                .iter()
                .map(py_repr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        JsonType::Object => {
            let inner = value
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{}: {}", py_string_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
    }
}

// Python repr() of a str: prefer single quotes (double only when the string has a single
// quote and no double), escape the quote/backslash/\n\r\t, \x/\u/\U-escape non-printables.
fn py_string_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(quote);
            }
            c if is_py_printable(c) => out.push(c),
            c => match c as u32 {
                cp if cp <= 0xff => out.push_str(&format!("\\x{cp:02x}")),
                cp if cp <= 0xffff => out.push_str(&format!("\\u{cp:04x}")),
                cp => out.push_str(&format!("\\U{cp:08x}")),
            },
        }
    }
    out.push(quote);
    out
}

// Python str.isprintable(): false for the Unicode Other (C*) / Separator (Z*) code points,
// except ASCII space. Covers Cc + the Z separators + the common Cf format chars.
fn is_py_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !matches!(c as u32,
        0x00..=0x1f | 0x7f..=0x9f
        | 0xa0 | 0xad | 0x1680
        | 0x2000..=0x200f
        | 0x2028 | 0x2029 | 0x202a..=0x202f
        | 0x205f | 0x2060..=0x206f
        | 0x3000 | 0xfeff)
}

fn line_budget(width: usize) -> Budget {
    let w = if width == 0 { usize::MAX } else { width };
    Budget {
        turn_chars: w,
        tool_chars: w,
    }
}

fn prefixed(prefix: &str, text: &str) -> Vec<String> {
    let lines: Vec<String> = py_splitlines(text)
        .into_iter()
        .map(|line| format!("{prefix}{line}"))
        .collect();
    if lines.is_empty() {
        vec![pystr::rstrip(prefix).to_string()]
    } else {
        lines
    }
}

fn hunk_lines(old: &str, new: &str, budget: &Budget) -> Vec<String> {
    let mut lines = prefixed("- ", &clip(old, budget.tool_chars));
    lines.extend(prefixed("+ ", &clip(new, budget.tool_chars)));
    lines
}

fn primary_arg(raw: &Value) -> String {
    let Some(obj) = raw.as_object() else {
        return String::new();
    };
    for key in PRIMARY_KEYS {
        if let Some(v) = field(raw, key) {
            return py_str(v);
        }
    }
    match obj.iter().next() {
        Some((_, v)) => py_str(v),
        None => String::new(),
    }
}

/// Render a typed tool call, clipping each content piece to the tool budget
/// (render.py render_tool_call).
pub fn render_tool_call(call: &ToolCall, budget: &Budget) -> String {
    match call {
        ToolCall::Bash(c) => clip(&c.command, budget.tool_chars),
        ToolCall::Edit(c) => {
            let mut lines = vec![format!("Edit {}", c.file_path)];
            lines.extend(hunk_lines(&c.old, &c.new, budget));
            lines.join("\n")
        }
        ToolCall::MultiEdit(c) => {
            let n = c.edits.len();
            let mut lines = vec![format!("MultiEdit {}", c.file_path)];
            for (i, span) in c.edits.iter().enumerate() {
                lines.push(format!("edit {}/{}", i + 1, n));
                lines.extend(hunk_lines(&span.old, &span.new, budget));
            }
            lines.join("\n")
        }
        ToolCall::Write(c) => format!(
            "Write {}\n{}",
            c.file_path,
            clip(&c.content, budget.tool_chars)
        ),
        _ => format!(
            "{}({})",
            call.name(),
            clip(&primary_arg(call.raw()), budget.tool_chars)
        ),
    }
}

/// Render one turn — the prompt, assistant prose, and every tool call, in order
/// (render.py render_turn). Prose clips to `budget.turn_chars`; each tool call
/// renders via [`render_tool_call`] under `budget.tool_chars`.
pub fn render_turn(turn: &Turn, budget: &Budget) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !turn.prompt.is_empty() {
        parts.push(format!("user: {}", clip(&turn.prompt, budget.turn_chars)));
    }
    for event in turn.events {
        let Entry::Assistant(assistant) = event else {
            continue;
        };
        for block in &assistant.blocks {
            match block {
                ContentBlock::Text(text) if !pystr::strip(text).is_empty() => {
                    parts.push(format!("assistant: {}", clip(text, budget.turn_chars)));
                }
                ContentBlock::ToolUse(tool_use) => {
                    parts.push(render_tool_call(
                        &parse_tool_call(&tool_use.name, &tool_use.input),
                        budget,
                    ));
                }
                _ => {}
            }
        }
    }
    parts.join("\n")
}

/// One compact `index tag time payload [uuid]` line (render.py compact_line).
pub fn compact_line(
    index: usize,
    event: &Entry,
    names: &HashMap<&str, &str>,
    width: usize,
    thinking: bool,
    uuids: bool,
) -> String {
    let meta = event.meta();
    let sidechain = meta.is_some_and(|m| m.is_sidechain);
    let tag = format!(
        "{}{}",
        tag_for(event_kind(event)),
        if sidechain { "*" } else { "" }
    );
    let time = match meta {
        Some(m) => m.timestamp.format(TIME_FMT).to_string(),
        None => BLANK_TIME.to_string(),
    };
    let payload = event_payload(event, names, width, thinking);
    let line = pystr::rstrip(&format!("{index:>5} {tag:<5} {time} {payload}")).to_string();
    match (uuids, meta) {
        (true, Some(m)) => format!("{line} {}", m.uuid),
        _ => line,
    }
}

fn tag_for(kind: &str) -> &'static str {
    match kind {
        "user" => "user",
        "assistant" => "asst",
        "system" => "sys",
        "mode" => "mode",
        "other" => "other",
        "attachment" => "att",
        _ => unreachable!("event_kind yields one of the six tags"),
    }
}

/// The one-line payload of an event, without index/tag/time (render.py event_payload).
pub fn event_payload(
    event: &Entry,
    names: &HashMap<&str, &str>,
    width: usize,
    thinking: bool,
) -> String {
    match event {
        Entry::User(u) => user_payload(u, names, width),
        Entry::Assistant(a) => assistant_payload(a, width, thinking),
        Entry::System(s) => match &s.content {
            Some(c) if !c.is_empty() => format!("{}: {}", s.subtype, truncate(c, width)),
            _ => s.subtype.clone(),
        },
        Entry::Mode(m) => format!("{}={}", m.channel.as_str(), m.value),
        Entry::Other(o) => o.ty.clone(),
        Entry::Attachment(a) => pystr::strip(&format!(
            "{} {}",
            a.attachment_type,
            truncate(&attachment_text(&a.detail), width)
        ))
        .to_string(),
    }
}

fn attachment_text(detail: &AttachmentDetail) -> String {
    // Python `a or b or c or ""`: the first non-empty, treating None and "" alike.
    let or_empty = |opts: &[&Option<String>]| -> String {
        opts.iter()
            .find_map(|o| o.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or("")
            .to_string()
    };
    match detail {
        AttachmentDetail::HookSuccess(h) => or_empty(&[&h.content, &h.stdout, &h.command]),
        AttachmentDetail::HookNonBlockingError(h) => or_empty(&[&h.stderr, &h.stdout, &h.command]),
        AttachmentDetail::HookBlockingError(h) => match &h.blocking_error {
            Some(v) if truthy(v) => orjson_dumps(v),
            _ => String::new(),
        },
        AttachmentDetail::HookCancelled(h) => or_empty(&[&h.command]),
        AttachmentDetail::HookAdditionalContext(h) => h.content.join(" "),
        AttachmentDetail::AsyncHookResponse(h) => or_empty(&[&h.stdout, &h.stderr]),
        AttachmentDetail::QueuedCommand(q) => or_empty(&[&q.prompt]),
        AttachmentDetail::Other(raw) => orjson_dumps(raw),
    }
}

// Python truthiness of a JSON value (the `if blocking_error` guard).
fn truthy(v: &Value) -> bool {
    match v.get_type() {
        JsonType::Null => false,
        JsonType::Boolean => v.as_bool().unwrap(),
        JsonType::Number => v.as_f64().map(|f| f != 0.0).unwrap_or(true),
        JsonType::String => !v.as_str().unwrap().is_empty(),
        JsonType::Array => !v.as_array().unwrap().is_empty(),
        JsonType::Object => v.as_object().unwrap().iter().next().is_some(),
    }
}

// Python UserEvent.interrupted: the text marker OR a present interruptedMessageId
// (parser.py); the core `interrupted()` method covers only the text marker.
fn user_interrupted(u: &UserEntry) -> bool {
    u.interrupted() || u.interrupted_message_id.is_some()
}

fn user_payload(u: &UserEntry, names: &HashMap<&str, &str>, width: usize) -> String {
    let text = u.content.text();
    let head = if user_interrupted(u) {
        pystr::strip(&format!("[int] {}", truncate(&text, width))).to_string()
    } else {
        truncate(&text, width)
    };
    let mut parts: Vec<String> = Vec::new();
    if !head.is_empty() {
        parts.push(head);
    }
    for block in u.blocks() {
        if let ContentBlock::ToolResult(tr) = block {
            let part = result_payload(tr, names, width);
            if !part.is_empty() {
                parts.push(part);
            }
        }
    }
    parts.join(" ")
}

fn result_payload(tr: &ToolResultBlock, names: &HashMap<&str, &str>, width: usize) -> String {
    let name = names.get(tr.tool_use_id.as_str()).copied().unwrap_or("?");
    let err = if tr.is_error { "[err]" } else { "" };
    pystr::rstrip(&format!(
        "<-{name}{err} ({}ch) {}",
        char_len(&tr.content),
        truncate(&tr.content, width)
    ))
    .to_string()
}

fn assistant_payload(a: &AssistantEntry, width: usize, thinking: bool) -> String {
    let mut parts = vec![format!("[{}]", a.model)];
    for block in &a.blocks {
        let part = block_payload(block, width, thinking);
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join(" ")
}

fn block_payload(block: &ContentBlock, width: usize, thinking: bool) -> String {
    match block {
        ContentBlock::Text(t) if !pystr::strip(t).is_empty() => {
            format!("\"{}\"", truncate(t, width))
        }
        ContentBlock::Text(_) => String::new(),
        ContentBlock::Thinking(thought) if thinking => {
            format!("th({}ch) {}", char_len(thought), truncate(thought, width))
        }
        ContentBlock::Thinking(thought) => format!("th({}ch)", char_len(thought)),
        ContentBlock::ToolUse(tu) => collapse_ws(&render_tool_call(
            &parse_tool_call(&tu.name, &tu.input),
            &line_budget(width),
        )),
        ContentBlock::ToolResult(_) => String::new(),
        ContentBlock::Fallback(f) => format!("fallback {}->{}", f.from_model, f.to_model),
        ContentBlock::Other { ty, .. } => ty.clone(),
    }
}

/// The searchable text of an event, scoped to the requested areas (render.py haystack).
pub fn haystack(
    event: &Entry,
    where_text: bool,
    where_thinking: bool,
    where_tools: bool,
) -> String {
    match event {
        Entry::User(_) | Entry::Assistant(_) => {
            let mut parts: Vec<String> = Vec::new();
            if where_text {
                parts.push(entry_text(event));
            }
            if where_thinking {
                parts.extend(event.blocks().iter().filter_map(|b| match b {
                    ContentBlock::Thinking(t) => Some(t.clone()),
                    _ => None,
                }));
            }
            if where_tools {
                parts.extend(event.blocks().iter().filter_map(|b| match b {
                    ContentBlock::ToolUse(_) | ContentBlock::ToolResult(_) => {
                        Some(tool_haystack(b))
                    }
                    _ => None,
                }));
            }
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        Entry::System(s) if where_text => match &s.content {
            Some(c) if !c.is_empty() => format!("{}: {c}", s.subtype),
            _ => s.subtype.clone(),
        },
        Entry::Mode(m) if where_text => format!("{}={}", m.channel.as_str(), m.value),
        Entry::Other(o) if where_text => o.ty.clone(),
        Entry::Attachment(a) if where_text => pystr::strip(&format!(
            "{} {}",
            a.attachment_type,
            attachment_text(&a.detail)
        ))
        .to_string(),
        _ => String::new(),
    }
}

fn tool_haystack(block: &ContentBlock) -> String {
    match block {
        ContentBlock::ToolUse(tu) => format!("{} {}", tu.name, orjson_dumps(&tu.input)),
        ContentBlock::ToolResult(tr) => tr.content.clone(),
        _ => unreachable!("tool_haystack is called only on tool blocks"),
    }
}

/// Corpus-wide statistics over parsed transcripts (render.py Stats).
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub files: u64,
    pub events: u64,
    pub kinds: Vec<(String, u64)>,
    pub models: Vec<(String, u64)>,
    pub tools: Vec<(String, u64)>,
    pub text_chars: u64,
    pub thinking_chars: u64,
    pub tool_io_chars: u64,
    pub sessions: u64,
    pub first_timestamp: Option<DateTime<FixedOffset>>,
    pub last_timestamp: Option<DateTime<FixedOffset>>,
    pub interrupts: u64,
    pub tool_errors: u64,
    pub sidechain: u64,
}

// Insertion-ordered counter; most_common sorts by count descending with ties keeping
// first-seen order, matching collections.Counter.most_common().
#[derive(Default)]
struct Counter {
    order: Vec<String>,
    counts: HashMap<String, u64>,
}

impl Counter {
    fn add(&mut self, key: &str) {
        match self.counts.get_mut(key) {
            Some(count) => *count += 1,
            None => {
                self.order.push(key.to_string());
                self.counts.insert(key.to_string(), 1);
            }
        }
    }

    fn most_common(&self) -> Vec<(String, u64)> {
        let mut items: Vec<(String, u64)> = self
            .order
            .iter()
            .map(|k| (k.clone(), self.counts[k]))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1));
        items
    }
}

/// Aggregate [`Stats`] over parsed transcripts (render.py collect_stats).
pub fn collect_stats(transcripts: &[Vec<Entry>]) -> Stats {
    let mut kinds = Counter::default();
    let mut models = Counter::default();
    let mut tools = Counter::default();
    let mut sessions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut files = 0u64;
    let mut events = 0u64;
    let mut text_chars = 0u64;
    let mut thinking_chars = 0u64;
    let mut tool_io_chars = 0u64;
    let mut interrupts = 0u64;
    let mut tool_errors = 0u64;
    let mut sidechain = 0u64;
    let mut first: Option<DateTime<FixedOffset>> = None;
    let mut last: Option<DateTime<FixedOffset>> = None;
    for entries in transcripts {
        files += 1;
        for event in entries {
            events += 1;
            kinds.add(event_kind(event));
            text_chars += char_len(&entry_text(event)) as u64;
            if let Some(meta) = event.meta() {
                sessions.insert(meta.session_id.clone());
                sidechain += meta.is_sidechain as u64;
                first = Some(match first {
                    Some(f) if f <= meta.timestamp => f,
                    _ => meta.timestamp,
                });
                last = Some(match last {
                    Some(l) if l >= meta.timestamp => l,
                    _ => meta.timestamp,
                });
            }
            match event {
                Entry::User(u) => {
                    interrupts += user_interrupted(u) as u64;
                    for block in u.blocks() {
                        if let ContentBlock::ToolResult(tr) = block {
                            tool_io_chars += char_len(&tr.content) as u64;
                            tool_errors += tr.is_error as u64;
                        }
                    }
                }
                Entry::Assistant(a) => {
                    models.add(&a.model);
                    for block in &a.blocks {
                        match block {
                            ContentBlock::Thinking(thought) => {
                                thinking_chars += char_len(thought) as u64
                            }
                            ContentBlock::ToolUse(tu) => {
                                tools.add(&tu.name);
                                tool_io_chars += orjson_dumps(&tu.input).len() as u64;
                            }
                            _ => {}
                        }
                    }
                }
                Entry::Mode(m) => {
                    sessions.insert(m.session_id.clone());
                }
                _ => {}
            }
        }
    }
    Stats {
        files,
        events,
        kinds: kinds.most_common(),
        models: models.most_common(),
        tools: tools.most_common(),
        text_chars,
        thinking_chars,
        tool_io_chars,
        sessions: sessions.len() as u64,
        first_timestamp: first,
        last_timestamp: last,
        interrupts,
        tool_errors,
        sidechain,
    }
}

/// The aligned multi-line statistics block (render.py render_stats).
pub fn render_stats(stats: &Stats) -> String {
    let rows: [(&str, String); 13] = [
        ("files", stats.files.to_string()),
        ("events", stats.events.to_string()),
        ("kinds", render_histogram(&stats.kinds)),
        ("models", render_histogram(&stats.models)),
        ("tools", render_histogram(&stats.tools)),
        ("text", human_size(stats.text_chars)),
        ("thinking", human_size(stats.thinking_chars)),
        ("tool io", human_size(stats.tool_io_chars)),
        ("sessions", stats.sessions.to_string()),
        (
            "span",
            render_span(stats.first_timestamp, stats.last_timestamp),
        ),
        ("interrupts", stats.interrupts.to_string()),
        ("tool errors", stats.tool_errors.to_string()),
        ("sidechain", stats.sidechain.to_string()),
    ];
    rows.iter()
        .map(|(label, value)| format!("{label:<12} {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_span(
    first: Option<DateTime<FixedOffset>>,
    last: Option<DateTime<FixedOffset>>,
) -> String {
    match (first, last) {
        (Some(f), Some(l)) => format!("{} → {}", f.format(SPAN_FMT), l.format(SPAN_FMT)),
        _ => "-".to_string(),
    }
}

fn render_histogram(counts: &[(String, u64)]) -> String {
    if counts.is_empty() {
        return "-".to_string();
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Insertion-ordered JSON for the CLI's orjson-parity emission — the Rust twin of the
/// dict projections render.py hands to `orjson.dumps`.
#[derive(Debug, Clone)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// An integer lexeme beyond 64 bits, kept verbatim (Python ints are unbounded).
    RawNum(String),
    Str(String),
    Datetime(DateTime<FixedOffset>),
    Value(Value),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn dumps(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// orjson `OPT_INDENT_2` layout: two-space indent, `": "` separators, empty
    /// containers inline.
    pub fn dumps_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, depth: usize) {
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&"  ".repeat(depth + 1));
                    item.write_pretty(out, depth + 1);
                }
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
                out.push(']');
            }
            Json::Obj(pairs) if !pairs.is_empty() => {
                out.push_str("{\n");
                for (i, (key, item)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    out.push_str(&"  ".repeat(depth + 1));
                    encode_string(key, out);
                    out.push_str(": ");
                    item.write_pretty(out, depth + 1);
                }
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
                out.push('}');
            }
            Json::Value(v) => write_orjson_pretty(v, out, depth),
            other => other.write(out),
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::UInt(u) => out.push_str(&u.to_string()),
            Json::Float(f) => out.push_str(&py_float_repr(*f, false)),
            Json::RawNum(lexeme) => out.push_str(lexeme),
            Json::Str(s) => encode_string(s, out),
            Json::Datetime(dt) => encode_string(&format_datetime(dt), out),
            Json::Value(v) => write_orjson(v, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (key, item)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode_string(key, out);
                    out.push(':');
                    item.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_orjson_pretty(value: &Value, out: &mut String, depth: usize) {
    match value.get_type() {
        JsonType::Array if !value.as_array().unwrap().is_empty() => {
            out.push_str("[\n");
            for (i, item) in value.as_array().unwrap().iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&"  ".repeat(depth + 1));
                write_orjson_pretty(item, out, depth + 1);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push(']');
        }
        JsonType::Object if value.as_object().unwrap().iter().next().is_some() => {
            out.push_str("{\n");
            for (i, (key, item)) in crate::value::deduped_pairs(value.as_object().unwrap())
                .into_iter()
                .enumerate()
            {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&"  ".repeat(depth + 1));
                encode_string(key, out);
                out.push_str(": ");
                write_orjson_pretty(item, out, depth + 1);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push('}');
        }
        _ => write_orjson(value, out),
    }
}

/// Python `json.loads`: strict JSON plus the NaN/Infinity/-Infinity literals, duplicate
/// object keys collapsing last-wins at the first slot (the corrections detail contract).
pub fn pyjson_loads(text: &str) -> Result<Json, String> {
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    let value = pyjson_value(bytes, &mut pos)?;
    pyjson_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err(format!("trailing data at position {pos}"));
    }
    Ok(value)
}

fn pyjson_ws(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn pyjson_expect(bytes: &[u8], pos: &mut usize, token: &str) -> Result<(), String> {
    if bytes[*pos..].starts_with(token.as_bytes()) {
        *pos += token.len();
        Ok(())
    } else {
        Err(format!("expected {token} at position {pos}"))
    }
}

fn pyjson_value(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    pyjson_ws(bytes, pos);
    match bytes.get(*pos) {
        None => Err("unexpected end of input".to_string()),
        Some(b'{') => {
            *pos += 1;
            let mut order: Vec<String> = Vec::new();
            let mut pairs: Vec<(String, Json)> = Vec::new();
            pyjson_ws(bytes, pos);
            if bytes.get(*pos) == Some(&b'}') {
                *pos += 1;
                return Ok(Json::Obj(pairs));
            }
            loop {
                pyjson_ws(bytes, pos);
                let key = match pyjson_value(bytes, pos)? {
                    Json::Str(key) => key,
                    _ => return Err(format!("expected a string key at position {pos}")),
                };
                pyjson_ws(bytes, pos);
                pyjson_expect(bytes, pos, ":")?;
                let value = pyjson_value(bytes, pos)?;
                match order.iter().position(|seen| *seen == key) {
                    Some(index) => pairs[index].1 = value,
                    None => {
                        order.push(key.clone());
                        pairs.push((key, value));
                    }
                }
                pyjson_ws(bytes, pos);
                match bytes.get(*pos) {
                    Some(b',') => *pos += 1,
                    Some(b'}') => {
                        *pos += 1;
                        return Ok(Json::Obj(pairs));
                    }
                    _ => return Err(format!("expected ',' or '}}' at position {pos}")),
                }
            }
        }
        Some(b'[') => {
            *pos += 1;
            let mut items: Vec<Json> = Vec::new();
            pyjson_ws(bytes, pos);
            if bytes.get(*pos) == Some(&b']') {
                *pos += 1;
                return Ok(Json::Arr(items));
            }
            loop {
                items.push(pyjson_value(bytes, pos)?);
                pyjson_ws(bytes, pos);
                match bytes.get(*pos) {
                    Some(b',') => *pos += 1,
                    Some(b']') => {
                        *pos += 1;
                        return Ok(Json::Arr(items));
                    }
                    _ => return Err(format!("expected ',' or ']' at position {pos}")),
                }
            }
        }
        Some(b'"') => pyjson_string(bytes, pos).map(Json::Str),
        Some(b't') => pyjson_expect(bytes, pos, "true").map(|()| Json::Bool(true)),
        Some(b'f') => pyjson_expect(bytes, pos, "false").map(|()| Json::Bool(false)),
        Some(b'n') => pyjson_expect(bytes, pos, "null").map(|()| Json::Null),
        Some(b'N') => pyjson_expect(bytes, pos, "NaN").map(|()| Json::Float(f64::NAN)),
        Some(b'I') => pyjson_expect(bytes, pos, "Infinity").map(|()| Json::Float(f64::INFINITY)),
        Some(b'-') if bytes[*pos..].starts_with(b"-Infinity") => {
            *pos += "-Infinity".len();
            Ok(Json::Float(f64::NEG_INFINITY))
        }
        Some(b'-' | b'0'..=b'9') => pyjson_number(bytes, pos),
        Some(other) => Err(format!("unexpected byte {other:#04x} at position {pos}")),
    }
}

fn pyjson_string(bytes: &[u8], pos: &mut usize) -> Result<String, String> {
    *pos += 1;
    let mut out = String::new();
    loop {
        match bytes.get(*pos) {
            None => return Err("unterminated string".to_string()),
            Some(b'"') => {
                *pos += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *pos += 1;
                match bytes.get(*pos) {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{08}'),
                    Some(b'f') => out.push('\u{0c}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let unit = pyjson_hex4(bytes, *pos + 1)?;
                        *pos += 4;
                        let code = if (0xD800..0xDC00).contains(&unit) {
                            if bytes.get(*pos + 1..*pos + 3).is_some_and(|s| s == b"\\u") {
                                let low = pyjson_hex4(bytes, *pos + 3)?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err("unpaired surrogate escape".to_string());
                                }
                                *pos += 6;
                                0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                return Err("lone surrogate escape".to_string());
                            }
                        } else if (0xDC00..0xE000).contains(&unit) {
                            return Err("lone surrogate escape".to_string());
                        } else {
                            unit
                        };
                        out.push(char::from_u32(code).ok_or("invalid escape code point")?);
                    }
                    _ => return Err(format!("invalid escape at position {pos}")),
                }
                *pos += 1;
            }
            Some(raw) if *raw < 0x20 => {
                // json.loads strict mode: raw control characters are invalid in strings.
                return Err(format!("invalid control character at position {pos}"));
            }
            Some(_) => {
                let rest = std::str::from_utf8(&bytes[*pos..]).map_err(|e| e.to_string())?;
                let c = rest.chars().next().expect("non-empty remainder");
                out.push(c);
                *pos += c.len_utf8();
            }
        }
    }
}

fn pyjson_hex4(bytes: &[u8], at: usize) -> Result<u32, String> {
    let hex = bytes
        .get(at..at + 4)
        .and_then(|s| std::str::from_utf8(s).ok())
        .ok_or("truncated \\u escape")?;
    u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
}

// The JSON number grammar json.loads enforces: -?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?
fn pyjson_number_lexeme(lexeme: &str) -> bool {
    let b = lexeme.strip_prefix('-').unwrap_or(lexeme).as_bytes();
    let mut i = match b.first() {
        Some(b'0') => 1,
        Some(b'1'..=b'9') => b.iter().take_while(|c| c.is_ascii_digit()).count(),
        _ => return false,
    };
    if b.get(i) == Some(&b'.') {
        let frac = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if frac == 0 {
            return false;
        }
        i += 1 + frac;
    }
    if let Some(b'e' | b'E') = b.get(i) {
        i += 1;
        if let Some(b'+' | b'-') = b.get(i) {
            i += 1;
        }
        let exp = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if exp == 0 {
            return false;
        }
        i += exp;
    }
    i == b.len()
}

fn pyjson_number(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    let start = *pos;
    if bytes.get(*pos) == Some(&b'-') {
        *pos += 1;
    }
    while bytes
        .get(*pos)
        .is_some_and(|b| matches!(b, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        *pos += 1;
    }
    let lexeme = std::str::from_utf8(&bytes[start..*pos]).map_err(|e| e.to_string())?;
    if !pyjson_number_lexeme(lexeme) {
        return Err(format!("invalid number {lexeme:?}"));
    }
    if lexeme.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        return lexeme
            .parse::<f64>()
            .map(Json::Float)
            .map_err(|e| e.to_string());
    }
    if let Ok(int) = lexeme.parse::<i64>() {
        return Ok(Json::Int(int));
    }
    Ok(Json::RawNum(py_int(lexeme)))
}

/// Python `json.dumps` with defaults — `", "`/`": "` separators, `ensure_ascii`
/// escaping, NaN/Infinity literals — the corrections detail storage byte contract.
pub fn pyjson_dumps(json: &Json) -> String {
    let mut out = String::new();
    write_pyjson(json, &mut out);
    out
}

fn write_pyjson(json: &Json, out: &mut String) {
    match json {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => out.push_str(&i.to_string()),
        Json::UInt(u) => out.push_str(&u.to_string()),
        Json::RawNum(lexeme) => out.push_str(lexeme),
        Json::Float(f) if f.is_nan() => out.push_str("NaN"),
        Json::Float(f) if f.is_infinite() && *f > 0.0 => out.push_str("Infinity"),
        Json::Float(f) if f.is_infinite() => out.push_str("-Infinity"),
        Json::Float(f) => out.push_str(&py_float_repr(*f, true)),
        Json::Str(s) => encode_string_ascii(s, out),
        Json::Datetime(_) | Json::Value(_) => {
            unreachable!("pyjson trees carry only parsed scalars and containers")
        }
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_pyjson(item, out);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            out.push('{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                encode_string_ascii(key, out);
                out.push_str(": ");
                write_pyjson(item, out);
            }
            out.push('}');
        }
    }
}

// json.dumps ensure_ascii: every non-ASCII code point becomes \uXXXX (surrogate pairs
// for astral), controls use the short escapes.
fn encode_string_ascii(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if c.is_ascii() => out.push(c),
            c if (c as u32) > 0xFFFF => {
                let v = c as u32 - 0x10000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xD800 + (v >> 10),
                    0xDC00 + (v & 0x3FF)
                ));
            }
            c => out.push_str(&format!("\\u{:04x}", c as u32)),
        }
    }
    out.push('"');
}

fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn opt_str(value: &Option<String>) -> Json {
    match value {
        Some(s) => Json::Str(s.clone()),
        None => Json::Null,
    }
}

fn opt_int(value: Option<i64>) -> Json {
    match value {
        Some(i) => Json::Int(i),
        None => Json::Null,
    }
}

fn opt_bool(value: Option<bool>) -> Json {
    match value {
        Some(b) => Json::Bool(b),
        None => Json::Null,
    }
}

fn opt_value(value: &Option<Value>) -> Json {
    match value {
        Some(v) => Json::Value(v.clone()),
        None => Json::Null,
    }
}

fn str_arr(items: &[String]) -> Json {
    Json::Arr(items.iter().map(|s| Json::Str(s.clone())).collect())
}

fn opt_json<T>(value: &Option<T>, f: impl Fn(&T) -> Json) -> Json {
    match value {
        Some(v) => f(v),
        None => Json::Null,
    }
}

// orjson datetime serialization: datetime.isoformat() — seconds always, microseconds
// only when nonzero, offset as ±HH:MM.
fn format_datetime(dt: &DateTime<FixedOffset>) -> String {
    use chrono::Timelike;
    let micros = dt.nanosecond() / 1_000;
    if micros == 0 {
        format!("{}{}", dt.format("%Y-%m-%dT%H:%M:%S"), dt.format("%:z"))
    } else {
        format!(
            "{}.{micros:06}{}",
            dt.format("%Y-%m-%dT%H:%M:%S"),
            dt.format("%:z")
        )
    }
}

/// `~`-relativize a path under `$HOME` (render.py display_path; `Path.home()` reads
/// the environment on POSIX).
pub fn display_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home)
            if !home.is_empty() && path.strip_prefix(&home).is_some_and(|r| r.starts_with('/')) =>
        {
            format!("~/{}", &path[home.len() + 1..])
        }
        _ => path.to_string(),
    }
}

/// The `== <path>` block header over grouped per-file output (render.py transcript_header).
pub fn transcript_header(path: &str) -> String {
    format!("== {}", display_path(path))
}

fn meta_json(meta: &EntryMeta) -> Json {
    obj(vec![
        ("uuid", Json::Str(meta.uuid.clone())),
        ("parent_uuid", opt_str(&meta.parent_uuid)),
        ("session_id", Json::Str(meta.session_id.clone())),
        ("timestamp", Json::Datetime(meta.timestamp)),
        ("cwd", opt_str(&meta.cwd)),
        ("git_branch", opt_str(&meta.git_branch)),
        ("cc_version", opt_str(&meta.version)),
        ("is_sidechain", Json::Bool(meta.is_sidechain)),
        ("is_meta", Json::Bool(meta.is_meta)),
        ("entrypoint", opt_str(&meta.entrypoint)),
        ("is_compact_summary", Json::Bool(meta.is_compact_summary)),
        (
            "is_visible_in_transcript_only",
            Json::Bool(meta.is_visible_in_transcript_only),
        ),
        ("user_type", opt_str(&meta.user_type)),
        ("slug", opt_str(&meta.slug)),
    ])
}

fn block_json(block: &ContentBlock) -> Json {
    match block {
        ContentBlock::Text(text) => obj(vec![("text", Json::Str(text.clone()))]),
        ContentBlock::Thinking(thinking) => obj(vec![("thinking", Json::Str(thinking.clone()))]),
        ContentBlock::ToolUse(tu) => obj(vec![
            ("id", Json::Str(tu.id.clone())),
            ("name", Json::Str(tu.name.clone())),
            ("input", Json::Value(tu.input.clone())),
        ]),
        ContentBlock::ToolResult(tr) => obj(vec![
            ("tool_use_id", Json::Str(tr.tool_use_id.clone())),
            ("content", Json::Str(tr.content.clone())),
            ("is_error", Json::Bool(tr.is_error)),
            ("is_async", Json::Bool(tr.is_async)),
            ("tool_use_result", opt_value(&tr.tool_use_result)),
            ("denial_kind", opt_str(&tr.denial_kind)),
        ]),
        ContentBlock::Fallback(f) => obj(vec![
            ("from_model", Json::Str(f.from_model.clone())),
            ("to_model", Json::Str(f.to_model.clone())),
        ]),
        ContentBlock::Other { ty, raw } => obj(vec![
            ("type", Json::Str(ty.clone())),
            ("raw", Json::Value(raw.clone())),
        ]),
    }
}

fn blocks_json(blocks: &[ContentBlock]) -> Json {
    Json::Arr(blocks.iter().map(block_json).collect())
}

// The user view's block projection: text blocks first, then tool results, nothing else.
fn user_blocks_json(blocks: &[ContentBlock]) -> Json {
    Json::Arr(
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Text(_)))
            .chain(
                blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult(_))),
            )
            .map(block_json)
            .collect(),
    )
}

fn cache_creation_json(c: &CacheCreation) -> Json {
    obj(vec![
        (
            "ephemeral_5m_input_tokens",
            Json::Int(c.ephemeral_5m_input_tokens),
        ),
        (
            "ephemeral_1h_input_tokens",
            Json::Int(c.ephemeral_1h_input_tokens),
        ),
    ])
}

fn server_tool_use_json(s: &ServerToolUse) -> Json {
    obj(vec![
        ("web_search_requests", Json::Int(s.web_search_requests)),
        ("web_fetch_requests", Json::Int(s.web_fetch_requests)),
    ])
}

fn usage_json(u: &Usage) -> Json {
    obj(vec![
        ("input_tokens", Json::Int(u.input_tokens)),
        ("output_tokens", Json::Int(u.output_tokens)),
        (
            "cache_read_input_tokens",
            Json::Int(u.cache_read_input_tokens),
        ),
        (
            "cache_creation_input_tokens",
            Json::Int(u.cache_creation_input_tokens),
        ),
        (
            "cache_creation",
            opt_json(&u.cache_creation, cache_creation_json),
        ),
        ("service_tier", opt_str(&u.service_tier)),
        ("inference_geo", opt_str(&u.inference_geo)),
        (
            "server_tool_use",
            opt_json(&u.server_tool_use, server_tool_use_json),
        ),
    ])
}

fn attribution_json(a: &Attribution) -> Json {
    obj(vec![
        ("plugin", opt_str(&a.plugin)),
        ("skill", opt_str(&a.skill)),
        ("mcp_server", opt_str(&a.mcp_server)),
        ("mcp_tool", opt_str(&a.mcp_tool)),
    ])
}

fn api_error_json(e: &ApiError) -> Json {
    obj(vec![
        ("error", opt_str(&e.error)),
        ("status", opt_int(e.status)),
        ("details", opt_str(&e.details)),
    ])
}

fn hook_info_json(h: &HookInfo) -> Json {
    obj(vec![
        ("command", Json::Str(h.command.clone())),
        ("duration_ms", opt_int(h.duration_ms)),
    ])
}

fn stop_hook_summary_json(s: &StopHookSummary) -> Json {
    obj(vec![
        ("hook_count", opt_int(s.hook_count)),
        (
            "hook_infos",
            Json::Arr(s.hook_infos.iter().map(hook_info_json).collect()),
        ),
        ("hook_errors", str_arr(&s.hook_errors)),
        (
            "hook_additional_context",
            str_arr(&s.hook_additional_context),
        ),
        (
            "prevented_continuation",
            Json::Bool(s.prevented_continuation),
        ),
        ("stop_reason", opt_str(&s.stop_reason)),
        ("has_output", Json::Bool(s.has_output)),
        ("tool_use_id", opt_str(&s.tool_use_id)),
    ])
}

fn preserved_segment_json(p: &PreservedSegment) -> Json {
    obj(vec![
        ("head_uuid", opt_str(&p.head_uuid)),
        ("anchor_uuid", opt_str(&p.anchor_uuid)),
        ("tail_uuid", opt_str(&p.tail_uuid)),
    ])
}

fn preserved_messages_json(p: &PreservedMessages) -> Json {
    obj(vec![
        ("anchor_uuid", opt_str(&p.anchor_uuid)),
        ("uuids", str_arr(&p.uuids)),
        ("all_uuids", str_arr(&p.all_uuids)),
    ])
}

fn compact_boundary_json(c: &CompactBoundary) -> Json {
    obj(vec![
        ("trigger", opt_str(&c.trigger)),
        ("pre_tokens", opt_int(c.pre_tokens)),
        ("post_tokens", opt_int(c.post_tokens)),
        ("duration_ms", opt_int(c.duration_ms)),
        (
            "cumulative_dropped_tokens",
            opt_int(c.cumulative_dropped_tokens),
        ),
        (
            "pre_compact_discovered_tools",
            str_arr(&c.pre_compact_discovered_tools),
        ),
        (
            "preserved_segment",
            opt_json(&c.preserved_segment, preserved_segment_json),
        ),
        (
            "preserved_messages",
            opt_json(&c.preserved_messages, preserved_messages_json),
        ),
        ("logical_parent_uuid", opt_str(&c.logical_parent_uuid)),
        ("precomputed", opt_bool(c.precomputed)),
    ])
}

fn turn_duration_json(t: &TurnDuration) -> Json {
    obj(vec![
        ("duration_ms", opt_int(t.duration_ms)),
        ("message_count", opt_int(t.message_count)),
        ("pending_workflow_count", opt_int(t.pending_workflow_count)),
        (
            "pending_background_agent_count",
            opt_int(t.pending_background_agent_count),
        ),
    ])
}

fn model_refusal_fallback_json(m: &ModelRefusalFallback) -> Json {
    obj(vec![
        ("api_refusal_category", opt_str(&m.api_refusal_category)),
        (
            "api_refusal_explanation",
            opt_str(&m.api_refusal_explanation),
        ),
        ("trigger", opt_str(&m.trigger)),
        ("direction", opt_str(&m.direction)),
        ("original_model", opt_str(&m.original_model)),
        ("fallback_model", opt_str(&m.fallback_model)),
        (
            "retracted_message_uuids",
            str_arr(&m.retracted_message_uuids),
        ),
        (
            "refused_user_message_uuid",
            opt_str(&m.refused_user_message_uuid),
        ),
    ])
}

fn system_detail_json(detail: &SystemDetail) -> Json {
    match detail {
        SystemDetail::StopHookSummary(s) => stop_hook_summary_json(s),
        SystemDetail::CompactBoundary(c) => compact_boundary_json(c),
        SystemDetail::TurnDuration(t) => turn_duration_json(t),
        SystemDetail::ModelRefusalFallback(m) => model_refusal_fallback_json(m),
        SystemDetail::Other(raw) => obj(vec![("raw", Json::Value(raw.clone()))]),
    }
}

fn attachment_detail_json(detail: &AttachmentDetail) -> Json {
    match detail {
        AttachmentDetail::HookSuccess(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("tool_use_id", opt_str(&h.tool_use_id)),
            ("command", opt_str(&h.command)),
            ("content", opt_str(&h.content)),
            ("stdout", opt_str(&h.stdout)),
            ("stderr", opt_str(&h.stderr)),
            ("exit_code", opt_int(h.exit_code)),
            ("duration_ms", opt_int(h.duration_ms)),
        ]),
        AttachmentDetail::HookBlockingError(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("tool_use_id", opt_str(&h.tool_use_id)),
            ("blocking_error", opt_value(&h.blocking_error)),
        ]),
        AttachmentDetail::HookNonBlockingError(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("tool_use_id", opt_str(&h.tool_use_id)),
            ("command", opt_str(&h.command)),
            ("stdout", opt_str(&h.stdout)),
            ("stderr", opt_str(&h.stderr)),
            ("exit_code", opt_int(h.exit_code)),
            ("duration_ms", opt_int(h.duration_ms)),
        ]),
        AttachmentDetail::HookCancelled(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("tool_use_id", opt_str(&h.tool_use_id)),
            ("command", opt_str(&h.command)),
            ("duration_ms", opt_int(h.duration_ms)),
            ("timed_out", opt_bool(h.timed_out)),
            ("timeout_ms", opt_int(h.timeout_ms)),
        ]),
        AttachmentDetail::HookAdditionalContext(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("tool_use_id", opt_str(&h.tool_use_id)),
            ("content", str_arr(&h.content)),
        ]),
        AttachmentDetail::AsyncHookResponse(h) => obj(vec![
            ("hook_name", opt_str(&h.hook_name)),
            ("hook_event", opt_str(&h.hook_event)),
            ("process_id", opt_str(&h.process_id)),
            ("stdout", opt_str(&h.stdout)),
            ("stderr", opt_str(&h.stderr)),
            ("exit_code", opt_int(h.exit_code)),
            ("response", opt_value(&h.response)),
        ]),
        AttachmentDetail::QueuedCommand(q) => obj(vec![
            ("prompt", opt_str(&q.prompt)),
            ("command_mode", opt_str(&q.command_mode)),
        ]),
        AttachmentDetail::Other(raw) => obj(vec![("raw", Json::Value(raw.clone()))]),
    }
}

/// One event as the CLI's JSON projection, fields in the typed views' `__match_args__`
/// order (render.py event_dict over view_asdict).
pub fn event_json(index: usize, event: &Entry) -> Json {
    let mut pairs = vec![
        ("i", Json::UInt(index as u64)),
        ("kind", Json::Str(event_kind(event).to_string())),
    ];
    match event {
        Entry::User(u) => pairs.extend(vec![
            ("meta", meta_json(&u.meta)),
            ("text", Json::Str(u.content.text())),
            ("blocks", user_blocks_json(u.blocks())),
            ("interrupted", Json::Bool(u.interrupted())),
            ("is_agent_injected", Json::Bool(u.is_agent_injected())),
            ("prompt_id", opt_str(&u.prompt_id)),
            ("prompt_source", opt_str(&u.prompt_source)),
            ("queue_priority", opt_str(&u.queue_priority)),
            (
                "image_paste_ids",
                opt_json(&u.image_paste_ids, |ids| {
                    Json::Arr(ids.iter().map(|i| Json::Int(*i)).collect())
                }),
            ),
            ("source_tool_use_id", opt_str(&u.source_tool_use_id)),
            (
                "source_tool_assistant_uuid",
                opt_str(&u.source_tool_assistant_uuid),
            ),
            ("mcp_meta", opt_value(&u.mcp_meta)),
            ("permission_mode", opt_str(&u.permission_mode)),
            ("interrupted_message_id", opt_str(&u.interrupted_message_id)),
        ]),
        Entry::Assistant(a) => pairs.extend(vec![
            ("meta", meta_json(&a.meta)),
            ("model", Json::Str(a.model.clone())),
            ("text", Json::Str(joined_text(&a.blocks))),
            ("blocks", blocks_json(&a.blocks)),
            ("stop_reason", opt_str(&a.stop_reason)),
            ("usage", opt_json(&a.usage, usage_json)),
            ("request_id", opt_str(&a.request_id)),
            ("forked_from", opt_str(&a.forked_from)),
            ("attribution", opt_json(&a.attribution, attribution_json)),
            ("api_error", opt_json(&a.api_error, api_error_json)),
        ]),
        Entry::System(s) => pairs.extend(vec![
            ("meta", meta_json(&s.meta)),
            ("subtype", Json::Str(s.subtype.clone())),
            ("content", opt_str(&s.content)),
            ("level", opt_str(&s.level)),
            ("detail", system_detail_json(&s.detail)),
        ]),
        Entry::Mode(m) => pairs.extend(vec![
            ("session_id", Json::Str(m.session_id.clone())),
            ("channel", Json::Str(m.channel.as_str().to_string())),
            ("value", Json::Str(m.value.clone())),
        ]),
        Entry::Other(o) => pairs.extend(vec![
            ("type", Json::Str(o.ty.clone())),
            ("raw", Json::Value(o.raw.clone())),
        ]),
        Entry::Attachment(a) => pairs.extend(vec![
            ("meta", meta_json(&a.meta)),
            ("attachment_type", Json::Str(a.attachment_type.clone())),
            ("detail", attachment_detail_json(&a.detail)),
        ]),
    }
    obj(pairs)
}

fn counts_obj(counts: &[(String, u64)]) -> Json {
    Json::Obj(
        counts
            .iter()
            .map(|(k, v)| (k.clone(), Json::UInt(*v)))
            .collect(),
    )
}

/// [`Stats`] as the CLI's JSON projection (render.py stats_dict via dataclasses.asdict).
pub fn stats_json(stats: &Stats) -> Json {
    obj(vec![
        ("files", Json::UInt(stats.files)),
        ("events", Json::UInt(stats.events)),
        ("kinds", counts_obj(&stats.kinds)),
        ("models", counts_obj(&stats.models)),
        ("tools", counts_obj(&stats.tools)),
        ("text_chars", Json::UInt(stats.text_chars)),
        ("thinking_chars", Json::UInt(stats.thinking_chars)),
        ("tool_io_chars", Json::UInt(stats.tool_io_chars)),
        ("sessions", Json::UInt(stats.sessions)),
        (
            "first_timestamp",
            opt_json(&stats.first_timestamp, |dt| Json::Datetime(*dt)),
        ),
        (
            "last_timestamp",
            opt_json(&stats.last_timestamp, |dt| Json::Datetime(*dt)),
        ),
        ("interrupts", Json::UInt(stats.interrupts)),
        ("tool_errors", Json::UInt(stats.tool_errors)),
        ("sidechain", Json::UInt(stats.sidechain)),
    ])
}

/// Right-aligned `  count  name` rows, most frequent first with alphabetic ties
/// (render.py render_counts).
pub fn render_counts(counts: &[(String, usize)]) -> Vec<String> {
    let width = counts
        .iter()
        .map(|(_, count)| count.to_string().len())
        .max()
        .unwrap_or(1);
    let mut sorted: Vec<&(String, usize)> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
        .iter()
        .map(|(name, count)| format!("  {count:>width$}  {name}"))
        .collect()
}

#[cfg(feature = "command")]
fn usize_histogram(counts: &[(String, usize)]) -> String {
    if counts.is_empty() {
        return "-".to_string();
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// One aligned row per MCP server (render.py render_mcp).
#[cfg(feature = "command")]
pub fn render_mcp(summary: &[(String, McpServerSummary)]) -> Vec<String> {
    let width = summary
        .iter()
        .map(|(server, _)| server.chars().count())
        .max()
        .unwrap_or(0);
    summary
        .iter()
        .map(|(server, data)| {
            format!(
                "{server:<width$}  read {} · write {} · total {}  {}",
                data.read,
                data.write,
                data.total,
                usize_histogram(&data.tools[..data.tools.len().min(5)])
            )
        })
        .collect()
}

/// One tool fact as the CLI's JSON projection (render.py fact_dict).
#[cfg(feature = "command")]
pub fn fact_json(fact: &ToolFact) -> Json {
    obj(vec![
        ("ts", Json::Datetime(fact.ts)),
        ("session_id", Json::Str(fact.session_id.clone())),
        ("path", Json::Str(fact.path.clone())),
        ("tool_use_id", Json::Str(fact.tool_use_id.clone())),
        ("tool", Json::Str(fact.tool.clone())),
        ("command_prefixes", str_arr(&fact.command_prefixes)),
        ("command", opt_str(&fact.command)),
        ("mcp_server", opt_str(&fact.mcp_server)),
        ("mcp_tool", opt_str(&fact.mcp_tool)),
        ("mcp_access", opt_str(&fact.mcp_access)),
        ("file_path", opt_str(&fact.file_path)),
        ("is_error", Json::Bool(fact.is_error)),
        ("denied", Json::Bool(fact.denied)),
        ("denial_kind", opt_str(&fact.denial_kind)),
        ("user_said", opt_str(&fact.user_said)),
        ("duration_ms", opt_int(fact.duration_ms)),
    ])
}

/// One compact `ts session tool` line per fact (render.py fact_line).
#[cfg(feature = "command")]
pub fn fact_line(fact: &ToolFact) -> String {
    let ts = fact.ts.format(SPAN_FMT);
    let name = match (&fact.mcp_server, &fact.mcp_tool) {
        (Some(server), Some(tool)) => format!("{server}/{tool}"),
        _ => fact.tool.clone(),
    };
    let prefixes = if fact.command_prefixes.is_empty() {
        String::new()
    } else {
        format!(" {}", fact.command_prefixes.join(","))
    };
    let marker = if fact.denied {
        " [denied]"
    } else if fact.is_error {
        " [err]"
    } else {
        ""
    };
    format!(
        "{ts} {} {name}{prefixes}{marker}",
        char_prefix(&fact.session_id, 8)
    )
}

/// One denial as the CLI's JSON projection (render.py denial_dict).
#[cfg(feature = "command")]
pub fn denial_json(fact: &ToolFact) -> Json {
    obj(vec![
        ("ts", Json::Datetime(fact.ts)),
        ("session", Json::Str(fact.session_id.clone())),
        ("path", Json::Str(fact.path.clone())),
        ("tool", Json::Str(fact.tool.clone())),
        ("command", opt_str(&fact.command)),
        ("file_path", opt_str(&fact.file_path)),
        ("denial_kind", opt_str(&fact.denial_kind)),
        ("user_said", opt_str(&fact.user_said)),
    ])
}

/// `tool target → user_said` (render.py denial_line).
#[cfg(feature = "command")]
pub fn denial_line(fact: &ToolFact) -> String {
    let target = fact
        .command
        .as_deref()
        .filter(|c| !c.is_empty())
        .or(fact.file_path.as_deref().filter(|f| !f.is_empty()));
    let head = match target {
        Some(target) => format!("{} {target}", fact.tool),
        None => fact.tool.clone(),
    };
    match fact.user_said.as_deref().filter(|u| !u.is_empty()) {
        Some(user_said) => format!("{head} → {user_said}"),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_collapses_then_cuts_by_code_point() {
        assert_eq!(truncate("a  b\n\tc", 100), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("a    bcdef", 4), "a b…");
        assert_eq!(truncate("ab   cd", 0), "ab cd");
        assert_eq!(truncate("abcd", 4), "abcd");
        // é and 🤖 each count as one code point, so the cut lands on a char boundary.
        assert_eq!(truncate("héllo 🤖 漢字 end", 5), "héll…");
        assert_eq!(char_len("héllo 🤖 漢字 end"), 14);
    }

    #[test]
    fn clip_marks_overflow_in_code_points() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc…(+3ch)");
        assert_eq!(clip("héllo🤖", 3), "hél…(+3ch)");
    }

    #[test]
    fn json_dumps_matches_orjson_layout() {
        let dt: DateTime<FixedOffset> = "2026-01-25T09:01:03.739+00:00".parse().unwrap();
        let whole: DateTime<FixedOffset> = "2026-01-06T09:01:06+00:00".parse().unwrap();
        let json = Json::Obj(vec![
            ("ts".into(), Json::Datetime(dt)),
            ("whole".into(), Json::Datetime(whole)),
            ("mtime".into(), Json::Float(1700072000.0)),
            ("n".into(), Json::Int(-3)),
            ("s".into(), Json::Str("é\"".into())),
            ("none".into(), Json::Null),
            ("arr".into(), Json::Arr(vec![Json::Bool(true)])),
        ]);
        assert_eq!(
            json.dumps(),
            "{\"ts\":\"2026-01-25T09:01:03.739000+00:00\",\"whole\":\"2026-01-06T09:01:06+00:00\",\
             \"mtime\":1700072000.0,\"n\":-3,\"s\":\"é\\\"\",\"none\":null,\"arr\":[true]}"
        );
    }

    #[test]
    fn orjson_layout_keeps_exponent_minus_five_fixed() {
        // orjson 3.11.9: fixed through E == -5 (repr says 1e-05), scientific from E <= -6.
        for (value, expected) in [
            (0.0001, "0.0001"),
            (0.00001, "0.00001"),
            (0.0000999, "0.0000999"),
            (-0.00001, "-0.00001"),
            (1e-6, "1e-6"),
            (1.5e-6, "1.5e-6"),
            (1e-7, "1e-7"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (5e-324, "5e-324"),
        ] {
            assert_eq!(py_float_repr(value, false), expected, "{value}");
        }
        assert_eq!(py_float_repr(0.00001, true), "1e-05");
    }

    #[test]
    fn nonfinite_projects_as_null_in_orjson_layout_and_text_in_repr() {
        let parsed: Value = sonic_rs::from_str(r#"{"big":1e400,"neg":-1e400}"#).unwrap();
        assert_eq!(orjson_dumps(&parsed), r#"{"big":null,"neg":null}"#);
        assert_eq!(py_float_repr(f64::NAN, true), "nan");
        assert_eq!(py_float_repr(f64::INFINITY, true), "inf");
        assert_eq!(py_float_repr(f64::NEG_INFINITY, true), "-inf");
        assert_eq!(Json::Float(f64::NAN).dumps(), "null");
    }

    #[test]
    fn pyjson_round_trips_the_detail_contract() {
        // Expected bytes pinned from CPython json.dumps defaults.
        let parsed =
            pyjson_loads(r#"{"n": NaN, "i": Infinity, "ni": -Infinity, "a": 1, "a": 2}"#).unwrap();
        assert_eq!(
            pyjson_dumps(&parsed),
            r#"{"n": NaN, "i": Infinity, "ni": -Infinity, "a": 2}"#
        );
        let unicode = pyjson_loads(r#"{"k": "éé 🤖 x"}"#).unwrap();
        assert_eq!(
            pyjson_dumps(&unicode),
            r#"{"k": "\u00e9\u00e9 \ud83e\udd16 x"}"#
        );
        let nested = pyjson_loads(r#"[true, null, 1.5, 1e-5, 18446744073709551616, "s"]"#).unwrap();
        assert_eq!(
            pyjson_dumps(&nested),
            r#"[true, null, 1.5, 1e-05, 18446744073709551616, "s"]"#
        );
        assert_eq!(pyjson_dumps(&pyjson_loads("1e400").unwrap()), "Infinity");
        assert!(pyjson_loads(r#"{"k": "\ud800"}"#).is_err());
        assert!(pyjson_loads("{").is_err());
        assert!(pyjson_loads("[1,]").is_err());
    }

    #[test]
    fn pyjson_rejects_what_python_json_rejects() {
        // json.loads strictness: number grammar and raw control characters.
        for invalid in [
            "01",
            "-01",
            "1.",
            ".5",
            "+1",
            "1.e5",
            "1e",
            "1e+",
            "--1",
            "1-",
            "0x1",
            "nan",
            "-NaN",
            "\"\u{01}\"",
            "\"\t\"",
        ] {
            assert!(pyjson_loads(invalid).is_err(), "{invalid:?}");
        }
        for (valid, dumped) in [
            ("-0", "0"),
            ("0.5", "0.5"),
            ("1E5", "100000.0"),
            ("1e+5", "100000.0"),
            ("-0.0", "-0.0"),
            (" 7\t", "7"),
        ] {
            assert_eq!(
                pyjson_dumps(&pyjson_loads(valid).unwrap()),
                dumped,
                "{valid:?}"
            );
        }
    }

    #[test]
    fn oversized_integers_render_verbatim() {
        // Improvement over orjson, which raised on >64-bit ints; pinned deliberately.
        let parsed: Value =
            sonic_rs::from_str(r#"{"a":18446744073709551616,"b":-9223372036854775809}"#).unwrap();
        assert_eq!(
            orjson_dumps(&parsed),
            r#"{"a":18446744073709551616,"b":-9223372036854775809}"#
        );
    }

    #[test]
    fn duplicate_keys_collapse_last_wins_first_position() {
        let parsed: Value = sonic_rs::from_str(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(orjson_dumps(&parsed), r#"{"a":3,"b":2}"#);
        let mut pretty = String::new();
        write_orjson_pretty(&parsed, &mut pretty, 0);
        assert_eq!(pretty, "{\n  \"a\": 3,\n  \"b\": 2\n}");
    }

    #[test]
    fn json_dumps_pretty_matches_orjson_indent2() {
        let json = Json::Arr(vec![Json::Obj(vec![
            ("tool".into(), Json::Str("Bash".into())),
            ("empty".into(), Json::Obj(vec![])),
            ("items".into(), Json::Arr(vec![Json::Int(1), Json::Int(2)])),
        ])]);
        assert_eq!(
            json.dumps_pretty(),
            "[\n  {\n    \"tool\": \"Bash\",\n    \"empty\": {},\n    \"items\": [\n      1,\n      2\n    ]\n  }\n]"
        );
    }

    #[test]
    fn display_path_relativizes_under_home() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        assert_eq!(
            display_path(&format!("{home}/x/y.jsonl")),
            "~/x/y.jsonl".to_string()
        );
        assert_eq!(display_path("/elsewhere/y.jsonl"), "/elsewhere/y.jsonl");
        assert_eq!(display_path(".fixtures/corpus"), ".fixtures/corpus");
        assert_eq!(
            display_path(&format!("{home}extra/y.jsonl")),
            format!("{home}extra/y.jsonl")
        );
    }

    #[test]
    fn render_counts_sorts_desc_then_alpha() {
        let counts = vec![
            ("b".to_string(), 2usize),
            ("a".to_string(), 2usize),
            ("c".to_string(), 10usize),
        ];
        assert_eq!(
            render_counts(&counts),
            vec!["  10  c", "   2  a", "   2  b"]
        );
    }

    #[test]
    fn human_size_crosses_unit_boundaries() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0KB");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn py_splitlines_matches_python() {
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("a"), vec!["a"]);
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("\n"), vec![""]);
    }

    #[test]
    fn collapse_ws_uses_python_whitespace() {
        assert_eq!(collapse_ws("  a\t b \n c  "), "a b c");
        // U+001F is Python whitespace but not Unicode White_Space.
        assert_eq!(collapse_ws("a\u{1f}b"), "a b");
    }

    #[test]
    fn py_string_repr_selects_quotes() {
        assert_eq!(py_string_repr("plain"), "'plain'");
        assert_eq!(py_string_repr("it's"), "\"it's\"");
        assert_eq!(py_string_repr("say \"hi\""), "'say \"hi\"'");
        assert_eq!(py_string_repr("both ' and \""), "'both \\' and \"'");
    }

    #[test]
    fn py_string_repr_escapes_non_printables() {
        assert_eq!(py_string_repr("z\u{01}w"), "'z\\x01w'");
        assert_eq!(py_string_repr("a\u{85}b"), "'a\\x85b'");
        assert_eq!(py_string_repr("x\u{2028}y"), "'x\\u2028y'");
        assert_eq!(py_string_repr("\u{a0}"), "'\\xa0'");
        // Printable code points (ASCII, accented, astral emoji) stay verbatim.
        assert_eq!(py_string_repr("é 漢 😀"), "'é 漢 😀'");
    }

    #[test]
    fn orjson_dumps_is_compact_and_ordered() {
        let value = sonic_rs::from_str(r#"{"b":1,"a":"x","n":null,"f":1.5}"#).unwrap();
        assert_eq!(orjson_dumps(&value), r#"{"b":1,"a":"x","n":null,"f":1.5}"#);
    }

    fn num(lexeme: &str) -> Value {
        sonic_rs::from_str(lexeme).unwrap()
    }

    #[test]
    fn numbers_format_like_python_str_not_raw_lexeme() {
        // str()/repr() layout (padded exponent), the primary_arg path.
        assert_eq!(format_number(&num("1e-7"), true), "1e-07");
        assert_eq!(format_number(&num("1e16"), true), "1e+16");
        assert_eq!(format_number(&num("1e15"), true), "1000000000000000.0");
        assert_eq!(format_number(&num("-0.5"), true), "-0.5");
        assert_eq!(format_number(&num("0.1"), true), "0.1");
        assert_eq!(format_number(&num("1.5"), true), "1.5");
        assert_eq!(format_number(&num("123.456"), true), "123.456");
        // Signed zero and big ints via parsed-value semantics.
        assert_eq!(format_number(&num("-0"), true), "0");
        assert_eq!(
            format_number(&num("99999999999999999999"), true),
            "99999999999999999999"
        );
        // ryu-js breaks the shortest tie like Python (Rust std would give ...293).
        assert_eq!(
            format_number(&num("698957826421429.2"), true),
            "698957826421429.2"
        );
        // orjson layout omits the exponent zero-pad.
        assert_eq!(format_number(&num("1e-7"), false), "1e-7");
        assert_eq!(format_number(&num("1e16"), false), "1e+16");
    }
}
