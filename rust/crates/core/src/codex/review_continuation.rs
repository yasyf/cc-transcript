use std::collections::{HashMap, HashSet};

use memchr::memchr_iter;

use super::lower::{blocks_text, lower_entry};
use super::parse::parse_codex_line;
use super::types::{CodexEntry, CodexItem, EventMsg, ResponseItemPayload};
use crate::snapshot::SnapshotError;
use crate::types::Entry;
use crate::value::field_str;

#[derive(Default)]
struct Echoes {
    texts: HashSet<String>,
    counts: HashMap<String, usize>,
    events: HashMap<String, Vec<usize>>,
    payload_bytes: usize,
    max_event_capacity: usize,
}

impl Echoes {
    fn ingest_growth_bound(&self) -> Option<usize> {
        use crate::snapshot_ledger::{map_growth, set_growth};
        let peak = |old: usize, growth: usize| {
            if growth == 0 {
                Some(0)
            } else {
                old.checked_add(growth)
            }
        };
        let counts = peak(
            self.counts
                .capacity()
                .checked_mul(std::mem::size_of::<(String, usize)>())?,
            map_growth(&self.counts, 1),
        )?;
        let texts = peak(
            self.texts
                .capacity()
                .checked_mul(std::mem::size_of::<String>())?,
            set_growth(&self.texts, 1),
        )?;
        let events = peak(
            self.events
                .capacity()
                .checked_mul(std::mem::size_of::<(String, Vec<usize>)>())?,
            map_growth(&self.events, 1),
        )?;
        let response = counts.checked_add(texts)?;
        let event = events.checked_add(
            self.max_event_capacity
                .max(4)
                .checked_mul(2)?
                .checked_mul(std::mem::size_of::<usize>())?,
        )?;
        Some(response.max(event))
    }

    fn key_bytes(&self, text: &str) -> usize {
        self.texts.get(text).map_or(0, String::capacity)
            + self
                .counts
                .get_key_value(text)
                .map_or(0, |(key, _)| key.capacity())
            + self.events.get_key_value(text).map_or(0, |(key, rows)| {
                key.capacity() + rows.capacity() * std::mem::size_of::<usize>()
            })
    }
    fn accounted_bytes(&self) -> usize {
        self.texts.capacity() * std::mem::size_of::<String>()
            + self.counts.capacity() * std::mem::size_of::<(String, usize)>()
            + self.events.capacity() * std::mem::size_of::<(String, Vec<usize>)>()
            + self.payload_bytes
    }
    fn add_response(&mut self, text: String) -> Option<usize> {
        let before = self.key_bytes(&text);
        let count = self.counts.entry(text.clone()).or_default();
        *count += 1;
        let affected = if *count == 1 {
            self.texts.insert(text.clone());
            self.events
                .get(&text)
                .and_then(|rows| rows.first().copied())
        } else {
            None
        };
        self.payload_bytes = self.payload_bytes - before + self.key_bytes(&text);
        affected
    }
    fn remove_response(&mut self, text: &str) -> Option<usize> {
        let before = self.key_bytes(text);
        let count = self.counts.get_mut(text).unwrap();
        *count -= 1;
        let affected = if *count == 0 {
            self.counts.remove(text);
            self.texts.remove(text);
            self.events.get(text).and_then(|rows| rows.first().copied())
        } else {
            None
        };
        self.payload_bytes = self.payload_bytes - before + self.key_bytes(text);
        affected
    }
    fn add_event(&mut self, text: &str, position: usize) {
        let before = self.key_bytes(text);
        let rows = self.events.entry(text.to_owned()).or_default();
        rows.push(position);
        self.max_event_capacity = self.max_event_capacity.max(rows.capacity());
        self.payload_bytes = self.payload_bytes - before + self.key_bytes(text);
    }
    fn remove_event(&mut self, text: &str, position: usize) {
        let before = self.key_bytes(text);
        let rows = self.events.get_mut(text).unwrap();
        assert_eq!(rows.pop(), Some(position));
        if rows.is_empty() {
            self.events.remove(text);
        }
        self.payload_bytes = self.payload_bytes - before + self.key_bytes(text);
    }
}

/// Canonical Codex rows replacing the retained suffix beginning at `first_changed`.
pub struct CodexReviewDelta {
    pub first_changed: usize,
    pub entries: Vec<Entry>,
    pub source_lines: Vec<usize>,
}

/// Retains parsed Codex normalization facts across append-only source updates.
#[derive(Default)]
pub struct CodexReviewContinuation {
    entries: Vec<CodexEntry>,
    entry_charges: Vec<usize>,
    entry_bytes: usize,
    fact_strings: usize,
    models: Vec<(usize, String)>,
    cwds: Vec<(usize, String)>,
    meta: Option<usize>,
    user: Echoes,
    assistant: Echoes,
    tail: Vec<u8>,
    tail_has_entry: bool,
    next_line: usize,
    #[cfg(test)]
    parsed_lines: usize,
    #[cfg(test)]
    lowered_entries: usize,
}

impl CodexReviewContinuation {
    pub fn ingest_peak_bound(&self, source_bytes: usize) -> Option<usize> {
        use crate::snapshot_ledger::vec_growth;
        let bytes = source_bytes.checked_add(self.tail.len())?;
        let parsed = crate::snapshot_memory::dom_parse_bound(bytes)?
            .checked_add(bytes.checked_mul(16)?)?
            .checked_add(4 * std::mem::size_of::<CodexEntry>() + 8192)?;
        let peak = |old: usize, growth: usize| {
            if growth == 0 {
                Some(0)
            } else {
                old.checked_add(growth)
            }
        };
        let entries = peak(
            self.entries
                .capacity()
                .checked_mul(std::mem::size_of::<CodexEntry>())?,
            vec_growth(&self.entries, 1),
        )?
        .checked_add(peak(
            self.entry_charges
                .capacity()
                .checked_mul(std::mem::size_of::<usize>())?,
            vec_growth(&self.entry_charges, 1),
        )?)?;
        let metadata = peak(
            self.models
                .capacity()
                .checked_mul(std::mem::size_of::<(usize, String)>())?,
            vec_growth(&self.models, 1),
        )?
        .checked_add(peak(
            self.cwds
                .capacity()
                .checked_mul(std::mem::size_of::<(usize, String)>())?,
            vec_growth(&self.cwds, 1),
        )?)?;
        let facts = metadata
            .max(self.user.ingest_growth_bound()?)
            .max(self.assistant.ingest_growth_bound()?);
        parsed.checked_add(entries)?.checked_add(facts)
    }

    pub fn accounted_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.entries.capacity() * std::mem::size_of::<CodexEntry>()
            + self.entry_charges.capacity() * std::mem::size_of::<usize>()
            + self.entry_bytes
            + self.fact_strings
            + (self.models.capacity() + self.cwds.capacity())
                * std::mem::size_of::<(usize, String)>()
            + self.user.accounted_bytes()
            + self.assistant.accounted_bytes()
            + self.tail.capacity()
    }
    pub fn canonical_rows(&mut self) -> CodexReviewDelta {
        self.delta(0)
    }

    /// Applies newly appended bytes, returning only the canonical suffix that changed.
    pub fn append(&mut self, bytes: &[u8]) -> Result<CodexReviewDelta, SnapshotError> {
        let changed = self.ingest(bytes)?;
        Ok(self.delta(changed))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn source_line(&self, position: usize) -> usize {
        self.entries[position].line_index
    }
    pub fn entry_bound(&self, position: usize) -> usize {
        let previous = position
            .checked_sub(1)
            .map_or(0, |index| self.entry_charges[index]);
        let meta = self.meta.map_or(0, |index| self.entry_charges[index]);
        let cwd = self
            .cwds
            .partition_point(|(index, _)| *index <= position)
            .checked_sub(1)
            .map_or(0, |index| self.cwds[index].1.len());
        let model = self
            .models
            .partition_point(|(index, _)| *index <= position)
            .checked_sub(1)
            .or_else(|| (!self.models.is_empty()).then_some(0))
            .map_or(0, |index| self.models[index].1.len());
        self.entry_charges[position]
            + previous
            + meta * 2
            + cwd
            + model
            + std::mem::size_of::<Entry>()
    }

    pub fn ingest(&mut self, bytes: &[u8]) -> Result<usize, SnapshotError> {
        let mut first_changed = self.entries.len();
        if bytes.is_empty() {
            return Ok(first_changed);
        }
        let bytes = if !self.tail.is_empty() && bytes[0] == b'\n' {
            self.tail.clear();
            self.tail_has_entry = false;
            self.next_line += 1;
            &bytes[1..]
        } else {
            if self.tail_has_entry {
                let entry = self.entries.pop().unwrap();
                self.entry_bytes -= self.entry_charges.pop().unwrap();
                first_changed = self.remove_facts(&entry, self.entries.len());
                self.tail_has_entry = false;
            }
            bytes
        };
        let mut pending = std::mem::take(&mut self.tail);
        pending.extend_from_slice(bytes);
        let mut start = 0;
        for end in memchr_iter(b'\n', &pending) {
            first_changed = first_changed.min(self.append_line(&pending[start..end]));
            self.next_line += 1;
            start = end + 1;
        }
        if start < pending.len() {
            let before = self.entries.len();
            first_changed = first_changed.min(self.append_line(&pending[start..]));
            self.tail_has_entry = self.entries.len() != before;
            self.tail.extend_from_slice(&pending[start..]);
        }
        Ok(first_changed)
    }

    fn append_line(&mut self, line: &[u8]) -> usize {
        #[cfg(test)]
        {
            self.parsed_lines += 1;
        }
        let position = self.entries.len();
        parse_codex_line(line, self.next_line, &mut self.entries);
        if self.entries.len() == position {
            return position;
        }
        let entry = self.entries.pop().unwrap();
        let changed = self.add_facts(&entry, position);
        let charge = crate::snapshot_memory::codex_entry_charge(&entry);
        let bytes = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
        self.entry_bytes += bytes;
        self.entry_charges.push(bytes);
        self.entries.push(entry);
        changed
    }

    fn add_facts(&mut self, entry: &CodexEntry, position: usize) -> usize {
        let mut changed = position;
        match &entry.item {
            CodexItem::SessionMeta(meta)
                if self.meta.is_none() && meta.id.as_deref().is_some_and(|id| !id.is_empty()) =>
            {
                self.meta = Some(position);
                changed = 0;
            }
            CodexItem::TurnContext(value) => {
                if let Some(model) = field_str(value, "model") {
                    if self.models.is_empty() {
                        changed = 0;
                    }
                    let model = model.to_owned();
                    self.fact_strings += model.capacity();
                    self.models.push((position, model));
                }
                if let Some(cwd) = field_str(value, "cwd") {
                    let cwd = cwd.to_owned();
                    self.fact_strings += cwd.capacity();
                    self.cwds.push((position, cwd));
                }
            }
            CodexItem::ResponseItem(response) => {
                if let ResponseItemPayload::Message { role, content, .. } = &response.payload {
                    let affected = match role.as_deref() {
                        Some("user") => self.user.add_response(blocks_text(content)),
                        Some("assistant") => self.assistant.add_response(blocks_text(content)),
                        _ => None,
                    };
                    if let Some(affected) = affected {
                        changed = changed.min(affected);
                    }
                }
            }
            CodexItem::EventMsg(EventMsg::UserMessage { message, .. }) => {
                self.user
                    .add_event(message.as_deref().unwrap_or_default(), position);
            }
            CodexItem::EventMsg(EventMsg::AgentMessage { message, .. }) => {
                self.assistant
                    .add_event(message.as_deref().unwrap_or_default(), position);
            }
            _ => {}
        }
        changed
    }

    fn remove_facts(&mut self, entry: &CodexEntry, position: usize) -> usize {
        let mut changed = position;
        match &entry.item {
            CodexItem::SessionMeta(_) if self.meta == Some(position) => {
                self.meta = None;
                changed = 0;
            }
            CodexItem::TurnContext(value) => {
                if field_str(value, "model").is_some() {
                    let (index, model) = self.models.pop().unwrap();
                    self.fact_strings -= model.capacity();
                    assert_eq!(index, position);
                    if self.models.is_empty() {
                        changed = 0;
                    }
                }
                if field_str(value, "cwd").is_some() {
                    let (index, cwd) = self.cwds.pop().unwrap();
                    self.fact_strings -= cwd.capacity();
                    assert_eq!(index, position);
                }
            }
            CodexItem::ResponseItem(response) => {
                if let ResponseItemPayload::Message { role, content, .. } = &response.payload {
                    let affected = match role.as_deref() {
                        Some("user") => self.user.remove_response(&blocks_text(content)),
                        Some("assistant") => self.assistant.remove_response(&blocks_text(content)),
                        _ => None,
                    };
                    if let Some(affected) = affected {
                        changed = changed.min(affected);
                    }
                }
            }
            CodexItem::EventMsg(EventMsg::UserMessage { message, .. }) => {
                self.user
                    .remove_event(message.as_deref().unwrap_or_default(), position);
            }
            CodexItem::EventMsg(EventMsg::AgentMessage { message, .. }) => {
                self.assistant
                    .remove_event(message.as_deref().unwrap_or_default(), position);
            }
            _ => {}
        }
        changed
    }

    pub fn canonical_entry(&self, position: usize) -> Entry {
        let (thread_id, initial_cwd) = match self.meta.map(|index| &self.entries[index].item) {
            Some(CodexItem::SessionMeta(meta)) => {
                (meta.id.as_deref().unwrap(), meta.cwd.as_deref())
            }
            _ => ("", None),
        };
        let before = self.cwds.partition_point(|(index, _)| *index <= position);
        let cwd = before
            .checked_sub(1)
            .map(|index| self.cwds[index].1.as_str())
            .or(initial_cwd);
        lower_entry(
            &self.entries[position],
            position,
            position.checked_sub(1).map(|index| &self.entries[index]),
            thread_id,
            cwd,
            &self.models,
            &self.user.texts,
            &self.assistant.texts,
        )
    }

    fn delta(&mut self, first_changed: usize) -> CodexReviewDelta {
        let (thread_id, initial_cwd) = match self.meta.map(|index| &self.entries[index].item) {
            Some(CodexItem::SessionMeta(meta)) => {
                (meta.id.as_deref().unwrap(), meta.cwd.as_deref())
            }
            _ => ("", None),
        };
        let before = self
            .cwds
            .partition_point(|(index, _)| *index < first_changed);
        let mut cwd = before
            .checked_sub(1)
            .map(|index| self.cwds[index].1.as_str())
            .or(initial_cwd);
        let mut entries = Vec::with_capacity(self.entries.len() - first_changed);
        let mut source_lines = Vec::with_capacity(entries.capacity());
        for position in first_changed..self.entries.len() {
            let entry = &self.entries[position];
            if let CodexItem::TurnContext(value) = &entry.item {
                if let Some(next) = field_str(value, "cwd") {
                    cwd = Some(next);
                }
            }
            entries.push(lower_entry(
                entry,
                position,
                position.checked_sub(1).map(|index| &self.entries[index]),
                thread_id,
                cwd,
                &self.models,
                &self.user.texts,
                &self.assistant.texts,
            ));
            source_lines.push(entry.line_index);
        }
        #[cfg(test)]
        {
            self.lowered_entries += entries.len();
        }
        CodexReviewDelta {
            first_changed,
            entries,
            source_lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{lower, parse_codex_bytes};
    use sonic_rs::{json, Value};

    fn line(kind: &str, payload: Value) -> Vec<u8> {
        let mut bytes = sonic_rs::to_vec(&json!({
            "timestamp": "2026-11-02T06:00:00Z",
            "type": kind,
            "payload": payload
        }))
        .unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn message(role: &str, text: &str) -> Vec<u8> {
        line(
            "response_item",
            json!({
                "type": "message", "role": role,
                "content": [{"type": "input_text", "text": text}]
            }),
        )
    }

    fn apply(rows: &mut Vec<Entry>, lines: &mut Vec<usize>, delta: CodexReviewDelta) {
        assert_eq!(delta.entries.len(), delta.source_lines.len());
        assert!(delta.first_changed <= rows.len());
        rows.truncate(delta.first_changed);
        rows.extend(delta.entries);
        lines.truncate(delta.first_changed);
        lines.extend(delta.source_lines);
    }

    fn assert_canonical(source: &[u8], rows: &[Entry], lines: &[usize]) {
        let parsed = parse_codex_bytes(source);
        assert_eq!(
            lines,
            parsed
                .entries
                .iter()
                .map(|entry| entry.line_index)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            sonic_rs::to_vec(&rows).unwrap(),
            sonic_rs::to_vec(&lower(&parsed).entries).unwrap()
        );
    }

    fn sample() -> Vec<u8> {
        [
            line("session_meta", json!({"id":"thread", "cwd":"/initial"})),
            line("event_msg", json!({"type":"user_message", "message":"héllo 🌲"})),
            b"\nnot json\n".to_vec(),
            line("turn_context", json!({"model":"first", "cwd":"/next"})),
            message("user", "héllo 🌲"),
            line("event_msg", json!({"type":"agent_message", "message":"answer"})),
            message("assistant", "answer"),
            line("response_item", json!({"type":"function_call", "name":"exec_command", "call_id":"call", "arguments":"{\"cmd\":\"true\"}"})),
            line("response_item", json!({"type":"function_call_output", "call_id":"call", "output":"done"})),
            line("turn_context", json!({"model":"second"})),
            line("compacted", json!({"message":"summary"})),
        ].concat()
    }

    #[test]
    fn every_byte_boundary_matches_the_existing_cold_normalizer() {
        let source = sample();
        for split in 0..=source.len() {
            let mut state = CodexReviewContinuation::default();
            let mut rows = Vec::new();
            let mut lines = Vec::new();
            apply(
                &mut rows,
                &mut lines,
                state.append(&source[..split]).unwrap(),
            );
            assert_canonical(&source[..split], &rows, &lines);
            apply(
                &mut rows,
                &mut lines,
                state.append(&source[split..]).unwrap(),
            );
            assert_canonical(&source, &rows, &lines);
        }
    }

    #[test]
    fn bytewise_continuation_replaces_provisional_rows_and_preserves_line_numbers() {
        let source = sample();
        let mut state = CodexReviewContinuation::default();
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        for end in 1..=source.len() {
            apply(
                &mut rows,
                &mut lines,
                state.append(&source[end - 1..end]).unwrap(),
            );
            assert_canonical(&source[..end], &rows, &lines);
        }
    }

    #[test]
    fn late_echoes_return_the_earliest_affected_suffix() {
        for (role, kind) in [("user", "user_message"), ("assistant", "agent_message")] {
            let mut state = CodexReviewContinuation::default();
            let first = line("event_msg", json!({"type":kind, "message":"echo"}));
            let mut rows = Vec::new();
            let mut lines = Vec::new();
            apply(&mut rows, &mut lines, state.append(&first).unwrap());
            let next = message(role, "echo");
            let delta = state.append(&next).unwrap();
            assert_eq!(delta.first_changed, 0);
            assert!(matches!(delta.entries.first(), Some(Entry::Other(_))));
            apply(&mut rows, &mut lines, delta);
            assert_canonical(&[first, next].concat(), &rows, &lines);
        }
    }

    #[test]
    fn first_metadata_and_first_model_backfill_match_cold_rows() {
        let mut state = CodexReviewContinuation::default();
        let mut source = message("assistant", "before metadata");
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        apply(&mut rows, &mut lines, state.append(&source).unwrap());
        for next in [
            line("session_meta", json!({"id":"late", "cwd":"/late"})),
            line("turn_context", json!({"model":"late-model"})),
        ] {
            let delta = state.append(&next).unwrap();
            assert_eq!(delta.first_changed, 0);
            apply(&mut rows, &mut lines, delta);
            source.extend(next);
            assert_canonical(&source, &rows, &lines);
        }
    }

    #[test]
    fn repeated_responses_and_replaced_unterminated_responses_keep_exact_echo_facts() {
        let chunks = [
            line(
                "event_msg",
                json!({"type":"user_message", "message":"echo"}),
            ),
            message("user", "echo"),
            message("user", "echo"),
            line(
                "event_msg",
                json!({"type":"agent_message", "message":"plain"}),
            ),
        ];
        let mut source = chunks.concat();
        source.pop();
        let mut state = CodexReviewContinuation::default();
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        apply(&mut rows, &mut lines, state.append(&source).unwrap());
        let response = message("assistant", "plain");
        for next in [b" ".as_slice(), b"\n".as_slice(), response.as_slice()] {
            apply(&mut rows, &mut lines, state.append(next).unwrap());
            source.extend_from_slice(next);
            assert_canonical(&source, &rows, &lines);
        }
    }

    #[test]
    fn fixed_append_and_warm_calls_do_not_reparse_or_relower_the_old_population() {
        for count in [32, 10_000] {
            let mut source = line("session_meta", json!({"id":"thread"}));
            for index in 0..count {
                source.extend(message("user", &format!("{index}:{}", "x".repeat(128))));
            }
            let mut state = CodexReviewContinuation::default();
            let mut rows = Vec::new();
            let mut lines = Vec::new();
            apply(&mut rows, &mut lines, state.append(&source).unwrap());
            let before = (state.parsed_lines, state.lowered_entries);
            let next = message("assistant", "one new row");
            let delta = state.append(&next).unwrap();
            assert_eq!(delta.first_changed, count + 1);
            assert_eq!(delta.entries.len(), 1);
            assert_eq!(
                (
                    state.parsed_lines - before.0,
                    state.lowered_entries - before.1
                ),
                (1, 1)
            );
            apply(&mut rows, &mut lines, delta);
            source.extend(next);
            assert_canonical(&source, &rows, &lines);
            let before = (state.parsed_lines, state.lowered_entries);
            for _ in 0..3 {
                let warm = state.append(&[]).unwrap();
                assert_eq!(warm.first_changed, count + 2);
                assert!(warm.entries.is_empty() && warm.source_lines.is_empty());
            }
            assert_eq!((state.parsed_lines, state.lowered_entries), before);
        }
    }
}
