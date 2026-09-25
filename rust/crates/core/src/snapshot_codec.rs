use std::io::{self, Write};

use chrono::{DateTime, FixedOffset};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::activity::{Hunk, ToolUse};
use crate::snapshot::{SnapshotError, Status};
use crate::toolcall::ToolCall;
use crate::types::{Entry, ToolResultBlock};

pub const EVENT_CODEC: &str = "cc-transcript.native-entry/1";
pub const TOOL_CODEC: &str = "cc-transcript.native-tool-use/1";
pub const TURN_CODEC: &str = "cc-transcript.native-turn/1";
pub const MAX_RECORDS: usize = 256;
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
pub struct EventWire<'a> {
    pub codec: &'static str,
    pub i: usize,
    pub event: &'a Entry,
}

impl<'a> EventWire<'a> {
    pub fn new(i: usize, event: &'a Entry) -> Self {
        Self {
            codec: EVENT_CODEC,
            i,
            event,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRecord {
    pub codec: String,
    pub i: usize,
    pub event: Entry,
}

#[derive(Serialize)]
pub struct RefWire<'a> {
    pub session_id: &'a str,
    pub event_uuid: &'a str,
    pub tool_use_id: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefRecord {
    pub session_id: String,
    pub event_uuid: String,
    pub tool_use_id: String,
}

#[derive(Serialize)]
pub struct ToolUseWire<'a> {
    pub codec: &'static str,
    pub r#ref: RefWire<'a>,
    pub call: &'a ToolCall,
    pub result: Option<&'a ToolResultBlock>,
    pub result_ts: Option<DateTime<FixedOffset>>,
    pub edits: &'a [(String, Vec<Hunk>)],
    pub turn_index: usize,
    pub ts: DateTime<FixedOffset>,
}

impl<'a> ToolUseWire<'a> {
    pub fn new(use_: &'a ToolUse<'_>, session_id: &'a str) -> Self {
        Self {
            codec: TOOL_CODEC,
            r#ref: RefWire {
                session_id,
                event_uuid: use_.event_uuid,
                tool_use_id: use_.tool_use_id,
            },
            call: &use_.call,
            result: use_.result,
            result_ts: use_.result_ts,
            edits: &use_.edits,
            turn_index: use_.turn_index,
            ts: use_.ts,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUseRecord {
    pub codec: String,
    pub r#ref: RefRecord,
    pub call: ToolCall,
    pub result: Option<ToolResultBlock>,
    pub result_ts: Option<DateTime<FixedOffset>>,
    pub edits: Vec<(String, Vec<Hunk>)>,
    pub turn_index: usize,
    pub ts: DateTime<FixedOffset>,
}

#[derive(Serialize)]
pub struct TurnWire<'a> {
    pub codec: &'static str,
    pub index: usize,
    pub prompt: &'a str,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub ended_at: Option<DateTime<FixedOffset>>,
    pub events: Vec<EventWire<'a>>,
    pub tool_uses: Vec<ToolUseWire<'a>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRecord {
    pub codec: String,
    pub index: usize,
    pub prompt: String,
    pub started_at: Option<DateTime<FixedOffset>>,
    pub ended_at: Option<DateTime<FixedOffset>>,
    pub events: Vec<EventRecord>,
    pub tool_uses: Vec<ToolUseRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRefRecord {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredicateInputsRecord {
    pub calls: Vec<(String, Vec<String>)>,
    pub commands: Vec<String>,
    pub edited_files: Vec<FileRefRecord>,
    pub skills: Vec<String>,
}

struct LimitedWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("snapshot record byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode<T: Serialize + ?Sized>(
    record: &T,
    max_bytes: usize,
) -> Result<String, SnapshotError> {
    let mut out = LimitedWriter {
        bytes: Vec::new(),
        max_bytes: max_bytes.min(MAX_RECORD_BYTES),
    };
    sonic_rs::to_writer(&mut out, record)
        .map_err(|error| SnapshotError::new(Status::OutputLimit, error.to_string()))?;
    Ok(String::from_utf8(out.bytes).expect("JSON is UTF-8"))
}

pub fn encoded_size<T: Serialize + ?Sized>(
    record: &T,
    max_bytes: usize,
) -> Result<usize, SnapshotError> {
    struct Counter {
        remaining: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(io::Error::other("snapshot record byte limit"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let limit = max_bytes.min(MAX_RECORD_BYTES);
    let mut out = Counter { remaining: limit };
    sonic_rs::to_writer(&mut out, record)
        .map_err(|error| SnapshotError::new(Status::OutputLimit, error.to_string()))?;
    Ok(limit - out.remaining)
}

pub fn check_page(records: &[String], max_bytes: usize) -> Result<(), SnapshotError> {
    if records.len() > MAX_RECORDS {
        return Err(SnapshotError::new(
            Status::OutputLimit,
            "snapshot record count limit",
        ));
    }
    let mut remaining = max_bytes.min(MAX_PAGE_BYTES);
    for record in records {
        if record.len() > MAX_RECORD_BYTES || record.len() > remaining {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "snapshot record byte limit",
            ));
        }
        remaining -= record.len();
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<T>, SnapshotError> {
    check_page(records, max_bytes)?;
    records
        .iter()
        .map(|record| {
            sonic_rs::from_str(record)
                .map_err(|error| SnapshotError::new(Status::ParseError, error.to_string()))
        })
        .collect()
}

fn version(actual: &str, expected: &str) -> Result<(), SnapshotError> {
    if actual != expected {
        return Err(SnapshotError::new(
            Status::InvalidRequest,
            format!("unsupported native projection codec {actual}"),
        ));
    }
    Ok(())
}

pub fn decode_events(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<EventRecord>, SnapshotError> {
    let records: Vec<EventRecord> = decode(records, max_bytes)?;
    for record in &records {
        version(&record.codec, EVENT_CODEC)?;
    }
    Ok(records)
}

pub fn decode_tool_uses(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<ToolUseRecord>, SnapshotError> {
    let records: Vec<ToolUseRecord> = decode(records, max_bytes)?;
    for record in &records {
        version(&record.codec, TOOL_CODEC)?;
    }
    Ok(records)
}

pub fn decode_turns(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<TurnRecord>, SnapshotError> {
    let records: Vec<TurnRecord> = decode(records, max_bytes)?;
    for record in &records {
        version(&record.codec, TURN_CODEC)?;
        for event in &record.events {
            version(&event.codec, EVENT_CODEC)?;
        }
        for use_ in &record.tool_uses {
            version(&use_.codec, TOOL_CODEC)?;
        }
    }
    Ok(records)
}

pub fn decode_files(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<FileRefRecord>, SnapshotError> {
    decode(records, max_bytes)
}

pub fn decode_predicate_inputs(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<PredicateInputsRecord>, SnapshotError> {
    decode(records, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::lift_session;
    use crate::parse::parse_entry;
    use sonic_rs::{json, JsonValueTrait, Value};

    fn event(value: Value) -> Entry {
        parse_entry(value).unwrap()
    }

    fn user(uuid: &str, text: &str) -> Entry {
        event(
            json!({"type":"user","uuid":uuid,"sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":text}}),
        )
    }

    #[test]
    fn native_events_roundtrip_every_variant_and_all_serialized_fields() {
        let records = vec![
            user("same", "one"),
            event(
                json!({"type":"assistant","uuid":"same","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"a.rs"}}],"model":"model","usage":{"input_tokens":7,"output_tokens":9}}}),
            ),
            event(
                json!({"type":"system","uuid":"sys","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","subtype":"turn_duration","durationMs":123,"content":"system text"}),
            ),
            event(
                json!({"type":"mode","uuid":"mode","sessionId":"s","timestamp":"2026-01-02T03:04:08Z","mode":"plan"}),
            ),
            event(
                json!({"type":"attachment","uuid":"attachment","sessionId":"s","timestamp":"2026-01-02T03:04:09Z","attachment":{"type":"queued_command","prompt":"queued","sourceAgentId":"agent"}}),
            ),
            event(json!({"type":"future-event","payload":{"key":"value"}})),
        ];
        let encoded: Vec<_> = records
            .iter()
            .enumerate()
            .map(|(index, entry)| encode(&EventWire::new(index, entry), MAX_RECORD_BYTES).unwrap())
            .collect();
        let bytes = encoded.iter().map(String::len).sum();
        let decoded = decode_events(&encoded, bytes).unwrap();
        assert_eq!(decoded.len(), records.len());
        for (index, (expected, actual)) in records.iter().zip(&decoded).enumerate() {
            assert_eq!(actual.i, index);
            assert_eq!(
                sonic_rs::to_string(expected).unwrap(),
                sonic_rs::to_string(&actual.event).unwrap()
            );
        }
        assert!(matches!(decoded[0].event, Entry::User(_)));
        assert!(matches!(decoded[1].event, Entry::Assistant(_)));
        assert!(matches!(decoded[2].event, Entry::System(_)));
        assert!(matches!(decoded[3].event, Entry::Mode(_)));
        assert!(matches!(decoded[4].event, Entry::Attachment(_)));
        assert!(matches!(decoded[5].event, Entry::Other(_)));
    }

    #[test]
    fn decoded_pages_preserve_duplicate_uuids_and_native_derived_details() {
        let mut first = user("same", "first");
        let Entry::User(first_user) = &mut first else {
            unreachable!()
        };
        first_user.meta.is_compact_summary = true;
        first_user.meta.is_visible_in_transcript_only = true;
        let second = user("same", "second");
        let records = [
            encode(&EventWire::new(4, &first), MAX_RECORD_BYTES).unwrap(),
            encode(&EventWire::new(5, &second), MAX_RECORD_BYTES).unwrap(),
        ];
        let decoded = decode_events(&records, MAX_RECORD_BYTES).unwrap();
        assert_eq!((decoded[0].i, decoded[1].i), (4, 5));
        assert_eq!(
            decoded[0].event.meta().unwrap().uuid,
            decoded[1].event.meta().unwrap().uuid
        );
        assert!(decoded[0].event.meta().unwrap().is_compact_summary);
        assert!(
            decoded[0]
                .event
                .meta()
                .unwrap()
                .is_visible_in_transcript_only
        );
        assert!(!decoded[1].event.meta().unwrap().is_compact_summary);
    }

    #[test]
    fn typed_calls_results_and_cached_edits_roundtrip_without_call_parsing() {
        let entries = vec![
            user("u", "edit"),
            event(
                json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"edit","name":"Edit","input":{"file_path":"a.rs","old_string":"old","new_string":"new"}}]}}),
            ),
            event(
                json!({"type":"user","uuid":"r","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","toolUseResult":{"x":42},"message":{"content":[{"type":"tool_result","tool_use_id":"edit","content":"result","is_error":true}]}}),
            ),
        ];
        let mut activity = lift_session("s", &entries);
        let use_ = &mut activity.turns[0].tool_uses[0];
        let ToolCall::Edit(call) = &mut use_.call else {
            unreachable!()
        };
        call.new = "cached interpretation independent of raw".to_owned();
        let raw = encode(&ToolUseWire::new(use_, "s"), MAX_RECORD_BYTES).unwrap();
        let decoded = decode_tool_uses(&[raw], MAX_RECORD_BYTES)
            .unwrap()
            .pop()
            .unwrap();
        let ToolCall::Edit(call) = &decoded.call else {
            panic!("typed Edit expected")
        };
        assert_eq!(call.new, "cached interpretation independent of raw");
        assert_eq!(call.raw["new_string"].as_str(), Some("new"));
        assert_eq!(decoded.edits[0].1[0].new, "new");
        assert!(decoded.result.unwrap().is_error);
        assert_eq!(decoded.r#ref.tool_use_id, "edit");
    }

    #[test]
    fn byte_and_item_caps_precede_deserialization() {
        let entry = user("u", "payload");
        let record = encode(&EventWire::new(0, &entry), MAX_RECORD_BYTES).unwrap();
        assert!(decode_events(std::slice::from_ref(&record), record.len()).is_ok());
        assert!(
            matches!(decode_events(std::slice::from_ref(&record), record.len() - 1), Err(error) if error.status == Status::OutputLimit)
        );
        let malformed = vec!["{".to_owned(); MAX_RECORDS + 1];
        assert!(
            matches!(decode_events(&malformed, usize::MAX), Err(error) if error.status == Status::OutputLimit)
        );
        assert!(
            matches!(decode_events(&["{".to_owned()], 0), Err(error) if error.status == Status::OutputLimit)
        );
        assert!(
            matches!(decode_events(&["{".to_owned()], 1), Err(error) if error.status == Status::ParseError)
        );
        assert!(
            matches!(encode(&EventWire::new(0, &entry), record.len() - 1), Err(error) if error.status == Status::OutputLimit)
        );
    }

    #[test]
    fn versions_and_original_jsonl_are_not_accepted_as_native_records() {
        let entry = user("u", "payload");
        let record = encode(&EventWire::new(0, &entry), MAX_RECORD_BYTES).unwrap();
        let unknown_version = record.replace(EVENT_CODEC, "cc-transcript.native-entry/9");
        assert!(
            matches!(decode_events(&[unknown_version], MAX_RECORD_BYTES), Err(error) if error.status == Status::InvalidRequest)
        );
        let original = r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":"hello"}}"#.to_owned();
        assert!(
            matches!(decode_events(&[original], MAX_RECORD_BYTES), Err(error) if error.status == Status::ParseError)
        );
    }

    #[test]
    fn full_turn_roundtrip_keeps_events_and_tool_use_ordinals() {
        let entries = vec![user("same", "first"), user("same", "second")];
        let activity = lift_session("s", &entries);
        let turn = &activity.turns[0];
        let wire = TurnWire {
            codec: TURN_CODEC,
            index: turn.index,
            prompt: &turn.prompt,
            started_at: turn.started_at,
            ended_at: turn.ended_at,
            events: turn
                .events
                .iter()
                .enumerate()
                .map(|(index, event)| EventWire::new(index, event))
                .collect(),
            tool_uses: turn
                .tool_uses
                .iter()
                .map(|use_| ToolUseWire::new(use_, "s"))
                .collect(),
        };
        let encoded = encode(&wire, MAX_RECORD_BYTES).unwrap();
        let decoded = decode_turns(&[encoded], MAX_RECORD_BYTES)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(decoded.prompt, "first");
        assert_eq!(decoded.index, 0);
        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.events[0].event.meta().unwrap().uuid, "same");
    }
}
