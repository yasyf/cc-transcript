//! Durable context windows, ported from `cc_transcript/context.py`: refs plus
//! capture-time previews, serialized byte-stably as `cc-transcript.context/2`.
//! Hydration resolves refs back through `SessionActivity.from_session` (disk +
//! async), so it stays in the Python layer; the core owns capture, the previews,
//! and the wire format.

use std::collections::BTreeMap;

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::activity::{LiftedSession, ToolUse, Turn};
use crate::ids::{encode_string, tool_digest, EventRef};
use crate::parse::parse_questions;
use crate::render::{clip, render_tool_call, render_turn, Budget};
use crate::toolcall::{parse_tool_call, parse_tool_result, ToolResult};
use crate::types::{ContentBlock, Entry, Question, ToolUseBlock};
use crate::value::{field, field_bool, field_str};

pub const SCHEMA: &str = "cc-transcript.context/2";
pub const PREVIEW_SCHEMA: &str = "cc-transcript.preview/1";
pub const SUMMARY_LABEL: &str = "[summary fidelity — transcript unavailable]";
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// One structured preview part of a turn (context.py `Preview`): typed text, a tool
/// call reduced to name + digest + summary, or an AskUserQuestion round with its
/// rounds and answers — the shape cc-steer consumes instead of scraping a repr.
#[derive(Debug, Clone, PartialEq)]
pub enum Preview {
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        digest: String,
        summary: String,
    },
    AskUserQuestion {
        questions: Vec<PreviewQuestion>,
        selections: BTreeMap<String, String>,
        notes: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewQuestion {
    pub question: String,
    pub header: Option<String>,
    pub multi_select: bool,
    pub labels: Vec<String>,
}

impl From<Question> for PreviewQuestion {
    fn from(q: Question) -> Self {
        PreviewQuestion {
            question: q.question,
            header: q.header,
            multi_select: q.multi_select,
            labels: q.labels,
        }
    }
}

#[derive(Debug)]
pub struct SchemaError(pub String);

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    Full,
    Summary,
}

impl Fidelity {
    pub fn as_str(self) -> &'static str {
        match self {
            Fidelity::Full => "full",
            Fidelity::Summary => "summary",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Fidelity::Full),
            "summary" => Some(Fidelity::Summary),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnRef {
    pub role: Role,
    pub refs: Vec<EventRef>,
    pub preview: String,
    pub tool_digests: Vec<String>,
    /// The typed previews (`cc-transcript.preview/1`), or None for a legacy window
    /// persisted before typed previews existed.
    pub previews: Option<Vec<Preview>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextWindow {
    pub anchor: EventRef,
    pub before: Vec<TurnRef>,
    pub trigger: Option<TurnRef>,
    pub after: Vec<TurnRef>,
    pub fidelity: Fidelity,
    pub preview_chars: i64,
    /// Whether the turn refs carry typed previews — the `cc-transcript.preview/1`
    /// marker. False for a legacy window (previews absent).
    pub preview_schema: bool,
}

impl ContextWindow {
    /// Render the persisted previews, never touching the transcript
    /// (context.py `ContextWindow.render_preview`). Summary-fidelity windows lead
    /// with the [`SUMMARY_LABEL`].
    pub fn render_preview(&self, turn_chars: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.fidelity == Fidelity::Summary {
            parts.push(SUMMARY_LABEL.to_string());
        }
        parts.extend(
            self.window_refs()
                .filter(|tr| !tr.preview.is_empty())
                .map(|tr| clip(&tr.preview, turn_chars)),
        );
        parts.join("\n\n")
    }

    fn window_refs(&self) -> impl Iterator<Item = &TurnRef> {
        self.before
            .iter()
            .chain(self.trigger.as_ref())
            .chain(self.after.iter())
    }

    /// Serialize to the `cc-transcript.context/2` wire schema, byte-stably —
    /// `json.dumps(..., ensure_ascii=False, separators=(",", ":"), sort_keys=True)`.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"after\":");
        push_turn_refs(&self.after, &mut out);
        out.push_str(",\"anchor\":");
        push_ref(&self.anchor, &mut out);
        out.push_str(",\"before\":");
        push_turn_refs(&self.before, &mut out);
        out.push_str(",\"fidelity\":");
        encode_string(self.fidelity.as_str(), &mut out);
        out.push_str(",\"preview_chars\":");
        out.push_str(&self.preview_chars.to_string());
        if self.preview_schema {
            out.push_str(",\"preview_schema\":");
            encode_string(PREVIEW_SCHEMA, &mut out);
        }
        out.push_str(",\"schema\":");
        encode_string(SCHEMA, &mut out);
        out.push_str(",\"trigger\":");
        match &self.trigger {
            Some(tr) => push_turn_ref(tr, &mut out),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }

    /// Deserialize a window persisted by [`ContextWindow::to_json`], rejecting any
    /// payload not carrying the literal `cc-transcript.context/2` schema.
    pub fn from_json(data: &str) -> Result<ContextWindow, SchemaError> {
        let payload: Value =
            sonic_rs::from_str(data).map_err(|e| SchemaError(format!("invalid JSON: {e}")))?;
        if field_str(&payload, "schema") != Some(SCHEMA) {
            let head: String = data.chars().take(120).collect();
            return Err(SchemaError(format!(
                "expected schema {SCHEMA:?}, got: {head}"
            )));
        }
        // Mirror Python's `payload.get("preview_schema")`: absent or null reads as
        // "no typed previews"; only a present, non-null value that isn't the version
        // is rejected.
        match field(&payload, "preview_schema") {
            None => {}
            Some(v) if v.is_null() || v.as_str() == Some(PREVIEW_SCHEMA) => {}
            Some(other) => {
                return Err(SchemaError(format!(
                    "expected preview schema {PREVIEW_SCHEMA:?}, got: {other:?}"
                )))
            }
        }
        window_from(&payload)
            .ok_or_else(|| SchemaError(format!("malformed context window: {data}")))
    }
}

fn window_from(payload: &Value) -> Option<ContextWindow> {
    Some(ContextWindow {
        anchor: ref_from(field(payload, "anchor")?)?,
        before: turn_refs_from(field(payload, "before")?)?,
        trigger: match field(payload, "trigger")? {
            v if v.is_null() => None,
            v => Some(turn_ref_from(v)?),
        },
        after: turn_refs_from(field(payload, "after")?)?,
        fidelity: Fidelity::parse(field_str(payload, "fidelity")?)?,
        preview_chars: field(payload, "preview_chars")?.as_i64()?,
        preview_schema: field_str(payload, "preview_schema") == Some(PREVIEW_SCHEMA),
    })
}

fn turn_refs_from(value: &Value) -> Option<Vec<TurnRef>> {
    value.as_array()?.iter().map(turn_ref_from).collect()
}

fn turn_ref_from(value: &Value) -> Option<TurnRef> {
    Some(TurnRef {
        role: Role::parse(field_str(value, "role")?)?,
        refs: field(value, "refs")?
            .as_array()?
            .iter()
            .map(ref_from)
            .collect::<Option<Vec<_>>>()?,
        preview: field_str(value, "preview")?.to_string(),
        tool_digests: field(value, "tool_digests")?
            .as_array()?
            .iter()
            .map(|d| d.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?,
        previews: match field(value, "previews") {
            None => None,
            Some(v) => Some(
                v.as_array()?
                    .iter()
                    .map(preview_from)
                    .collect::<Option<Vec<_>>>()?,
            ),
        },
    })
}

fn preview_from(value: &Value) -> Option<Preview> {
    match field_str(value, "kind")? {
        "text" => Some(Preview::Text {
            text: field_str(value, "text")?.to_string(),
        }),
        "tool_call" => Some(Preview::ToolCall {
            name: field_str(value, "name")?.to_string(),
            digest: field_str(value, "digest")?.to_string(),
            summary: field_str(value, "summary")?.to_string(),
        }),
        "ask_user_question" => Some(Preview::AskUserQuestion {
            questions: field(value, "questions")?
                .as_array()?
                .iter()
                .map(question_from)
                .collect::<Option<Vec<_>>>()?,
            selections: str_map_from(field(value, "selections")?)?,
            notes: str_map_from(field(value, "notes")?)?,
        }),
        _ => None,
    }
}

fn question_from(value: &Value) -> Option<PreviewQuestion> {
    Some(PreviewQuestion {
        question: field_str(value, "question")?.to_string(),
        header: field_str(value, "header").map(str::to_string),
        multi_select: field_bool(value, "multi_select"),
        labels: field(value, "labels")?
            .as_array()?
            .iter()
            .map(|l| l.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?,
    })
}

fn str_map_from(value: &Value) -> Option<BTreeMap<String, String>> {
    Some(
        value
            .as_object()?
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
            .collect(),
    )
}

fn ref_from(value: &Value) -> Option<EventRef> {
    Some(EventRef {
        session_id: field_str(value, "session_id")?.to_string(),
        event_uuid: field_str(value, "event_uuid")?.to_string(),
        tool_use_id: field_str(value, "tool_use_id").map(str::to_string),
    })
}

fn push_turn_refs(items: &[TurnRef], out: &mut String) {
    out.push('[');
    for (i, tr) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_turn_ref(tr, out);
    }
    out.push(']');
}

fn push_turn_ref(tr: &TurnRef, out: &mut String) {
    out.push_str("{\"preview\":");
    encode_string(&tr.preview, out);
    if let Some(previews) = &tr.previews {
        out.push_str(",\"previews\":[");
        for (i, preview) in previews.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_preview(preview, out);
        }
        out.push(']');
    }
    out.push_str(",\"refs\":[");
    for (i, r) in tr.refs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_ref(r, out);
    }
    out.push_str("],\"role\":");
    encode_string(tr.role.as_str(), out);
    out.push_str(",\"tool_digests\":[");
    for (i, d) in tr.tool_digests.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        encode_string(d, out);
    }
    out.push_str("]}");
}

fn push_ref(r: &EventRef, out: &mut String) {
    out.push_str("{\"event_uuid\":");
    encode_string(&r.event_uuid, out);
    out.push_str(",\"session_id\":");
    encode_string(&r.session_id, out);
    out.push_str(",\"tool_use_id\":");
    match &r.tool_use_id {
        Some(t) => encode_string(t, out),
        None => out.push_str("null"),
    }
    out.push('}');
}

fn push_preview(preview: &Preview, out: &mut String) {
    match preview {
        Preview::Text { text } => {
            out.push_str("{\"kind\":\"text\",\"text\":");
            encode_string(text, out);
            out.push('}');
        }
        Preview::ToolCall {
            name,
            digest,
            summary,
        } => {
            out.push_str("{\"digest\":");
            encode_string(digest, out);
            out.push_str(",\"kind\":\"tool_call\",\"name\":");
            encode_string(name, out);
            out.push_str(",\"summary\":");
            encode_string(summary, out);
            out.push('}');
        }
        Preview::AskUserQuestion {
            questions,
            selections,
            notes,
        } => {
            out.push_str("{\"kind\":\"ask_user_question\",\"notes\":");
            push_str_map(notes, out);
            out.push_str(",\"questions\":[");
            for (i, q) in questions.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_question(q, out);
            }
            out.push_str("],\"selections\":");
            push_str_map(selections, out);
            out.push('}');
        }
    }
}

fn push_question(q: &PreviewQuestion, out: &mut String) {
    out.push_str("{\"header\":");
    match &q.header {
        Some(h) => encode_string(h, out),
        None => out.push_str("null"),
    }
    out.push_str(",\"labels\":[");
    for (i, label) in q.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        encode_string(label, out);
    }
    out.push_str("],\"multi_select\":");
    out.push_str(if q.multi_select { "true" } else { "false" });
    out.push_str(",\"question\":");
    encode_string(&q.question, out);
    out.push('}');
}

fn push_str_map(map: &BTreeMap<String, String>, out: &mut String) {
    out.push('{');
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        encode_string(k, out);
        out.push(':');
        encode_string(v, out);
    }
    out.push('}');
}

/// Capture the turns around `anchor` as a live, full-fidelity window
/// (context.py `capture_window`). Errors when `anchor` does not resolve.
pub fn capture_window(
    lift: &LiftedSession,
    anchor: &EventRef,
    before: usize,
    after: usize,
    preview_chars: i64,
) -> Result<ContextWindow, String> {
    let trigger = turn_of(lift, anchor).ok_or_else(|| {
        format!(
            "anchor {} not found in session {}",
            anchor.event_uuid, lift.session_id
        )
    })?;
    let width = preview_chars.max(0) as usize;
    let budget = Budget {
        turn_chars: width,
        tool_chars: width,
    };
    let ti = trigger.index;
    let n = lift.turns.len();
    Ok(ContextWindow {
        anchor: anchor.clone(),
        before: lift.turns[ti.saturating_sub(before)..ti]
            .iter()
            .map(|t| turn_ref(t, &budget))
            .collect(),
        trigger: Some(turn_ref(trigger, &budget)),
        after: lift.turns[(ti + 1).min(n)..(ti + 1 + after).min(n)]
            .iter()
            .map(|t| turn_ref(t, &budget))
            .collect(),
        fidelity: Fidelity::Full,
        preview_chars,
        preview_schema: true,
    })
}

fn turn_of<'a>(lift: &'a LiftedSession<'a>, anchor: &EventRef) -> Option<&'a Turn<'a>> {
    lift.turns.iter().find(|turn| {
        turn.events
            .iter()
            .any(|event| event.meta().is_some_and(|m| m.uuid == anchor.event_uuid))
    })
}

fn turn_ref(turn: &Turn, budget: &Budget) -> TurnRef {
    TurnRef {
        role: if turn.prompt.is_empty() {
            Role::Assistant
        } else {
            Role::User
        },
        refs: turn
            .events
            .iter()
            .filter_map(Entry::meta)
            .map(|m| EventRef {
                session_id: m.session_id.clone(),
                event_uuid: m.uuid.clone(),
                tool_use_id: None,
            })
            .collect(),
        preview: render_turn(turn, budget),
        tool_digests: turn_tool_uses(turn)
            .map(|tu| tool_digest(&tu.name, &tu.input).expect("tool input digests"))
            .collect(),
        previews: Some(build_previews(turn, budget)),
    }
}

// The turn's tool-use blocks in lift order (mirrors activity.rs `lift_turn`).
fn turn_tool_uses<'a>(turn: &'a Turn<'a>) -> impl Iterator<Item = &'a ToolUseBlock> {
    turn.events
        .iter()
        .filter_map(|event| match event {
            Entry::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| {
            assistant.blocks.iter().filter_map(|block| match block {
                ContentBlock::ToolUse(tool_use) => Some(tool_use),
                _ => None,
            })
        })
}

// The turn's typed previews, in render_turn order (context.py build_previews).
fn build_previews(turn: &Turn, budget: &Budget) -> Vec<Preview> {
    let mut parts: Vec<Preview> = Vec::new();
    if !turn.prompt.is_empty() {
        parts.push(Preview::Text {
            text: clip(&turn.prompt, budget.turn_chars),
        });
    }
    let mut uses = turn.tool_uses.iter();
    for event in turn.events {
        let Entry::Assistant(assistant) = event else {
            continue;
        };
        for block in &assistant.blocks {
            match block {
                ContentBlock::Text(text) if !crate::pystr::strip(text).is_empty() => {
                    parts.push(Preview::Text {
                        text: clip(text, budget.turn_chars),
                    });
                }
                ContentBlock::ToolUse(tool_use) => {
                    let use_ = uses.next().expect("tool_uses aligns with tool-use blocks");
                    parts.push(preview_of_call(tool_use, use_, budget));
                }
                _ => {}
            }
        }
    }
    parts
}

fn preview_of_call(tu: &ToolUseBlock, use_: &ToolUse, budget: &Budget) -> Preview {
    if tu.name == ASK_USER_QUESTION {
        return ask_preview(tu, use_);
    }
    Preview::ToolCall {
        name: tu.name.clone(),
        digest: tool_digest(&tu.name, &tu.input).expect("tool input digests"),
        summary: render_tool_call(&parse_tool_call(&tu.name, &tu.input), budget),
    }
}

fn ask_preview(tu: &ToolUseBlock, use_: &ToolUse) -> Preview {
    let (selections, notes) = match use_
        .result
        .and_then(|r| r.tool_use_result.as_ref())
        .map(|payload| parse_tool_result(&tu.name, payload))
    {
        Some(ToolResult::AskUserQuestion(result)) => (
            result.answers,
            result
                .annotations
                .into_iter()
                .filter_map(|(k, a)| a.notes.map(|n| (k, n)))
                .collect(),
        ),
        _ => (BTreeMap::new(), BTreeMap::new()),
    };
    Preview::AskUserQuestion {
        questions: parse_questions(&tu.input)
            .unwrap_or_default()
            .into_iter()
            .map(PreviewQuestion::from)
            .collect(),
        selections,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_bytes;

    const SESSION: &str = "22222222-2222-2222-2222-222222222222";

    fn transcript() -> Vec<u8> {
        [
            r#"{"type":"user","uuid":"u0","sessionId":"SID","timestamp":"2026-02-01T09:00:00.000Z","message":{"role":"user","content":"one"}}"#,
            r#"{"type":"assistant","uuid":"a0","parentUuid":"u0","sessionId":"SID","timestamp":"2026-02-01T09:00:01.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"working"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/a.py","old_string":"x = 1","new_string":"x = 2"}}]}}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":"a0","sessionId":"SID","timestamp":"2026-02-01T09:00:02.000Z","message":{"role":"user","content":"two"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"SID","timestamp":"2026-02-01T09:00:03.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"uv run pytest"}}]}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","sessionId":"SID","timestamp":"2026-02-01T09:00:04.000Z","message":{"role":"user","content":"three"}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":"SID","timestamp":"2026-02-01T09:00:05.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"tool_use","id":"t3","name":"Edit","input":{"file_path":"/a.py","old_string":"ooo","new_string":"nnn"}}]}}"#,
            r#"{"type":"user","uuid":"u3","parentUuid":"a2","sessionId":"SID","timestamp":"2026-02-01T09:00:06.000Z","message":{"role":"user","content":"four"}}"#,
            r#"{"type":"assistant","uuid":"a3","parentUuid":"u3","sessionId":"SID","timestamp":"2026-02-01T09:00:07.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"done"}]}}"#,
        ]
        .join("\n")
        .replace("SID", SESSION)
        .into_bytes()
    }

    fn anchor(uuid: &str, tool_use_id: Option<&str>) -> EventRef {
        EventRef {
            session_id: SESSION.to_string(),
            event_uuid: uuid.to_string(),
            tool_use_id: tool_use_id.map(str::to_string),
        }
    }

    fn capture() -> ContextWindow {
        let entries = parse_bytes(&transcript(), |_| true).unwrap();
        let lift = crate::activity::lift_session(SESSION, &entries);
        capture_window(&lift, &anchor("a2", Some("t3")), 2, 1, 50).unwrap()
    }

    #[test]
    fn capture_builds_refs_previews_and_digests() {
        let window = capture();
        let trigger = window.trigger.as_ref().unwrap();
        assert_eq!(trigger.role, Role::User);
        assert_eq!(trigger.refs, vec![anchor("u2", None), anchor("a2", None)]);
        assert_eq!(trigger.preview, "user: three\nEdit /a.py\n- ooo\n+ nnn");
        assert_eq!(
            trigger.tool_digests,
            vec![tool_digest(
                "Edit",
                &sonic_rs::json!({"file_path":"/a.py","old_string":"ooo","new_string":"nnn"})
            )
            .unwrap()]
        );
        assert_eq!(
            window
                .before
                .iter()
                .map(|tr| tr.preview.lines().next().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec!["user: one", "user: two"]
        );
        assert_eq!(window.after.len(), 1);
        assert_eq!(
            window.after[0].refs,
            vec![anchor("u3", None), anchor("a3", None)]
        );
    }

    #[test]
    fn capture_unknown_anchor_errors() {
        let entries = parse_bytes(&transcript(), |_| true).unwrap();
        let lift = crate::activity::lift_session(SESSION, &entries);
        assert!(capture_window(&lift, &anchor("gone", None), 6, 2, 200).is_err());
    }

    #[test]
    fn round_trip_is_byte_stable() {
        let window = capture();
        let data = window.to_json();
        assert!(data.starts_with("{\"after\":"));
        let restored = ContextWindow::from_json(&data).unwrap();
        assert_eq!(restored, window);
        assert_eq!(restored.to_json(), data);
    }

    #[test]
    fn round_trip_preserves_legacy_window_without_typed_previews() {
        let window = ContextWindow {
            anchor: anchor("a2", Some("t3")),
            before: vec![TurnRef {
                role: Role::User,
                refs: vec![],
                preview: "converted prose".to_string(),
                tool_digests: vec![],
                previews: None,
            }],
            trigger: None,
            after: vec![],
            fidelity: Fidelity::Summary,
            preview_chars: 200,
            preview_schema: false,
        };
        let data = window.to_json();
        assert!(!data.contains("preview_schema"));
        assert!(!data.contains("\"previews\""));
        assert_eq!(ContextWindow::from_json(&data).unwrap(), window);
    }

    #[test]
    fn capture_attaches_typed_previews_versioned() {
        let window = capture();
        assert!(window.preview_schema);
        let data = window.to_json();
        assert!(data.contains("\"preview_schema\":\"cc-transcript.preview/1\""));
        let trigger = window.trigger.as_ref().unwrap();
        assert_eq!(
            trigger.previews,
            Some(vec![
                Preview::Text {
                    text: "three".to_string()
                },
                Preview::ToolCall {
                    name: "Edit".to_string(),
                    digest: tool_digest(
                        "Edit",
                        &sonic_rs::json!({"file_path":"/a.py","old_string":"ooo","new_string":"nnn"})
                    )
                    .unwrap(),
                    summary: "Edit /a.py\n- ooo\n+ nnn".to_string(),
                },
            ])
        );
    }

    #[test]
    fn ask_preview_carries_rounds_selections_and_notes() {
        let raw = [
            r#"{"type":"user","uuid":"u0","sessionId":"SID","timestamp":"2026-02-01T09:00:00.000Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","uuid":"a0","parentUuid":"u0","sessionId":"SID","timestamp":"2026-02-01T09:00:01.000Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"question":"Which?","header":"Pick","multiSelect":true,"options":[{"label":"A (Recommended)"},{"label":"B"}]}]}}]}}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":"a0","sessionId":"SID","timestamp":"2026-02-01T09:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"ok","is_error":false}]},"toolUseResult":{"answers":{"Which?":"A (Recommended), B"},"annotations":{"Which?":{"preview":"A (Recommended)","notes":"keep it"}}}}"#,
        ]
        .join("\n")
        .replace("SID", SESSION)
        .into_bytes();
        let entries = parse_bytes(&raw, |_| true).unwrap();
        let lift = crate::activity::lift_session(SESSION, &entries);
        let window = capture_window(&lift, &anchor("a0", Some("q1")), 0, 0, 200).unwrap();
        let trigger = window.trigger.as_ref().unwrap();
        let ask = trigger.previews.as_ref().unwrap().last().unwrap();
        assert_eq!(
            ask,
            &Preview::AskUserQuestion {
                questions: vec![PreviewQuestion {
                    question: "Which?".to_string(),
                    header: Some("Pick".to_string()),
                    multi_select: true,
                    labels: vec!["A (Recommended)".to_string(), "B".to_string()],
                }],
                selections: BTreeMap::from([(
                    "Which?".to_string(),
                    "A (Recommended), B".to_string()
                )]),
                notes: BTreeMap::from([("Which?".to_string(), "keep it".to_string())]),
            }
        );
        // Round-trips byte-stably through the versioned wire format.
        assert_eq!(ContextWindow::from_json(&window.to_json()).unwrap(), window);
    }

    #[test]
    fn from_json_rejects_unknown_schema() {
        assert!(ContextWindow::from_json("[]").is_err());
        assert!(ContextWindow::from_json(r#"{"schema":"cc-transcript.context/3"}"#).is_err());
        assert!(ContextWindow::from_json(r#"{"anchor":null}"#).is_err());
    }

    #[test]
    fn from_json_rejects_unknown_preview_schema() {
        let window = capture();
        let tampered = window
            .to_json()
            .replace("cc-transcript.preview/1", "cc-transcript.preview/2");
        assert!(ContextWindow::from_json(&tampered).is_err());
    }

    #[test]
    fn render_preview_labels_summary_and_clips() {
        let window = capture();
        let full = window.render_preview(usize::MAX);
        assert!(!full.starts_with(SUMMARY_LABEL));
        let summary = ContextWindow {
            fidelity: Fidelity::Summary,
            ..window.clone()
        };
        assert_eq!(
            summary.render_preview(usize::MAX).lines().next().unwrap(),
            SUMMARY_LABEL
        );
        let clipped = window.render_preview(5);
        assert!(clipped.split("\n\n").all(|part| part.contains("…(+")));
    }
}
