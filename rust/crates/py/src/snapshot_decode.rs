use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use cc_transcript_core::activity::{Hunk, ToolUse};
use cc_transcript_core::snapshot::{Cancellation, SnapshotError, TranscriptSnapshot, WorkLimits};
use cc_transcript_core::snapshot_codec::{self, RefRecord, ToolUseRecord, TurnRecord};
use cc_transcript_core::snapshot_projection::{bounded_turn_range_with_usage, ProjectionUsage};
use cc_transcript_core::types::{ContentBlock, Entry, ToolResultBlock};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use crate::views::blocks::block_view;
use crate::views::events::event_view;
use crate::views::store::{BlockHost, EventRef};
use crate::views::toolcall::{call_view, HunkView};

use crate::snapshots::error;

fn reference<'py>(py: Python<'py>, record: RefRecord) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("session_id", record.session_id)?;
    result.set_item("event_uuid", record.event_uuid)?;
    result.set_item("tool_use_id", record.tool_use_id)?;
    Ok(result)
}

fn edits<'py>(py: Python<'py>, edits: Vec<(String, Vec<Hunk>)>) -> PyResult<Bound<'py, PyTuple>> {
    let mut results = Vec::with_capacity(edits.len());
    for (path, hunks) in edits {
        let hunks = hunks
            .into_iter()
            .map(|hunk| {
                Bound::new(
                    py,
                    HunkView {
                        old: hunk.old,
                        new: hunk.new,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        results.push((path, PyTuple::new(py, hunks)?));
    }
    PyTuple::new(py, results)
}

fn tool_payload<'py>(
    py: Python<'py>,
    record: ToolUseRecord,
    result: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let payload = PyDict::new(py);
    payload.set_item("ref", reference(py, record.r#ref)?)?;
    payload.set_item("call", call_view(py, Arc::new(record.call))?)?;
    payload.set_item("result", result)?;
    payload.set_item("result_ts", record.result_ts)?;
    payload.set_item("edits", edits(py, record.edits)?)?;
    payload.set_item("turn_index", record.turn_index)?;
    payload.set_item("ts", record.ts)?;
    Ok(payload)
}

fn result_blocks<'a>(
    records: impl Iterator<Item = &'a mut ToolUseRecord>,
) -> (BlockHost, Vec<Option<usize>>) {
    let mut blocks = Vec::new();
    let indices = records
        .map(|record| {
            record.result.take().map(|result| {
                let index = blocks.len();
                blocks.push(ContentBlock::ToolResult(result));
                index
            })
        })
        .collect();
    (BlockHost::Owned(Arc::new(blocks)), indices)
}

fn detached_tools<'py>(
    py: Python<'py>,
    mut records: Vec<ToolUseRecord>,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let (host, indices) = result_blocks(records.iter_mut());
    records
        .into_iter()
        .zip(indices)
        .map(|(record, index)| {
            let result = match index {
                Some(index) => block_view(py, &host, index)?,
                None => py.None().into_bound(py),
            };
            tool_payload(py, record, result)
        })
        .collect()
}

fn detached_turns<'py>(
    py: Python<'py>,
    mut records: Vec<TurnRecord>,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let mut entries = Vec::new();
    let event_ranges: Vec<_> = records
        .iter_mut()
        .map(|record| {
            let start = entries.len();
            entries.extend(
                std::mem::take(&mut record.events)
                    .into_iter()
                    .map(|event| event.event),
            );
            start..entries.len()
        })
        .collect();
    let entries = Arc::new(entries);
    let (host, indices) = result_blocks(
        records
            .iter_mut()
            .flat_map(|record| record.tool_uses.iter_mut()),
    );
    let mut result_indices = indices.into_iter();
    records
        .into_iter()
        .zip(event_ranges)
        .map(|(record, event_range)| {
            let payload = PyDict::new(py);
            payload.set_item("index", record.index)?;
            payload.set_item("prompt", record.prompt)?;
            payload.set_item("started_at", record.started_at)?;
            payload.set_item("ended_at", record.ended_at)?;
            let events = event_range
                .map(|index| event_view(py, &entries, index))
                .collect::<PyResult<Vec<_>>>()?;
            payload.set_item("events", PyTuple::new(py, events)?)?;
            let mut tools = Vec::with_capacity(record.tool_uses.len());
            for tool in record.tool_uses {
                let result = match result_indices.next().expect("result index for each tool") {
                    Some(index) => block_view(py, &host, index)?,
                    None => py.None().into_bound(py),
                };
                tools.push(tool_payload(py, tool, result)?);
            }
            payload.set_item("tool_uses", PyTuple::new(py, tools)?)?;
            Ok(payload)
        })
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "list[cc_transcript.models.TranscriptEvent]", imports = ("cc_transcript.models",)))]
pub(crate) fn decode_snapshot_events<'py>(
    py: Python<'py>,
    records: Vec<String>,
    max_bytes: usize,
) -> PyResult<Bound<'py, PyList>> {
    let records = py
        .detach(|| snapshot_codec::decode_events(&records, max_bytes))
        .map_err(error)?;
    let entries = Arc::new(
        records
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>(),
    );
    let views = (0..entries.len())
        .map(|index| event_view(py, &entries, index))
        .collect::<PyResult<Vec<_>>>()?;
    PyList::new(py, views)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "list[typing.Any]", imports = ("typing",)))]
pub(crate) fn decode_snapshot_projection<'py>(
    py: Python<'py>,
    record_schema: &str,
    records: Vec<String>,
    max_bytes: usize,
) -> PyResult<Bound<'py, PyList>> {
    match record_schema {
        "cc-transcript.event/1" => decode_snapshot_events(py, records, max_bytes),
        "cc-transcript.tool-use/1" => {
            let records = py
                .detach(|| snapshot_codec::decode_tool_uses(&records, max_bytes))
                .map_err(error)?;
            PyList::new(py, detached_tools(py, records)?)
        }
        "cc-transcript.turn/1" => {
            let records = py
                .detach(|| snapshot_codec::decode_turns(&records, max_bytes))
                .map_err(error)?;
            PyList::new(py, detached_turns(py, records)?)
        }
        "cc-transcript.file-ref/1" => {
            let records = py
                .detach(|| snapshot_codec::decode_files(&records, max_bytes))
                .map_err(error)?;
            let payloads = records
                .into_iter()
                .map(|record| {
                    let payload = PyDict::new(py);
                    payload.set_item("path", record.path)?;
                    Ok(payload)
                })
                .collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, payloads)
        }
        "cc-transcript.predicate-inputs/1" => {
            let records = py
                .detach(|| snapshot_codec::decode_predicate_inputs(&records, max_bytes))
                .map_err(error)?;
            let payloads = records
                .into_iter()
                .map(|record| {
                    let payload = PyDict::new(py);
                    let calls = record
                        .calls
                        .into_iter()
                        .map(|(name, paths)| Ok((name, PyTuple::new(py, paths)?)))
                        .collect::<PyResult<Vec<_>>>()?;
                    payload.set_item("calls", PyTuple::new(py, calls)?)?;
                    payload.set_item("commands", PyTuple::new(py, record.commands)?)?;
                    let files = record
                        .edited_files
                        .into_iter()
                        .map(|file| {
                            let file_payload = PyDict::new(py);
                            file_payload.set_item("path", file.path)?;
                            Ok(file_payload)
                        })
                        .collect::<PyResult<Vec<_>>>()?;
                    payload.set_item("edited_files", PyTuple::new(py, files)?)?;
                    payload.set_item("skills", PyTuple::new(py, record.skills)?)?;
                    Ok(payload)
                })
                .collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, payloads)
        }
        _ => Err(PyValueError::new_err(format!(
            "unsupported snapshot record schema {record_schema}"
        ))),
    }
}

fn source_event(snapshot: &TranscriptSnapshot, position: usize) -> EventRef {
    let chunk_index = snapshot
        .chunks
        .partition_point(|chunk| chunk.start <= position)
        - 1;
    let chunk = &snapshot.chunks[chunk_index];
    EventRef {
        entries: Arc::clone(&chunk.entries),
        idx: position - chunk.start,
    }
}

fn owner_tool_payload<'py>(
    py: Python<'py>,
    session_id: &str,
    use_: ToolUse,
    result: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let payload = PyDict::new(py);
    payload.set_item(
        "ref",
        reference(
            py,
            RefRecord {
                session_id: session_id.to_owned(),
                event_uuid: use_.event_uuid.to_owned(),
                tool_use_id: use_.tool_use_id.to_owned(),
            },
        )?,
    )?;
    payload.set_item("call", call_view(py, Arc::new(use_.call))?)?;
    payload.set_item("result", result)?;
    payload.set_item("result_ts", use_.result_ts)?;
    payload.set_item("edits", edits(py, use_.edits)?)?;
    payload.set_item("turn_index", use_.turn_index)?;
    payload.set_item("ts", use_.ts)?;
    Ok(payload)
}

#[derive(Default)]
pub(crate) struct ActivityUsage {
    pub read_bytes: usize,
    pub events: usize,
    pub items: usize,
    pub output_bytes: usize,
}

pub(crate) fn snapshot_activity_payload<'py>(
    py: Python<'py>,
    snapshot: &TranscriptSnapshot,
    turns: Range<usize>,
    limits: &WorkLimits,
    cancel: &Cancellation,
    usage: &mut ActivityUsage,
) -> PyResult<Bound<'py, PyDict>> {
    let activity = py
        .detach(|| -> Result<_, SnapshotError> {
            let mut projection_usage = ProjectionUsage::default();
            let activity = bounded_turn_range_with_usage(
                snapshot,
                turns,
                limits,
                cancel,
                &mut projection_usage,
            );
            usage.read_bytes = projection_usage.read_bytes;
            usage.events = projection_usage.events;
            let activity = activity?;
            usage.items = activity.turns.len();
            let overhead = snapshot_codec::encoded_size(
                &sonic_rs::json!({"session_id":snapshot.session_id,"turns":[]}),
                limits.max_output_bytes,
            )?;
            let mut remaining = limits.max_output_bytes - overhead;
            for (ordinal, turn) in activity.turns.iter().enumerate() {
                if ordinal != 0 {
                    remaining = remaining.checked_sub(1).ok_or_else(|| {
                        SnapshotError::new(
                            cc_transcript_core::snapshot::Status::OutputLimit,
                            "activity payload exceeds output budget",
                        )
                    })?;
                }
                cancel.check(limits.deadline_unix_ms)?;
                let start = snapshot
                    .activity
                    .turn_bounds(turn.index)
                    .expect("projected turn")
                    .start;
                let wire = snapshot_codec::TurnWire {
                    codec: snapshot_codec::TURN_CODEC,
                    index: turn.index,
                    prompt: &turn.prompt,
                    started_at: turn.started_at,
                    ended_at: turn.ended_at,
                    events: turn
                        .events
                        .iter()
                        .enumerate()
                        .map(|(offset, event)| {
                            snapshot_codec::EventWire::new(start + offset, event)
                        })
                        .collect(),
                    tool_uses: turn
                        .tool_uses
                        .iter()
                        .map(|use_| snapshot_codec::ToolUseWire::new(use_, &snapshot.session_id))
                        .collect(),
                };
                remaining -= snapshot_codec::encoded_size(&wire, remaining)?;
            }
            usage.output_bytes = limits.max_output_bytes - remaining;
            Ok(activity)
        })
        .map_err(error)?;
    let mut output = Vec::with_capacity(activity.turns.len());
    for turn in activity.turns {
        cancel.check(limits.deadline_unix_ms).map_err(error)?;
        let mut results = HashMap::<*const ToolResultBlock, (EventRef, usize)>::new();
        for position in snapshot.activity.result_events(turn.index) {
            let event = source_event(snapshot, position);
            for (block_index, block) in snapshot.entry(position).blocks().iter().enumerate() {
                if let ContentBlock::ToolResult(result) = block {
                    results.insert(
                        result as *const ToolResultBlock,
                        (event.clone(), block_index),
                    );
                }
            }
        }
        let bounds = snapshot
            .activity
            .turn_bounds(turn.index)
            .expect("projected turn");
        let events = bounds
            .map(|position| {
                let event = source_event(snapshot, position);
                event_view(py, &event.entries, event.idx)
            })
            .collect::<PyResult<Vec<_>>>()?;
        let mut tools = Vec::with_capacity(turn.tool_uses.len());
        for use_ in turn.tool_uses {
            let result = match use_.result {
                Some(result) => {
                    let (event, block) = results
                        .get(&(result as *const ToolResultBlock))
                        .expect("indexed tool result");
                    block_view(py, &BlockHost::Entry(event.clone()), *block)?
                }
                None => py.None().into_bound(py),
            };
            tools.push(owner_tool_payload(py, &snapshot.session_id, use_, result)?);
        }
        let payload = PyDict::new(py);
        payload.set_item("index", turn.index)?;
        payload.set_item("prompt", turn.prompt)?;
        payload.set_item("started_at", turn.started_at)?;
        payload.set_item("ended_at", turn.ended_at)?;
        payload.set_item("events", PyTuple::new(py, events)?)?;
        payload.set_item("tool_uses", PyTuple::new(py, tools)?)?;
        output.push(payload);
    }
    cancel.check(limits.deadline_unix_ms).map_err(error)?;
    let payload = PyDict::new(py);
    payload.set_item("session_id", &snapshot.session_id)?;
    payload.set_item("turns", PyTuple::new(py, output)?)?;
    Ok(payload)
}

pub(crate) fn add_functions(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(decode_snapshot_events, module)?)?;
    module.add_function(wrap_pyfunction!(decode_snapshot_projection, module)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::events::UserEventView;
    use cc_transcript_core::parse::parse_entry;
    use cc_transcript_core::snapshot_codec::{encode, EventWire, MAX_RECORD_BYTES};
    use sonic_rs::json;

    fn records() -> Vec<String> {
        ["first", "second"].into_iter().enumerate().map(|(index, text)| {
            let entry = parse_entry(json!({"type":"user","uuid":"same","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":text}})).unwrap();
            encode(&EventWire::new(index, &entry), MAX_RECORD_BYTES).unwrap()
        }).collect()
    }

    #[test]
    fn detached_page_uses_one_owned_arc_and_survives_input_release() {
        Python::initialize();
        Python::attach(|py| {
            let decoded = decode_snapshot_events(py, records(), MAX_RECORD_BYTES).unwrap();
            let first = decoded.get_item(0).unwrap();
            let second = decoded.get_item(1).unwrap();
            let first_view = first.extract::<PyRef<UserEventView>>().unwrap();
            let second_view = second.extract::<PyRef<UserEventView>>().unwrap();
            assert!(Arc::ptr_eq(&first_view.r.entries, &second_view.r.entries));
            assert_eq!(first_view.r.idx, 0);
            assert_eq!(second_view.r.idx, 1);
            drop(first_view);
            drop(second_view);
            drop(decoded);
            assert_eq!(
                first.getattr("text").unwrap().extract::<String>().unwrap(),
                "first"
            );
            assert_eq!(
                second.getattr("text").unwrap().extract::<String>().unwrap(),
                "second"
            );
        });
    }

    #[test]
    fn owner_activity_shares_chunks_and_enforces_caps_before_payloads() {
        use cc_transcript_core::gateway::Provider;
        use cc_transcript_core::snapshot::{EntryChunk, SourceIdentity, SourceStamp};
        use cc_transcript_core::snapshot_activity::ActivityIndex;
        let entries = snapshot_codec::decode_events(&records(), MAX_RECORD_BYTES)
            .unwrap()
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>();
        let activity = ActivityIndex::new(&entries.iter().collect::<Vec<_>>(), None);
        let snapshot = TranscriptSnapshot {
            id: "test".into(),
            canonical_path: "/test.jsonl".into(),
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
            event_count: 2,
        };
        let mut limits = WorkLimits {
            max_read_bytes: MAX_RECORD_BYTES,
            max_events: 100,
            max_items: 100,
            max_output_bytes: 1,
            max_discovery_entries: 100,
            max_sources: 1,
            deadline_unix_ms: u64::MAX,
        };
        Python::initialize();
        Python::attach(|py| {
            let owners = Arc::strong_count(&snapshot.chunks[0].entries);
            assert!(snapshot_activity_payload(
                py,
                &snapshot,
                0..snapshot.activity.turn_count(),
                &limits,
                &Cancellation::default(),
                &mut ActivityUsage::default()
            )
            .is_err());
            assert_eq!(Arc::strong_count(&snapshot.chunks[0].entries), owners);
            limits.max_output_bytes = MAX_RECORD_BYTES;
            let payload = snapshot_activity_payload(
                py,
                &snapshot,
                0..snapshot.activity.turn_count(),
                &limits,
                &Cancellation::default(),
                &mut ActivityUsage::default(),
            )
            .unwrap();
            let turns = payload.get_item("turns").unwrap().unwrap();
            let first = turns.get_item(0).unwrap();
            let events = first.get_item("events").unwrap();
            let event = events.get_item(0).unwrap();
            let view = event.extract::<PyRef<UserEventView>>().unwrap();
            assert!(Arc::ptr_eq(&view.r.entries, &snapshot.chunks[0].entries));
        });
    }

    #[test]
    fn detached_decode_rejects_limits_and_unknown_schema() {
        Python::initialize();
        Python::attach(|py| {
            assert!(decode_snapshot_events(py, records(), 0).is_err());
            assert!(decode_snapshot_events(py, vec!["{".into()], MAX_RECORD_BYTES).is_err());
            assert!(
                decode_snapshot_projection(py, "unknown", Vec::new(), MAX_RECORD_BYTES).is_err()
            );
        });
    }
}
