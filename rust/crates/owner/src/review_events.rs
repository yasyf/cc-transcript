use std::sync::Arc;

use cc_transcript_core::snapshot::{
    Cancellation, NativeStore, SnapshotError, Status, TranscriptSnapshot,
};
use cc_transcript_core::snapshot_codec::{self, EventWire, MAX_RECORDS, MAX_RECORD_BYTES};
use cc_transcript_core::snapshot_input::native_input;
use sonic_rs::{json, JsonValueTrait, Value};

use crate::frame::MAX_FRAME_BYTES;
use crate::owner::{mismatch, scope_limits, text, CORE_SCHEMA};
use crate::pyjson::escaped_len;
use crate::scope::{Failure, Scope, ScopeWork};

const PAGE_ENVELOPE_BYTES: usize = 16 * 1024;

struct Mark {
    source: String,
    position: usize,
}

impl Mark {
    fn encode(&self) -> String {
        format!("v2:{}:{}", self.source, self.position)
    }

    fn decode(token: &str) -> Option<Self> {
        let (source, position) = token.strip_prefix("v2:")?.rsplit_once(':')?;
        Some(Self {
            source: source.to_owned(),
            position: position.parse().ok()?,
        })
    }
}

fn source(snapshot: &TranscriptSnapshot) -> String {
    let identity = snapshot.stamp.identity;
    format!(
        "{}:{}:{}:{}",
        identity.device, identity.inode, identity.window_base, snapshot.window_start
    )
}

fn record(snapshot: &TranscriptSnapshot, position: usize) -> Result<String, SnapshotError> {
    let chunk = &snapshot.chunks[snapshot
        .chunks
        .partition_point(|chunk| chunk.start <= position)
        - 1];
    snapshot_codec::encode(
        &EventWire::new(position, &chunk.entries[position - chunk.start]),
        MAX_RECORD_BYTES,
    )
}

fn revision(snapshot: &TranscriptSnapshot) -> String {
    format!("agent:{}:{}", source(snapshot), snapshot.event_count)
}

fn extends(snapshot: &TranscriptSnapshot, revision: &str) -> bool {
    revision
        .strip_prefix("agent:")
        .and_then(|rest| rest.rsplit_once(':'))
        .is_some_and(|(source_id, events)| {
            source_id == source(snapshot)
                && events
                    .parse::<usize>()
                    .is_ok_and(|events| events <= snapshot.event_count)
        })
}

fn charge(snapshot: &TranscriptSnapshot, position: usize, output_bytes: usize) -> ScopeWork {
    let chunk = &snapshot.chunks[snapshot
        .chunks
        .partition_point(|chunk| chunk.start <= position)
        - 1];
    let entry = chunk.entry_charges[position - chunk.start];
    ScopeWork {
        read_bytes: entry
            .owned_capacity_bytes
            .saturating_add(entry.opaque_dom_accounted_bytes),
        events: 1,
        items: 1,
        output_bytes,
    }
}

fn resumed(snapshot: &TranscriptSnapshot, token: Option<&str>) -> Option<usize> {
    token
        .and_then(Mark::decode)
        .filter(|mark| mark.source == source(snapshot) && mark.position <= snapshot.event_count)
        .map(|mark| mark.position)
}

pub fn serve(
    store: &NativeStore,
    request: &Value,
    context: &Value,
    cancel: &Cancellation,
    id: &str,
) -> Result<Value, Failure> {
    let limits = request.get("limits").ok_or_else(mismatch)?;
    let deadline = request.get("deadline_unix_ms").ok_or_else(mismatch)?;
    let bounds = scope_limits(limits, deadline)?;
    let mut acquired = store.request_agent(
        &json!({"schema":CORE_SCHEMA,"id":id,"operation":"acquire","path":text(request, "path")?,
            "classifier":{"id":"native","version":"1"},"deadline_unix_ms":deadline,"limits":limits}),
        context,
        cancel,
    );
    while acquired["status"].as_str() == Some("incomplete") {
        acquired = store.request_agent(
            &json!({"schema":CORE_SCHEMA,"id":id,"operation":"resume","cursor":acquired["cursor"]}),
            context,
            cancel,
        );
    }
    if acquired["status"].as_str() != Some("ok") {
        return Ok(acquired);
    }
    let handle = acquired["data"]["description"]["handle"].clone();
    let paged = native_input(&handle)
        .and_then(|handle| Scope::borrow(store, handle, context, bounds, cancel))
        .map_err(Failure::from)
        .and_then(|mut scope| page(&mut scope, request, id));
    store.request_agent(
        &json!({"schema":CORE_SCHEMA,"id":id,"operation":"release","kind":"lease",
            "token":handle["lease_id"],"owner_epoch":handle["owner_epoch"]}),
        context,
        cancel,
    );
    paged
}

fn page(scope: &mut Scope<'_>, request: &Value, id: &str) -> Result<Value, Failure> {
    scope.checkpoint()?;
    let snapshot = Arc::clone(&scope.snapshot);
    if let Some(expected) = request
        .get("source_revision")
        .and_then(|revision| revision.as_str())
    {
        if !extends(&snapshot, expected) {
            return Err(scope.fail(SnapshotError::new(
                Status::Changed,
                "review source revision changed",
            )));
        }
    }
    let token = request.get("watermark").and_then(|token| token.as_str());
    let context_events = request
        .get("context_events")
        .and_then(|count| count.as_u64())
        .map_or(0, |count| usize::try_from(count).unwrap_or(usize::MAX));
    let resumed = resumed(&snapshot, token);
    let fresh = resumed.unwrap_or(0);
    let start = fresh.saturating_sub(context_events);
    let budget = MAX_FRAME_BYTES - PAGE_ENVELOPE_BYTES;
    let mut used = 0usize;
    let mut records: Vec<String> = Vec::with_capacity(MAX_RECORDS);
    let mut skipped = Vec::new();
    let mut next = start;
    while next < snapshot.event_count && records.len() < MAX_RECORDS && skipped.len() < MAX_RECORDS
    {
        scope.checkpoint()?;
        let encoded = match record(&snapshot, next) {
            Ok(encoded) => encoded,
            Err(error) if error.status == Status::OutputLimit => {
                skipped.push(next);
                next += 1;
                continue;
            }
            Err(error) => return Err(scope.fail(error)),
        };
        let cost = escaped_len(&encoded) + 1;
        if cost > budget {
            skipped.push(next);
            next += 1;
            continue;
        }
        if used + cost > budget {
            break;
        }
        if let Err(failure) = scope.charge(charge(&snapshot, next, encoded.len())) {
            if records.is_empty() {
                return Err(failure);
            }
            break;
        }
        used += cost;
        records.push(encoded);
        next += 1;
    }
    let position = next.max(fresh);
    let watermark = Mark {
        source: source(&snapshot),
        position,
    };
    scope.materialize(used);
    scope.checkpoint()?;
    Ok(json!({
        "schema": CORE_SCHEMA,
        "id": id,
        "status": "ok",
        "complete": true,
        "data": {
            "kind": "review_events",
            "source_revision": revision(&snapshot),
            "canonical_path": snapshot.canonical_path.to_string_lossy().as_ref(),
            "mtime_ns": snapshot.stamp.mtime_ns.to_string(),
            "record_schema": "cc-transcript.event/1",
            "records_json": records,
            "event_start": start,
            "fresh_start": fresh,
            "skipped": skipped,
            "watermark": watermark.encode(),
            "caught_up": position == snapshot.event_count,
            "reset": token.is_some() && resumed.is_none(),
        },
        "cursor": null,
        "reason": null,
        "usage": scope.usage(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_round_trip_through_their_token() {
        let mark = Mark {
            source: "1:2:0:0".to_owned(),
            position: 42,
        };
        let decoded = Mark::decode(&mark.encode()).unwrap();
        assert_eq!(
            (decoded.source, decoded.position),
            ("1:2:0:0".to_owned(), 42)
        );
        assert!(Mark::decode("v1:1:2:0:0:42:00ff").is_none());
        assert!(Mark::decode("v2:1:2:0:0:x").is_none());
    }
}
