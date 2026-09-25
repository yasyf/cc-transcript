use std::collections::HashSet;
use std::io::{self, Write};
use std::ops::Range;

use regex::RegexBuilder;
use sonic_rs::{json, JsonContainerTrait, JsonValueTrait, Value};

use crate::activity::{session_activity_refs, ActivityOpts, LiftedSession, Turn};
use crate::context::{ContextWindow, Fidelity};
use crate::ids::EventRef;
use crate::query::{InputRule, Session};
use crate::render::{self, Budget};
use crate::snapshot::{
    Cancellation, Projection, SnapshotError, Status, TranscriptSnapshot, WorkLimits,
};
use crate::snapshot_codec::{self, EventWire, FileRefRecord, ToolUseWire, TurnWire, TURN_CODEC};
use crate::toolcall::{tool_name_matches, ToolCall};
use crate::types::{AttachmentDetail, ContentBlock, Entry, UserContent};

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, SnapshotError> {
    value
        .get(key)
        .ok_or_else(|| invalid(format!("missing {key}")))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| invalid(format!("{key} must be a string")))
}

fn count(value: &Value, key: &str) -> Result<usize, SnapshotError> {
    field(value, key)?
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| invalid(format!("{key} must be a count")))
}

fn strings<'a>(value: &'a Value, key: &str) -> Result<Vec<&'a str>, SnapshotError> {
    field(value, key)?
        .as_array()
        .ok_or_else(|| invalid(format!("{key} must be an array")))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| invalid(format!("{key} must contain strings")))
        })
        .collect()
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a sonic_rs::Array, SnapshotError> {
    field(value, key)?
        .as_array()
        .ok_or_else(|| invalid(format!("{key} must be an array")))
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("projection output limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded(value: &Value, limit: usize) -> Result<String, SnapshotError> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
    };
    snapshot_codec::write_json(&mut writer, value, limit).map_err(|_| {
        SnapshotError::new(
            Status::OutputLimit,
            "projection exceeds serialized output limit",
        )
    })?;
    Ok(String::from_utf8(writer.bytes).expect("JSON is UTF-8"))
}

struct OutputCounter {
    remaining: usize,
}

impl Write for OutputCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("projection output limit"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn finish(
    data: Value,
    limits: &WorkLimits,
    next: Option<usize>,
) -> Result<Projection, SnapshotError> {
    snapshot_codec::write_json(
        &mut OutputCounter {
            remaining: limits.max_output_bytes,
        },
        &data,
        limits.max_output_bytes,
    )
    .map_err(|_| {
        SnapshotError::new(
            Status::OutputLimit,
            "projection exceeds serialized output limit",
        )
    })?;
    let items = ["records_json", "values", "windows_json", "windows"]
        .iter()
        .find_map(|key| {
            data.get(*key)
                .and_then(|value| value.as_array())
                .map(|values| values.len())
        })
        .unwrap_or(1);
    Ok(Projection {
        read_bytes: 0,
        events: 0,
        items,
        data,
        complete: next.is_none(),
        next,
        reason: next.map(|_| "item_limit".to_owned()),
    })
}

fn incomplete(reason: &str) -> Projection {
    Projection {
        read_bytes: 0,
        events: 0,
        items: 0,
        data: Value::new(),
        complete: false,
        next: None,
        reason: Some(reason.to_owned()),
    }
}

struct Work<'a, 'w> {
    snapshot: &'a TranscriptSnapshot,
    limits: &'w WorkLimits,
    cancel: &'w Cancellation,
    events: usize,
    bytes: usize,
    charged: HashSet<usize>,
}

impl<'a, 'w> Work<'a, 'w> {
    fn new(
        snapshot: &'a TranscriptSnapshot,
        limits: &'w WorkLimits,
        cancel: &'w Cancellation,
    ) -> Self {
        Self {
            snapshot,
            limits,
            cancel,
            events: 0,
            bytes: 0,
            charged: HashSet::new(),
        }
    }

    fn charge_range(&mut self, range: Range<usize>) -> Result<(), SnapshotError> {
        self.cancel.check(self.limits.deadline_unix_ms)?;
        if range.len() > self.limits.max_events {
            return Err(SnapshotError::new(Status::Incomplete, "event_limit"));
        }
        for index in range {
            self.cancel.check(self.limits.deadline_unix_ms)?;
            if self.charged.contains(&index) {
                continue;
            }
            if self.events == self.limits.max_events {
                return Err(SnapshotError::new(Status::Incomplete, "event_limit"));
            }
            let at = self
                .snapshot
                .chunks
                .partition_point(|chunk| chunk.start <= index)
                - 1;
            let chunk = &self.snapshot.chunks[at];
            let charge = chunk.entry_charges[index - chunk.start];
            let bytes = charge
                .owned_capacity_bytes
                .saturating_add(charge.opaque_dom_accounted_bytes);
            if bytes > self.limits.max_read_bytes.saturating_sub(self.bytes) {
                return Err(SnapshotError::new(Status::Incomplete, "read_limit"));
            }
            self.bytes += bytes;
            self.events += 1;
            self.charged.insert(index);
        }
        Ok(())
    }

    fn turn(&mut self, index: usize) -> Result<Turn<'a>, SnapshotError> {
        let range = self
            .snapshot
            .activity
            .turn_bounds(index)
            .ok_or_else(|| invalid("turn index out of range"))?;
        self.charge_range(range)?;
        for event in self.snapshot.activity.result_events(index) {
            self.charge_range(event..event + 1)?;
        }
        let bytes = self
            .snapshot
            .activity
            .projected_bytes(index)
            .expect("existing turn");
        if bytes > self.limits.max_read_bytes.saturating_sub(self.bytes) {
            return Err(SnapshotError::new(Status::Incomplete, "read_limit"));
        }
        self.bytes += bytes;
        Ok(self
            .snapshot
            .activity
            .project_turn_with(index, |i| self.snapshot.entry(i))
            .expect("existing turn"))
    }
}

pub fn bounded_turns<'a>(
    snapshot: &'a TranscriptSnapshot,
    limits: &WorkLimits,
    cancel: &Cancellation,
) -> Result<LiftedSession<'a>, SnapshotError> {
    bounded_turn_range(snapshot, 0..snapshot.activity.turn_count(), limits, cancel)
}

pub fn bounded_turn_range<'a>(
    snapshot: &'a TranscriptSnapshot,
    turns: Range<usize>,
    limits: &WorkLimits,
    cancel: &Cancellation,
) -> Result<LiftedSession<'a>, SnapshotError> {
    bounded_turn_range_with_usage(
        snapshot,
        turns,
        limits,
        cancel,
        &mut ProjectionUsage::default(),
        |_| Ok(()),
    )
}

#[derive(Default)]
pub struct ProjectionUsage {
    pub read_bytes: usize,
    pub events: usize,
}

pub fn bounded_turn_range_with_usage<'a>(
    snapshot: &'a TranscriptSnapshot,
    turns: Range<usize>,
    limits: &WorkLimits,
    cancel: &Cancellation,
    usage: &mut ProjectionUsage,
    before_materialize: impl FnOnce(usize) -> Result<(), SnapshotError>,
) -> Result<LiftedSession<'a>, SnapshotError> {
    let mut work = Work::new(snapshot, limits, cancel);
    let result = (|| {
        cancel.check(limits.deadline_unix_ms)?;
        if turns.start > turns.end || turns.end > snapshot.activity.turn_count() {
            return Err(invalid("turn range outside snapshot"));
        }
        if turns.len() > limits.max_items {
            return Err(SnapshotError::new(Status::Incomplete, "item_limit"));
        }
        for index in turns.clone() {
            work.charge_range(snapshot.activity.turn_bounds(index).expect("existing turn"))?;
            for result in snapshot.activity.result_events(index) {
                work.charge_range(result..result + 1)?;
            }
        }
        let mut remaining = limits.max_read_bytes.saturating_sub(work.bytes);
        for index in turns.clone() {
            cancel.check(limits.deadline_unix_ms)?;
            let bytes = snapshot
                .activity
                .projected_bytes(index)
                .expect("existing turn");
            if bytes > remaining {
                return Err(SnapshotError::new(Status::Incomplete, "read_limit"));
            }
            remaining -= bytes;
        }
        let retained_projection = limits.max_read_bytes.saturating_sub(remaining);
        if retained_projection > limits.max_output_bytes {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "activity materialization limit",
            ));
        }
        let repeated_result_bytes = turns.clone().fold(0usize, |total, index| {
            total.saturating_add(snapshot.activity.repeated_result_bytes(index))
        });
        let staging_bytes = retained_projection
            .saturating_mul(2)
            .saturating_add(
                work.events
                    .saturating_mul(std::mem::size_of::<Entry>())
                    .saturating_mul(2),
            )
            .saturating_add(limits.max_output_bytes.saturating_mul(2))
            .saturating_add(repeated_result_bytes.saturating_mul(2));
        before_materialize(staging_bytes)?;
        let mut selected = Vec::with_capacity(turns.len());
        for index in turns {
            selected.push(work.turn(index)?);
        }
        cancel.check(limits.deadline_unix_ms)?;
        Ok(LiftedSession {
            session_id: &snapshot.session_id,
            turns: selected,
        })
    })();
    usage.read_bytes = work.bytes;
    usage.events = work.events;
    result
}

pub fn project(
    snapshot: &TranscriptSnapshot,
    request: &Value,
    limits: &WorkLimits,
    cancel: &Cancellation,
    next: usize,
) -> Result<Projection, SnapshotError> {
    project_with_usage(
        snapshot,
        request,
        limits,
        cancel,
        next,
        &mut ProjectionUsage::default(),
    )
}

pub fn project_with_usage(
    snapshot: &TranscriptSnapshot,
    request: &Value,
    limits: &WorkLimits,
    cancel: &Cancellation,
    next: usize,
    usage: &mut ProjectionUsage,
) -> Result<Projection, SnapshotError> {
    let mut work = Work::new(snapshot, limits, cancel);
    let result = (|| {
        cancel.check(limits.deadline_unix_ms)?;
        let result = match string(request, "operation")? {
            "capture" => capture(&mut work, request, next),
            "hydrate" => hydrate(&mut work, request, next),
            "query" => query(&mut work, request, next),
            "activity_probe" => probe(&mut work, request),
            operation => Err(invalid(format!(
                "unsupported projection operation {operation}"
            ))),
        };
        cancel.check(limits.deadline_unix_ms)?;
        let mut result = match result {
            Err(error) if error.status == Status::Incomplete => incomplete(&error.reason),
            other => other?,
        };
        result.read_bytes = work.bytes;
        result.events = work.events;
        Ok(result)
    })();
    usage.read_bytes = work.bytes;
    usage.events = work.events;
    result
}

fn selected_range(work: &mut Work, request: &Value) -> Result<Range<usize>, SnapshotError> {
    let view = field(request, "view")?;
    if !array(view, "attachments")?.is_empty() {
        return Err(invalid("attachments require host snapshot composition"));
    }
    let mut range = 0..work.snapshot.event_count;
    for selector in array(view, "selectors")? {
        work.cancel.check(work.limits.deadline_unix_ms)?;
        match string(selector, "kind")? {
            "current_turn" => {
                if range.is_empty() {
                    continue;
                }
                let turn = work
                    .snapshot
                    .activity
                    .turn_of_event(range.end - 1)
                    .expect("existing event");
                range.start = range.start.max(
                    work.snapshot
                        .activity
                        .turn_bounds(turn)
                        .expect("existing turn")
                        .start,
                );
            }
            "recent_events" => {
                range.start = range
                    .start
                    .max(range.end.saturating_sub(count(selector, "count")?))
            }
            "event_range" => {
                let len = range.len();
                let start = count(selector, "start")?.min(len);
                let stop = count(selector, "stop")?.min(len).max(start);
                range = range.start + start..range.start + stop;
            }
            "prior" | "recent_messages" => {
                let prior = string(selector, "kind")? == "prior";
                let wanted = if prior { 1 } else { count(selector, "count")? };
                if wanted == 0 {
                    range.start = range.end;
                    continue;
                }
                let mut found = 0;
                let mut boundary = None;
                for index in range.clone().rev() {
                    work.charge_range(index..index + 1)?;
                    let entry = work.snapshot.entry(index);
                    let message = matches!(entry, Entry::User(_) | Entry::Assistant(_))
                        || (!prior
                            && matches!(entry, Entry::Attachment(a) if matches!(&a.detail, AttachmentDetail::QueuedCommand(_))));
                    if message {
                        found += 1;
                        if found == wanted {
                            boundary = Some(index);
                            break;
                        }
                    }
                }
                if prior {
                    range.end = boundary.unwrap_or(range.start);
                } else if let Some(index) = boundary {
                    range.start = index;
                }
            }
            "after_last_tool" | "before_last_tool" => {
                let name = string(selector, "name")?;
                let file = selector.get("file").and_then(|v| v.as_str());
                let after = string(selector, "kind")? == "after_last_tool";
                let mut boundary = None;
                if !range.is_empty() {
                    let first = work
                        .snapshot
                        .activity
                        .turn_of_event(range.start)
                        .expect("event turn");
                    let last = work
                        .snapshot
                        .activity
                        .turn_of_event(range.end - 1)
                        .expect("event turn");
                    for index in (first..=last).rev() {
                        let turn = work.turn(index)?;
                        for position in work.snapshot.activity.turn_bounds(index).unwrap().rev() {
                            if !range.contains(&position) {
                                continue;
                            }
                            let Some(meta) = work.snapshot.entry(position).meta() else {
                                continue;
                            };
                            if turn.tool_uses.iter().any(|use_| {
                                std::ptr::eq(use_.event_uuid.as_ptr(), meta.uuid.as_ptr())
                                    && tool_name_matches(use_.name, name)
                                    && file.is_none_or(|f| {
                                        use_.call.file_paths().iter().any(|p| p.contains(f))
                                    })
                            }) {
                                boundary = Some(position);
                                break;
                            }
                        }
                        if boundary.is_some() {
                            break;
                        }
                    }
                }
                match (after, boundary) {
                    (true, Some(index)) => range.start = index + 1,
                    (true, None) => range.end = range.start,
                    (false, Some(index)) => range.end = index,
                    (false, None) => {}
                }
            }
            kind => return Err(invalid(format!("unsupported selector {kind}"))),
        }
    }
    Ok(range)
}

fn lift_range<'a>(
    work: &mut Work<'a, '_>,
    range: &Range<usize>,
) -> Result<LiftedSession<'a>, SnapshotError> {
    let mut turns = Vec::new();
    if !range.is_empty() {
        let first = work
            .snapshot
            .activity
            .turn_of_event(range.start)
            .expect("event turn");
        let last = work
            .snapshot
            .activity
            .turn_of_event(range.end - 1)
            .expect("event turn");
        for index in first..=last {
            turns.push(work.turn(index)?);
        }
    }
    Ok(LiftedSession {
        session_id: &work.snapshot.session_id,
        turns,
    })
}

fn view<'a>(
    lift: &'a LiftedSession<'a>,
    range: &Range<usize>,
    snapshot: &TranscriptSnapshot,
) -> Session<'a> {
    let base = lift.turns.first().map_or(range.start, |turn| {
        snapshot.activity.turn_bounds(turn.index).unwrap().start
    });
    Session::from_lift(lift).windowed(range.start - base, range.end - base)
}

fn budget(value: &Value) -> Result<Budget, SnapshotError> {
    Ok(Budget {
        turn_chars: count(value, "turn_chars")?,
        tool_chars: count(value, "tool_chars")?,
    })
}

fn capture(work: &mut Work, request: &Value, next: usize) -> Result<Projection, SnapshotError> {
    let range = selected_range(work, request)?;
    let anchors = array(request, "anchors")?;
    if next > anchors.len() {
        return Err(invalid("capture cursor out of range"));
    }
    let stop = anchors
        .len()
        .min(next.saturating_add(work.limits.max_items.min(256)));
    let mut windows = Vec::new();
    for value in &anchors[next..stop] {
        let anchor = EventRef {
            session_id: string(value, "session_id")?.to_owned(),
            event_uuid: string(value, "event_uuid")?.to_owned(),
            tool_use_id: value
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        };
        if anchor.session_id != work.snapshot.session_id {
            return Err(invalid("capture anchor session mismatch"));
        }
        let event = work
            .snapshot
            .activity
            .event_of_uuid(&anchor.event_uuid)
            .ok_or_else(|| invalid("capture anchor not found"))?;
        if !range.contains(&event) {
            return Err(invalid("capture anchor outside view"));
        }
        let trigger = work
            .snapshot
            .activity
            .turn_of_event(event)
            .expect("anchor turn");
        let first = trigger.saturating_sub(count(request, "before")?);
        let stop_turn = trigger
            .saturating_add(count(request, "after")?)
            .saturating_add(1)
            .min(work.snapshot.activity.turn_count());
        let selected = (first..stop_turn).filter(|&index| {
            let bounds = work.snapshot.activity.turn_bounds(index).unwrap();
            bounds.start < range.end && range.start < bounds.end
        });
        let mut turns = Vec::new();
        for index in selected {
            let bounds = work
                .snapshot
                .activity
                .turn_bounds(index)
                .expect("selected turn");
            let mut turn = work.turn(index)?;
            let lo = range.start.saturating_sub(bounds.start);
            let hi = range.end.min(bounds.end) - bounds.start;
            if lo != 0 {
                turn.prompt.clear();
            }
            turn.events = turn.events[lo..hi].to_vec();
            let identities: HashSet<_> = turn
                .events
                .iter()
                .filter_map(|event| event.meta().map(|meta| meta.uuid.as_ptr()))
                .collect();
            turn.tool_uses
                .retain(|use_| identities.contains(&use_.event_uuid.as_ptr()));
            turns.push(turn);
        }
        let width = count(request, "preview_chars")?;
        let render_budget = Budget {
            turn_chars: width,
            tool_chars: width,
        };
        let mut window = ContextWindow {
            anchor,
            before: Vec::new(),
            trigger: None,
            after: Vec::new(),
            fidelity: Fidelity::Full,
            preview_chars: width
                .try_into()
                .map_err(|_| invalid("preview_chars overflow"))?,
            preview_schema: true,
        };
        for turn in &turns {
            let reference = crate::context::turn_ref(turn, &render_budget);
            if turn.index < trigger {
                window.before.push(reference);
            } else if turn.index == trigger {
                window.trigger = Some(reference);
            } else {
                window.after.push(reference);
            }
        }
        let text = window
            .to_json_bounded(work.limits.max_output_bytes.min(1024 * 1024))
            .map_err(|error| SnapshotError::new(Status::OutputLimit, error.to_string()))?;
        windows.push(text);
    }
    finish(
        json!({"kind":"captured","windows_json":windows}),
        work.limits,
        (stop < anchors.len()).then_some(stop),
    )
}

fn hydrate(work: &mut Work, request: &Value, next: usize) -> Result<Projection, SnapshotError> {
    let windows = array(request, "windows_json")?;
    if next > windows.len() {
        return Err(invalid("hydration cursor out of range"));
    }
    let stop = windows
        .len()
        .min(next.saturating_add(work.limits.max_items.min(256)));
    let render = field(request, "render")?;
    let before = budget(field(render, "before")?)?;
    let trigger = budget(field(render, "trigger")?)?;
    let after = budget(field(render, "after")?)?;
    let mut items = Vec::new();
    for input_index in next..stop {
        work.cancel.check(work.limits.deadline_unix_ms)?;
        let text = windows[input_index]
            .as_str()
            .ok_or_else(|| invalid("window must be JSON text"))?;
        if text.len() > work.limits.max_read_bytes.saturating_sub(work.bytes) {
            return Err(SnapshotError::new(Status::Incomplete, "read_limit"));
        }
        work.bytes += text.len();
        let window = ContextWindow::from_json(text).map_err(|e| invalid(e.to_string()))?;
        if window.anchor.session_id != work.snapshot.session_id {
            return Err(invalid("hydration window session mismatch"));
        }
        let refs = window
            .before
            .iter()
            .map(|r| (r, &before))
            .chain(window.trigger.iter().map(|r| (r, &trigger)))
            .chain(window.after.iter().map(|r| (r, &after)));
        let mut parts = Vec::new();
        let mut missing = false;
        for (reference, budget) in refs {
            let mut turn_index = None;
            if reference.refs.is_empty() {
                missing = true;
                break;
            }
            for event in &reference.refs {
                if event.session_id != work.snapshot.session_id {
                    missing = true;
                    break;
                }
                match work.snapshot.activity.turn_of_uuid(&event.event_uuid) {
                    Some(index) => {
                        if turn_index.is_none() {
                            turn_index = Some(index);
                        }
                    }
                    None => {
                        missing = true;
                        break;
                    }
                }
            }
            if missing {
                break;
            }
            let turn = work.turn(turn_index.expect("nonempty refs"))?;
            let rendered = render::render_turn(&turn, budget, true);
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
        items.push(if missing {
            json!({"input_index":input_index,"availability":"missing_ref","rendered":null})
        } else {
            json!({"input_index":input_index,"availability":"full","rendered":parts.join("\n\n")})
        });
    }
    finish(
        json!({"kind":"hydrated","windows":items}),
        work.limits,
        (stop < windows.len()).then_some(stop),
    )
}

fn scalar(value: Value, work: &Work) -> Result<Projection, SnapshotError> {
    finish(json!({"kind":"scalar","value":value}), work.limits, None)
}

fn record_page(
    values: impl DoubleEndedIterator<Item = Result<String, SnapshotError>> + ExactSizeIterator,
    schema: &str,
    next: usize,
    reverse: bool,
    work: &Work,
) -> Result<Projection, SnapshotError> {
    let len = values.len();
    if next > len {
        return Err(invalid("record cursor out of range"));
    }
    let stop = len.min(next.saturating_add(work.limits.max_items.min(256)));
    let values: Box<dyn Iterator<Item = Result<String, SnapshotError>>> = if reverse {
        Box::new(values.rev())
    } else {
        Box::new(values)
    };
    let mut records = Vec::new();
    let mut remaining = work.limits.max_output_bytes;
    for value in values.skip(next).take(stop - next) {
        work.cancel.check(work.limits.deadline_unix_ms)?;
        let record = value?;
        let outer_bytes = encoded(&json!(&record), remaining)?.len();
        remaining = remaining.saturating_sub(outer_bytes.saturating_add(1));
        records.push(record);
    }
    finish(
        json!({"kind":"records","record_schema":schema,"records_json":records}),
        work.limits,
        (stop < len).then_some(stop),
    )
}

fn output_limit() -> SnapshotError {
    SnapshotError::new(Status::OutputLimit, "projection text exceeds output budget")
}

fn text_plan_size(parts: &[&str], max_bytes: usize) -> Result<usize, SnapshotError> {
    let mut remaining = max_bytes.checked_sub(2).ok_or_else(output_limit)?;
    for part in parts {
        for ch in part.chars() {
            let bytes = match ch {
                '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
                ch if (ch as u32) < 0x20 => 6,
                ch => ch.len_utf8(),
            };
            remaining = remaining.checked_sub(bytes).ok_or_else(output_limit)?;
        }
    }
    Ok(max_bytes - remaining)
}

fn with_text_budget<T>(
    parts: &[&str],
    max_bytes: usize,
    produce: impl FnOnce() -> T,
) -> Result<T, SnapshotError> {
    text_plan_size(parts, max_bytes)?;
    Ok(produce())
}

fn scalar_text(parts: &[&str], work: &Work) -> Result<Projection, SnapshotError> {
    let allowance = work
        .limits
        .max_output_bytes
        .checked_sub(b"{\"kind\":\"scalar\",\"value\":}".len())
        .ok_or_else(output_limit)?;
    let text = with_text_budget(parts, allowance, || parts.concat())?;
    scalar(json!(text), work)
}

fn string_page(plans: &[Vec<&str>], next: usize, work: &Work) -> Result<Projection, SnapshotError> {
    if next > plans.len() {
        return Err(invalid("string cursor out of range"));
    }
    let stop = plans
        .len()
        .min(next.saturating_add(work.limits.max_items.min(256)));
    let mut remaining = work
        .limits
        .max_output_bytes
        .checked_sub(b"{\"kind\":\"strings\",\"values\":[]}".len())
        .ok_or_else(output_limit)?;
    for (ordinal, plan) in plans[next..stop].iter().enumerate() {
        work.cancel.check(work.limits.deadline_unix_ms)?;
        if ordinal != 0 {
            remaining = remaining.checked_sub(1).ok_or_else(output_limit)?;
        }
        remaining -= text_plan_size(plan, remaining)?;
    }
    let values: Vec<_> = plans[next..stop]
        .iter()
        .map(|parts| parts.concat())
        .collect();
    finish(
        json!({"kind":"strings","values":values}),
        work.limits,
        (stop < plans.len()).then_some(stop),
    )
}

fn joined_event_parts(event: &Entry) -> Vec<&str> {
    match event {
        Entry::User(user) => match &user.content {
            UserContent::Plain(text) => return vec![text],
            UserContent::Blocks(_) => {}
        },
        Entry::Assistant(_) => {}
        _ => return Vec::new(),
    }
    let mut parts = Vec::new();
    for block in event.blocks() {
        if let ContentBlock::Text(text) = block {
            if !parts.is_empty() {
                parts.push(" ");
            }
            parts.push(text);
        }
    }
    parts
}

fn strip_parts(parts: &mut Vec<&str>) {
    let Some(first) = parts
        .iter()
        .position(|part| !crate::pystr::lstrip(part).is_empty())
    else {
        parts.clear();
        return;
    };
    let last = parts
        .iter()
        .rposition(|part| !crate::pystr::rstrip(part).is_empty())
        .expect("nonempty text");
    parts.truncate(last + 1);
    parts.drain(..first);
    parts[0] = crate::pystr::lstrip(parts[0]);
    let last = parts.len() - 1;
    parts[last] = crate::pystr::rstrip(parts[last]);
}

fn prefix_parts(parts: &mut Vec<&str>, count: usize) {
    let mut remaining = count;
    for index in 0..parts.len() {
        let part = parts[index];
        if let Some((end, _)) = part.char_indices().nth(remaining) {
            parts[index] = &part[..end];
            parts.truncate(index + 1);
            return;
        }
        remaining -= part.chars().count();
    }
}

fn prompt_at<'a>(
    work: &mut Work<'a, '_>,
    index: usize,
    range: &Range<usize>,
) -> Result<&'a str, SnapshotError> {
    let bounds = work
        .snapshot
        .activity
        .turn_bounds(index)
        .expect("view turn");
    if bounds.start < range.start {
        return Ok("");
    }
    work.charge_range(bounds.start..bounds.start + 1)?;
    Ok(work.snapshot.activity.prompt(index).expect("view turn"))
}

fn prompt_query(
    work: &mut Work,
    range: Range<usize>,
    query: &Value,
    next: usize,
) -> Result<Projection, SnapshotError> {
    let kind = string(query, "kind")?;
    if range.is_empty() {
        return if kind == "prompts" {
            string_page(&[], next, work)
        } else if kind == "first_prompt" {
            scalar(json!(null), work)
        } else {
            scalar_text(&[], work)
        };
    }
    let first = work
        .snapshot
        .activity
        .turn_of_event(range.start)
        .expect("view turn");
    let last = work
        .snapshot
        .activity
        .turn_of_event(range.end - 1)
        .expect("view turn");
    if kind == "user_text" {
        let prompt = prompt_at(work, last, &range)?;
        return scalar_text(&[prompt], work);
    }
    if kind == "first_prompt" {
        for index in first..=last {
            let prompt = prompt_at(work, index, &range)?;
            if !prompt.is_empty() {
                return scalar_text(&[prompt], work);
            }
        }
        return scalar(json!(null), work);
    }
    let selection = string(query, "selection")?;
    if selection == "current" {
        let prompt = prompt_at(work, last, &range)?;
        return string_page(&[vec![prompt]], next, work);
    }
    let count = count(query, "count")?;
    let mut plans = Vec::new();
    let indices: Box<dyn Iterator<Item = usize>> = match selection {
        "first" => Box::new(first..=last),
        "last" => Box::new((first..=last).rev()),
        _ => return Err(invalid("invalid prompt selection")),
    };
    for index in indices {
        if plans.len() == count {
            break;
        }
        let prompt = prompt_at(work, index, &range)?;
        if !prompt.is_empty() {
            plans.push(vec![prompt]);
        }
    }
    if selection == "last" {
        plans.reverse();
    }
    string_page(&plans, next, work)
}

fn assistant_query(
    work: &mut Work,
    range: Range<usize>,
    query: &Value,
) -> Result<Projection, SnapshotError> {
    let message_limit = count(query, "count")?;
    let max_chars = count(query, "max_per_message")?;
    let mut messages = Vec::new();
    let mut scanned = 0;
    for index in range.rev() {
        if message_limit != 0 && scanned == message_limit {
            break;
        }
        work.charge_range(index..index + 1)?;
        let entry = work.snapshot.entry(index);
        if !matches!(entry, Entry::Assistant(_)) {
            continue;
        }
        scanned += 1;
        let mut parts = joined_event_parts(entry);
        strip_parts(&mut parts);
        if parts.is_empty() {
            continue;
        }
        prefix_parts(&mut parts, max_chars);
        messages.push(parts);
    }
    let mut parts = Vec::new();
    for message in messages.into_iter().rev() {
        if !parts.is_empty() {
            parts.push("\n---\n");
        }
        parts.extend(message);
    }
    scalar_text(&parts, work)
}

fn json_truthy(value: &Value) -> bool {
    if value.is_null() {
        false
    } else if let Some(value) = value.as_bool() {
        value
    } else if let Some(value) = value.as_str() {
        !value.is_empty()
    } else if let Some(value) = value.as_array() {
        !value.is_empty()
    } else if let Some(value) = value.as_object() {
        !value.is_empty()
    } else {
        !(value.as_i64() == Some(0) || value.as_u64() == Some(0) || value.as_f64() == Some(0.0))
    }
}

fn prose_field(value: Option<&Value>) -> Result<Option<&str>, SnapshotError> {
    let Some(value) = value.filter(|value| json_truthy(value)) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(Some)
        .ok_or_else(|| invalid("prose field must be a string"))
}

fn prose_pair<'a>(left: Option<&'a str>, right: Option<&'a str>) -> Vec<&'a str> {
    match (
        left.filter(|text| !text.is_empty()),
        right.filter(|text| !text.is_empty()),
    ) {
        (Some(left), Some(right)) => vec![left, " ", right],
        (Some(text), None) | (None, Some(text)) => vec![text],
        (None, None) => Vec::new(),
    }
}

fn signal_event<'a>(
    work: &mut Work<'a, '_>,
    index: usize,
    origin: &str,
) -> Result<Vec<Vec<&'a str>>, SnapshotError> {
    work.charge_range(index..index + 1)?;
    let event = work.snapshot.entry(index);
    if !matches!(event, Entry::Assistant(_) | Entry::User(_)) {
        return Ok(Vec::new());
    }
    let meta = event.meta().expect("message meta");
    if meta.is_meta
        || meta.is_compact_summary
        || (origin == "assistant" && matches!(event, Entry::User(_)))
    {
        return Ok(Vec::new());
    }
    let event_text = joined_event_parts(event);
    if matches!(event, Entry::User(_)) {
        let injected = if event_text.len() == 1 {
            crate::protocol::is_agent_injection(event_text[0])
        } else {
            with_text_budget(&event_text, work.limits.max_output_bytes, || {
                crate::protocol::is_agent_injection(&event_text.concat())
            })?
        };
        if injected {
            return Ok(Vec::new());
        }
    }
    let mut plans = Vec::new();
    if event_text.iter().any(|text| !text.is_empty()) {
        plans.push(event_text);
    }
    for (ordinal, block) in event.blocks().iter().enumerate() {
        work.cancel.check(work.limits.deadline_unix_ms)?;
        match block {
            ContentBlock::Thinking(text) if !text.is_empty() => plans.push(vec![text]),
            ContentBlock::ToolUse(tool) => {
                let call = work
                    .snapshot
                    .activity
                    .call_at(index, ordinal)
                    .ok_or_else(|| invalid("tool call missing from activity index"))?;
                let pair = match call {
                    ToolCall::TaskCreate(call) => Some(prose_pair(
                        Some(&call.subject),
                        prose_field(call.description.as_ref())?,
                    )),
                    ToolCall::TaskUpdate(call) => Some(prose_pair(
                        prose_field(call.subject.as_ref())?,
                        prose_field(call.description.as_ref())?,
                    )),
                    _ => None,
                };
                if let Some(pair) = pair {
                    if !pair.is_empty() {
                        plans.push(pair);
                    }
                    continue;
                }
                let (collection, left, right) = match tool.name.as_str() {
                    "ReportFindings" => ("findings", "summary", "failure_scenario"),
                    "TodoWrite" => ("todos", "content", "subject"),
                    _ => continue,
                };
                if !call.raw().is_object() {
                    return Err(invalid("prose tool input must be an object"));
                }
                let rows = match call.raw().get(collection) {
                    None => continue,
                    Some(rows) => rows
                        .as_array()
                        .ok_or_else(|| invalid("prose tool entries must be an array"))?,
                };
                for row in rows {
                    if !row.is_object() {
                        return Err(invalid("prose tool entry must be an object"));
                    }
                    let pair =
                        prose_pair(prose_field(row.get(left))?, prose_field(row.get(right))?);
                    if !pair.is_empty() {
                        plans.push(pair);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(plans)
}

fn signal_query(
    work: &mut Work,
    mut range: Range<usize>,
    query: &Value,
    next: usize,
) -> Result<Projection, SnapshotError> {
    let origin = string(query, "origin")?;
    if !["any", "assistant"].contains(&origin) {
        return Err(invalid("invalid signal origin"));
    }
    let window = field(query, "window")?;
    let mut plans = Vec::new();
    if window.as_str() == Some("current_turn") {
        if !range.is_empty() {
            let turn = work
                .snapshot
                .activity
                .turn_of_event(range.end - 1)
                .expect("view turn");
            range.start = range
                .start
                .max(work.snapshot.activity.turn_bounds(turn).unwrap().start);
        }
        for index in range {
            plans.extend(signal_event(work, index, origin)?);
        }
    } else {
        let count = count(query, "window")?;
        let mut events = Vec::new();
        let mut length = 0;
        for index in range.rev() {
            if length >= count {
                break;
            }
            let texts = signal_event(work, index, origin)?;
            length += texts.len();
            events.push(texts);
        }
        for texts in events.into_iter().rev() {
            plans.extend(texts);
        }
        let remove = plans.len().saturating_sub(count);
        plans.drain(..remove);
    }
    string_page(&plans, next, work)
}

fn workflow_query(
    work: &mut Work,
    range: Range<usize>,
    query: &Value,
) -> Result<Projection, SnapshotError> {
    let mode = string(query, "mode")?;
    if !["contains", "regex"].contains(&mode) {
        return Err(invalid("invalid workflow text mode"));
    }
    let mut parts = Vec::new();
    let mut messages = 0;
    for index in range {
        work.charge_range(index..index + 1)?;
        let event = work.snapshot.entry(index);
        if matches!(event, Entry::User(_) | Entry::Assistant(_)) {
            if messages != 0 {
                parts.push("\n");
            }
            parts.extend(joined_event_parts(event));
            messages += 1;
        }
    }
    let text = with_text_budget(&parts, work.limits.max_output_bytes, || parts.concat())?;
    let pattern = string(query, "pattern")?;
    let matched = if mode == "contains" {
        text.contains(pattern)
    } else {
        regex::Regex::new(pattern)
            .map_err(|error| invalid(error.to_string()))?
            .is_match(&text)
    };
    scalar(json!(matched), work)
}

fn checked_render<T>(
    work: &mut Work,
    range: &Range<usize>,
    produce: impl FnOnce(&mut Work) -> Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    let mut input = OutputCounter {
        remaining: work
            .limits
            .max_output_bytes
            .checked_sub(64)
            .ok_or_else(output_limit)?
            / 16,
    };
    let mut clones = work.limits.max_output_bytes;
    if !range.is_empty() {
        let first = work
            .snapshot
            .activity
            .turn_of_event(range.start)
            .expect("view turn");
        let last = work
            .snapshot
            .activity
            .turn_of_event(range.end - 1)
            .expect("view turn");
        for index in first..=last {
            let copied = work
                .snapshot
                .activity
                .projected_bytes(index)
                .expect("view turn");
            clones = clones.checked_sub(copied).ok_or_else(output_limit)?;
            let bounds = work
                .snapshot
                .activity
                .turn_bounds(index)
                .expect("view turn");
            work.charge_range(bounds.clone())?;
            let mut names = std::collections::HashMap::new();
            for position in bounds {
                let entry = work.snapshot.entry(position);
                snapshot_codec::write_json(&mut input, entry, work.limits.max_output_bytes)
                    .map_err(|_| output_limit())?;
                for block in entry.blocks() {
                    match block {
                        ContentBlock::ToolUse(tool) => {
                            names.insert(tool.id.as_str(), tool.name.as_str());
                        }
                        ContentBlock::ToolResult(result) => {
                            if let Some(name) = names.get(result.tool_use_id.as_str()) {
                                snapshot_codec::write_json(
                                    &mut input,
                                    name,
                                    work.limits.max_output_bytes,
                                )
                                .map_err(|_| output_limit())?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            snapshot_codec::write_json(
                &mut input,
                work.snapshot.activity.prompt(index).expect("view turn"),
                work.limits.max_output_bytes,
            )
            .map_err(|_| output_limit())?;
        }
    }
    produce(work)
}

fn render_query(
    work: &mut Work,
    range: Range<usize>,
    query: &Value,
) -> Result<Projection, SnapshotError> {
    let budget = budget(field(query, "budget")?)?;
    let tool_results = field(query, "tool_results")?
        .as_bool()
        .ok_or_else(|| invalid("tool_results must be boolean"))?;
    checked_render(work, &range, |work| {
        let lift = lift_range(work, &range)?;
        let session = view(&lift, &range, work.snapshot);
        let parts: Vec<_> = session
            .turn_views()
            .map(|(_, prompt, events, uses)| {
                render::render_turn_parts(
                    prompt,
                    events,
                    &uses.iter().map(|use_| &use_.call).collect::<Vec<_>>(),
                    &budget,
                    tool_results,
                )
            })
            .filter(|text| !text.is_empty())
            .collect();
        scalar(json!(parts.join("\n\n")), work)
    })
}

#[derive(serde::Serialize)]
struct PredicateFileWire<'a> {
    path: &'a str,
}

#[derive(serde::Serialize)]
struct PredicateInputWire<'a> {
    calls: Vec<(&'a str, Vec<&'a str>)>,
    commands: Vec<&'a str>,
    edited_files: Vec<PredicateFileWire<'a>>,
    skills: Vec<&'a str>,
}

fn predicate_inputs_query(
    session: &Session,
    next: usize,
    work: &Work,
) -> Result<Projection, SnapshotError> {
    let calls = session.tool_calls().items();
    let wire = PredicateInputWire {
        calls: calls
            .iter()
            .map(|use_| (use_.call.name(), use_.call.file_paths()))
            .collect(),
        commands: session.commands(),
        edited_files: calls
            .iter()
            .flat_map(|use_| {
                use_.edits
                    .iter()
                    .map(|(path, _)| PredicateFileWire { path })
            })
            .collect(),
        skills: session
            .tool_calls()
            .named("Skill")
            .items()
            .into_iter()
            .filter_map(|use_| match &use_.call {
                ToolCall::Skill(call) => Some(call.skill.as_str()),
                _ => None,
            })
            .collect(),
    };
    record_page(
        std::iter::once(snapshot_codec::encode(&wire, work.limits.max_output_bytes)),
        "cc-transcript.predicate-inputs/1",
        next,
        false,
        work,
    )
}

fn query(work: &mut Work, request: &Value, next: usize) -> Result<Projection, SnapshotError> {
    let query = field(request, "query")?;
    if query.get("subagents").and_then(|v| v.as_bool()) == Some(true) {
        return Err(invalid("subagents require host snapshot composition"));
    }
    let kind = string(query, "kind")?;
    let mut range = selected_range(work, request)?;
    if kind == "pending_named_task" && !range.is_empty() {
        let turn = work
            .snapshot
            .activity
            .turn_of_event(range.end - 1)
            .expect("view turn");
        range.start = range.start.max(
            work.snapshot
                .activity
                .turn_bounds(turn)
                .expect("view turn")
                .start,
        );
    }
    match kind {
        "user_text" | "first_prompt" | "prompts" => return prompt_query(work, range, query, next),
        "assistant_text" => return assistant_query(work, range, query),
        "signal_texts" => return signal_query(work, range, query, next),
        "workflow_text" => return workflow_query(work, range, query),
        "render" => return render_query(work, range, query),
        _ => {}
    }
    if kind == "event_count" {
        return scalar(json!(range.len()), work);
    }
    if kind == "turn_count" {
        let count = if range.is_empty() {
            0
        } else {
            work.snapshot.activity.turn_of_event(range.end - 1).unwrap()
                - work.snapshot.activity.turn_of_event(range.start).unwrap()
                + 1
        };
        return scalar(json!(count), work);
    }
    if kind == "events" {
        if next > range.len() {
            return Err(invalid("event cursor out of range"));
        }
        let reverse = string(query, "order")? == "reverse";
        let stop = range.len().min(
            next.saturating_add(work.limits.max_items.min(256))
                .min(next.saturating_add(work.limits.max_events)),
        );
        let mut records = Vec::new();
        for offset in next..stop {
            let index = if reverse {
                range.end - 1 - offset
            } else {
                range.start + offset
            };
            work.charge_range(index..index + 1)?;
            let payload = snapshot_codec::encode(
                &EventWire::new(index, work.snapshot.entry(index)),
                work.limits.max_output_bytes,
            )?;
            records.push(payload);
        }
        return finish(
            json!({"kind":"records","record_schema":"cc-transcript.event/1","records_json":records}),
            work.limits,
            (stop < range.len()).then_some(stop),
        );
    }
    let lift = lift_range(work, &range)?;
    let session = view(&lift, &range, work.snapshot);
    let values = || strings(query, "values");
    let pattern = || string(query, "pattern");
    let reverse = query.get("order").and_then(|v| v.as_str()) == Some("reverse");
    match kind {
        "has_tool" => scalar(json!(session.has_tool(pattern()?)), work),
        "has_read" => scalar(json!(session.has_read(pattern()?)), work),
        "has_edit_to" => scalar(json!(session.has_edit_to(&values()?)), work),
        "has_skill" => scalar(json!(session.has_skill(&values()?)), work),
        "has_edit" => scalar(json!(!session.edited_files().is_empty()), work),
        "has_error" => scalar(json!(session.count_failures() != 0), work),
        "has_read_glob" => scalar(
            json!(session
                .tool_calls()
                .named("Read")
                .touching(&values()?)
                .any()),
            work,
        ),
        "has_pending_tool" => {
            let spec = values()?.join("|");
            scalar(
                json!(session
                    .tool_calls()
                    .named(&spec)
                    .where_(|use_| use_.result.is_none())
                    .any()),
                work,
            )
        }
        "has_skill_suffix" => {
            let names = values()?;
            scalar(
                json!(session
                    .tool_calls()
                    .named("Skill")
                    .items()
                    .iter()
                    .any(|use_| match &use_.call {
                        ToolCall::Skill(call) =>
                            names.contains(&call.skill.as_str())
                                || names.contains(
                                    &call
                                        .skill
                                        .split_once(':')
                                        .map_or(call.skill.as_str(), |(_, tail)| tail)
                                ),
                        _ => false,
                    })),
                work,
            )
        }
        "has_command_regex" => {
            let regex = regex::Regex::new(pattern()?).map_err(|e| invalid(e.to_string()))?;
            scalar(
                json!(session
                    .commands()
                    .iter()
                    .any(|command| regex.is_match(command))),
                work,
            )
        }
        "has_command" => {
            #[cfg(feature = "command")]
            {
                scalar(json!(session.has_command(&values()?)), work)
            }
            #[cfg(not(feature = "command"))]
            {
                Err(invalid("has_command requires the command feature"))
            }
        }
        "has_override" => scalar(
            json!(session.has_override(string(query, "token")?, &strings(query, "invalidated_by")?)),
            work,
        ),
        "failures" => scalar(json!(session.count_failures()), work),
        "message_count" => {
            let role = string(query, "role")?;
            if !["user", "assistant", "any"].contains(&role) {
                return Err(invalid("invalid message role"));
            }
            scalar(
                json!(session
                    .events()
                    .iter()
                    .filter(|event| match event {
                        Entry::User(_) => role != "assistant",
                        Entry::Assistant(_) => role != "user",
                        _ => false,
                    })
                    .count()),
                work,
            )
        }
        "unresolved_tools" => {
            let spec = strings(query, "names")?.join("|");
            scalar(
                json!(session
                    .tool_calls()
                    .named(&spec)
                    .where_(|use_| use_.result.is_none())
                    .count()),
                work,
            )
        }
        "tool_count" => {
            let calls = session.tool_calls();
            let calls = if field(query, "name")?.is_null() {
                calls
            } else {
                calls.named(string(query, "name")?)
            };
            let calls = match string(query, "errors")? {
                "exclude" => calls,
                "include" => calls.with_errors(),
                "only" => calls.failed(),
                _ => return Err(invalid("invalid errors mode")),
            };
            let Some(input) = query.get("input_regex").filter(|v| !v.is_null()) else {
                return scalar(json!(calls.count()), work);
            };
            let flags = count(input, "flags")?;
            if flags & !(2 | 8 | 16 | 32 | 64) != 0 {
                return Err(invalid("unsupported regular expression flags"));
            }
            let regex = RegexBuilder::new(string(input, "pattern")?)
                .case_insensitive(flags & 2 != 0)
                .multi_line(flags & 8 != 0)
                .dot_matches_new_line(flags & 16 != 0)
                .ignore_whitespace(flags & 64 != 0)
                .build()
                .map_err(|e| invalid(e.to_string()))?;
            scalar(
                json!(calls
                    .where_input(&[(string(input, "field")?, InputRule::Regex(&regex))])
                    .count()),
                work,
            )
        }
        "turns" => record_page(
            session.turn_views().map(|(index, prompt, events, uses)| {
                let original = &lift.turns[index - lift.turns[0].index];
                let start = work
                    .snapshot
                    .activity
                    .turn_bounds(index)
                    .unwrap()
                    .start
                    .max(range.start);
                let record = TurnWire {
                    codec: TURN_CODEC,
                    index,
                    prompt,
                    started_at: original.started_at,
                    ended_at: original.ended_at,
                    events: events
                        .iter()
                        .enumerate()
                        .map(|(offset, event)| EventWire::new(start + offset, event))
                        .collect(),
                    tool_uses: uses
                        .iter()
                        .map(|use_| ToolUseWire::new(use_, &work.snapshot.session_id))
                        .collect(),
                };
                snapshot_codec::encode(&record, work.limits.max_output_bytes)
            }),
            "cc-transcript.turn/1",
            next,
            reverse,
            work,
        ),
        "tool_calls" => {
            let calls = session.tool_calls().with_errors();
            let calls = if query.get("name").is_some_and(|name| !name.is_null()) {
                calls.named(string(query, "name")?)
            } else {
                calls
            };
            record_page(
                calls.items().into_iter().map(|use_| {
                    snapshot_codec::encode(
                        &ToolUseWire::new(use_, &work.snapshot.session_id),
                        work.limits.max_output_bytes,
                    )
                }),
                "cc-transcript.tool-use/1",
                next,
                reverse,
                work,
            )
        },
        "files_touched" | "edited_files" => {
            let files = if kind == "files_touched" {
                session.files_touched()
            } else {
                session.edited_files()
            };
            record_page(
                files.into_iter().map(|file| {
                    snapshot_codec::encode(
                        &FileRefRecord { path: file.path },
                        work.limits.max_output_bytes,
                    )
                }),
                "cc-transcript.file-ref/1",
                next,
                reverse,
                work,
            )
        }
        "predicate_inputs" | "deep_predicate_inputs" => predicate_inputs_query(&session,next,work),
        "sidechain_membership" => Err(invalid("sidechain_membership requires host snapshot composition")),
        "pending_named_task" => scalar(json!(session.current_turn().tool_calls().named("Task").items().iter().any(|use_| {
            use_.result.is_none() && matches!(&use_.call,ToolCall::Task(call) if call.agent_name.as_ref().is_some_and(json_truthy))
        })), work),
        _ => Err(invalid(format!("unsupported query {kind}"))),
    }
}

fn probe(work: &mut Work, request: &Value) -> Result<Projection, SnapshotError> {
    let range = selected_range(work, request)?;
    work.charge_range(range.clone())?;
    let entries = work.snapshot.range(range);
    let opts = ActivityOpts {
        waiting_tools: strings(request, "waiting_tools")?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        human_facing_tools: strings(request, "human_facing_tools")?
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    let activity = session_activity_refs(&entries, &opts);
    let reason = if activity.is_waiting {
        "pending_activity"
    } else if activity.mid_tool {
        "mid_tool"
    } else {
        "idle"
    };
    finish(
        json!({"kind":"activity_probe","waiting":activity.is_waiting,"reason":reason,"tool_registry_generation":string(request,"tool_registry_generation")?}),
        work.limits,
        None,
    )
}

fn user_prefix(content: &crate::types::UserContent, prefix: &str) -> bool {
    let characters: Box<dyn Iterator<Item = char> + '_> = match content {
        crate::types::UserContent::Plain(text) => Box::new(text.chars()),
        crate::types::UserContent::Blocks(blocks) => Box::new(
            blocks
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::Text(text) = block {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .enumerate()
                .flat_map(|(index, text)| {
                    (index != 0).then_some(' ').into_iter().chain(text.chars())
                }),
        ),
    };
    let mut characters = characters.skip_while(|character| crate::pystr::is_space(*character));
    let mut trailing_space = false;
    for expected in prefix.chars() {
        if characters.next() != Some(expected) {
            return false;
        }
        trailing_space = crate::pystr::is_space(expected);
    }
    !trailing_space || characters.any(|character| !crate::pystr::is_space(character))
}

pub fn classifier_facts(
    snapshot: &TranscriptSnapshot,
    prefix: &str,
    event_limit: usize,
    limits: &WorkLimits,
    cancel: &Cancellation,
    usage: &mut ProjectionUsage,
) -> Result<Value, SnapshotError> {
    let mut work = Work::new(snapshot, limits, cancel);
    let result = (|| {
        cancel.check(limits.deadline_unix_ms)?;
        if limits.max_items == 0 {
            return Err(SnapshotError::new(Status::Incomplete, "item_limit"));
        }
        let mut users = 0usize;
        let mut sidechain_users = 0usize;
        for chunk in &snapshot.chunks {
            cancel.check(limits.deadline_unix_ms)?;
            users = users.saturating_add(chunk.user_count);
            sidechain_users = sidechain_users.saturating_add(chunk.sidechain_user_count);
        }
        let mut has_user_prefix = false;
        for index in 0..snapshot.event_count.min(event_limit) {
            work.charge_range(index..index + 1)?;
            if let Entry::User(user) = snapshot.entry(index) {
                if user_prefix(&user.content, prefix) {
                    has_user_prefix = true;
                    break;
                }
            }
        }
        let value = json!({"has_users":users != 0,"all_users_sidechain":users != 0 && users == sidechain_users,"has_user_prefix":has_user_prefix});
        snapshot_codec::encoded_size(&value, limits.max_output_bytes)?;
        Ok(value)
    })();
    usage.read_bytes = work.bytes;
    usage.events = work.events;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_user_prefix_matches_joined_python_strip() {
        use crate::types::UserContent;
        for text in ["", " a ", "\u{1c}<system_instruction> x", " a b  "] {
            for prefix in ["", "a", "a ", "a b ", "<system_instruction>"] {
                let content = UserContent::Plain(text.into());
                assert_eq!(
                    user_prefix(&content, prefix),
                    crate::pystr::strip(&content.text()).starts_with(prefix)
                );
            }
        }
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(" ".into()),
            ContentBlock::Text("a".into()),
            ContentBlock::Text("b  ".into()),
        ]);
        for prefix in ["", "a", "a ", "a b", "a b "] {
            assert_eq!(
                user_prefix(&content, prefix),
                crate::pystr::strip(&content.text()).starts_with(prefix)
            );
        }
    }

    use crate::gateway::Provider;
    use crate::parse::parse_entry;
    use crate::snapshot::{EntryChunk, SourceIdentity, SourceStamp};
    use crate::snapshot_activity::ActivityIndex;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn snapshot(raw: &[Value]) -> TranscriptSnapshot {
        let entries: Vec<_> = raw
            .iter()
            .map(|value| parse_entry(value.clone()).unwrap())
            .collect();
        let activity = ActivityIndex::new(&entries.iter().collect::<Vec<_>>(), None);
        let count = entries.len();
        TranscriptSnapshot {
            id: "test".into(),
            canonical_path: PathBuf::from("/snapshot.jsonl"),
            stamp: SourceStamp {
                identity: SourceIdentity {
                    device: 1,
                    inode: 1,
                },
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            },
            provider: Provider::Claude,
            session_id: "s".into(),
            chunks: vec![Arc::new(EntryChunk::new(0, entries))],
            activity: Arc::new(activity),
            committed_bytes: 0,
            provisional_tail: false,
            fence: Vec::new(),
            event_count: count,
        }
    }

    fn user(uuid: &str, text: &str) -> Value {
        json!({"type":"user","uuid":uuid,"sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":text}})
    }

    fn tool(uuid: &str, id: &str, name: &str, input: Value) -> Value {
        json!({"type":"assistant","uuid":uuid,"sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}})
    }

    fn limits() -> WorkLimits {
        WorkLimits {
            max_read_bytes: 4 * 1024 * 1024,
            max_events: 100,
            max_items: 100,
            max_output_bytes: 1024 * 1024,
            max_discovery_entries: 100,
            max_sources: 10,
            deadline_unix_ms: u64::MAX,
        }
    }

    fn request(query: Value, selectors: Value) -> Value {
        json!({"operation":"query","view":{"attachments":[],"selectors":selectors},"query":query})
    }

    fn run(snapshot: &TranscriptSnapshot, request: &Value) -> Projection {
        project(snapshot, request, &limits(), &Cancellation::default(), 0).unwrap()
    }

    #[test]
    fn duplicate_uuids_remain_positional_in_trimmed_tool_queries() {
        let snap = snapshot(&[
            user("u", "hello"),
            tool("same", "first", "Read", json!({"file_path":"first.rs"})),
            tool("same", "second", "Read", json!({"file_path":"second.rs"})),
        ]);
        let first = run(
            &snap,
            &request(
                json!({"kind":"has_read","pattern":"first.rs","subagents":false}),
                json!([{"kind":"event_range","start":1,"stop":2}]),
            ),
        );
        assert_eq!(first.data["value"].as_bool(), Some(true));
        let second = run(
            &snap,
            &request(
                json!({"kind":"has_read","pattern":"second.rs","subagents":false}),
                json!([{"kind":"event_range","start":1,"stop":2}]),
            ),
        );
        assert_eq!(second.data["value"].as_bool(), Some(false));
        let render = run(
            &snap,
            &request(
                json!({"kind":"render","budget":{"turn_chars":100,"tool_chars":100},"tool_results":false}),
                json!([{"kind":"event_range","start":1,"stop":2}]),
            ),
        );
        assert!(render.data["value"].as_str().unwrap().contains("first.rs"));
        assert!(!render.data["value"].as_str().unwrap().contains("second.rs"));
    }

    #[test]
    fn insufficient_work_does_not_report_a_false_predicate() {
        let snap = snapshot(&[
            user("u", "hello"),
            tool("a", "read", "Read", json!({"file_path":"found.rs"})),
        ]);
        let mut cap = limits();
        cap.max_events = 1;
        let result = project(
            &snap,
            &request(
                json!({"kind":"has_read","pattern":"found.rs","subagents":false}),
                json!([]),
            ),
            &cap,
            &Cancellation::default(),
            0,
        )
        .unwrap();
        assert!(!result.complete);
        assert!(result.data.is_null());
        assert_eq!(result.reason.as_deref(), Some("event_limit"));
    }

    #[test]
    fn pagination_is_positional_and_reversible() {
        let snap = snapshot(&[
            user("same", "first"),
            user("same", "second"),
            user("third", "third"),
        ]);
        let req = request(json!({"kind":"events","order":"reverse"}), json!([]));
        let mut cap = limits();
        cap.max_items = 1;
        cap.max_events = 1;
        let mut seen = Vec::new();
        let mut next = 0;
        loop {
            let result = project(&snap, &req, &cap, &Cancellation::default(), next).unwrap();
            let raw = result.data["records_json"][0].as_str().unwrap();
            let value: Value = sonic_rs::from_str(raw).unwrap();
            seen.push(value["i"].as_u64().unwrap());
            match result.next {
                Some(cursor) => next = cursor,
                None => {
                    assert!(result.complete);
                    break;
                }
            }
        }
        assert_eq!(seen, [2, 1, 0]);
    }

    #[test]
    fn capture_matches_canonical_context_and_hydrates() {
        let snap = snapshot(&[
            user("first", "first prompt"),
            tool("a", "read", "Read", json!({"file_path":"a.rs"})),
            user("last", "last prompt"),
        ]);
        let anchor = EventRef {
            session_id: "s".into(),
            event_uuid: "a".into(),
            tool_use_id: Some("read".into()),
        };
        let req = json!({"operation":"capture","view":{"attachments":[],"selectors":[]},"anchors":[{"session_id":"s","event_uuid":"a","tool_use_id":"read"}],"before":1,"after":1,"preview_chars":100});
        let captured = run(&snap, &req);
        let raw = captured.data["windows_json"][0].as_str().unwrap();
        let entries = snap.entries();
        let lift = snap.activity.project("s", &entries, &[0, 1]);
        let expected = crate::context::capture_window(&lift, &anchor, 1, 1, 100).unwrap();
        assert_eq!(raw, expected.to_json());
        let hydrated = run(
            &snap,
            &json!({"operation":"hydrate","windows_json":[raw],"render":{"before":{"turn_chars":100,"tool_chars":100},"trigger":{"turn_chars":100,"tool_chars":100},"after":{"turn_chars":100,"tool_chars":100}}}),
        );
        let expected_text = lift
            .turns
            .iter()
            .map(|turn| {
                render::render_turn(
                    turn,
                    &Budget {
                        turn_chars: 100,
                        tool_chars: 100,
                    },
                    true,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(
            hydrated.data["windows"][0]["availability"].as_str(),
            Some("full")
        );
        assert_eq!(
            hydrated.data["windows"][0]["rendered"].as_str(),
            Some(expected_text.as_str())
        );
    }

    #[test]
    fn context_writer_matches_existing_serialization_and_stops_at_cap() {
        let snap = snapshot(&[user("u", "hello \n\t\u{0} \u{1f600}")]);
        let entries = snap.entries();
        let lift = snap.activity.project("s", &entries, &[0]);
        let window = crate::context::capture_window(
            &lift,
            &EventRef::new("s".into(), "u".into()),
            0,
            0,
            100,
        )
        .unwrap();
        let raw = window.to_json();
        assert_eq!(window.to_json_bounded(raw.len()).unwrap(), raw);
        assert!(window.to_json_bounded(raw.len() - 1).is_err());
    }

    #[test]
    fn cancellation_deadlines_and_output_caps_are_explicit() {
        let snap = snapshot(&[user("u", "large prompt")]);
        let req = request(json!({"kind":"user_text"}), json!([]));
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(
            matches!(project(&snap,&req,&limits(),&cancel,0),Err(error) if error.status == Status::Cancelled)
        );
        let mut cap = limits();
        cap.deadline_unix_ms = 0;
        assert!(
            matches!(project(&snap,&req,&cap,&Cancellation::default(),0),Err(error) if error.status == Status::Deadline)
        );
        cap = limits();
        cap.max_output_bytes = 8;
        assert!(
            matches!(project(&snap,&req,&cap,&Cancellation::default(),0),Err(error) if error.status == Status::OutputLimit)
        );
        let mut writer = BoundedWriter {
            bytes: Vec::new(),
            limit: 8,
        };
        assert!(writer.write_all(b"12345678").is_ok());
        assert!(writer.write_all(b"9").is_err());
        assert_eq!(writer.bytes.len(), 8);
    }

    #[test]
    fn large_input_and_external_results_are_charged_before_projection() {
        let snap = snapshot(&[
            user("u", "hello"),
            tool("a", "read", "Read", json!({"file_path":"a.rs"})),
            user("next", "next"),
            json!({"type":"user","uuid":"result","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","message":{"content":[{"type":"tool_result","tool_use_id":"read","content":"x".repeat(32768)}]}}),
        ]);
        let mut cap = limits();
        cap.max_read_bytes = 16 * 1024;
        let req = request(
            json!({"kind":"has_tool","pattern":"Read","subagents":false}),
            json!([{"kind":"event_range","start":0,"stop":2}]),
        );
        let result = project(&snap, &req, &cap, &Cancellation::default(), 0).unwrap();
        assert!(!result.complete);
        assert_eq!(result.reason.as_deref(), Some("read_limit"));
        assert!(result.data.is_null());
    }

    #[test]
    fn current_turn_does_not_charge_old_turns() {
        let snap = snapshot(&[user("old", &"x".repeat(32768)), user("new", "current")]);
        let mut cap = limits();
        cap.max_events = 1;
        cap.max_read_bytes = 16 * 1024;
        let result = project(
            &snap,
            &request(
                json!({"kind":"user_text"}),
                json!([{"kind":"current_turn"}]),
            ),
            &cap,
            &Cancellation::default(),
            0,
        )
        .unwrap();
        assert!(result.complete);
        assert_eq!(result.data["value"].as_str(), Some("current"));
    }

    #[test]
    fn assistant_zero_count_and_prompt_pages_match_python_selection() {
        let snap = snapshot(&[
            json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"model":"test","content":[{"type":"text","text":"first"}]}}),
            user("u", "prompt"),
            json!({"type":"assistant","uuid":"b","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"text","text":"second"}]}}),
        ]);
        let text = run(
            &snap,
            &request(
                json!({"kind":"assistant_text","count":0,"max_per_message":100}),
                json!([]),
            ),
        );
        assert_eq!(text.data["value"].as_str(), Some("first\n---\nsecond"));
        let prompts = run(
            &snap,
            &request(
                json!({"kind":"prompts","selection":"last","count":10}),
                json!([]),
            ),
        );
        assert_eq!(prompts.data["values"].as_array().unwrap().len(), 1);
        assert_eq!(prompts.data["values"][0].as_str(), Some("prompt"));
    }

    #[test]
    fn text_and_render_admission_precede_the_producer() {
        use std::cell::Cell;
        let visits = Cell::new(0);
        let text = "x".repeat(32768);
        let produced = with_text_budget(&[&text], 128, || {
            visits.set(visits.get() + 1);
            text.clone()
        });
        assert!(matches!(produced,Err(error) if error.status==Status::OutputLimit));
        assert_eq!(visits.get(), 0);
        let snap = snapshot(&[user("u", &text)]);
        let mut cap = limits();
        cap.max_output_bytes = 128;
        let cancel = Cancellation::default();
        let mut work = Work::new(&snap, &cap, &cancel);
        let produced = checked_render(&mut work, &(0..1), |_| {
            visits.set(visits.get() + 1);
            Ok(())
        });
        assert!(matches!(produced,Err(error) if error.status==Status::OutputLimit));
        assert_eq!(visits.get(), 0);
        let req = request(json!({"kind":"user_text"}), json!([]));
        assert!(
            matches!(project(&snap,&req,&cap,&cancel,0),Err(error) if error.status==Status::OutputLimit)
        );
    }

    #[test]
    fn text_preflight_counts_escaping_and_keeps_unicode_prefixes() {
        let parts = ["\n\t\u{0}", "\"\\😀"];
        let text = parts.concat();
        let exact = sonic_rs::to_string(&text).unwrap().len();
        assert_eq!(text_plan_size(&parts, exact).unwrap(), exact);
        assert!(text_plan_size(&parts, exact - 1).is_err());
        let snap = snapshot(&[
            json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"model":"test","content":[{"type":"text","text":"  😀"},{"type":"text","text":"é end  "}]}}),
        ]);
        let result = run(
            &snap,
            &request(
                json!({"kind":"assistant_text","count":1,"max_per_message":3}),
                json!([]),
            ),
        );
        assert_eq!(result.data["value"].as_str(), Some("😀 é"));
    }

    #[test]
    fn assistant_prefix_does_not_materialize_discarded_text() {
        let snap = snapshot(&[
            json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"model":"test","content":[{"type":"text","text":"x".repeat(32768)}]}}),
        ]);
        let mut cap = limits();
        cap.max_output_bytes = 128;
        let result = project(
            &snap,
            &request(
                json!({"kind":"assistant_text","count":1,"max_per_message":3}),
                json!([]),
            ),
            &cap,
            &Cancellation::default(),
            0,
        )
        .unwrap();
        assert_eq!(result.data["value"].as_str(), Some("xxx"));
    }

    #[test]
    fn workflow_text_keeps_message_and_block_separators() {
        let snap = snapshot(&[
            json!({"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":[{"type":"text","text":"alpha"},{"type":"text","text":"beta"}]}}),
            json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"text","text":"gamma"}]}}),
        ]);
        for (mode, pattern) in [
            ("contains", "beta\ngamma"),
            ("regex", r"alpha beta\ngamma$"),
        ] {
            let result = run(
                &snap,
                &request(
                    json!({"kind":"workflow_text","mode":mode,"pattern":pattern}),
                    json!([]),
                ),
            );
            assert_eq!(result.data["value"].as_bool(), Some(true));
        }
    }

    #[test]
    fn named_pending_tasks_are_limited_to_current_view_turn() {
        let snap = snapshot(&[
            user("u", "old"),
            tool(
                "old-task",
                "old",
                "Task",
                json!({"prompt":"work","name":"teammate"}),
            ),
            user("v", "current"),
            tool("new-task", "new", "Task", json!({"prompt":"work"})),
        ]);
        let result = run(
            &snap,
            &request(json!({"kind":"pending_named_task"}), json!([])),
        );
        assert_eq!(result.data["value"].as_bool(), Some(false));
        let result = run(
            &snap,
            &request(
                json!({"kind":"pending_named_task"}),
                json!([{"kind":"event_range","start":0,"stop":2}]),
            ),
        );
        assert_eq!(result.data["value"].as_bool(), Some(true));
    }

    #[test]
    fn signal_texts_preserve_typed_prose_order_and_count_texts() {
        let snap = snapshot(&[
            user("u", "human"),
            json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[
                {"type":"text","text":"visible"},{"type":"thinking","thinking":"thought"},
                {"type":"tool_use","id":"task","name":"TaskCreate","input":{"subject":"task","description":"details"}},
                {"type":"tool_use","id":"report","name":"ReportFindings","input":{"findings":[{"summary":"finding","failure_scenario":"failure"},{"summary":"second"}]}},
                {"type":"tool_use","id":"todo","name":"TodoWrite","input":{"todos":[{"content":"todo","subject":"subject"}]}}
            ]}}),
        ]);
        let all = run(
            &snap,
            &request(
                json!({"kind":"signal_texts","window":"current_turn","origin":"any"}),
                json!([]),
            ),
        );
        let actual: Vec<_> = all.data["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            actual,
            [
                "human",
                "visible",
                "thought",
                "task details",
                "finding failure",
                "second",
                "todo subject"
            ]
        );
        let last = run(
            &snap,
            &request(
                json!({"kind":"signal_texts","window":3,"origin":"assistant"}),
                json!([]),
            ),
        );
        let actual: Vec<_> = last.data["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(actual, ["finding failure", "second", "todo subject"]);
    }

    #[test]
    fn signal_texts_exclude_harness_and_relay_messages() {
        let snap = snapshot(&[
            user("u", "human"),
            json!({"type":"user","uuid":"meta","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","isMeta":true,"message":{"content":"metadata"}}),
            json!({"type":"user","uuid":"summary","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","isCompactSummary":true,"message":{"content":"summary"}}),
            user(
                "relay",
                "<teammate-message teammate_id=\"peer\">relay</teammate-message>",
            ),
        ]);
        let result = run(
            &snap,
            &request(
                json!({"kind":"signal_texts","window":10,"origin":"any"}),
                json!([]),
            ),
        );
        let actual: Vec<_> = result.data["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(actual, ["human"]);
        let zero = run(
            &snap,
            &request(
                json!({"kind":"signal_texts","window":0,"origin":"any"}),
                json!([]),
            ),
        );
        assert!(zero.data["values"].as_array().unwrap().is_empty());
    }

    #[test]
    fn local_predicate_inputs_match_cached_session_semantics() {
        let snap = snapshot(&[
            user("u", "read"),
            tool("a", "read", "Read", json!({"file_path":"a.rs"})),
            tool("b", "skill", "Skill", json!({"skill":"review"})),
        ]);
        for kind in ["predicate_inputs", "deep_predicate_inputs"] {
            let result = run(
                &snap,
                &request(json!({"kind":kind,"order":"forward"}), json!([])),
            );
            assert!(result.complete);
            assert_eq!(result.data["records_json"].as_array().unwrap().len(), 1);
            let raw = result.data["records_json"][0].as_str().unwrap();
            let decoded =
                crate::snapshot_codec::decode_predicate_inputs(&[raw.to_owned()], 1024 * 1024)
                    .unwrap();
            assert_eq!(
                decoded[0].calls[0],
                ("Read".to_owned(), vec!["a.rs".to_owned()])
            );
            assert_eq!(decoded[0].skills, ["review"]);
        }
    }

    #[test]
    fn activity_probe_reuses_native_waiting_semantics() {
        let snap = snapshot(&[
            user("u", "hello"),
            tool("a", "monitor", "Monitor", json!({})),
        ]);
        let result = run(
            &snap,
            &json!({"operation":"activity_probe","view":{"attachments":[],"selectors":[]},"waiting_tools":["Monitor"],"human_facing_tools":["AskUserQuestion"],"tool_registry_generation":"test","policy_version":"test"}),
        );
        assert_eq!(result.data["waiting"].as_bool(), Some(true));
        assert_eq!(result.data["reason"].as_str(), Some("pending_activity"));
    }
}
