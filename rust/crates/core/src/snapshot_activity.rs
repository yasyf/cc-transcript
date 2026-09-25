use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use crate::activity::{lift_session_index_tail, lower_edit, Hunk, LiftedSession, ToolUse, Turn};
use crate::snapshot_memory::{block_charge, value_charge, MemoryCharge};
use crate::toolcall::{parse_tool_call, ToolCall};
use crate::types::{ContentBlock, Entry};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityWork {
    pub indexed_entries: usize,
    pub parsed_tool_calls: usize,
    pub copied_index_entries: usize,
}

#[derive(Debug)]
struct CachedCall {
    event: usize,
    ordinal: usize,
    id: String,
    call: ToolCall,
    edits: Vec<(String, Vec<Hunk>)>,
    accounted_bytes: usize,
}

#[derive(Debug, Clone)]
struct CachedTurn {
    prompt: String,
    bounds: Range<usize>,
    started: Option<usize>,
    ended: Option<usize>,
    calls: Vec<Arc<CachedCall>>,
}

#[derive(Debug, Clone, Copy)]
struct ResultPosition {
    event: usize,
    ordinal: usize,
    charge: MemoryCharge,
}

#[derive(Debug, Clone, Default)]
pub struct ActivityIndex {
    entries: usize,
    turns: Arc<Vec<Arc<CachedTurn>>>,
    uuids: Arc<HashMap<String, usize>>,
    results: Arc<HashMap<String, ResultPosition>>,
    work: ActivityWork,
}

impl ActivityIndex {
    pub fn new(entries: &[&Entry], openers: Option<&[bool]>) -> Self {
        Self::default().append_tail(entries, openers)
    }

    pub fn append(
        &self,
        all_entries: &[&Entry],
        first_new_index: usize,
        openers: Option<&[bool]>,
    ) -> Self {
        assert_eq!(first_new_index, self.entries);
        assert!(first_new_index <= all_entries.len());
        if let Some(flags) = openers {
            assert_eq!(flags.len(), all_entries.len());
        }
        self.clone().append_tail(
            &all_entries[first_new_index..],
            openers.map(|flags| &flags[first_new_index..]),
        )
    }

    pub fn append_tail(self, entries: &[&Entry], openers: Option<&[bool]>) -> Self {
        if let Some(flags) = openers {
            assert_eq!(flags.len(), entries.len());
        }
        let first_new_index = self.entries;
        let tail = lift_session_index_tail(entries, openers, !self.turns.is_empty());
        let mut next = self;
        next.entries += entries.len();
        next.work = ActivityWork {
            indexed_entries: entries.len(),
            ..ActivityWork::default()
        };
        for (local, entry) in entries.iter().enumerate() {
            let event = first_new_index + local;
            if let Some(meta) = entry.meta() {
                if !next.uuids.contains_key(&meta.uuid) {
                    if Arc::strong_count(&next.uuids) > 1 {
                        next.work.copied_index_entries += next.uuids.len();
                    }
                    Arc::make_mut(&mut next.uuids).insert(meta.uuid.clone(), event);
                }
            }
            if let Entry::User(user) = entry {
                for (ordinal, block) in user.blocks().iter().enumerate() {
                    let ContentBlock::ToolResult(result) = block else {
                        continue;
                    };
                    if Arc::strong_count(&next.results) > 1 {
                        next.work.copied_index_entries += next.results.len();
                    }
                    Arc::make_mut(&mut next.results).insert(
                        result.tool_use_id.clone(),
                        ResultPosition {
                            event,
                            ordinal,
                            charge: block_charge(block),
                        },
                    );
                }
            }
        }
        if !tail.turns.is_empty() && Arc::strong_count(&next.turns) > 1 {
            next.work.copied_index_entries += next.turns.len();
        }
        for (position, indexed) in tail.turns.into_iter().enumerate() {
            let mut calls = Vec::new();
            for (local, entry) in entries
                .iter()
                .enumerate()
                .take(indexed.end)
                .skip(indexed.start)
            {
                let Entry::Assistant(assistant) = entry else {
                    continue;
                };
                for (ordinal, block) in assistant.blocks.iter().enumerate() {
                    let ContentBlock::ToolUse(tool) = block else {
                        continue;
                    };
                    let call = parse_tool_call(&tool.name, &tool.input);
                    let edits = lower_edit(&call);
                    let id = tool.id.clone();
                    let accounted_bytes = size_of::<CachedCall>()
                        + id.capacity()
                        + call_bytes(&call, value_charge(&tool.input).opaque_dom_accounted_bytes)
                        + edits.capacity() * size_of::<(String, Vec<Hunk>)>()
                        + edits
                            .iter()
                            .map(|(path, hunks)| {
                                path.capacity()
                                    + hunks.capacity() * size_of::<Hunk>()
                                    + hunks
                                        .iter()
                                        .map(|hunk| hunk.old.capacity() + hunk.new.capacity())
                                        .sum::<usize>()
                            })
                            .sum::<usize>();
                    calls.push(Arc::new(CachedCall {
                        event: first_new_index + local,
                        ordinal,
                        id,
                        call,
                        edits,
                        accounted_bytes,
                    }));
                    next.work.parsed_tool_calls += 1;
                }
            }
            let turn = CachedTurn {
                prompt: indexed.prompt,
                bounds: first_new_index + indexed.start..first_new_index + indexed.end,
                started: indexed.started_idx.map(|i| first_new_index + i),
                ended: indexed.ended_idx.map(|i| first_new_index + i),
                calls,
            };
            if position == 0 && tail.continued {
                let prior = Arc::make_mut(&mut next.turns).last_mut().unwrap();
                if Arc::strong_count(prior) > 1 {
                    next.work.copied_index_entries += prior.calls.len();
                }
                let prior = Arc::make_mut(prior);
                prior.bounds.end = turn.bounds.end;
                prior.started = prior.started.or(turn.started);
                prior.ended = turn.ended.or(prior.ended);
                prior.calls.extend(turn.calls);
            } else {
                Arc::make_mut(&mut next.turns).push(Arc::new(turn));
            }
        }
        next
    }

    pub fn entry_count(&self) -> usize {
        self.entries
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn prompt(&self, index: usize) -> Option<&str> {
        self.turns.get(index).map(|turn| turn.prompt.as_str())
    }

    pub fn turn_bounds(&self, index: usize) -> Option<Range<usize>> {
        self.turns.get(index).map(|turn| turn.bounds.clone())
    }

    pub fn turn_of_event(&self, index: usize) -> Option<usize> {
        (index < self.entries).then(|| self.turns.partition_point(|turn| turn.bounds.end <= index))
    }

    pub fn turn_of_uuid(&self, uuid: &str) -> Option<usize> {
        self.uuids
            .get(uuid)
            .and_then(|&index| self.turn_of_event(index))
    }

    pub fn event_of_uuid(&self, uuid: &str) -> Option<usize> {
        self.uuids.get(uuid).copied()
    }

    pub fn call_at(&self, event: usize, block_ordinal: usize) -> Option<&ToolCall> {
        self.turns
            .get(self.turn_of_event(event)?)?
            .calls
            .iter()
            .find(|call| call.event == event && call.ordinal == block_ordinal)
            .map(|call| &call.call)
    }

    pub fn work(&self) -> ActivityWork {
        self.work
    }

    pub fn project_turn<'a>(&self, entries: &[&'a Entry], index: usize) -> Option<Turn<'a>> {
        assert_eq!(entries.len(), self.entries);
        self.project_turn_with(index, |position| entries[position])
    }

    pub fn projected_bytes(&self, index: usize) -> Option<usize> {
        let turn = self.turns.get(index)?;
        Some(
            size_of::<Turn<'_>>()
                + turn.prompt.capacity()
                + turn.bounds.len() * size_of::<&Entry>()
                + turn.calls.len() * size_of::<ToolUse<'_>>()
                + turn
                    .calls
                    .iter()
                    .map(|call| call.accounted_bytes)
                    .sum::<usize>(),
        )
    }

    pub fn repeated_result_bytes(&self, index: usize) -> usize {
        self.turns
            .get(index)
            .into_iter()
            .flat_map(|turn| turn.calls.iter())
            .filter_map(|call| self.results.get(&call.id))
            .fold(0usize, |total, result| {
                total
                    .saturating_add(result.charge.owned_capacity_bytes)
                    .saturating_add(result.charge.opaque_dom_accounted_bytes)
            })
    }

    pub fn result_events(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        self.turns
            .get(index)
            .into_iter()
            .flat_map(|turn| turn.calls.iter())
            .filter_map(|call| self.results.get(&call.id).map(|position| position.event))
    }

    pub fn project_turn_with<'a>(
        &self,
        index: usize,
        entry: impl Fn(usize) -> &'a Entry,
    ) -> Option<Turn<'a>> {
        let turn = self.turns.get(index)?;
        Some(Turn {
            index,
            prompt: turn.prompt.clone(),
            started_at: turn.started.map(|i| entry(i).meta().unwrap().timestamp),
            ended_at: turn.ended.map(|i| entry(i).meta().unwrap().timestamp),
            events: turn.bounds.clone().map(&entry).collect(),
            tool_uses: turn
                .calls
                .iter()
                .map(|cached| {
                    let Entry::Assistant(assistant) = entry(cached.event) else {
                        unreachable!()
                    };
                    let ContentBlock::ToolUse(tool) = &assistant.blocks[cached.ordinal] else {
                        unreachable!()
                    };
                    let result = self.results.get(&cached.id).map(|position| {
                        let Entry::User(user) = entry(position.event) else {
                            unreachable!()
                        };
                        let ContentBlock::ToolResult(result) = &user.blocks()[position.ordinal]
                        else {
                            unreachable!()
                        };
                        (result, user.meta.timestamp)
                    });
                    ToolUse {
                        event_uuid: &assistant.meta.uuid,
                        tool_use_id: &tool.id,
                        name: &tool.name,
                        ts: assistant.meta.timestamp,
                        cwd: assistant.meta.cwd.as_deref(),
                        result: result.map(|(block, _)| block),
                        result_ts: result.map(|(_, ts)| ts),
                        turn_index: index,
                        call: cached.call.clone(),
                        edits: cached.edits.clone(),
                    }
                })
                .collect(),
        })
    }

    pub fn project<'a>(
        &self,
        session_id: &'a str,
        entries: &[&'a Entry],
        turn_indices: &[usize],
    ) -> LiftedSession<'a> {
        LiftedSession {
            session_id,
            turns: turn_indices
                .iter()
                .map(|&index| self.project_turn(entries, index).expect("valid turn index"))
                .collect(),
        }
    }

    pub fn accounted_allocations(&self) -> Vec<(usize, usize)> {
        let mut allocations = vec![
            (self as *const Self as usize, size_of::<Self>()),
            (
                Arc::as_ptr(&self.turns) as usize,
                size_of::<Vec<Arc<CachedTurn>>>()
                    + self.turns.capacity() * size_of::<Arc<CachedTurn>>(),
            ),
            (
                Arc::as_ptr(&self.uuids) as usize,
                size_of::<HashMap<String, usize>>()
                    + self.uuids.capacity() * size_of::<(String, usize)>()
                    + self.uuids.keys().map(String::capacity).sum::<usize>(),
            ),
            (
                Arc::as_ptr(&self.results) as usize,
                size_of::<HashMap<String, ResultPosition>>()
                    + self.results.capacity() * size_of::<(String, ResultPosition)>()
                    + self.results.keys().map(String::capacity).sum::<usize>(),
            ),
        ];
        for turn in self.turns.iter() {
            allocations.push((
                Arc::as_ptr(turn) as usize,
                size_of::<CachedTurn>()
                    + turn.prompt.capacity()
                    + turn.calls.capacity() * size_of::<Arc<CachedCall>>(),
            ));
            allocations.extend(
                turn.calls
                    .iter()
                    .map(|call| (Arc::as_ptr(call) as usize, call.accounted_bytes)),
            );
        }
        allocations
    }

    pub fn accounted_bytes(&self) -> usize {
        self.accounted_allocations()
            .iter()
            .map(|(_, bytes)| bytes)
            .sum()
    }

    pub fn append_container_reservation_bytes(
        &self,
        entries: usize,
        calls: usize,
        results: usize,
    ) -> usize {
        let growth = |capacity: usize, needed: usize, width: usize| {
            if needed > capacity {
                needed
                    .max(capacity)
                    .saturating_mul(2)
                    .max(8)
                    .saturating_mul(width)
            } else {
                0
            }
        };
        let uuid_copy = if Arc::strong_count(&self.uuids) > 1 {
            self.uuids.capacity() * size_of::<(String, usize)>()
                + self.uuids.keys().map(String::capacity).sum::<usize>()
        } else {
            0
        };
        let result_copy = if Arc::strong_count(&self.results) > 1 {
            self.results.capacity() * size_of::<(String, ResultPosition)>()
                + self.results.keys().map(String::capacity).sum::<usize>()
        } else {
            0
        };
        let turn_copy = if Arc::strong_count(&self.turns) > 1 {
            self.turns.capacity() * size_of::<Arc<CachedTurn>>()
        } else {
            0
        };
        let open_turn = self.turns.last().map_or(0, |turn| {
            growth(
                turn.calls.capacity(),
                turn.calls.len().saturating_add(calls),
                size_of::<Arc<CachedCall>>(),
            ) + if Arc::strong_count(turn) > 1 || Arc::strong_count(&self.turns) > 1 {
                size_of::<CachedTurn>()
                    + turn.prompt.capacity()
                    + turn.calls.capacity() * size_of::<Arc<CachedCall>>()
            } else {
                0
            }
        });
        uuid_copy
            .saturating_add(result_copy)
            .saturating_add(turn_copy)
            .saturating_add(open_turn)
            .saturating_add(growth(
                self.uuids.capacity(),
                self.uuids.len().saturating_add(entries),
                size_of::<(String, usize)>(),
            ))
            .saturating_add(growth(
                self.results.capacity(),
                self.results.len().saturating_add(results),
                size_of::<(String, ResultPosition)>(),
            ))
            .saturating_add(growth(
                self.turns.capacity(),
                self.turns.len().saturating_add(entries),
                size_of::<Arc<CachedTurn>>(),
            ))
            .saturating_add(entries.saturating_mul(size_of::<CachedTurn>()))
            .saturating_add(
                calls.saturating_mul(size_of::<CachedCall>() + 4 * size_of::<Arc<CachedCall>>()),
            )
    }
}

fn call_bytes(call: &ToolCall, opaque_dom_accounted_bytes: usize) -> usize {
    let fields = match call {
        ToolCall::Bash(c) => c.name.capacity() + c.command.capacity(),
        ToolCall::Edit(c) => {
            c.name.capacity() + c.file_path.capacity() + c.old.capacity() + c.new.capacity()
        }
        ToolCall::MultiEdit(c) => {
            c.name.capacity()
                + c.file_path.capacity()
                + c.edits.capacity() * size_of::<crate::toolcall::EditSpan>()
                + c.edits
                    .iter()
                    .map(|e| e.old.capacity() + e.new.capacity())
                    .sum::<usize>()
        }
        ToolCall::Write(c) => c.name.capacity() + c.file_path.capacity() + c.content.capacity(),
        ToolCall::Read(c) => c.name.capacity() + c.file_path.capacity(),
        ToolCall::NotebookEdit(c) => {
            c.name.capacity() + c.notebook_path.capacity() + c.new_source.capacity()
        }
        ToolCall::Grep(c) => c.name.capacity() + c.pattern.capacity(),
        ToolCall::Glob(c) => c.name.capacity() + c.pattern.capacity(),
        ToolCall::Task(c) => c.name.capacity() + c.prompt.capacity(),
        ToolCall::Workflow(c) => c.name.capacity(),
        ToolCall::Skill(c) => c.name.capacity() + c.skill.capacity(),
        ToolCall::TaskCreate(c) => c.name.capacity() + c.subject.capacity(),
        ToolCall::TaskUpdate(c) => c.name.capacity() + c.task_id.capacity(),
        ToolCall::ExitPlanMode(c) => c.name.capacity() + c.plan.capacity(),
        ToolCall::CodeMode(c) => c.name.capacity() + c.source.capacity(),
        ToolCall::ApplyPatch(c) => {
            c.name.capacity()
                + c.edits.capacity() * size_of::<crate::toolcall::PatchEdit>()
                + c.edits
                    .iter()
                    .map(|e| {
                        e.file_path.capacity()
                            + e.move_path.as_ref().map_or(0, String::capacity)
                            + e.hunks.capacity() * size_of::<crate::toolcall::Hunk>()
                            + e.hunks
                                .iter()
                                .map(|h| h.old.capacity() + h.new.capacity())
                                .sum::<usize>()
                    })
                    .sum::<usize>()
        }
        ToolCall::UpdatePlan(c) => {
            c.name.capacity()
                + c.explanation.as_ref().map_or(0, String::capacity)
                + c.plan
                    .as_ref()
                    .map_or(0, |plan| value_charge(plan).opaque_dom_accounted_bytes)
        }
        ToolCall::WriteStdin(c) => {
            c.name.capacity()
                + c.chars
                    .as_ref()
                    .map_or(0, |chars| value_charge(chars).opaque_dom_accounted_bytes)
        }
        ToolCall::SpanEdit(c) => {
            c.name.capacity() + c.file_path.capacity() + c.new.as_ref().map_or(0, String::capacity)
        }
        ToolCall::Other(c) => c.name.capacity() + c.error.as_ref().map_or(0, String::capacity),
    };
    fields + opaque_dom_accounted_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::lift_session_refs;
    use crate::parse::parse_entry;

    fn entry(raw: &str) -> Entry {
        parse_entry(sonic_rs::from_str(raw).unwrap()).unwrap()
    }

    fn user(uuid: &str, text: &str) -> Entry {
        entry(&format!(
            r#"{{"type":"user","uuid":"{uuid}","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{text}"}}}}"#
        ))
    }

    fn tools() -> Entry {
        entry(
            r#"{"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":"edit","name":"Edit","input":{"file_path":"a.rs","old_string":"old","new_string":"new"}},{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
        )
    }

    fn results() -> Entry {
        entry(
            r#"{"type":"user","uuid":"r","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","message":{"content":[{"type":"tool_result","tool_use_id":"edit","content":"first"},{"type":"tool_result","tool_use_id":"edit","content":"last","is_error":true},{"type":"tool_result","tool_use_id":"read","content":"read"}]}}"#,
        )
    }

    fn parity(index: &ActivityIndex, entries: &[&Entry], flags: Option<&[bool]>) {
        let projected = index.project("s", entries, &(0..index.turn_count()).collect::<Vec<_>>());
        let expected = lift_session_refs("s", entries, flags);
        assert_eq!(format!("{projected:?}"), format!("{expected:?}"));
    }

    #[test]
    fn repeated_result_charge_counts_each_owned_result_copy() {
        let mut calls = tools();
        let Entry::Assistant(assistant) = &mut calls else {
            panic!()
        };
        let ContentBlock::ToolUse(second) = &mut assistant.blocks[1] else {
            panic!()
        };
        second.id = "edit".into();
        let mut response = results();
        let Entry::User(user) = &mut response else {
            panic!()
        };
        let crate::types::UserContent::Blocks(blocks) = &mut user.content else {
            panic!()
        };
        let ContentBlock::ToolResult(result) = &mut blocks[1] else {
            panic!()
        };
        result.tool_use_result = Some(sonic_rs::json!(vec![0; 4096]));
        let entries = [calls, response];
        let refs: Vec<_> = entries.iter().collect();
        let index = ActivityIndex::new(&refs, None);
        assert!(index.repeated_result_bytes(0) >= 2 * 4096 * size_of::<sonic_rs::Value>());
    }

    #[test]
    fn append_reuses_calls_and_repairs_late_results_without_changing_prior() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let prior = ActivityIndex::new(&refs[..3], None);
        let next = prior.append(&refs, 3, None);
        parity(&prior, &refs[..3], None);
        parity(&next, &refs, None);
        assert_eq!(next.work().indexed_entries, 1);
        assert_eq!(next.work().parsed_tool_calls, 0);
        assert!(Arc::ptr_eq(&prior.turns[0], &next.turns[0]));
        assert!(!Arc::ptr_eq(&prior.turns[1], &next.turns[1]));
        assert!(prior.project_turn(&refs[..3], 0).unwrap().tool_uses[0]
            .result
            .is_none());
        assert!(
            next.project_turn(&refs, 0).unwrap().tool_uses[0]
                .result
                .unwrap()
                .is_error
        );
    }

    #[test]
    fn continuation_shares_cached_calls_and_keeps_first_uuid() {
        let entries = [
            user("same", "first"),
            tools(),
            results(),
            user("same", "second"),
        ];
        let refs: Vec<_> = entries.iter().collect();
        let prior = ActivityIndex::new(&refs[..2], None);
        let next = prior.append(&refs, 2, None);
        parity(&next, &refs, None);
        assert_eq!(prior.turn_bounds(0), Some(0..2));
        assert_eq!(next.turn_bounds(0), Some(0..3));
        assert_eq!(next.turn_bounds(1), Some(3..4));
        assert_eq!(next.turn_of_uuid("same"), Some(0));
        assert_eq!(next.event_of_uuid("same"), Some(0));
        assert_eq!(next.turn_of_event(3), Some(1));
        assert_eq!(next.turn_of_event(4), None);
        assert_eq!(next.turn_of_uuid("absent"), None);
        assert!(Arc::ptr_eq(
            &prior.turns[0].calls[0],
            &next.turns[0].calls[0]
        ));
    }

    #[test]
    fn every_append_boundary_matches_full_lift() {
        let entries = [
            tools(),
            user("u", "first"),
            tools(),
            user("v", "second"),
            results(),
        ];
        let refs: Vec<_> = entries.iter().collect();
        for boundary in 0..=refs.len() {
            let prior = ActivityIndex::new(&refs[..boundary], None);
            let next = prior.append(&refs, boundary, None);
            parity(&next, &refs, None);
            parity(&prior, &refs[..boundary], None);
        }
    }

    #[test]
    fn custom_classifier_and_empty_append_preserve_semantics() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let flags = [false, false, true, false];
        let prior = ActivityIndex::new(&refs[..2], Some(&flags[..2]));
        let next = prior.append(&refs, 2, Some(&flags));
        parity(&next, &refs, Some(&flags));
        let unchanged = next.append(&refs, refs.len(), Some(&flags));
        assert_eq!(unchanged.work(), ActivityWork::default());
        assert!(Arc::ptr_eq(&next.turns[0], &unchanged.turns[0]));
        assert!(Arc::ptr_eq(&next.uuids, &unchanged.uuids));
        assert!(Arc::ptr_eq(&next.results, &unchanged.results));
    }

    #[test]
    fn accounting_identifies_shared_payload_once_across_generations() {
        let entries = [user("u", "first"), tools(), user("v", "second")];
        let refs: Vec<_> = entries.iter().collect();
        let prior = ActivityIndex::new(&refs[..2], None);
        let next = prior.append(&refs, 2, None);
        let mut allocations = HashMap::new();
        for (identity, bytes) in prior
            .accounted_allocations()
            .into_iter()
            .chain(next.accounted_allocations())
        {
            if let Some(previous) = allocations.insert(identity, bytes) {
                assert_eq!(previous, bytes);
            }
        }
        let unique: usize = allocations.values().sum();
        assert!(unique < prior.accounted_bytes() + next.accounted_bytes());
        assert!(next.accounted_bytes() > prior.accounted_bytes());
    }

    #[test]
    fn projection_accessor_reads_only_selected_turn_and_its_results() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let index = ActivityIndex::new(&refs, None);
        let visited = std::cell::RefCell::new(std::collections::HashSet::new());
        let projected = index
            .project_turn_with(0, |position| {
                assert_ne!(position, 2);
                visited.borrow_mut().insert(position);
                &entries[position]
            })
            .unwrap();
        let expected = index.project_turn(&refs, 0).unwrap();
        assert_eq!(format!("{projected:?}"), format!("{expected:?}"));
        assert_eq!(
            *visited.borrow(),
            std::collections::HashSet::from([0, 1, 3])
        );
        assert!(index
            .project_turn_with(2, |_| panic!("out-of-range turn reads no entries"))
            .is_none());
    }

    #[test]
    fn projection_preflight_includes_cloned_payloads_and_vector_storage() {
        let entries = [user("u", "first"), tools(), user("v", "second")];
        let refs: Vec<_> = entries.iter().collect();
        let index = ActivityIndex::new(&refs, None);
        let turn = &index.turns[0];
        let expected = size_of::<Turn<'_>>()
            + turn.prompt.capacity()
            + 2 * size_of::<&Entry>()
            + 2 * size_of::<ToolUse<'_>>()
            + turn
                .calls
                .iter()
                .map(|call| call.accounted_bytes)
                .sum::<usize>();
        assert_eq!(index.projected_bytes(0), Some(expected));
        assert!(index.projected_bytes(0).unwrap() > index.projected_bytes(1).unwrap());
        assert_eq!(index.projected_bytes(2), None);
    }

    #[test]
    fn result_event_iterator_exposes_external_results_without_entry_reads() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let index = ActivityIndex::new(&refs, None);
        assert_eq!(index.result_events(0).collect::<Vec<_>>(), vec![3, 3]);
        assert_eq!(index.result_events(1).count(), 0);
        assert_eq!(index.result_events(2).count(), 0);
    }

    #[test]
    fn numeric_plan_charge_includes_separately_decoded_dom() {
        let arguments = format!("{{\"plan\":[{}]}}", vec!["0"; 4096].join(","));
        let raw = sonic_rs::Value::from(arguments.as_str());
        let call = parse_tool_call("update_plan", &raw);
        assert!(matches!(call, ToolCall::UpdatePlan(_)));
        let raw_charge = value_charge(&raw).opaque_dom_accounted_bytes;
        let total = call_bytes(&call, raw_charge);
        assert!(total >= raw_charge + 4096 * size_of::<sonic_rs::Value>());
        assert!(total > arguments.len() * 4);
    }

    #[test]
    fn write_stdin_charge_includes_separately_decoded_chars() {
        let arguments = format!("{{\"session_id\":1,\"chars\":\"{}\"}}", "x".repeat(8192));
        let raw = sonic_rs::Value::from(arguments.as_str());
        let call = parse_tool_call("write_stdin", &raw);
        assert!(matches!(call, ToolCall::WriteStdin(_)));
        let raw_charge = value_charge(&raw).opaque_dom_accounted_bytes;
        assert!(call_bytes(&call, raw_charge) >= raw_charge + 8192);
    }

    #[test]
    fn ordinary_object_fields_do_not_double_charge_shared_dom() {
        let raw: sonic_rs::Value =
            sonic_rs::from_str(r#"{"command":"true","description":{"deep":[1,2,3]}}"#).unwrap();
        let call = parse_tool_call("Bash", &raw);
        let ToolCall::Bash(bash) = &call else {
            panic!()
        };
        let raw_charge = value_charge(&raw).opaque_dom_accounted_bytes;
        assert_eq!(
            call_bytes(&call, raw_charge),
            raw_charge + bash.name.capacity() + bash.command.capacity()
        );
    }

    #[test]
    fn consuming_tail_append_reuses_unique_metadata_and_global_positions() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let mut index = ActivityIndex::default();
        let turns = Arc::as_ptr(&index.turns);
        let uuids = Arc::as_ptr(&index.uuids);
        let results = Arc::as_ptr(&index.results);
        for position in 0..refs.len() {
            index = std::mem::take(&mut index).append_tail(&refs[position..position + 1], None);
            assert_eq!(index.work().indexed_entries, 1);
            assert_eq!(index.work().copied_index_entries, 0);
            assert_eq!(Arc::as_ptr(&index.turns), turns);
            assert_eq!(Arc::as_ptr(&index.uuids), uuids);
            assert_eq!(Arc::as_ptr(&index.results), results);
            parity(&index, &refs[..position + 1], None);
        }
        assert_eq!(index.event_of_uuid("v"), Some(2));
        assert_eq!(index.result_events(0).collect::<Vec<_>>(), vec![3, 3]);
    }

    #[test]
    fn consuming_tail_append_preserves_shared_generation_and_local_classifier_flags() {
        let entries = [user("u", "first"), tools(), user("v", "second"), results()];
        let refs: Vec<_> = entries.iter().collect();
        let flags = [true, false, false, false];
        let prior = ActivityIndex::new(&refs[..2], Some(&flags[..2]));
        let next = prior.clone().append_tail(&refs[2..], Some(&flags[2..]));
        parity(&prior, &refs[..2], Some(&flags[..2]));
        parity(&next, &refs, Some(&flags));
        assert_eq!(prior.turn_bounds(0), Some(0..2));
        assert_eq!(next.turn_bounds(0), Some(0..4));
        assert!(next.work().copied_index_entries > 0);
        assert!(Arc::ptr_eq(
            &prior.turns[0].calls[0],
            &next.turns[0].calls[0]
        ));
    }
}
