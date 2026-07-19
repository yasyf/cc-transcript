use std::collections::{HashMap, HashSet};

use chrono::{DateTime, FixedOffset};
use once_cell::sync::Lazy;

use crate::pystr;
use crate::toolcall::{parse_tool_call, ToolCall};
use crate::types::{
    matches_names, AssistantEntry, AttachmentDetail, ContentBlock, Entry, ToolResultBlock,
    ToolUseBlock, UserEntry,
};
use crate::value::field_str;

const NOTIFICATION_MARKER: &str = "<task-notification>";

// tools.py expand_tool_names("Agent|Task|Bash") / ("Agent|Task"), pre-expanded
// here because the alias table never crosses the language boundary.
static BACKGROUND_TOOLS: Lazy<HashSet<String>> = Lazy::new(|| {
    HashSet::from(["Agent", "Task", "Bash", "Execute", "exec_command"].map(String::from))
});
static TASK_TOOLS: Lazy<HashSet<String>> =
    Lazy::new(|| HashSet::from(["Agent", "Task"].map(String::from)));

pub struct ActivityOpts {
    pub waiting_tools: HashSet<String>,
    pub human_facing_tools: HashSet<String>,
}

impl Default for ActivityOpts {
    fn default() -> Self {
        Self {
            waiting_tools: HashSet::from(
                ["Monitor", "ScheduleWakeup", "SendMessage", "TeamCreate"].map(String::from),
            ),
            // tools.py expand_tool_names("AskUserQuestion|ExitPlanMode"), pre-expanded
            // here because the alias table never crosses the language boundary.
            human_facing_tools: HashSet::from(
                ["AskUserQuestion", "ExitPlanMode", "ExitSpecMode"].map(String::from),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    WaitingTool,
    Background,
    SubagentlessTask,
    PendingAsyncTask,
    PendingAsyncWorkflow,
    MidTool,
}

impl PendingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingKind::WaitingTool => "waiting_tool",
            PendingKind::Background => "background",
            PendingKind::SubagentlessTask => "subagentless_task",
            PendingKind::PendingAsyncTask => "pending_async_task",
            PendingKind::PendingAsyncWorkflow => "pending_async_workflow",
            PendingKind::MidTool => "mid_tool",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PendingItem {
    pub tool_use_id: Option<String>,
    pub name: String,
    pub kind: PendingKind,
}

#[derive(Debug)]
pub struct SessionActivity {
    pub is_waiting: bool,
    pub mid_tool: bool,
    pub pending: Vec<PendingItem>,
    pub last_event_epoch: Option<i64>,
}

/// Whether a user entry opens a turn (activity.py ``native_user_classifier``):
/// a real prompt — non-meta, non-sidechain, not an interruption marker, not an
/// agent-injected relay banner, with non-blank text. Compact-summary exclusion
/// matches captain-hook, which consumes this probe since the 10.2.0
/// shared-classifier fix.
fn opens_turn(user: &UserEntry) -> bool {
    !(user.meta.is_meta
        || user.meta.is_sidechain
        || user.meta.is_compact_summary
        || user.interrupted()
        || user.is_agent_injected())
        && !pystr::strip(&user.content.text()).is_empty()
}

/// The index opening the current turn: the last turn-opening user entry, or 0
/// when no prompt qualifies (activity.py ``from_events`` turn 0).
fn current_turn_start(entries: &[Entry]) -> usize {
    entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::User(user) if opens_turn(user)))
        .unwrap_or(0)
}

/// conditions.py ``ephemeral_wait``: a waiting tool, a backgrounded
/// Agent/Task/Bash, or a subagentless Agent/Task — every name matched alias-
/// and MCP-prefix-aware (tools.py matches_names).
fn ephemeral_wait(tool_use: &ToolUseBlock, waiting_tools: &HashSet<String>) -> Option<PendingKind> {
    if matches_names(&tool_use.name, waiting_tools) {
        return Some(PendingKind::WaitingTool);
    }
    if matches_names(&tool_use.name, &BACKGROUND_TOOLS) && tool_use.run_in_background == Some(true)
    {
        return Some(PendingKind::Background);
    }
    if matches_names(&tool_use.name, &TASK_TOOLS) && tool_use.subagent_type.is_none() {
        return Some(PendingKind::SubagentlessTask);
    }
    None
}

/// conditions.py ``pending_async``: an async Agent/Task launch or a live
/// Workflow whose completion notification has not reached the agent.
fn pending_async(
    tool_use: &ToolUseBlock,
    result: Option<&ToolResultBlock>,
    notifications: &Notifications,
) -> Option<PendingKind> {
    match tool_use.name.as_str() {
        "Agent" | "Task"
            if result.is_some_and(|r| r.is_async) && !notifications.completed(&tool_use.id) =>
        {
            Some(PendingKind::PendingAsyncTask)
        }
        "Workflow" if !notifications.completed(&tool_use.id) => {
            Some(PendingKind::PendingAsyncWorkflow)
        }
        _ => None,
    }
}

/// notifications.py ``Notifications``: the harness delivery queue replayed
/// from the transcript's ``queue-operation`` audit records.
struct Notifications {
    queued: Vec<String>,
    delivered: Vec<String>,
    enqueued: Vec<String>,
}

impl Notifications {
    fn from_entries(entries: &[Entry]) -> Self {
        let mut queued: Vec<String> = Vec::new();
        let mut delivered: Vec<String> = Vec::new();
        let mut enqueued: Vec<String> = Vec::new();
        for entry in entries {
            if let Entry::Other(other) = entry {
                if other.ty == "queue-operation" {
                    match field_str(&other.raw, "operation") {
                        Some("enqueue") => {
                            let content = field_str(&other.raw, "content")
                                .unwrap_or_default()
                                .to_string();
                            enqueued.push(content.clone());
                            queued.push(content);
                        }
                        Some("dequeue" | "remove") if !queued.is_empty() => {
                            queued.remove(0);
                        }
                        Some("popAll") => {
                            let content = field_str(&other.raw, "content").unwrap_or_default();
                            queued.retain(|item| !content.contains(item.as_str()));
                        }
                        _ => {}
                    }
                }
            }
            if let Some(text) = delivered_text(entry) {
                delivered.push(text);
            }
        }
        Self {
            queued,
            delivered,
            enqueued,
        }
    }

    /// notifications.py ``Notifications.completed``: delivered, or enqueued at
    /// some point yet no longer sitting undelivered in the queue.
    fn completed(&self, tool_use_id: &str) -> bool {
        let marker = format!("<tool-use-id>{tool_use_id}</tool-use-id>");
        let holds = |texts: &[String]| texts.iter().any(|text| text.contains(&marker));
        holds(&self.delivered) || (holds(&self.enqueued) && !holds(&self.queued))
    }

    /// notifications.py ``Notifications.has_pending``: any queued item is an
    /// undelivered task notification.
    fn has_pending(&self) -> bool {
        self.queued
            .iter()
            .any(|text| text.contains(NOTIFICATION_MARKER))
    }
}

/// notifications.py ``delivered_text``: a user turn carrying a task
/// notification, or a ``queued_command`` attachment replayed to the model.
fn delivered_text(entry: &Entry) -> Option<String> {
    match entry {
        Entry::User(user) => {
            let text = user.content.text();
            text.contains(NOTIFICATION_MARKER).then_some(text)
        }
        Entry::Attachment(att) => match &att.detail {
            AttachmentDetail::QueuedCommand(qc) => Some(qc.prompt.clone().unwrap_or_default()),
            _ => None,
        },
        _ => None,
    }
}

/// The session-activity oracle over parsed entries: captain-hook's
/// ``is_waiting`` verdict (post-d2e07cc, minus its hook-side Stop-payload
/// layer) over undelivered notifications, ephemeral waits, and pending async
/// launches, plus the mid-tool flag and the contributing tool calls. An
/// undelivered notification alone sets ``is_waiting`` with no pending item —
/// a resumed session's orphan has no launch to point at.
pub fn session_activity(entries: &[Entry], opts: &ActivityOpts) -> SessionActivity {
    let results: HashMap<&str, &ToolResultBlock> = entries
        .iter()
        .flat_map(Entry::tool_results)
        .map(|result| (result.tool_use_id.as_str(), result))
        .collect();
    let notifications = Notifications::from_entries(entries);
    let turn_start = current_turn_start(entries);

    let mut is_waiting = notifications.has_pending();
    let mut mid_tool = false;
    let mut pending: Vec<PendingItem> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        for tool_use in entry.tool_uses() {
            let result = results.get(tool_use.id.as_str()).copied();
            if result.is_some_and(|r| r.is_error) {
                continue;
            }
            let in_current_turn = index >= turn_start;
            let waiting_kind = in_current_turn
                .then(|| ephemeral_wait(tool_use, &opts.waiting_tools))
                .flatten()
                .or_else(|| pending_async(tool_use, result, &notifications));
            let unmatched = in_current_turn
                && result.is_none()
                && !matches_names(&tool_use.name, &opts.human_facing_tools);
            is_waiting |= waiting_kind.is_some();
            mid_tool |= unmatched;
            let Some(kind) = waiting_kind.or(unmatched.then_some(PendingKind::MidTool)) else {
                continue;
            };
            if seen.insert(tool_use.id.as_str()) {
                pending.push(PendingItem {
                    tool_use_id: Some(tool_use.id.clone()),
                    name: tool_use.name.clone(),
                    kind,
                });
            }
        }
    }
    SessionActivity {
        is_waiting,
        mid_tool,
        pending,
        last_event_epoch: entries
            .iter()
            .filter_map(Entry::meta)
            .map(|meta| meta.timestamp.timestamp())
            .max(),
    }
}

/// A before/after content pair lowered from an edit-shaped call (tools.py Hunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old: String,
    pub new: String,
}

/// One tool invocation lifted from a turn's assistant events (activity.py ToolUse);
/// `call` is the typed tool call parsed once at lift time, `edits` its lowered
/// `(file_path, hunks)` entries — one per patched file for apply_patch, else 0 or 1.
#[derive(Debug)]
pub struct ToolUse<'a> {
    pub event_uuid: &'a str,
    pub tool_use_id: &'a str,
    pub name: &'a str,
    pub ts: DateTime<FixedOffset>,
    pub result: Option<&'a ToolResultBlock>,
    pub result_ts: Option<DateTime<FixedOffset>>,
    pub turn_index: usize,
    pub call: ToolCall,
    pub edits: Vec<(String, Vec<Hunk>)>,
}

impl ToolUse<'_> {
    /// Milliseconds from the call to its result, or None without a result
    /// timestamp (activity.py ToolUse.duration_ms).
    pub fn duration_ms(&self) -> Option<i64> {
        self.result_ts.map(|rt| {
            let micros = (rt - self.ts)
                .num_microseconds()
                .expect("duration fits i64 microseconds");
            ((micros as f64 / 1_000_000.0) * 1000.0).round_ties_even() as i64
        })
    }
}

/// A file modification lowered from an edit-shaped tool call (activity.py Edit).
#[derive(Debug)]
pub struct Edit<'a> {
    pub file_path: String,
    pub hunks: Vec<Hunk>,
    pub tool: &'a str,
    pub event_uuid: &'a str,
    pub tool_use_id: &'a str,
    pub turn_index: usize,
    pub ts: DateTime<FixedOffset>,
}

/// One prompt-to-prompt span of a session (activity.py Turn); `events` is the
/// turn's contiguous run of the parsed entries as borrowed refs, edits derived
/// on demand.
#[derive(Debug)]
pub struct Turn<'a> {
    pub index: usize,
    pub prompt: String,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub ended_at: Option<DateTime<FixedOffset>>,
    pub events: Vec<&'a Entry>,
    pub tool_uses: Vec<ToolUse<'a>>,
}

impl<'a> Turn<'a> {
    /// The turn's file modifications: tool uses that lowered to hunks and a path
    /// (activity.py Turn.edits).
    pub fn edits(&self) -> Vec<Edit<'a>> {
        self.tool_uses
            .iter()
            .flat_map(|use_| {
                use_.edits.iter().map(move |(path, hunks)| Edit {
                    file_path: path.clone(),
                    hunks: hunks.clone(),
                    tool: use_.name,
                    event_uuid: use_.event_uuid,
                    tool_use_id: use_.tool_use_id,
                    turn_index: use_.turn_index,
                    ts: use_.ts,
                })
            })
            .collect()
    }
}

/// A session's transcript lifted into turns (activity.py SessionActivity).
#[derive(Debug)]
pub struct LiftedSession<'a> {
    pub session_id: &'a str,
    pub turns: Vec<Turn<'a>>,
}

impl<'a> LiftedSession<'a> {
    /// Every edit in the session, in chronological order (activity.py SessionActivity.edits).
    pub fn edits(&self) -> Vec<Edit<'a>> {
        self.turns.iter().flat_map(|turn| turn.edits()).collect()
    }
}

/// A tool result paired with the tool-use id it answers, in activity.py
/// `result_index` order (last-write-wins, first-occurrence position: dict semantics).
#[derive(Debug)]
pub struct ResultRef<'a> {
    pub tool_use_id: &'a str,
    pub block: &'a ToolResultBlock,
    pub result_ts: Option<DateTime<FixedOffset>>,
}

/// Indexes tool results by the tool-use id they answer (activity.py `result_index`):
/// each user entry's tool-result blocks paired with that entry's timestamp.
pub fn result_index<'a>(entries: &[&'a Entry]) -> Vec<ResultRef<'a>> {
    let mut order: Vec<&'a str> = Vec::new();
    let mut latest: HashMap<&'a str, (&'a ToolResultBlock, Option<DateTime<FixedOffset>>)> =
        HashMap::new();
    for &entry in entries {
        if let Entry::User(user) = entry {
            for block in user.tool_results() {
                let key = block.tool_use_id.as_str();
                if latest
                    .insert(key, (block, Some(user.meta.timestamp)))
                    .is_none()
                {
                    order.push(key);
                }
            }
        }
    }
    order
        .into_iter()
        .map(|key| {
            let (block, result_ts) = latest[key];
            ResultRef {
                tool_use_id: key,
                block,
                result_ts,
            }
        })
        .collect()
}

const LINE_BREAKS: &[char] = &[
    '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}',
];

// str.splitlines() then " ".join(line.split()), dropping empties; the "\r\n"
// empty slice drops, so it counts as one break.
fn normalized_lines(text: &str) -> Vec<String> {
    text.split(|c| LINE_BREAKS.contains(&c))
        .filter_map(|line| {
            let normalized = pystr::split_whitespace(line).collect::<Vec<_>>().join(" ");
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

/// The fraction of `a.new`'s non-empty normalized lines present in `b.old`
/// (activity.py `hunk_overlap`); 0.0 when `a.new` has no non-empty lines.
pub fn hunk_overlap(a: &Hunk, b: &Hunk) -> f64 {
    let lines = normalized_lines(&a.new);
    if lines.is_empty() {
        return 0.0;
    }
    let olds: HashSet<String> = normalized_lines(&b.old).into_iter().collect();
    lines.iter().filter(|line| olds.contains(*line)).count() as f64 / lines.len() as f64
}

/// The greatest `hunk_overlap` over the cartesian product of two edits' hunks
/// (evidence.py `overlap_between`); 0.0 when either side is empty.
pub fn overlap_between(incorrect: &[Hunk], correction: &[Hunk]) -> f64 {
    incorrect
        .iter()
        .flat_map(|a| correction.iter().map(move |b| hunk_overlap(a, b)))
        .fold(0.0_f64, f64::max)
}

fn lower_edit(call: &ToolCall) -> Vec<(String, Vec<Hunk>)> {
    call.edits()
        .into_iter()
        .map(|(path, hunks)| {
            (
                path.to_string(),
                hunks
                    .into_iter()
                    .map(|h| Hunk {
                        old: h.old,
                        new: h.new,
                    })
                    .collect(),
            )
        })
        .collect()
}

struct Segment {
    prompt: String,
    start: usize,
    end: usize,
}

// activity.py from_events segmentation: a turn-opening user entry starts a contiguous
// segment, else folds into the current one; `Some(openers)` replaces that per-entry decision.
fn segments(entries: &[&Entry], openers: Option<&[bool]>) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for (index, &entry) in entries.iter().enumerate() {
        let opens = openers.map_or_else(
            || matches!(entry, Entry::User(user) if opens_turn(user)),
            |flags| flags[index],
        );
        match entry {
            Entry::User(user) if opens => segments.push(Segment {
                prompt: user.content.text(),
                start: index,
                end: index + 1,
            }),
            _ => match segments.last_mut() {
                Some(segment) => segment.end = index + 1,
                None => segments.push(Segment {
                    prompt: String::new(),
                    start: index,
                    end: index + 1,
                }),
            },
        }
    }
    segments
}

// activity.py event_stamps: first and last timestamps among a turn's events;
// Mode and Other entries carry no envelope and are skipped.
fn event_stamps(
    events: &[&Entry],
) -> (Option<DateTime<FixedOffset>>, Option<DateTime<FixedOffset>>) {
    let stamps: Vec<DateTime<FixedOffset>> = events
        .iter()
        .copied()
        .filter_map(Entry::meta)
        .map(|m| m.timestamp)
        .collect();
    (stamps.first().copied(), stamps.last().copied())
}

fn lift_turn<'a>(
    index: usize,
    segment: &Segment,
    entries: &[&'a Entry],
    results: &HashMap<&'a str, (&'a ToolResultBlock, Option<DateTime<FixedOffset>>)>,
) -> Turn<'a> {
    let events: Vec<&'a Entry> = entries[segment.start..segment.end].to_vec();
    let (started_at, ended_at) = event_stamps(&events);
    let tool_uses = events
        .iter()
        .copied()
        .filter_map(|entry| match entry {
            Entry::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant: &'a AssistantEntry| {
            assistant
                .blocks
                .iter()
                .filter_map(move |block| match block {
                    ContentBlock::ToolUse(tool_use) => Some((assistant, tool_use)),
                    _ => None,
                })
        })
        .map(|(assistant, tool_use)| {
            let pair = results.get(tool_use.id.as_str());
            let call = parse_tool_call(&tool_use.name, &tool_use.input);
            ToolUse {
                event_uuid: &assistant.meta.uuid,
                tool_use_id: &tool_use.id,
                name: &tool_use.name,
                ts: assistant.meta.timestamp,
                result: pair.map(|(block, _)| *block),
                result_ts: pair.and_then(|(_, ts)| *ts),
                turn_index: index,
                edits: lower_edit(&call),
                call,
            }
        })
        .collect();
    Turn {
        index,
        prompt: segment.prompt.clone(),
        started_at,
        ended_at,
        events,
        tool_uses,
    }
}

/// Lifts parsed entries into turns (activity.py SessionActivity.from_events): a
/// turn-opening user opens a turn, and each tool-use is paired via `result_index`.
pub fn lift_session<'a>(session_id: &'a str, entries: &'a [Entry]) -> LiftedSession<'a> {
    lift_session_refs(session_id, &entries.iter().collect::<Vec<_>>(), None)
}

/// `lift_session` over borrowed entry views — the events-in native path, where each
/// view borrows its `&Entry` behind the shared parse buffer (no re-parse). `Some(openers)`
/// supplies one caller-precomputed turn-opening flag per entry in place of `opens_turn`.
pub fn lift_session_refs<'a>(
    session_id: &'a str,
    entries: &[&'a Entry],
    openers: Option<&[bool]>,
) -> LiftedSession<'a> {
    let results: HashMap<&'a str, (&'a ToolResultBlock, Option<DateTime<FixedOffset>>)> =
        result_index(entries)
            .into_iter()
            .map(|r| (r.tool_use_id, (r.block, r.result_ts)))
            .collect();
    let turns = segments(entries, openers)
        .iter()
        .enumerate()
        .map(|(index, segment)| lift_turn(index, segment, entries, &results))
        .collect();
    LiftedSession { session_id, turns }
}

/// A tool use located by positional index within the input slice: `event_idx` is
/// the assistant entry's position and `result_event_idx` the position of the user
/// entry answering it. Both come straight off the segments walk, so a slice with
/// repeated entries or uuids never collapses.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolUseIndex<'a> {
    pub event_idx: usize,
    pub tool_use_id: &'a str,
    pub result_event_idx: Option<usize>,
}

/// One turn projected to positional indices over the input slice (activity.py
/// SessionActivity.from_events skeleton): the `start..end` span, the first and last
/// metadata-bearing event positions, and the turn's tool uses located by index.
#[derive(Debug, PartialEq, Eq)]
pub struct TurnIndex<'a> {
    pub prompt: String,
    pub start: usize,
    pub end: usize,
    pub started_idx: Option<usize>,
    pub ended_idx: Option<usize>,
    pub tool_uses: Vec<ToolUseIndex<'a>>,
}

/// Lifts entries into per-turn positional index skeletons over the input slice,
/// tracking every index directly from the segments walk. Unlike `lift_session_refs`,
/// whose turns hold borrowed entry refs, this hands the events-in binding the indices
/// themselves, so the binding never reverse-maps a `*const Entry` or uuid back to a
/// position (both last-write-wins on a slice that repeats an entry or uuid).
/// `Some(openers)` supplies one turn-opening flag per entry in place of `opens_turn`.
pub fn lift_session_index<'a>(
    entries: &[&'a Entry],
    openers: Option<&[bool]>,
) -> Vec<TurnIndex<'a>> {
    let result_pos: HashMap<&'a str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, &entry)| match entry {
            Entry::User(user) => Some((index, user)),
            _ => None,
        })
        .flat_map(|(index, user)| {
            user.tool_results()
                .map(move |block| (block.tool_use_id.as_str(), index))
        })
        .collect();
    segments(entries, openers)
        .into_iter()
        .map(|segment| {
            let meta_positions: Vec<usize> = (segment.start..segment.end)
                .filter(|&index| entries[index].meta().is_some())
                .collect();
            let tool_uses = (segment.start..segment.end)
                .filter_map(|index| match entries[index] {
                    Entry::Assistant(assistant) => Some((index, assistant)),
                    _ => None,
                })
                .flat_map(|(index, assistant)| {
                    assistant
                        .blocks
                        .iter()
                        .filter_map(move |block| match block {
                            ContentBlock::ToolUse(tool_use) => Some((index, tool_use)),
                            _ => None,
                        })
                })
                .map(|(index, tool_use)| ToolUseIndex {
                    event_idx: index,
                    tool_use_id: tool_use.id.as_str(),
                    result_event_idx: result_pos.get(tool_use.id.as_str()).copied(),
                })
                .collect();
            TurnIndex {
                prompt: segment.prompt,
                start: segment.start,
                end: segment.end,
                started_idx: meta_positions.first().copied(),
                ended_idx: meta_positions.last().copied(),
                tool_uses,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_entry;

    fn parse(raw: &str) -> Entry {
        parse_entry(sonic_rs::from_str(raw).unwrap()).unwrap()
    }

    fn user(text: &str) -> Entry {
        user_with(text, "")
    }

    fn user_with(text: &str, flags: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"user","uuid":"u","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z",{flags}"message":{{"content":{}}}}}"#,
            sonic_rs::to_string(&text).unwrap()
        ))
    }

    fn tool_use(name: &str, id: &str, input: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"assistant","uuid":"a","sessionId":"s1","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"m","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
        ))
    }

    fn tool_result(id: &str) -> Entry {
        result_entry(id, false, false)
    }

    fn result_entry(id: &str, is_error: bool, is_async: bool) -> Entry {
        parse(&format!(
            r#"{{"type":"user","uuid":"r","sessionId":"s1","timestamp":"2026-01-02T03:04:07Z","toolUseResult":{{"isAsync":{is_async}}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"done","is_error":{is_error}}}]}}}}"#
        ))
    }

    fn queue_op(content: &str) -> Entry {
        queue_entry("enqueue", content)
    }

    fn queue_entry(operation: &str, content: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"queue-operation","operation":"{operation}","content":{}}}"#,
            sonic_rs::to_string(&content).unwrap()
        ))
    }

    fn attachment(prompt: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"attachment","uuid":"att","sessionId":"s1","timestamp":"2026-01-02T03:04:08Z","attachment":{{"type":"queued_command","prompt":{}}}}}"#,
            sonic_rs::to_string(&prompt).unwrap()
        ))
    }

    fn delivered_notification(id: &str) -> Vec<Entry> {
        vec![
            queue_op(&notification(id)),
            queue_entry("dequeue", ""),
            user(&notification(id)),
        ]
    }

    fn notification(id: &str) -> String {
        format!(
            "<task-notification><task-id>t</task-id><tool-use-id>{id}</tool-use-id><status>completed</status></task-notification>"
        )
    }

    fn activity(entries: &[Entry]) -> SessionActivity {
        session_activity(entries, &ActivityOpts::default())
    }

    #[test]
    fn pending_workflow_is_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(!activity.mid_tool);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("wf1".to_string()),
                name: "Workflow".to_string(),
                kind: PendingKind::PendingAsyncWorkflow
            }]
        );
    }

    #[test]
    fn workflow_delivered_notification_clears_waiting() {
        let mut entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
        ];
        entries.extend(delivered_notification("wf1"));
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn enqueued_undelivered_notification_keeps_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf1")),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("wf1".to_string()),
                name: "Workflow".to_string(),
                kind: PendingKind::PendingAsyncWorkflow
            }]
        );
    }

    #[test]
    fn removed_notification_counts_completed() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf1")),
            queue_entry("remove", ""),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn popall_drains_command_but_not_notification() {
        let notif = notification("wf1");
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notif),
            queue_op("run the tests please"),
            queue_entry("popAll", "run the tests please"),
        ];
        let notifications = Notifications::from_entries(&entries);
        assert_eq!(
            notifications.queued,
            vec![notif],
            "popAll drains the matching command yet leaves the notification queued"
        );
        assert!(notifications.has_pending());
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("wf1".to_string()),
                name: "Workflow".to_string(),
                kind: PendingKind::PendingAsyncWorkflow
            }]
        );
    }

    #[test]
    fn popall_drains_the_notification_completes() {
        let notif = notification("wf1");
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notif),
            queue_entry("popAll", &notif),
        ];
        let notifications = Notifications::from_entries(&entries);
        assert!(
            notifications.queued.is_empty(),
            "popAll subtracts the notification itself out of the queue"
        );
        assert!(
            notifications.completed("wf1"),
            "an enqueued-then-drained notification counts as completed"
        );
        assert!(!notifications.has_pending());
        let activity = activity(&entries);
        assert!(
            !activity.is_waiting,
            "draining the notification closes the workflow's async wait"
        );
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn orphan_undelivered_notification_is_waiting() {
        let entries = vec![user("hi"), queue_op(&notification("tu_ghost"))];
        let activity = activity(&entries);
        assert!(
            activity.is_waiting,
            "a queued task notification holds the session on its own"
        );
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn attachment_delivery_completes() {
        let entries = vec![
            user("go"),
            tool_use(
                "Agent",
                "a1",
                r#"{"subagent_type":"Explore","prompt":"look"}"#,
            ),
            result_entry("a1", false, true),
            attachment(&notification("a1")),
        ];
        let activity = activity(&entries);
        assert!(
            !activity.is_waiting,
            "the queued_command attachment alone closes the async task"
        );
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:08Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity.last_event_epoch, Some(expected));
    }

    #[test]
    fn plain_user_delivery_completes() {
        let entries = vec![
            user("go"),
            tool_use(
                "Agent",
                "a1",
                r#"{"subagent_type":"Explore","prompt":"look"}"#,
            ),
            result_entry("a1", false, true),
            user(&notification("a1")),
        ];
        let activity = activity(&entries);
        assert!(
            !activity.is_waiting,
            "a user turn carrying the notification closes the async task"
        );
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:07Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity.last_event_epoch, Some(expected));
    }

    #[test]
    fn compact_summary_user_does_not_open_turn() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Bash",
                "b1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
            tool_result("b1"),
            user_with("compact recap", r#""isCompactSummary":true,"#),
        ];
        let activity = activity(&entries);
        assert!(
            activity.is_waiting,
            "auto-compaction must not retire a running background task"
        );
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn agent_injected_banner_does_not_open_turn() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Bash",
                "b1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
            tool_result("b1"),
            user("<teammate-message from='mate'>ping</teammate-message>"),
        ];
        let activity = activity(&entries);
        assert!(
            activity.is_waiting,
            "an agent-injected relay banner must not open a turn, so the background Bash stays current"
        );
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn unrelated_completion_marker_keeps_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf_other")),
        ];
        assert!(activity(&entries).is_waiting);
    }

    #[test]
    fn errored_workflow_is_not_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            result_entry("wf1", true, false),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn background_bash_in_current_turn_is_waiting() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Bash",
                "b1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
            tool_result("b1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(!activity.mid_tool);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn background_bash_in_previous_turn_is_not_waiting() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Bash",
                "b1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
            tool_result("b1"),
            user("now do something else"),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn background_execute_alias_in_current_turn_is_waiting() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Execute",
                "e1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn subagentless_agent_is_waiting() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"prompt":"look around"}"#),
            tool_result("a1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::SubagentlessTask);
    }

    #[test]
    fn typed_agent_with_sync_result_is_not_waiting() {
        let entries = vec![
            user("go"),
            tool_use(
                "Agent",
                "a1",
                r#"{"subagent_type":"Explore","prompt":"look"}"#,
            ),
            tool_result("a1"),
        ];
        assert!(!activity(&entries).is_waiting);
    }

    #[test]
    fn async_agent_in_previous_turn_still_pending() {
        let entries = vec![
            user("go"),
            tool_use(
                "Agent",
                "a1",
                r#"{"subagent_type":"Explore","prompt":"look"}"#,
            ),
            result_entry("a1", false, true),
            user("while that runs, plan"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::PendingAsyncTask);
    }

    #[test]
    fn async_agent_delivered_notification_clears_waiting() {
        let mut entries = vec![
            user("go"),
            tool_use(
                "Agent",
                "a1",
                r#"{"subagent_type":"Explore","prompt":"look"}"#,
            ),
            result_entry("a1", false, true),
            user("while that runs, plan"),
        ];
        entries.extend(delivered_notification("a1"));
        assert!(!activity(&entries).is_waiting);
    }

    #[test]
    fn waiting_tool_in_current_turn_is_waiting() {
        let entries = vec![
            user("watch it"),
            tool_use("Monitor", "m1", r#"{"until":"done"}"#),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(activity.mid_tool, "an unmatched Monitor is also mid-tool");
        assert_eq!(activity.pending[0].kind, PendingKind::WaitingTool);
    }

    #[test]
    fn mcp_prefixed_waiting_tool_is_waiting() {
        let entries = vec![
            user("ping the pool"),
            tool_use("mcp__pool__SendMessage", "s1", r#"{"text":"hi"}"#),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::WaitingTool);
    }

    #[test]
    fn pending_ask_user_question_is_quiet() {
        let entries = vec![
            user("choose"),
            tool_use("AskUserQuestion", "q1", r#"{"questions":[]}"#),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn mcp_prefixed_human_facing_tool_is_not_mid_tool() {
        let entries = vec![
            user("choose"),
            tool_use(
                "mcp__someserver__AskUserQuestion",
                "q1",
                r#"{"questions":[]}"#,
            ),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn unmatched_bash_in_current_turn_is_mid_tool() {
        let entries = vec![user("list"), tool_use("Bash", "b1", r#"{"command":"ls"}"#)];
        let activity = activity(&entries);
        assert!(activity.mid_tool);
        assert!(!activity.is_waiting);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("b1".to_string()),
                name: "Bash".to_string(),
                kind: PendingKind::MidTool
            }]
        );
    }

    #[test]
    fn unmatched_bash_in_previous_turn_is_not_mid_tool() {
        let entries = vec![
            user("list"),
            tool_use("Bash", "b1", r#"{"command":"ls"}"#),
            user("moving on"),
        ];
        let activity = activity(&entries);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn meta_sidechain_and_interrupt_users_do_not_open_turns() {
        let entries = vec![
            user("build it"),
            tool_use(
                "Bash",
                "b1",
                r#"{"command":"make","run_in_background":true}"#,
            ),
            tool_result("b1"),
            user_with("injected context", r#""isMeta":true,"#),
            user_with("sidechain prompt", r#""isSidechain":true,"#),
            user("[Request interrupted by user]"),
            user("   "),
        ];
        assert!(
            activity(&entries).is_waiting,
            "no later entry opens a turn, so the background Bash stays current"
        );
    }

    #[test]
    fn contributing_call_dedupes_by_tool_use_id() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"prompt":"x","run_in_background":true}"#),
            result_entry("a1", false, true),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending.len(), 1);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn last_event_epoch_is_max_meta_timestamp() {
        let entries = vec![
            user("hi"),
            tool_use("Bash", "b1", r#"{"command":"ls"}"#),
            queue_op("no meta here"),
        ];
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:06Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity(&entries).last_event_epoch, Some(expected));
    }

    #[test]
    fn empty_session_is_idle() {
        let activity = activity(&[]);
        assert!(!activity.is_waiting);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        assert_eq!(activity.last_event_epoch, None);
    }

    fn edit_use(name: &str, id: &str, input: &str) -> Entry {
        tool_use(name, id, input)
    }

    #[test]
    fn lift_segments_prelude_and_prompts() {
        let entries = vec![
            result_entry("orphan", false, false),
            user("do the thing"),
            tool_use("Bash", "b1", r#"{"command":"ls"}"#),
            tool_result("b1"),
            user("now do more"),
        ];
        let lift = lift_session("s1", &entries);
        assert_eq!(lift.turns.len(), 3);
        assert_eq!(lift.turns[0].prompt, "");
        assert_eq!(lift.turns[0].events.len(), 1);
        assert_eq!(lift.turns[1].prompt, "do the thing");
        assert_eq!(lift.turns[1].tool_uses.len(), 1);
        assert_eq!(lift.turns[1].tool_uses[0].tool_use_id, "b1");
        assert_eq!(lift.turns[2].prompt, "now do more");
    }

    #[test]
    fn lift_pairs_result_and_duration() {
        let entries = vec![
            user("go"),
            tool_use("Bash", "b1", r#"{"command":"make"}"#),
            tool_result("b1"),
        ];
        let lift = lift_session("s1", &entries);
        let paired = &lift.turns[0].tool_uses[0];
        assert!(paired.result.is_some());
        assert_eq!(paired.duration_ms(), Some(1000));
    }

    #[test]
    fn lift_unpaired_tool_use_has_no_duration() {
        let entries = vec![user("go"), tool_use("Bash", "b1", r#"{"command":"make"}"#)];
        let lift = lift_session("s1", &entries);
        let unpaired = &lift.turns[0].tool_uses[0];
        assert!(unpaired.result.is_none());
        assert_eq!(unpaired.duration_ms(), None);
    }

    #[test]
    fn lift_lowers_edit_shaped_calls() {
        let entries = vec![
            user("edit files"),
            edit_use(
                "Edit",
                "e1",
                r#"{"file_path":"/a.py","old_string":"x","new_string":"y"}"#,
            ),
            edit_use("Write", "w1", r#"{"file_path":"/b.py","content":"hello"}"#),
            edit_use(
                "MultiEdit",
                "m1",
                r#"{"file_path":"/c.py","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d"}]}"#,
            ),
            edit_use(
                "NotebookEdit",
                "n1",
                r#"{"notebook_path":"/d.ipynb","new_source":"cell"}"#,
            ),
            edit_use("Bash", "b1", r#"{"command":"ls"}"#),
        ];
        let lift = lift_session("s1", &entries);
        let edits = lift.edits();
        assert_eq!(edits.len(), 4);
        assert_eq!(edits[0].tool, "Edit");
        assert_eq!(edits[0].file_path, "/a.py");
        assert_eq!(
            edits[0].hunks,
            vec![Hunk {
                old: "x".into(),
                new: "y".into()
            }]
        );
        assert_eq!(
            edits[1].hunks,
            vec![Hunk {
                old: String::new(),
                new: "hello".into()
            }]
        );
        assert_eq!(edits[2].hunks.len(), 2);
        assert_eq!(edits[3].file_path, "/d.ipynb");
    }

    #[test]
    fn lift_malformed_edit_degrades_to_no_edit() {
        let entries = vec![
            user("edit"),
            edit_use("Edit", "e1", r#"{"file_path":"/a.py","new_string":"y"}"#),
            edit_use("Write", "w1", r#"{"file_path":"/b.py","content":123}"#),
            edit_use("Create", "c1", r#"{"file_path":"/c.py","content":"ok"}"#),
        ];
        let lift = lift_session("s1", &entries);
        let edits = lift.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].tool, "Create");
        assert_eq!(edits[0].file_path, "/c.py");
    }

    #[test]
    fn result_index_orders_by_first_occurrence() {
        let entries = vec![
            user("go"),
            tool_use("Bash", "b1", r#"{"command":"a"}"#),
            tool_use("Bash", "b2", r#"{"command":"b"}"#),
            tool_result("b2"),
            tool_result("b1"),
        ];
        assert_eq!(
            result_index(&entries.iter().collect::<Vec<_>>())
                .iter()
                .map(|r| r.tool_use_id)
                .collect::<Vec<_>>(),
            vec!["b2", "b1"]
        );
    }

    #[test]
    fn refs_matches_the_owned_slice_path() {
        let entries = vec![
            user("do the thing"),
            tool_use(
                "Edit",
                "e1",
                r#"{"file_path":"/a","old_string":"x","new_string":"y"}"#,
            ),
            tool_result("e1"),
            user("and then"),
            tool_use("Bash", "b1", r#"{"command":"make"}"#),
            result_entry("b1", true, false),
        ];
        let refs: Vec<&Entry> = entries.iter().collect();
        let via_owned = lift_session("s1", &entries);
        let via_refs = lift_session_refs("s1", &refs, None);
        let project = |lift: &LiftedSession| {
            (
                lift.session_id.to_owned(),
                lift.turns
                    .iter()
                    .map(|t| {
                        (
                            t.index,
                            t.prompt.clone(),
                            t.started_at,
                            t.ended_at,
                            t.events
                                .iter()
                                .filter_map(|e| e.meta())
                                .map(|m| m.uuid.clone())
                                .collect::<Vec<_>>(),
                            t.tool_uses
                                .iter()
                                .map(|u| {
                                    (
                                        u.tool_use_id.to_owned(),
                                        u.name.to_owned(),
                                        u.result.map(|r| r.is_error),
                                        u.duration_ms(),
                                        u.edits.clone(),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(project(&via_owned), project(&via_refs));
    }

    #[test]
    fn openers_override_supplants_the_classifier() {
        // openers=[false, true] must invert opens_turn's verdict on both entries.
        let entries = vec![
            user("first ask"),
            user_with("second ask", r#""isMeta":true,"#),
        ];
        let refs: Vec<&Entry> = entries.iter().collect();

        let classifier = lift_session_refs("s1", &refs, None);
        assert_eq!(
            classifier
                .turns
                .iter()
                .map(|t| t.prompt.as_str())
                .collect::<Vec<_>>(),
            ["first ask"],
            "classifier opens the real prompt and folds the meta entry into it"
        );

        let overridden = lift_session_refs("s1", &refs, Some(&[false, true]));
        assert_eq!(
            overridden
                .turns
                .iter()
                .map(|t| t.prompt.as_str())
                .collect::<Vec<_>>(),
            ["", "second ask"],
            "override (not OR: both open, not AND: neither) suppresses 0 and promotes meta 1"
        );
        assert_eq!(
            overridden
                .turns
                .iter()
                .map(|t| t.events.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );

        let skeleton = lift_session_index(&refs, Some(&[false, true]));
        assert_eq!(
            skeleton
                .iter()
                .map(|t| (t.prompt.as_str(), t.start, t.end))
                .collect::<Vec<_>>(),
            [("", 0, 1), ("second ask", 1, 2)],
            "the index skeleton aligns the promoted boundary to entry 1"
        );
    }

    #[test]
    fn hunk_overlap_matches_reference() {
        let hunk = |old: &str, new: &str| Hunk {
            old: old.into(),
            new: new.into(),
        };
        assert_eq!(
            hunk_overlap(&hunk("", "x = 1\ny = 2"), &hunk("x = 1\ny = 2", "")),
            1.0
        );
        assert_eq!(
            hunk_overlap(&hunk("", "x = 1\nz = 3"), &hunk("x = 1\ny = 2", "")),
            0.5
        );
        assert_eq!(
            hunk_overlap(&hunk("", "  x  =  1  "), &hunk("x = 1", "")),
            1.0
        );
        assert_eq!(hunk_overlap(&hunk("", ""), &hunk("x = 1", "")), 0.0);
        assert_eq!(
            overlap_between(
                &[hunk("", "x = 1\ny = 2")],
                &[hunk("q", "w"), hunk("x = 1\ny = 2", "")]
            ),
            1.0
        );
    }

    #[test]
    fn duration_ms_rounds_half_to_even() {
        for (micros, expected) in [(600u32, 1i64), (1100, 1), (500, 0), (1500, 2)] {
            let assistant = parse(
                r#"{"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-01T00:00:00.000000Z","message":{"model":"m","content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#,
            );
            let result = parse(&format!(
                r#"{{"type":"user","uuid":"r","sessionId":"s","timestamp":"2026-01-01T00:00:00.{micros:06}Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"t","content":"ok","is_error":false}}]}}}}"#
            ));
            let entries = vec![user("go"), assistant, result];
            assert_eq!(
                lift_session("s", &entries).turns[0].tool_uses[0].duration_ms(),
                Some(expected),
                "micros={micros}"
            );
        }
    }

    #[test]
    fn control_whitespace_only_user_does_not_open_turn() {
        let entries = vec![
            user("real prompt"),
            user("\u{1c}\u{1d}\u{1e}\u{1f}"),
            user("another prompt"),
        ];
        let lift = lift_session("s", &entries);
        assert_eq!(lift.turns.len(), 2);
        assert_eq!(lift.turns[0].prompt, "real prompt");
        assert_eq!(lift.turns[1].prompt, "another prompt");
    }

    #[test]
    fn lower_edit_takes_last_duplicate_key() {
        let entries = vec![
            user("edit"),
            parse(
                r#"{"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"model":"m","content":[{"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/first.py","file_path":"/last.py","old_string":"a","old_string":"b","new_string":"x","new_string":"y"}}]}}"#,
            ),
        ];
        let edits = lift_session("s", &entries).edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].file_path, "/last.py");
        assert_eq!(
            edits[0].hunks[0],
            Hunk {
                old: "b".into(),
                new: "y".into()
            }
        );
    }
}
