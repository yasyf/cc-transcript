use std::sync::Arc;

use cc_transcript_core::snapshot::{SnapshotError, Status, TranscriptSnapshot};
use cc_transcript_core::snapshot_codec::{self, Bound, EventWire, MAX_RECORDS, MAX_RECORD_BYTES};
use sha2::{Digest, Sha256};
use sonic_rs::{json, JsonValueTrait, Value};

use crate::frame::MAX_FRAME_BYTES;
use crate::owner::CORE_SCHEMA;
use crate::pyjson::escaped_len;
use crate::scope::{Failure, Scope, ScopeWork};

const PAGE_ENVELOPE_BYTES: usize = 16 * 1024;

struct Mark {
    source: String,
    position: usize,
    digest: String,
}

impl Mark {
    fn encode(&self) -> String {
        format!("v1:{}:{}:{}", self.source, self.position, self.digest)
    }

    fn decode(token: &str) -> Option<Self> {
        let rest = token.strip_prefix("v1:")?;
        let (rest, digest) = rest.rsplit_once(':')?;
        let (source, position) = rest.rsplit_once(':')?;
        Some(Self {
            source: source.to_owned(),
            position: position.parse().ok()?,
            digest: digest.to_owned(),
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

fn digest(snapshot: &TranscriptSnapshot, position: usize) -> Result<String, SnapshotError> {
    if position == 0 {
        return Ok(String::new());
    }
    let chunk = &snapshot.chunks[snapshot
        .chunks
        .partition_point(|chunk| chunk.start < position)
        - 1];
    let encoded = snapshot_codec::encode_bounded(
        &EventWire::new(position - 1, &chunk.entries[position - 1 - chunk.start]),
        Bound::budget("review event digest", usize::MAX),
    )?;
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes()))[..16].to_owned())
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

fn resumed(
    snapshot: &TranscriptSnapshot,
    token: Option<&str>,
) -> Result<Option<usize>, SnapshotError> {
    let Some(mark) = token.and_then(Mark::decode) else {
        return Ok(None);
    };
    if mark.source != source(snapshot)
        || mark.position > snapshot.event_count
        || mark.digest != digest(snapshot, mark.position)?
    {
        return Ok(None);
    }
    Ok(Some(mark.position))
}

pub fn page(scope: &mut Scope<'_>, request: &Value, id: &str) -> Result<Value, Failure> {
    scope.checkpoint()?;
    let snapshot = Arc::clone(&scope.snapshot);
    let token = request.get("watermark").and_then(|token| token.as_str());
    let context_events = request
        .get("context_events")
        .and_then(|count| count.as_u64())
        .map_or(0, |count| usize::try_from(count).unwrap_or(usize::MAX));
    let resumed = resumed(&snapshot, token).map_err(|error| scope.fail(error))?;
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
        digest: digest(&snapshot, position).map_err(|error| scope.fail(error))?,
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
            digest: "00ff".to_owned(),
        };
        let decoded = Mark::decode(&mark.encode()).unwrap();
        assert_eq!(
            (decoded.source, decoded.position, decoded.digest),
            ("1:2:0:0".to_owned(), 42, "00ff".to_owned())
        );
        assert!(Mark::decode("v2:1:2").is_none());
        assert!(Mark::decode("v1:1:2:0:0:x:00ff").is_none());
    }
}
