use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::size_of;

use crate::context::{ContextWindow, Fidelity, Preview, PreviewQuestion, Role, TurnRef};
use crate::ids::EventRef;
use crate::render::Budget;
use crate::snapshot::{SnapshotError, Status};
use crate::snapshot_activity::ActivityIndex;
use crate::types::{ContentBlock, Entry, ToolResultBlock};

const PREVIEW_CHARS: usize = 200;

enum CachedPreview {
    Fixed(Preview),
    Ask {
        id: String,
        questions: Vec<PreviewQuestion>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonic_rs::{json, Value};

    fn entry(value: Value) -> Entry {
        crate::parse::parse_bytes(&sonic_rs::to_vec(&value).unwrap(), |_| true)
            .unwrap()
            .pop()
            .unwrap()
    }

    fn user(id: &str, content: Value, result: Value) -> Entry {
        entry(
            json!({"type":"user","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:05Z",
            "message":{"content":content},"toolUseResult":result}),
        )
    }

    fn assistant(id: &str, content: Value) -> Entry {
        entry(
            json!({"type":"assistant","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:05Z",
            "message":{"role":"assistant","model":"m","content":content}}),
        )
    }

    fn ask(id: &str, question: &str) -> Value {
        json!({"type":"tool_use","id":id,"name":"AskUserQuestion","input":{"questions":[{"question":question,"header":"choice","multiSelect":false,"options":[{"label":"A"},{"label":"B"}]}]}})
    }

    #[test]
    fn registered_review_context_matches_full_capture_at_every_append_and_global_result_rebind() {
        let entries = vec![
            user(
                "u0",
                json!("start with α and preserve these words"),
                json!(null),
            ),
            assistant(
                "a0",
                json!([{"type":"text","text":"working"},ask("q","Which?"),{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/a"}}]),
            ),
            user(
                "u1",
                json!([{"type":"tool_result","tool_use_id":"q","content":"first answer"}]),
                json!({"answers":{"Which?":"A"},"annotations":{"Which?":{"notes":"first note"}}}),
            ),
            user("u2", json!("next turn"), json!(null)),
            assistant("a1", json!([{"type":"text","text":"followup"}])),
            user(
                "u3",
                json!([{"type":"tool_result","tool_use_id":"q","content":"late error payload","is_error":true}]),
                json!({"answers":{"Which?":"B"},"annotations":{"Which?":{"notes":"latest note"}}}),
            ),
            assistant("a2", json!([ask("q", "Another question?")])),
            user("u4", json!("third turn"), json!(null)),
            user(
                "u5",
                json!([{"type":"tool_result","tool_use_id":"late","content":"answer before question"}]),
                json!({"answers":{"Old?":"free answer"}}),
            ),
            assistant("a3", json!([ask("late", "Old?")])),
            user(
                "u6",
                json!([{"type":"tool_result","tool_use_id":"late","content":"latest result has no structured answer"}]),
                json!(null),
            ),
        ];
        let mut activity = ActivityIndex::default();
        let mut context = ReviewContextState::default();
        for (index, entry) in entries.iter().enumerate() {
            activity = activity.append_tail(&[entry], None);
            context.append(index, entry, &activity);
            assert_eq!(context.accounted_bytes(), context.audited_bytes());
            let payloads = context
                .required_payloads(&activity)
                .map(|event| (event, &entries[event]))
                .collect();
            context.resolve(&activity, &payloads);
            assert_eq!(context.accounted_bytes(), context.audited_bytes());
            let refs = entries[..=index].iter().collect::<Vec<_>>();
            let full = crate::activity::lift_session_refs("s", &refs, None);
            for prior in &entries[..=index] {
                let meta = prior.meta().unwrap();
                let anchor = EventRef {
                    session_id: meta.session_id.clone(),
                    event_uuid: meta.uuid.clone(),
                    tool_use_id: None,
                };
                let expected =
                    crate::context::capture_window(&full, &anchor, 6, 2, PREVIEW_CHARS as i64)
                        .unwrap();
                let actual = context.capture(&activity, &anchor, 6, 2).unwrap();
                assert_eq!(
                    actual.to_json(),
                    expected.to_json(),
                    "prefix={index} anchor={}",
                    meta.uuid
                );
            }
        }
    }
    #[test]
    fn context_suffix_rollback_reappend_matches_cold_questions_and_captures() {
        let rows = vec![
            user("u0", json!("owner words"), json!(null)),
            assistant("a0", json!([ask("q", "Which?"), {"type":"tool_use","id":"r","name":"Read","input":{"file_path":"/old"}}])),
            user("u1", json!([{"type":"tool_result","tool_use_id":"q","content":"first"}]), json!({"answers":{"Which?":"A"}})),
            user("u2", json!("next owner words"), json!(null)),
            assistant("a1", json!([ask("q", "Which?"), {"type":"tool_use","id":"r","name":"Edit","input":{"file_path":"/new","old_string":"a","new_string":"b"}}])),
            user("u3", json!([{"type":"tool_result","tool_use_id":"q","content":"last"}]), json!({"answers":{"Which?":"B"}})),
        ].into_iter().map(|entry| std::sync::Arc::new(crate::snapshot::ChunkRows::new(vec![entry]))).collect::<Vec<_>>();
        let refs = rows.iter().map(|row| &row[0]).collect::<Vec<_>>();
        for first in 0..=rows.len() {
            let mut activity = ActivityIndex::default();
            let mut context = ReviewContextState::default();
            for (event, entry) in refs.iter().enumerate() {
                activity = activity.append_tail(&[entry], None);
                context.append(event, entry, &activity);
            }
            let last_result = |id: &str| {
                refs[..first]
                    .iter()
                    .enumerate()
                    .filter_map(|(event, entry)| match entry {
                        Entry::User(user)
                            if user.tool_results().any(|result| result.tool_use_id == id) =>
                        {
                            Some(event)
                        }
                        _ => None,
                    })
                    .last()
            };
            activity.truncate(first, &rows, last_result,
                (0..first).find(|event| refs[*event].meta().is_some()),
                (0..first).rfind(|event| refs[*event].meta().is_some()),
                (0..first).rfind(|event| matches!(refs[*event], Entry::Assistant(assistant) if assistant.model != crate::types::SYNTHETIC_MODEL)));
            context.truncate(first);
            assert_eq!(context.accounted_bytes(), context.audited_bytes());
            for (event, entry) in refs.iter().enumerate().skip(first) {
                activity = activity.append_tail(&[entry], None);
                context.append(event, entry, &activity);
                assert_eq!(context.accounted_bytes(), context.audited_bytes());
            }
            let payloads = context
                .required_payloads(&activity)
                .map(|event| (event, refs[event]))
                .collect();
            context.resolve(&activity, &payloads);
            assert_eq!(context.accounted_bytes(), context.audited_bytes());
            let full = crate::activity::lift_session_refs("s", &refs, None);
            for entry in &refs {
                let meta = entry.meta().unwrap();
                let anchor = EventRef {
                    session_id: "s".to_owned(),
                    event_uuid: meta.uuid.clone(),
                    tool_use_id: None,
                };
                assert_eq!(
                    context.capture(&activity, &anchor, 6, 2).unwrap().to_json(),
                    crate::context::capture_window(&full, &anchor, 6, 2, PREVIEW_CHARS as i64)
                        .unwrap()
                        .to_json(),
                    "cut={first}"
                );
            }
        }
    }
}

struct CachedTurn {
    role: Role,
    refs: Vec<EventRef>,
    preview: String,
    digests: Vec<String>,
    parts: Vec<CachedPreview>,
}

struct Reply {
    selections: BTreeMap<String, String>,
    notes: BTreeMap<String, String>,
}

impl Reply {
    fn heap_bytes(&self) -> usize {
        [&self.selections, &self.notes]
            .into_iter()
            .map(|map| {
                map.len() * size_of::<(String, String)>()
                    + map
                        .iter()
                        .map(|(key, value)| key.capacity() + value.capacity())
                        .sum::<usize>()
            })
            .sum()
    }
}

enum TurnUndo {
    New,
    Append {
        refs: usize,
        digests: usize,
        parts: usize,
        preview: usize,
    },
}

#[derive(Default)]
struct ContextUndo {
    session_added: bool,
    turn: Option<(usize, TurnUndo)>,
    names_before: Option<(HashMap<String, String>, usize)>,
    name_changes: Vec<(String, Option<String>)>,
    new_asks: Vec<String>,
    reply_changes: Vec<(String, Option<Reply>)>,
    pending_removed: Vec<String>,
}

impl ContextUndo {
    #[cfg(test)]
    fn audited_heap_bytes(&self) -> usize {
        self.heap_bytes()
            - self
                .names_before
                .as_ref()
                .map_or(0, |(_, payload)| *payload)
            + self.names_before.as_ref().map_or(0, |(names, _)| {
                names
                    .iter()
                    .map(|(id, name)| id.capacity() + name.capacity())
                    .sum::<usize>()
            })
    }

    fn heap_bytes(&self) -> usize {
        self.names_before.as_ref().map_or(0, |(names, payload)| {
            names.capacity() * size_of::<(String, String)>() + payload
        }) + self.name_changes.capacity() * size_of::<(String, Option<String>)>()
            + self
                .name_changes
                .iter()
                .map(|(id, old)| id.capacity() + old.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self.new_asks.capacity() * size_of::<String>()
            + self.new_asks.iter().map(String::capacity).sum::<usize>()
            + self.reply_changes.capacity() * size_of::<(String, Option<Reply>)>()
            + self
                .reply_changes
                .iter()
                .map(|(id, old)| id.capacity() + old.as_ref().map_or(0, Reply::heap_bytes))
                .sum::<usize>()
            + self.pending_removed.capacity() * size_of::<String>()
            + self
                .pending_removed
                .iter()
                .map(String::capacity)
                .sum::<usize>()
    }
}

#[derive(Default)]
pub struct ReviewContextState {
    session_id: Option<String>,
    turns: Vec<CachedTurn>,
    names: HashMap<String, String>,
    asks: HashSet<String>,
    replies: HashMap<String, Reply>,
    pending: BTreeSet<String>,
    turn_bytes: usize,
    name_bytes: usize,
    ask_bytes: usize,
    reply_bytes: usize,
    pending_bytes: usize,
    undo: Vec<ContextUndo>,
    undo_bytes: usize,
}

fn ref_heap(reference: &EventRef) -> usize {
    reference.session_id.capacity()
        + reference.event_uuid.capacity()
        + reference.tool_use_id.as_ref().map_or(0, String::capacity)
}

fn question_heap(questions: &Vec<PreviewQuestion>) -> usize {
    questions.capacity() * size_of::<PreviewQuestion>()
        + questions
            .iter()
            .map(|question| {
                question.question.capacity()
                    + question.header.as_ref().map_or(0, String::capacity)
                    + question.labels.capacity() * size_of::<String>()
                    + question.labels.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>()
}

fn part_heap(part: &CachedPreview) -> usize {
    match part {
        CachedPreview::Fixed(Preview::Text { text }) => text.capacity(),
        CachedPreview::Fixed(Preview::ToolCall {
            name,
            digest,
            summary,
        }) => name.capacity() + digest.capacity() + summary.capacity(),
        CachedPreview::Ask { id, questions } => id.capacity() + question_heap(questions),
        CachedPreview::Fixed(Preview::AskUserQuestion { .. }) => unreachable!(),
    }
}

impl ReviewContextState {
    pub(crate) fn append_bound(&self, entry: &Entry) -> usize {
        use crate::snapshot_ledger::{
            map_reallocation_bound, set_reallocation_bound, vec_reallocation_bound,
        };
        let tools = entry.tool_uses().count();
        let results = entry.tool_results().count();
        let charge = crate::snapshot_memory::entry_charge(entry);
        let payload = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
        let slots = tools + results + 1;
        vec_reallocation_bound(&self.undo, 1)
            + vec_reallocation_bound(&self.turns, 1)
            + map_reallocation_bound(&self.names, tools)
            + set_reallocation_bound(&self.asks, tools)
            + map_reallocation_bound(&self.replies, results)
            + self.turns.last().map_or(0, |turn| {
                vec_reallocation_bound(&turn.refs, slots)
                    + vec_reallocation_bound(&turn.digests, tools)
                    + vec_reallocation_bound(&turn.parts, slots)
                    + if turn.preview.len() + payload > turn.preview.capacity() {
                        (turn.preview.len() + payload).max(turn.preview.capacity() * 2)
                    } else {
                        0
                    }
            })
            + payload * 8
            + slots * 2048
    }

    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.session_id.as_ref().map_or(0, String::capacity)
            + self.turns.capacity() * size_of::<CachedTurn>()
            + self.names.capacity() * size_of::<(String, String)>()
            + self.asks.capacity() * size_of::<String>()
            + self.replies.capacity() * size_of::<(String, Reply)>()
            + self.pending.len() * size_of::<String>()
            + self.turn_bytes
            + self.name_bytes
            + self.ask_bytes
            + self.reply_bytes
            + self.pending_bytes
            + self.undo.capacity() * size_of::<ContextUndo>()
            + self.undo_bytes
    }

    #[cfg(test)]
    pub(crate) fn audited_bytes(&self) -> usize {
        let turns = self
            .turns
            .iter()
            .map(|turn| {
                turn.refs.capacity() * size_of::<EventRef>()
                    + turn.refs.iter().map(ref_heap).sum::<usize>()
                    + turn.preview.capacity()
                    + turn.digests.capacity() * size_of::<String>()
                    + turn.digests.iter().map(String::capacity).sum::<usize>()
                    + turn.parts.capacity() * size_of::<CachedPreview>()
                    + turn.parts.iter().map(part_heap).sum::<usize>()
            })
            .sum::<usize>();
        self.accounted_bytes()
            - self.turn_bytes
            - self.name_bytes
            - self.ask_bytes
            - self.reply_bytes
            - self.pending_bytes
            - self.undo_bytes
            + self
                .undo
                .iter()
                .map(ContextUndo::audited_heap_bytes)
                .sum::<usize>()
            + turns
            + self
                .names
                .iter()
                .map(|(id, name)| id.capacity() + name.capacity())
                .sum::<usize>()
            + self.asks.iter().map(String::capacity).sum::<usize>()
            + self
                .replies
                .iter()
                .map(|(id, reply)| id.capacity() + reply.heap_bytes())
                .sum::<usize>()
            + self.pending.iter().map(String::capacity).sum::<usize>()
    }

    pub fn append(&mut self, event: usize, entry: &Entry, activity: &ActivityIndex) {
        assert_eq!(event, self.undo.len());
        let mut undo = ContextUndo::default();
        if self.session_id.is_none() {
            self.session_id = entry.meta().map(|meta| meta.session_id.clone());
            undo.session_added = self.session_id.is_some();
        }
        let Some(fragment) = activity.event_fragment(event, entry) else {
            self.undo.push(undo);
            return;
        };
        let index = fragment.index;
        let fresh = index == self.turns.len();
        assert!(index <= self.turns.len(), "review context turn sequence");
        if fresh {
            undo.turn = Some((index, TurnUndo::New));
            undo.names_before = Some((std::mem::take(&mut self.names), self.name_bytes));
            self.name_bytes = 0;
        } else {
            let turn = &self.turns[index];
            undo.turn = Some((
                index,
                TurnUndo::Append {
                    refs: turn.refs.len(),
                    digests: turn.digests.len(),
                    parts: turn.parts.len(),
                    preview: turn.preview.len(),
                },
            ));
        }
        let budget = Budget {
            turn_chars: PREVIEW_CHARS,
            tool_chars: PREVIEW_CHARS,
        };
        let mut reference = crate::context::turn_ref(&fragment, &budget);
        if let Entry::User(user) = entry {
            for result in user.tool_results() {
                if let Some(name) = self.names.get(&result.tool_use_id) {
                    for line in crate::render::registered_review_result_lines(name, result, &budget)
                    {
                        if !reference.preview.is_empty() {
                            reference.preview.push('\n');
                        }
                        reference.preview.push_str(&line);
                    }
                }
            }
        }
        let mut tools = match entry {
            Entry::Assistant(assistant) => assistant
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse(tool)
                        if tool.name == crate::context::ASK_USER_QUESTION =>
                    {
                        Some(tool)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .into_iter(),
            _ => Vec::new().into_iter(),
        };
        let parts = reference
            .previews
            .take()
            .expect("typed review context previews")
            .into_iter()
            .map(|part| match part {
                Preview::AskUserQuestion { questions, .. } => {
                    let tool = tools.next().expect("question preview source");
                    if !self.asks.contains(&tool.id) {
                        let id = tool.id.clone();
                        self.ask_bytes += id.capacity();
                        self.asks.insert(id);
                        undo.new_asks.push(tool.id.clone());
                        let id = tool.id.clone();
                        self.pending_bytes += id.capacity();
                        assert!(self.pending.insert(id));
                    }
                    CachedPreview::Ask {
                        id: tool.id.clone(),
                        questions,
                    }
                }
                part => CachedPreview::Fixed(part),
            })
            .collect::<Vec<_>>();
        let child_bytes = reference.refs.iter().map(ref_heap).sum::<usize>()
            + reference
                .tool_digests
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + parts.iter().map(part_heap).sum::<usize>();
        if fresh {
            self.turn_bytes += child_bytes
                + reference.preview.capacity()
                + reference.refs.capacity() * size_of::<EventRef>()
                + reference.tool_digests.capacity() * size_of::<String>()
                + parts.capacity() * size_of::<CachedPreview>();
            self.turns.push(CachedTurn {
                role: reference.role,
                refs: reference.refs,
                preview: reference.preview,
                digests: reference.tool_digests,
                parts,
            });
        } else {
            let target = &mut self.turns[index];
            let prior = target.preview.capacity()
                + target.refs.capacity() * size_of::<EventRef>()
                + target.digests.capacity() * size_of::<String>()
                + target.parts.capacity() * size_of::<CachedPreview>();
            if !target.preview.is_empty() && !reference.preview.is_empty() {
                target.preview.push('\n');
            }
            target.preview.push_str(&reference.preview);
            target.refs.extend(reference.refs);
            target.digests.extend(reference.tool_digests);
            target.parts.extend(parts);
            self.turn_bytes += child_bytes
                + target.preview.capacity()
                + target.refs.capacity() * size_of::<EventRef>()
                + target.digests.capacity() * size_of::<String>()
                + target.parts.capacity() * size_of::<CachedPreview>()
                - prior;
        }
        if let Entry::Assistant(assistant) = entry {
            for tool in assistant.blocks.iter().filter_map(|block| match block {
                ContentBlock::ToolUse(tool) => Some(tool),
                _ => None,
            }) {
                let name = tool.name.clone();
                self.name_bytes += name.capacity();
                match self.names.entry(tool.id.clone()) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        self.name_bytes += slot.key().capacity();
                        undo.name_changes.push((tool.id.clone(), None));
                        slot.insert(name);
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        let old = slot.insert(name);
                        self.name_bytes -= old.capacity();
                        undo.name_changes.push((tool.id.clone(), Some(old)));
                    }
                }
            }
        }
        if let Entry::User(user) = entry {
            for result in user.tool_results() {
                if self.asks.contains(&result.tool_use_id) {
                    self.apply_result(&result.tool_use_id, Some(result), Some(&mut undo));
                }
            }
        }
        self.undo_bytes += undo.heap_bytes();
        self.undo.push(undo);
    }

    fn apply_result(
        &mut self,
        id: &str,
        result: Option<&ToolResultBlock>,
        mut undo: Option<&mut ContextUndo>,
    ) {
        if let Some(undo) = undo.as_deref_mut() {
            let old = self.replies.remove_entry(id);
            if let Some((key, reply)) = old {
                self.reply_bytes -= key.capacity() + reply.heap_bytes();
                undo.reply_changes.push((key, Some(reply)));
            } else {
                undo.reply_changes.push((id.to_owned(), None));
            }
        }
        let (selections, notes) = crate::context::ask_result(result);
        if selections.is_empty() && notes.is_empty() {
            if let Some((key, reply)) = self.replies.remove_entry(id) {
                self.reply_bytes -= key.capacity() + reply.heap_bytes();
            }
        } else {
            let reply = Reply { selections, notes };
            self.reply_bytes += reply.heap_bytes();
            match self.replies.entry(id.to_owned()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    self.reply_bytes += slot.key().capacity();
                    slot.insert(reply);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    self.reply_bytes -= slot.insert(reply).heap_bytes();
                }
            }
        }
        if let Some(id) = self.pending.take(id) {
            self.pending_bytes -= id.capacity();
            if let Some(undo) = undo {
                undo.pending_removed.push(id);
            }
        }
    }

    pub fn truncate(&mut self, first: usize) {
        assert!(first <= self.undo.len());
        while self.undo.len() > first {
            let undo = self.undo.pop().expect("context event undo");
            self.undo_bytes -= undo.heap_bytes();
            for (id, old) in undo.reply_changes.into_iter().rev() {
                if let Some((key, reply)) = self.replies.remove_entry(&id) {
                    self.reply_bytes -= key.capacity() + reply.heap_bytes();
                }
                if let Some(reply) = old {
                    self.reply_bytes += id.capacity() + reply.heap_bytes();
                    assert!(self.replies.insert(id, reply).is_none());
                }
            }
            for id in undo.pending_removed {
                self.pending_bytes += id.capacity();
                assert!(self.pending.insert(id));
            }
            for id in undo.new_asks {
                let key = self.asks.take(&id).expect("introduced question ID");
                self.ask_bytes -= key.capacity();
                if let Some(key) = self.pending.take(&id) {
                    self.pending_bytes -= key.capacity();
                }
                if let Some((key, reply)) = self.replies.remove_entry(&id) {
                    self.reply_bytes -= key.capacity() + reply.heap_bytes();
                }
            }
            for (id, old) in undo.name_changes.into_iter().rev() {
                if let Some(old) = old {
                    let current = self.names.get_mut(&id).expect("updated tool name");
                    self.name_bytes = self.name_bytes - current.capacity() + old.capacity();
                    *current = old;
                } else {
                    let (key, name) = self.names.remove_entry(&id).expect("introduced tool name");
                    self.name_bytes -= key.capacity() + name.capacity();
                }
            }
            if let Some((names, payload)) = undo.names_before {
                self.names = names;
                self.name_bytes = payload;
            }
            if let Some((index, turn)) = undo.turn {
                match turn {
                    TurnUndo::New => {
                        assert_eq!(index + 1, self.turns.len());
                        let turn = self.turns.pop().expect("introduced turn");
                        self.turn_bytes -= turn.preview.capacity()
                            + turn.refs.capacity() * size_of::<EventRef>()
                            + turn.refs.iter().map(ref_heap).sum::<usize>()
                            + turn.digests.capacity() * size_of::<String>()
                            + turn.digests.iter().map(String::capacity).sum::<usize>()
                            + turn.parts.capacity() * size_of::<CachedPreview>()
                            + turn.parts.iter().map(part_heap).sum::<usize>();
                    }
                    TurnUndo::Append {
                        refs,
                        digests,
                        parts,
                        preview,
                    } => {
                        let turn = &mut self.turns[index];
                        self.turn_bytes -= turn.refs[refs..].iter().map(ref_heap).sum::<usize>()
                            + turn.digests[digests..]
                                .iter()
                                .map(String::capacity)
                                .sum::<usize>()
                            + turn.parts[parts..].iter().map(part_heap).sum::<usize>();
                        turn.refs.truncate(refs);
                        turn.digests.truncate(digests);
                        turn.parts.truncate(parts);
                        turn.preview.truncate(preview);
                    }
                }
            }
            if undo.session_added {
                self.session_id = None;
            }
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn required_payloads<'a>(
        &'a self,
        activity: &'a ActivityIndex,
    ) -> impl Iterator<Item = usize> + 'a {
        self.pending
            .iter()
            .filter_map(|id| activity.result_position(id).map(|(event, _)| event))
    }

    pub fn resolve(&mut self, activity: &ActivityIndex, payloads: &HashMap<usize, &Entry>) {
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        for id in pending {
            let result = activity.result_position(&id).map(|(event, ordinal)| {
                let Entry::User(user) = payloads[&event] else {
                    unreachable!()
                };
                let ContentBlock::ToolResult(result) = &user.blocks()[ordinal] else {
                    unreachable!()
                };
                result
            });
            self.apply_result(&id, result, None);
        }
    }

    fn turn_ref(&self, index: usize) -> TurnRef {
        let turn = &self.turns[index];
        TurnRef {
            role: turn.role,
            refs: turn.refs.clone(),
            preview: turn.preview.clone(),
            tool_digests: turn.digests.clone(),
            previews: Some(
                turn.parts
                    .iter()
                    .map(|part| match part {
                        CachedPreview::Fixed(part) => part.clone(),
                        CachedPreview::Ask { id, questions } => {
                            let reply = self.replies.get(id);
                            Preview::AskUserQuestion {
                                questions: questions.clone(),
                                selections: reply
                                    .map_or_else(BTreeMap::new, |reply| reply.selections.clone()),
                                notes: reply
                                    .map_or_else(BTreeMap::new, |reply| reply.notes.clone()),
                            }
                        }
                    })
                    .collect(),
            ),
        }
    }

    pub fn capture(
        &self,
        activity: &ActivityIndex,
        anchor: &EventRef,
        before: usize,
        after: usize,
    ) -> Result<ContextWindow, SnapshotError> {
        if !self.pending.is_empty() {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "review context has unresolved question replies",
            ));
        }
        if self.session_id.as_deref() != Some(anchor.session_id.as_str()) {
            return Err(SnapshotError::new(
                Status::InvalidRequest,
                "capture anchor session mismatch",
            ));
        }
        let event = activity.event_of_uuid(&anchor.event_uuid).ok_or_else(|| {
            SnapshotError::new(Status::InvalidRequest, "capture anchor not found")
        })?;
        let trigger = activity.turn_of_event(event).expect("review anchor turn");
        Ok(ContextWindow {
            anchor: anchor.clone(),
            before: (trigger.saturating_sub(before)..trigger)
                .map(|index| self.turn_ref(index))
                .collect(),
            trigger: Some(self.turn_ref(trigger)),
            after: (trigger + 1
                ..trigger
                    .saturating_add(after)
                    .saturating_add(1)
                    .min(self.turns.len()))
                .map(|index| self.turn_ref(index))
                .collect(),
            fidelity: Fidelity::Full,
            preview_chars: PREVIEW_CHARS as i64,
            preview_schema: true,
        })
    }
}
