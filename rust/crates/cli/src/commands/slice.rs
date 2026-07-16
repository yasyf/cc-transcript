//! cli.py `slice`.

use std::path::PathBuf;

use cc_transcript_core::discovery::find_transcript;
use cc_transcript_core::ids::tool_digest;
use cc_transcript_core::render::{render_tool_call, Budget, Json};
use cc_transcript_core::toolcall::parse_tool_call;
use cc_transcript_core::types::{epoch_ms, ContentBlock, Entry, ToolUseBlock};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};

use crate::output::{click_error, py_repr, usage_error, CliExit, Out};
use crate::target::{claude_projects_dir, parse_transcripts};

const SLICE_SCHEMA: &str = "cc-transcript.slice/1";
const USAGE: &str = "cc-transcript slice [OPTIONS]";
const HELP_PATH: &str = "cc-transcript slice";

fn parse_rfc3339(option: &str, value: &str) -> Result<DateTime<FixedOffset>, CliExit> {
    if let Ok(stamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(stamp);
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .is_ok()
        || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok();
    if naive {
        return Err(usage_error(
            USAGE,
            HELP_PATH,
            &format!(
                "invalid {option} {}; RFC 3339 requires a UTC offset",
                py_repr(value)
            ),
        ));
    }
    Err(usage_error(
        USAGE,
        HELP_PATH,
        &format!(
            "invalid {option} {}; expected an RFC 3339 timestamp",
            py_repr(value)
        ),
    ))
}

fn slice_line(
    meta_uuid: &str,
    ts: DateTime<FixedOffset>,
    block: &ToolUseBlock,
) -> Result<Json, CliExit> {
    let call = parse_tool_call(&block.name, &block.input);
    let digest = tool_digest(&block.name, &block.input).map_err(|e| click_error(&e))?;
    Ok(Json::Obj(vec![
        ("schema".into(), Json::Str(SLICE_SCHEMA.into())),
        ("event_uuid".into(), Json::Str(meta_uuid.into())),
        ("tool_use_id".into(), Json::Str(block.id.clone())),
        ("ts_ms".into(), Json::Int(epoch_ms(ts))),
        ("tool_name".into(), Json::Str(block.name.clone())),
        ("tool_digest".into(), Json::Str(digest)),
        (
            "file_path".into(),
            call.file_path()
                .map_or(Json::Null, |p| Json::Str(p.to_string())),
        ),
        (
            "summary".into(),
            Json::Str(render_tool_call(&call, &Budget::default())),
        ),
    ]))
}

pub fn run(session: &str, since: &str, until: &str, root: Option<PathBuf>) -> Result<(), CliExit> {
    let start = parse_rfc3339("--since", since)?;
    let end = parse_rfc3339("--until", until)?;
    let root = root.unwrap_or_else(claude_projects_dir);
    let Some(path) = find_transcript(&root, session) else {
        return Err(CliExit(1));
    };
    let mtime = std::fs::metadata(&path)
        .map(|m| cc_transcript_core::discovery::mtime_secs(&m))
        .unwrap_or(0.0);
    let parsed = parse_transcripts(&[(path, mtime)]);
    let Some(parsed) = parsed.into_iter().next() else {
        return Err(CliExit(2));
    };
    let mut out = Out::new();
    for event in &parsed.entries {
        let Entry::Assistant(a) = event else { continue };
        if !(start <= a.meta.timestamp && a.meta.timestamp < end) {
            continue;
        }
        for block in &a.blocks {
            if let ContentBlock::ToolUse(tu) = block {
                out.line(&slice_line(&a.meta.uuid, a.meta.timestamp, tu)?.dumps())?;
            }
        }
    }
    out.finish()
}
