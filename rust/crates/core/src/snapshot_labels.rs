use std::io::{self, Write};
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use serde::Serialize;
use sonic_rs::{json, Value};

use crate::snapshot::{Cancellation, SnapshotError, Status, TranscriptSnapshot, WorkLimits};
use crate::snapshot_activity::ActivityIndex;
use crate::snapshot_codec::{self, EventWire};
use crate::types::Entry;

const PAGE_EVENTS: usize = 256;
const PAGE_BYTES: usize = crate::snapshot::MAX_REPLY_BYTES - 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelBinding {
    pub owner_epoch: String,
    pub claimant: String,
    pub physical_generation: String,
    pub classifier_id: String,
    pub classifier_version: String,
    pub registry_generation: String,
    pub execution_id: String,
}

impl LabelBinding {
    fn accounted_bytes(&self) -> usize {
        self.owner_epoch.capacity()
            + self.claimant.capacity()
            + self.physical_generation.capacity()
            + self.classifier_id.capacity()
            + self.classifier_version.capacity()
            + self.registry_generation.capacity()
            + self.execution_id.capacity()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LabelUsage {
    pub events: usize,
    pub input_bytes: usize,
    pub items: usize,
    pub output_bytes: usize,
    pub activity_lifts: usize,
}

#[derive(Debug, Serialize)]
pub struct LabelPage {
    pub record_schema: &'static str,
    pub records_json: Vec<String>,
    pub event_start: usize,
    pub cursor: String,
    pub complete: bool,
}

struct PendingLabels {
    token: String,
    span: Range<usize>,
    user_positions: Vec<usize>,
    input_bytes: usize,
}

pub struct LabelPreparation {
    source: Arc<TranscriptSnapshot>,
    binding: LabelBinding,
    limits: WorkLimits,
    max_stage_bytes: usize,
    activity: ActivityIndex,
    pending: Option<PendingLabels>,
    usage: LabelUsage,
    failed: bool,
}

impl LabelPreparation {
    pub fn initial_reservation_bytes(binding: &LabelBinding) -> usize {
        size_of::<Self>()
            .saturating_add(binding.accounted_bytes())
            .saturating_add(4096)
            .saturating_add(Self::page_reservation_bytes())
    }

    pub fn next_operation_reservation_bytes(&self) -> usize {
        Self::page_reservation_bytes()
            .saturating_add(
                self.pending
                    .as_ref()
                    .map_or(0, |pending| self.append_reservation_bytes(pending)),
            )
            .saturating_add(self.publication_metadata_bytes())
    }

    fn page_reservation_bytes() -> usize {
        PAGE_BYTES
            .saturating_mul(4)
            .saturating_add(PAGE_EVENTS * (size_of::<String>() + size_of::<usize>()))
    }

    fn append_reservation_bytes(&self, pending: &PendingLabels) -> usize {
        let mut calls = 0usize;
        let mut results = 0usize;
        for position in pending.span.clone() {
            let entry = self.source.entry(position);
            calls += entry.tool_uses().count();
            results += entry.tool_results().count();
        }
        pending
            .input_bytes
            .saturating_mul(2)
            .saturating_add(pending.span.len() * (size_of::<&Entry>() + size_of::<bool>()))
            .saturating_add(self.activity.append_container_reservation_bytes(
                pending.span.len(),
                calls,
                results,
            ))
    }

    fn publication_metadata_bytes(&self) -> usize {
        size_of::<TranscriptSnapshot>()
            + self.source.canonical_path.as_os_str().len()
            + self.source.session_id.len()
            + self.source.chunks.len() * size_of::<Arc<crate::snapshot::EntryChunk>>()
            + self.source.fence.len()
            + self.binding.execution_id.len()
            + 7
    }

    pub fn operation_reservation_bytes(max_stage_bytes: usize) -> usize {
        max_stage_bytes
            .saturating_mul(2)
            .saturating_add(PAGE_BYTES.saturating_mul(4))
            .saturating_add(PAGE_EVENTS * (size_of::<String>() + size_of::<usize>()))
    }

    pub fn new(
        source: Arc<TranscriptSnapshot>,
        binding: LabelBinding,
        limits: WorkLimits,
        max_stage_bytes: usize,
    ) -> Result<Self, SnapshotError> {
        if source.id != binding.physical_generation {
            return Err(failure(
                Status::StaleCursor,
                "classifier source generation differs",
            ));
        }
        if [
            &binding.owner_epoch,
            &binding.claimant,
            &binding.classifier_id,
            &binding.classifier_version,
            &binding.registry_generation,
            &binding.execution_id,
        ]
        .iter()
        .any(|field| field.is_empty())
        {
            return Err(failure(
                Status::InvalidRequest,
                "classifier binding is incomplete",
            ));
        }
        let preparation = Self {
            source,
            binding,
            limits,
            max_stage_bytes,
            activity: ActivityIndex::default(),
            pending: None,
            usage: LabelUsage::default(),
            failed: false,
        };
        preparation.check_memory(preparation.accounted_bytes())?;
        Ok(preparation)
    }

    pub fn binding(&self) -> &LabelBinding {
        &self.binding
    }

    pub fn usage(&self) -> LabelUsage {
        self.usage
    }

    pub fn remaining_limits(&self) -> WorkLimits {
        WorkLimits {
            max_read_bytes: self
                .limits
                .max_read_bytes
                .saturating_sub(self.usage.input_bytes),
            max_events: self.limits.max_events.saturating_sub(self.usage.events),
            max_items: self.limits.max_items.saturating_sub(self.usage.items),
            max_output_bytes: self
                .limits
                .max_output_bytes
                .saturating_sub(self.usage.output_bytes),
            ..self.limits
        }
    }

    pub fn derived_classifier(&self) -> Value {
        json!({
            "id": self.binding.classifier_id,
            "version": format!("labels:{}", self.binding.execution_id),
        })
    }

    pub fn source(&self) -> &Arc<TranscriptSnapshot> {
        &self.source
    }

    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.binding.accounted_bytes()
            + self.activity.accounted_bytes()
            + self.pending.as_ref().map_or(0, |pending| {
                pending.token.capacity() + pending.user_positions.capacity() * size_of::<usize>()
            })
    }

    pub fn next_page(
        &mut self,
        page_token: String,
        binding: &LabelBinding,
        cancel: &Cancellation,
    ) -> Result<LabelPage, SnapshotError> {
        self.check(binding, cancel)?;
        if self.pending.is_some() {
            return Err(failure(
                Status::StaleCursor,
                "classifier page still awaits labels",
            ));
        }
        if page_token.is_empty() {
            return Err(failure(
                Status::InvalidRequest,
                "classifier cursor is empty",
            ));
        }
        let start = self.activity.entry_count();
        let used = self.usage;
        let mut stop = start;
        let mut input_bytes = 0usize;
        let mut user_positions = Vec::new();
        let mut page = LabelPage {
            record_schema: "cc-transcript.event/1",
            records_json: Vec::new(),
            event_start: start,
            cursor: page_token,
            complete: false,
        };
        let page_limit = PAGE_BYTES.min(
            self.limits
                .max_output_bytes
                .saturating_sub(self.usage.output_bytes),
        );
        let mut output_bytes = snapshot_codec::encoded_size(&page, page_limit)?;
        self.check_memory(
            self.accounted_bytes()
                .saturating_add(output_bytes)
                .saturating_add(page.cursor.len()),
        )?;
        self.usage.output_bytes = used.output_bytes + output_bytes;
        for position in start
            ..self
                .source
                .event_count
                .min(start.saturating_add(PAGE_EVENTS))
        {
            cancel.check(self.limits.deadline_unix_ms)?;
            if used.events + stop - start == self.limits.max_events {
                if stop > start {
                    break;
                }
                return Err(failure(
                    Status::EntryLimit,
                    "classifier event budget exhausted",
                ));
            }
            let charge = source_bytes(&self.source, position);
            if charge
                > self
                    .limits
                    .max_read_bytes
                    .saturating_sub(used.input_bytes)
                    .saturating_sub(input_bytes)
            {
                if stop > start {
                    break;
                }
                return Err(failure(
                    Status::SourceLimit,
                    "classifier input byte budget exhausted",
                ));
            }
            let entry = self.source.entry(position);
            if matches!(entry, Entry::User(_)) {
                if used.items + user_positions.len() == self.limits.max_items {
                    if stop > start {
                        break;
                    }
                    return Err(failure(
                        Status::OutputLimit,
                        "classifier label item budget exhausted",
                    ));
                }
                let wire = EventWire::new(position, entry);
                let (_, escaped) = record_size(&wire, PAGE_BYTES)?;
                let addition = escaped.saturating_add(usize::from(!page.records_json.is_empty()));
                if addition > page_limit.saturating_sub(output_bytes) {
                    if !page.records_json.is_empty() {
                        break;
                    }
                    return Err(failure(
                        Status::OutputLimit,
                        "complete user event exceeds classifier page budget",
                    ));
                }
                self.check_memory(
                    self.accounted_bytes()
                        .saturating_add(output_bytes)
                        .saturating_add(addition)
                        .saturating_add((user_positions.len() + 1) * size_of::<usize>())
                        .saturating_add(page.cursor.len()),
                )?;
                page.records_json
                    .push(snapshot_codec::encode(&wire, PAGE_BYTES)?);
                user_positions.push(position);
                output_bytes += addition;
            }
            input_bytes += charge;
            stop = position + 1;
            self.usage.events = used.events + stop - start;
            self.usage.input_bytes = used.input_bytes + input_bytes;
            self.usage.items = used.items + user_positions.len();
            self.usage.output_bytes = used.output_bytes + output_bytes;
        }
        cancel.check(self.limits.deadline_unix_ms)?;
        self.check_memory(
            self.accounted_bytes()
                .saturating_add(page.cursor.capacity().saturating_mul(2))
                .saturating_add(user_positions.capacity() * size_of::<usize>())
                .saturating_add(page.records_json.capacity() * size_of::<String>())
                .saturating_add(
                    page.records_json
                        .iter()
                        .map(String::capacity)
                        .sum::<usize>(),
                ),
        )?;
        self.pending = Some(PendingLabels {
            token: page.cursor.clone(),
            span: start..stop,
            user_positions,
            input_bytes,
        });
        Ok(page)
    }

    pub fn submit(
        &mut self,
        page_token: &str,
        labels: &[bool],
        binding: &LabelBinding,
        cancel: &Cancellation,
    ) -> Result<bool, SnapshotError> {
        self.check(binding, cancel)?;
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| failure(Status::StaleCursor, "classifier page is not pending"))?;
        if pending.token != page_token {
            return Err(failure(
                Status::StaleCursor,
                "classifier cursor addresses another page",
            ));
        }
        if labels.len() != pending.user_positions.len() {
            return Err(failure(
                Status::InvalidRequest,
                "classifier label count differs from user event count",
            ));
        }
        let reserve = self
            .accounted_bytes()
            .saturating_add(self.append_reservation_bytes(pending));
        self.check_memory(reserve)?;
        let mut flags = vec![false; pending.span.len()];
        for (&position, &label) in pending.user_positions.iter().zip(labels) {
            flags[position - pending.span.start] = label;
        }
        let entries = self.source.range(pending.span.clone());
        self.activity = std::mem::take(&mut self.activity).append_tail(&entries, Some(&flags));
        self.pending = None;
        self.usage.activity_lifts += 1;
        if let Err(error) = self.check_memory(self.accounted_bytes()) {
            self.activity = ActivityIndex::default();
            self.failed = true;
            return Err(error);
        }
        if let Err(error) = cancel.check(self.limits.deadline_unix_ms) {
            self.failed = true;
            return Err(error);
        }
        Ok(self.activity.entry_count() == self.source.event_count)
    }

    pub fn finish(
        self,
        binding: &LabelBinding,
        cancel: &Cancellation,
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        self.check(binding, cancel)?;
        if self.pending.is_some() || self.activity.entry_count() != self.source.event_count {
            return Err(failure(
                Status::Incomplete,
                "classifier labels are incomplete",
            ));
        }
        let metadata_bytes = self.publication_metadata_bytes();
        self.check_memory(self.accounted_bytes().saturating_add(metadata_bytes))?;
        Ok(Arc::new(TranscriptSnapshot {
            id: format!("labels:{}", self.binding.execution_id),
            canonical_path: self.source.canonical_path.clone(),
            stamp: self.source.stamp,
            provider: self.source.provider,
            session_id: self.source.session_id.clone(),
            chunks: self.source.chunks.clone(),
            activity: Arc::new(self.activity),
            committed_bytes: self.source.committed_bytes,
            provisional_tail: self.source.provisional_tail,
            fence: self.source.fence.clone(),
            event_count: self.source.event_count,
        }))
    }

    fn check(&self, binding: &LabelBinding, cancel: &Cancellation) -> Result<(), SnapshotError> {
        if binding.claimant != self.binding.claimant {
            return Err(failure(
                Status::PermissionDenied,
                "classifier cursor belongs to another claimant",
            ));
        }
        if binding != &self.binding {
            return Err(failure(
                Status::StaleCursor,
                "classifier cursor binding differs",
            ));
        }
        if self.failed {
            return Err(failure(Status::Incomplete, "classifier preparation failed"));
        }
        cancel.check(self.limits.deadline_unix_ms)
    }

    fn check_memory(&self, bytes: usize) -> Result<(), SnapshotError> {
        if bytes > self.max_stage_bytes {
            return Err(failure(
                Status::RetainedLimit,
                "classifier staging byte budget exceeded",
            ));
        }
        Ok(())
    }
}

fn failure(status: Status, reason: &str) -> SnapshotError {
    SnapshotError::new(status, reason)
}

fn source_bytes(source: &TranscriptSnapshot, position: usize) -> usize {
    let chunk = &source.chunks[source
        .chunks
        .partition_point(|chunk| chunk.start <= position)
        - 1];
    let charge = chunk.entry_charges[position - chunk.start];
    size_of::<Entry>()
        .saturating_add(charge.owned_capacity_bytes)
        .saturating_add(charge.opaque_dom_accounted_bytes)
}

fn record_size(record: &EventWire<'_>, max_bytes: usize) -> Result<(usize, usize), SnapshotError> {
    struct Counter {
        encoded: usize,
        escaped: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.encoded += bytes.len();
            self.escaped += bytes
                .iter()
                .map(|byte| match *byte {
                    b'"' | b'\\' => 2,
                    0..=31 => 6,
                    _ => 1,
                })
                .sum::<usize>();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        encoded: 0,
        escaped: 2,
    };
    snapshot_codec::write_json(&mut counter, record, max_bytes)
        .map_err(|error| SnapshotError::new(Status::OutputLimit, error.to_string()))?;
    Ok((counter.encoded, counter.escaped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::Provider;
    use crate::parse::parse_entry;
    use crate::snapshot::{EntryChunk, SourceIdentity, SourceStamp};

    fn user(index: usize, text: &str) -> Entry {
        parse_entry(
            json!({"type":"user","uuid":format!("u{index}"),"sessionId":"s",
            "timestamp":"2026-01-02T03:04:05Z","message":{"content":text}}),
        )
        .unwrap()
    }

    fn assistant(index: usize) -> Entry {
        parse_entry(
            json!({"type":"assistant","uuid":format!("a{index}"),"sessionId":"s",
            "timestamp":"2026-01-02T03:04:05Z","message":{"model":"m","content":[]}}),
        )
        .unwrap()
    }

    fn source(entries: Vec<Entry>) -> Arc<TranscriptSnapshot> {
        let activity = ActivityIndex::new(&entries.iter().collect::<Vec<_>>(), None);
        let count = entries.len();
        Arc::new(TranscriptSnapshot {
            id: "physical".into(),
            canonical_path: "/source.jsonl".into(),
            stamp: SourceStamp {
                identity: SourceIdentity {
                    device: 1,
                    inode: 2,
                },
                size: 10,
                mtime_ns: 3,
                ctime_ns: 4,
            },
            provider: Provider::Claude,
            session_id: "s".into(),
            chunks: vec![Arc::new(EntryChunk::new(0, entries))],
            activity: Arc::new(activity),
            committed_bytes: 10,
            provisional_tail: false,
            fence: Vec::new(),
            event_count: count,
        })
    }

    fn binding(execution: &str) -> LabelBinding {
        LabelBinding {
            owner_epoch: "owner".into(),
            claimant: "worker".into(),
            physical_generation: "physical".into(),
            classifier_id: "ambient".into(),
            classifier_version: "v1".into(),
            registry_generation: "r1".into(),
            execution_id: execution.into(),
        }
    }

    fn limits() -> WorkLimits {
        WorkLimits {
            max_read_bytes: 16 * PAGE_BYTES,
            max_events: 1000,
            max_items: 1000,
            max_output_bytes: 16 * PAGE_BYTES,
            max_discovery_entries: 0,
            max_sources: 0,
            deadline_unix_ms: u64::MAX,
        }
    }

    #[test]
    fn page_reservations_ignore_unseen_tail_and_final_stage_ceiling() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut bound = limits();
        bound.max_output_bytes = 2048;
        let mut short = LabelPreparation::new(
            source(vec![user(0, "first")]),
            binding.clone(),
            bound,
            8 * 1024 * 1024,
        )
        .unwrap();
        let mut extended = LabelPreparation::new(
            source(vec![user(0, "first"), user(1, &"x".repeat(64 * 1024))]),
            binding.clone(),
            bound,
            256 * 1024 * 1024,
        )
        .unwrap();
        assert_ne!(short.max_stage_bytes, extended.max_stage_bytes);
        assert_ne!(short.source.event_count, extended.source.event_count);
        assert_eq!(
            LabelPreparation::initial_reservation_bytes(short.binding()),
            LabelPreparation::initial_reservation_bytes(extended.binding())
        );
        assert_eq!(
            short.next_operation_reservation_bytes(),
            extended.next_operation_reservation_bytes()
        );
        let short_page = short.next_page("page1".into(), &binding, &cancel).unwrap();
        let extended_page = extended
            .next_page("page1".into(), &binding, &cancel)
            .unwrap();
        assert_eq!(short_page.records_json.len(), 1);
        assert_eq!(short_page.records_json, extended_page.records_json);
        assert_eq!(short.pending.as_ref().unwrap().span, 0..1);
        assert_eq!(extended.pending.as_ref().unwrap().span, 0..1);
        assert_eq!(
            short.next_operation_reservation_bytes(),
            extended.next_operation_reservation_bytes()
        );
        assert!(short.submit("page1", &[true], &binding, &cancel).unwrap());
        assert!(!extended
            .submit("page1", &[true], &binding, &cancel)
            .unwrap());
    }

    #[test]
    fn next_label_page_does_not_reserve_the_retained_prompt_again() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut reservations = Vec::new();
        for prompt in ["short".to_owned(), "x".repeat(256 * 1024)] {
            let mut entries = vec![user(0, &prompt)];
            entries.extend((1..PAGE_EVENTS).map(assistant));
            entries.push(user(PAGE_EVENTS, "next"));
            let mut stage =
                LabelPreparation::new(source(entries), binding.clone(), limits(), 8 * 1024 * 1024)
                    .unwrap();
            let first = stage.next_page("page1".into(), &binding, &cancel).unwrap();
            assert_eq!(first.records_json.len(), 1);
            assert!(!stage.submit("page1", &[true], &binding, &cancel).unwrap());
            assert_eq!(stage.activity.entry_count(), PAGE_EVENTS);
            assert!(stage.activity.accounted_bytes() > prompt.len());
            let next = stage.next_page("page2".into(), &binding, &cancel).unwrap();
            assert_eq!(next.event_start, PAGE_EVENTS);
            assert_eq!(next.records_json.len(), 1);
            assert_eq!(
                stage.pending.as_ref().unwrap().span,
                PAGE_EVENTS..PAGE_EVENTS + 1
            );
            assert!(stage.activity.append_container_reservation_bytes(1, 0, 0) < 4096);
            reservations.push(stage.next_operation_reservation_bytes());
            assert!(stage.submit("page2", &[true], &binding, &cancel).unwrap());
        }
        assert_eq!(reservations[0], reservations[1]);
    }

    #[test]
    fn pages_only_user_events_and_maps_labels_to_original_positions() {
        let source = source(vec![
            assistant(0),
            user(1, "first"),
            assistant(2),
            user(3, "second"),
        ]);
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut stage = LabelPreparation::new(
            Arc::clone(&source),
            binding.clone(),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        let page = stage.next_page("page1".into(), &binding, &cancel).unwrap();
        let decoded = snapshot_codec::decode_events(&page.records_json, PAGE_BYTES).unwrap();
        assert_eq!(
            decoded.iter().map(|record| record.i).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(decoded
            .iter()
            .all(|record| matches!(record.event, Entry::User(_))));
        assert_eq!(
            stage.usage().output_bytes,
            sonic_rs::to_vec(&page).unwrap().len()
        );
        assert!(stage
            .submit("page1", &[true, false], &binding, &cancel)
            .unwrap());
        let classifier = stage.derived_classifier();
        let derived = stage.finish(&binding, &cancel).unwrap();
        assert_eq!(derived.activity.turn_count(), 2);
        assert_eq!(derived.activity.turn_bounds(1), Some(1..4));
        assert_eq!(source.activity.turn_count(), 3);
        assert_eq!(classifier["version"], json!("labels:run1"));
        assert_ne!(derived.id, source.id);
    }

    #[test]
    fn wrong_count_and_replay_cannot_advance_activity() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut stage = LabelPreparation::new(
            source(vec![user(0, "one"), user(1, "two")]),
            binding.clone(),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        stage.next_page("page1".into(), &binding, &cancel).unwrap();
        assert_eq!(
            stage
                .submit("page1", &[true], &binding, &cancel)
                .unwrap_err()
                .status,
            Status::InvalidRequest
        );
        assert_eq!(stage.activity.entry_count(), 0);
        assert_eq!(
            stage
                .submit("wrong", &[true, true], &binding, &cancel)
                .unwrap_err()
                .status,
            Status::StaleCursor
        );
        assert!(stage
            .submit("page1", &[true, true], &binding, &cancel)
            .unwrap());
        assert_eq!(
            stage
                .submit("page1", &[false, false], &binding, &cancel)
                .unwrap_err()
                .status,
            Status::StaleCursor
        );
        assert_eq!(stage.activity.turn_count(), 2);
    }

    #[test]
    fn cursor_binds_generation_registry_classifier_execution_and_claimant() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut stage = LabelPreparation::new(
            source(vec![user(0, "one")]),
            binding.clone(),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        stage.next_page("page1".into(), &binding, &cancel).unwrap();
        for change in 0..6 {
            let mut other = binding.clone();
            match change {
                0 => other.physical_generation = "new-generation".into(),
                1 => other.registry_generation = "new-registry".into(),
                2 => other.classifier_version = "new-policy".into(),
                3 => other.execution_id = "run2".into(),
                4 => other.owner_epoch = "other-owner".into(),
                5 => other.classifier_id = "other-policy".into(),
                _ => unreachable!(),
            }
            assert_eq!(
                stage
                    .submit("page1", &[true], &other, &cancel)
                    .unwrap_err()
                    .status,
                Status::StaleCursor
            );
        }
        let mut other = binding.clone();
        other.claimant = "other-worker".into();
        assert_eq!(
            stage
                .submit("page1", &[true], &other, &cancel)
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        assert_eq!(stage.activity.entry_count(), 0);
    }

    #[test]
    fn pages_are_bounded_and_prior_cursor_cannot_label_the_next_page() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut stage = LabelPreparation::new(
            source((0..260).map(|index| user(index, "prompt")).collect()),
            binding.clone(),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        let first = stage.next_page("page1".into(), &binding, &cancel).unwrap();
        assert_eq!(first.records_json.len(), 256);
        assert!(sonic_rs::to_vec(&first).unwrap().len() <= PAGE_BYTES);
        assert!(!stage
            .submit("page1", &[true; 256], &binding, &cancel)
            .unwrap());
        let next = stage.next_page("page2".into(), &binding, &cancel).unwrap();
        assert_eq!(next.event_start, 256);
        assert_eq!(next.records_json.len(), 4);
        assert_eq!(
            stage
                .submit("page1", &[true; 4], &binding, &cancel)
                .unwrap_err()
                .status,
            Status::StaleCursor
        );
        assert!(stage
            .submit("page2", &[true; 4], &binding, &cancel)
            .unwrap());
        assert_eq!(
            stage
                .finish(&binding, &cancel)
                .unwrap()
                .activity
                .turn_count(),
            260
        );
    }

    #[test]
    fn oversized_user_never_produces_truncated_or_default_labels() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut cap = limits();
        cap.max_output_bytes = 512;
        let mut stage = LabelPreparation::new(
            source(vec![user(0, &"x".repeat(4096))]),
            binding.clone(),
            cap,
            8 * PAGE_BYTES,
        )
        .unwrap();
        assert_eq!(
            stage
                .next_page("page1".into(), &binding, &cancel)
                .unwrap_err()
                .status,
            Status::OutputLimit
        );
        assert!(stage.pending.is_none());
        assert_eq!(stage.activity.entry_count(), 0);
        assert_eq!(
            stage.finish(&binding, &cancel).unwrap_err().status,
            Status::Incomplete
        );
    }

    #[test]
    fn deadlines_cancellation_and_staging_caps_apply_before_publication() {
        let binding = binding("run1");
        let snapshot = source(vec![user(0, "prompt")]);
        assert_eq!(
            LabelPreparation::new(Arc::clone(&snapshot), binding.clone(), limits(), 1)
                .err()
                .unwrap()
                .status,
            Status::RetainedLimit
        );
        let mut cap = limits();
        cap.deadline_unix_ms = 0;
        let mut expired =
            LabelPreparation::new(Arc::clone(&snapshot), binding.clone(), cap, 8 * PAGE_BYTES)
                .unwrap();
        assert_eq!(
            expired
                .next_page("page1".into(), &binding, &Cancellation::default())
                .unwrap_err()
                .status,
            Status::Deadline
        );
        let mut stage =
            LabelPreparation::new(snapshot, binding.clone(), limits(), 8 * PAGE_BYTES).unwrap();
        let cancel = Cancellation::default();
        stage.next_page("page1".into(), &binding, &cancel).unwrap();
        cancel.cancel();
        assert_eq!(
            stage
                .submit("page1", &[true], &binding, &cancel)
                .unwrap_err()
                .status,
            Status::Cancelled
        );
        assert_eq!(stage.activity.entry_count(), 0);
    }

    #[test]
    fn each_execution_has_an_independent_derived_classifier() {
        let snapshot = source(vec![user(0, "prompt")]);
        let first = LabelPreparation::new(
            Arc::clone(&snapshot),
            binding("first"),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        let second =
            LabelPreparation::new(snapshot, binding("second"), limits(), 8 * PAGE_BYTES).unwrap();
        assert_ne!(first.derived_classifier(), second.derived_classifier());
    }

    #[test]
    fn failed_page_retains_work_without_accepting_partial_labels() {
        let binding = binding("run1");
        let cancel = Cancellation::default();
        let mut stage = LabelPreparation::new(
            source(vec![user(0, "small"), user(1, &"x".repeat(PAGE_BYTES))]),
            binding.clone(),
            limits(),
            8 * PAGE_BYTES,
        )
        .unwrap();
        assert_eq!(
            stage
                .next_page("page1".into(), &binding, &cancel)
                .unwrap_err()
                .status,
            Status::OutputLimit
        );
        assert_eq!(stage.usage().events, 1);
        assert_eq!(stage.usage().items, 1);
        assert!(stage.usage().input_bytes > 0);
        assert!(stage.usage().output_bytes > 0);
        assert!(stage.pending.is_none());
        assert_eq!(stage.activity.entry_count(), 0);
    }

    #[test]
    fn derived_tokens_do_not_concatenate_original_versions_or_generations() {
        let mut snapshot = source(vec![user(0, "prompt")]);
        Arc::get_mut(&mut snapshot).unwrap().id = "g".repeat(256);
        let mut binding = binding("execution");
        binding.physical_generation = snapshot.id.clone();
        binding.classifier_version = "v".repeat(256);
        let cancel = Cancellation::default();
        let mut stage =
            LabelPreparation::new(snapshot, binding.clone(), limits(), 8 * PAGE_BYTES).unwrap();
        assert_eq!(
            stage.derived_classifier()["version"],
            json!("labels:execution")
        );
        assert_eq!(stage.binding().classifier_version.len(), 256);
        stage.next_page("page1".into(), &binding, &cancel).unwrap();
        assert!(stage.submit("page1", &[true], &binding, &cancel).unwrap());
        assert_eq!(
            stage.finish(&binding, &cancel).unwrap().id,
            "labels:execution"
        );
    }
}
