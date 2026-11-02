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
        use crate::snapshot_ledger::{map_growth, set_growth, vec_growth};
        let tools = entry.tool_uses().count();
        let results = entry.tool_results().count();
        let charge = crate::snapshot_memory::entry_charge(entry);
        let payload = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
        let slots = tools + results + 1;
        vec_growth(&self.turns, 1)
            + map_growth(&self.names, tools)
            + set_growth(&self.asks, tools)
            + map_growth(&self.replies, results)
            + self.turns.last().map_or(0, |turn| {
                vec_growth(&turn.refs, slots)
                    + vec_growth(&turn.digests, tools)
                    + vec_growth(&turn.parts, slots)
                    + if turn.preview.len() + payload > turn.preview.capacity() {
                        (turn.preview.len() + payload).max(turn.preview.capacity() * 2)
                            - turn.preview.capacity()
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
        if self.session_id.is_none() {
            self.session_id = entry.meta().map(|meta| meta.session_id.clone());
        }
        let Some(fragment) = activity.event_fragment(event, entry) else {
            return;
        };
        let index = fragment.index;
        let fresh = index == self.turns.len();
        assert!(index <= self.turns.len(), "review context turn sequence");
        if fresh {
            self.names.clear();
            self.name_bytes = 0;
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
                        slot.insert(name);
                    }
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        self.name_bytes -= slot.insert(name).capacity();
                    }
                }
            }
        }
        if let Entry::User(user) = entry {
            for result in user.tool_results() {
                if self.asks.contains(&result.tool_use_id) {
                    self.apply_result(&result.tool_use_id, Some(result));
                }
            }
        }
    }

    fn apply_result(&mut self, id: &str, result: Option<&ToolResultBlock>) {
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
            self.apply_result(&id, result);
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
