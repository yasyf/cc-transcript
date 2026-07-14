//! The one renderer, ported from `cc_transcript/render.py` — every cut happens here,
//! under a [`Budget`]. Parity notes on the individual helpers.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::activity::Turn;
use crate::filter::{entry_text, event_kind};
use crate::ids::encode_string;
use crate::pystr;
use crate::toolcall::{parse_tool_call, ToolCall};
use crate::types::{
    AssistantEntry, AttachmentDetail, ContentBlock, Entry, ToolResultBlock, UserEntry,
};
use crate::value::field_last;

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

// render.py truncate: collapse whitespace, cut to `width - 1` code points plus ellipsis;
// width == 0 means no cut.
fn truncate(text: &str, width: usize) -> String {
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

fn human_size(n: u64) -> String {
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

// Python dict from a JSON object: first-seen key position, last value (last-wins dedup),
// so the mirror matches a dict materialized by json.loads.
fn object_entries(value: &Value) -> Vec<(&str, &Value)> {
    let mut order: Vec<&str> = Vec::new();
    let mut last: HashMap<&str, &Value> = HashMap::new();
    for (key, item) in value.as_object().unwrap().iter() {
        if last.insert(key, item).is_none() {
            order.push(key);
        }
    }
    order.into_iter().map(|key| (key, last[key])).collect()
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
            for (i, (key, item)) in object_entries(value).into_iter().enumerate() {
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
// float -> shortest Python-layout repr; pad_exp two-pads the exponent (1e-07 vs orjson 1e-7).
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

// Python repr(float): shortest digits in Python's layout — scientific when the base-10
// exponent E is <= -5 or >= 16, else fixed with a mandatory fractional part.
fn py_float_repr(value: f64, pad_exp: bool) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let (digits, exp) = shortest_digits(value.abs());
    let n = digits.len() as i64;
    if exp <= -5 || exp >= 16 {
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

// Python str() of a JSON value: a str verbatim, else its repr().
fn py_str(value: &Value) -> String {
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
            let inner = object_entries(value)
                .into_iter()
                .map(|(k, v)| format!("{}: {}", py_string_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
    }
}

// Python repr() of a str over the printable-ASCII + basic-escape range (the primary-arg
// shapes a transcript carries); non-ASCII non-printables are left verbatim.
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
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
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
    if raw.as_object().is_none() {
        return String::new();
    }
    for key in PRIMARY_KEYS {
        if let Some(v) = field_last(raw, key) {
            return py_str(v);
        }
    }
    match object_entries(raw).first() {
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

fn event_payload(
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
    fn orjson_dumps_is_compact_and_ordered() {
        let value = sonic_rs::from_str(r#"{"b":1,"a":"x","n":null,"f":1.5}"#).unwrap();
        assert_eq!(orjson_dumps(&value), r#"{"b":1,"a":"x","n":null,"f":1.5}"#);
    }

    #[test]
    fn duplicate_keys_resolve_last_wins_first_position() {
        // Python dict from json.loads: the key keeps its first position, its last value.
        let value = sonic_rs::from_str(r#"{"b":1,"a":2,"b":3}"#).unwrap();
        assert_eq!(orjson_dumps(&value), r#"{"b":3,"a":2}"#);
        assert_eq!(py_repr(&value), "{'b': 3, 'a': 2}");
        // primary_arg over a duplicated PRIMARY_KEY takes the last value.
        assert_eq!(
            primary_arg(
                &sonic_rs::from_str(r#"{"file_path":"/first","file_path":"/last"}"#).unwrap()
            ),
            "/last"
        );
        // The fallback first-value is the first key's last value.
        assert_eq!(
            primary_arg(&sonic_rs::from_str(r#"{"z":1,"y":2,"z":3}"#).unwrap()),
            "3"
        );
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
