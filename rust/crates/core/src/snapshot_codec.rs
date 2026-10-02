use std::io::{self, Write};

use chrono::{DateTime, FixedOffset};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sonic_rs::format::Formatter;

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

#[derive(Serialize)]
pub struct ActivityWire<'a> {
    pub session_id: &'a str,
    pub turns: &'a [TurnWire<'a>],
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

pub const PREDICATE_INPUT_CHUNK_BYTES: usize = 256 * 1024;

pub fn predicate_input_records(inputs: &sonic_rs::Value) -> Result<Vec<String>, SnapshotError> {
    use sonic_rs::JsonContainerTrait;

    let mut records = Vec::new();
    let mut fields: [Vec<String>; 4] = Default::default();
    let mut bytes = 0usize;
    for (slot, name) in ["calls", "commands", "edited_files", "skills"]
        .into_iter()
        .enumerate()
    {
        for item in inputs[name]
            .as_array()
            .expect("predicate input field")
            .iter()
        {
            let item = encode(item, MAX_RECORD_BYTES)?;
            let cost = encoded_size(item.as_str(), MAX_RECORD_BYTES)? + 1;
            if bytes > 0 && bytes + cost > PREDICATE_INPUT_CHUNK_BYTES {
                records.push(predicate_input_record(&fields));
                fields = Default::default();
                bytes = 0;
            }
            bytes += cost;
            fields[slot].push(item);
        }
    }
    records.push(predicate_input_record(&fields));
    Ok(records)
}

fn predicate_input_record([calls, commands, edited_files, skills]: &[Vec<String>; 4]) -> String {
    format!(
        r#"{{"calls":[{}],"commands":[{}],"edited_files":[{}],"skills":[{}]}}"#,
        calls.join(","),
        commands.join(","),
        edited_files.join(","),
        skills.join(",")
    )
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidechainRecord {
    pub path: String,
    pub session_id: String,
    pub provider: String,
    pub depth: usize,
    pub spawned_by: Option<String>,
    pub description: sonic_rs::Value,
}

struct StringWriter<'a, W: ?Sized> {
    inner: &'a mut W,
    error: Option<io::Error>,
}

impl<W: Write + ?Sized> std::fmt::Write for StringWriter<'_, W> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.inner.write_all(value.as_bytes()).map_err(|error| {
            self.error = Some(error);
            std::fmt::Error
        })
    }
}

#[derive(Clone)]
struct StreamingFormatter;

impl Formatter for StreamingFormatter {
    fn write_string_fast<W>(
        &mut self,
        writer: &mut W,
        value: &str,
        need_quote: bool,
    ) -> io::Result<()>
    where
        W: ?Sized + sonic_rs::writer::WriteExt,
    {
        let mut output = StringWriter {
            inner: writer,
            error: None,
        };
        let result = if need_quote {
            crate::ids::encode_string_to(value, &mut output)
        } else {
            crate::ids::encode_string_contents_to(value, &mut output)
        };
        result.map_err(|_| output.error.expect("string writer reported an I/O failure"))
    }
}

struct JsonWriter<'a, W> {
    inner: &'a mut W,
    remaining: usize,
}

impl<W: Write> Write for JsonWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("JSON output byte limit"));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> sonic_rs::writer::WriteExt for JsonWriter<'_, W> {
    fn reserve_with(&mut self, _additional: usize) -> io::Result<&mut [std::mem::MaybeUninit<u8>]> {
        Err(io::Error::other(
            "streaming JSON writer does not reserve scratch",
        ))
    }

    unsafe fn flush_len(&mut self, _additional: usize) -> io::Result<()> {
        Err(io::Error::other(
            "streaming JSON writer does not flush scratch",
        ))
    }
}

pub fn write_json<W: Write, T: Serialize + ?Sized>(
    writer: &mut W,
    value: &T,
    max_bytes: usize,
) -> Result<(), sonic_rs::Error> {
    value.serialize(&mut sonic_rs::Serializer::with_formatter(
        JsonWriter {
            inner: writer,
            remaining: max_bytes,
        },
        StreamingFormatter,
    ))
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

pub(crate) struct Counter {
    pub(crate) bytes: usize,
    pub(crate) limit: usize,
}

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes {
            return Err(io::Error::other("snapshot record byte limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct Bound<'a> {
    pub what: &'a str,
    pub budget: usize,
    pub record: bool,
}

impl<'a> Bound<'a> {
    pub fn record(what: &'a str, budget: usize) -> Self {
        Self {
            what,
            budget,
            record: true,
        }
    }

    pub fn budget(what: &'a str, budget: usize) -> Self {
        Self {
            what,
            budget,
            record: false,
        }
    }

    fn limit(self) -> usize {
        if self.record {
            self.budget.min(MAX_RECORD_BYTES)
        } else {
            self.budget
        }
    }

    fn refusal<T: Serialize + ?Sized>(self, record: &T) -> SnapshotError {
        let mut counter = Counter {
            bytes: 0,
            limit: usize::MAX,
        };
        let reason = match write_json(&mut counter, record, usize::MAX) {
            Ok(()) if self.record && counter.bytes > MAX_RECORD_BYTES => format!(
                "{} needs {} bytes, over the {MAX_RECORD_BYTES}-byte record bound",
                self.what, counter.bytes
            ),
            Ok(()) => format!(
                "{} needs {} bytes, over the {}-byte remaining output budget",
                self.what, counter.bytes, self.budget
            ),
            Err(error) => format!("{} could not be encoded: {error}", self.what),
        };
        SnapshotError::new(Status::OutputLimit, reason)
    }

    fn write<W: Write, T: Serialize + ?Sized>(
        self,
        out: &mut W,
        record: &T,
    ) -> Result<(), SnapshotError> {
        write_json(out, record, self.limit()).map_err(|error| {
            if error.is_io() {
                self.refusal(record)
            } else {
                SnapshotError::new(Status::OutputLimit, error.to_string())
            }
        })
    }
}

pub fn encode_bounded<T: Serialize + ?Sized>(
    record: &T,
    bound: Bound<'_>,
) -> Result<String, SnapshotError> {
    let mut out = LimitedWriter {
        bytes: Vec::new(),
        max_bytes: bound.limit(),
    };
    bound.write(&mut out, record)?;
    Ok(String::from_utf8(out.bytes).expect("JSON is UTF-8"))
}

pub fn encoded_size_bounded<T: Serialize + ?Sized>(
    record: &T,
    bound: Bound<'_>,
) -> Result<usize, SnapshotError> {
    let mut out = Counter {
        bytes: 0,
        limit: bound.limit(),
    };
    bound.write(&mut out, record)?;
    Ok(out.bytes)
}

pub fn encode<T: Serialize + ?Sized>(
    record: &T,
    max_bytes: usize,
) -> Result<String, SnapshotError> {
    encode_bounded(record, Bound::record("snapshot record", max_bytes))
}

pub fn encoded_size<T: Serialize + ?Sized>(
    record: &T,
    max_bytes: usize,
) -> Result<usize, SnapshotError> {
    encoded_size_bounded(record, Bound::record("snapshot record", max_bytes))
}

pub fn check_page(records: &[String], max_bytes: usize) -> Result<(), SnapshotError> {
    if records.len() > MAX_RECORDS {
        return Err(SnapshotError::new(
            Status::OutputLimit,
            format!(
                "snapshot page of {} records exceeds the {MAX_RECORDS}-record page bound",
                records.len()
            ),
        ));
    }
    let page = max_bytes.min(MAX_PAGE_BYTES);
    let mut remaining = page;
    for record in records {
        if record.len() > MAX_RECORD_BYTES {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                format!(
                    "snapshot record needs {} bytes, over the {MAX_RECORD_BYTES}-byte record bound",
                    record.len()
                ),
            ));
        }
        if record.len() > remaining {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                format!(
                    "snapshot page needs {} bytes, over the {page}-byte page bound",
                    records.iter().map(String::len).sum::<usize>()
                ),
            ));
        }
        remaining -= record.len();
    }
    Ok(())
}

fn parse<T: DeserializeOwned>(records: &[String]) -> Result<Vec<T>, SnapshotError> {
    records
        .iter()
        .map(|record| {
            sonic_rs::from_str(record)
                .map_err(|error| SnapshotError::new(Status::ParseError, error.to_string()))
        })
        .collect()
}

fn decode<T: DeserializeOwned>(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<T>, SnapshotError> {
    check_page(records, max_bytes)?;
    parse(records)
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
    turns(decode(records, max_bytes)?)
}

pub fn decode_owned_turns(records: &[String]) -> Result<Vec<TurnRecord>, SnapshotError> {
    turns(parse(records)?)
}

fn turns(records: Vec<TurnRecord>) -> Result<Vec<TurnRecord>, SnapshotError> {
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

pub fn decode_mining_signals(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<sonic_rs::Value>, SnapshotError> {
    decode(records, max_bytes)
}

pub fn decode_sidechains(
    records: &[String],
    max_bytes: usize,
) -> Result<Vec<SidechainRecord>, SnapshotError> {
    decode(records, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_does_not_allocate_scratch() {
        use sonic_rs::writer::WriteExt;
        let mut output = Vec::new();
        let mut writer = JsonWriter {
            inner: &mut output,
            remaining: 8,
        };
        assert!(writer.reserve_with(1).is_err());
        assert_eq!(writer.remaining, 8);
        assert!(writer.inner.is_empty());
    }

    #[test]
    fn writer_accepts_large_ascii_at_actual_encoded_boundary() {
        for length in [19, 256 * 1024, MAX_RECORD_BYTES - 2] {
            let value = "a".repeat(length);
            let expected = sonic_rs::to_string(&value).unwrap();
            assert_eq!(encode(&value, expected.len()).unwrap(), expected);
            assert_eq!(
                encoded_size(&value, expected.len()).unwrap(),
                expected.len()
            );
            assert_eq!(
                encode(&value, expected.len() - 1).unwrap_err().status,
                Status::OutputLimit
            );
        }
    }

    #[test]
    fn writer_preserves_escapes_and_unicode_at_actual_encoded_boundary() {
        let value = format!(
            "{}{}{}",
            "a".repeat(1023),
            "🌊\u{0000}\u{0008}\u{000c}\n\r\t\\\"".repeat(4096),
            "終"
        );
        let expected = sonic_rs::to_string(&value).unwrap();
        assert_eq!(encode(&value, expected.len()).unwrap(), expected);
        assert_eq!(
            encoded_size(&value, expected.len()).unwrap(),
            expected.len()
        );
        assert!(encode(&value, expected.len() - 1).is_err());
        let decoded: String = sonic_rs::from_str(&expected).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn streaming_string_failure_preserves_the_output_error() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "disconnected writer",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = JsonWriter {
            inner: &mut Broken,
            remaining: 100,
        };
        let error = StreamingFormatter
            .write_string_fast(&mut writer, "text", true)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "disconnected writer");
        assert_eq!(writer.remaining, 100);
    }

    #[test]
    fn writer_preserves_numeric_value_tokens_and_display_fragments() {
        let raw = r#"[9007199254740990.5,18446744073709551616,-18446744073709551617,1e999,-0,1.0000000000000000000001]"#;
        let value: sonic_rs::Value = sonic_rs::from_str(raw).unwrap();
        let expected = sonic_rs::to_string(&value).unwrap();
        let encoded = encode(&value, expected.len()).unwrap();
        assert_eq!(encoded, expected);
        let decoded: sonic_rs::Value = sonic_rs::from_str(&encoded).unwrap();
        assert_eq!(sonic_rs::to_string(&decoded).unwrap(), expected);
        let timestamp = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z").unwrap();
        let expected = sonic_rs::to_string(&timestamp).unwrap();
        assert_eq!(encode(&timestamp, expected.len()).unwrap(), expected);
    }

    #[test]
    fn writer_bounds_escaped_output_and_preserves_raw_numbers() {
        let value: sonic_rs::Value =
            sonic_rs::from_str(r#"{"value":9007199254740990.5,"text":"a\nb"}"#).unwrap();
        let encoded = encode(&value, 1024).unwrap();
        assert!(encoded.contains("9007199254740990.5"));
        assert!(encoded.contains(r#"a\nb"#));
        assert!(encode(&value, 8).is_err());
    }

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
                json!({"type":"assistant","uuid":"same","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"a.rs"}}],"model":"model","usage":{"input_tokens":7,"output_tokens":9,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
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
    fn oversized_event_refusal_names_its_bound_and_size() {
        let encoded_len = |wire: &EventWire<'_>| {
            let mut encoded = Vec::new();
            write_json(&mut encoded, wire, usize::MAX).unwrap();
            encoded.len()
        };
        let large = user("u", &"x".repeat(MAX_RECORD_BYTES));
        let wire = EventWire::new(7, &large);
        let needed = encoded_len(&wire);
        for budget in [64, MAX_PAGE_BYTES] {
            let refusal =
                encoded_size_bounded(&wire, Bound::record("event 7", budget)).unwrap_err();
            assert_eq!(refusal.status, Status::OutputLimit);
            assert_eq!(
                refusal.reason,
                format!(
                    "event 7 needs {needed} bytes, over the {MAX_RECORD_BYTES}-byte record bound"
                )
            );
        }
        let small = user("u", "payload");
        let wire = EventWire::new(0, &small);
        let needed = encoded_len(&wire);
        assert_eq!(
            encode(&wire, 8).unwrap_err().reason,
            format!(
                "snapshot record needs {needed} bytes, over the 8-byte remaining output budget"
            )
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
