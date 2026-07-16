//! cli.py `digest`.

use std::io::Read;
use std::path::Path;

use cc_transcript_core::ids::tool_digest;
use cc_transcript_core::render::Json;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::output::{click_error, eline, usage_error, CliExit, Out};

const USAGE: &str = "cc-transcript digest [OPTIONS]";
const HELP_PATH: &str = "cc-transcript digest";

fn row_parts(row: &Value) -> Result<(&str, &Value), CliExit> {
    let obj = row
        .as_object()
        .ok_or_else(|| click_error("digest rows must be JSON objects"))?;
    let tool = obj
        .get(&"tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| click_error("'tool'"))?;
    let input = obj.get(&"input").ok_or_else(|| click_error("'input'"))?;
    Ok((tool, input))
}

fn check(path: &Path) -> Result<(), CliExit> {
    crate::target::require_files(&[path.to_path_buf()], "'--check'", USAGE, HELP_PATH)?;
    let bytes =
        std::fs::read(path).map_err(|e| click_error(&format!("{}: {e}", path.display())))?;
    let rows: Value =
        sonic_rs::from_slice(&bytes).map_err(|e| click_error(&format!("invalid JSON: {e}")))?;
    let rows = rows
        .as_array()
        .ok_or_else(|| click_error("digest fixture must be a JSON array"))?;
    let mut mismatched = false;
    for row in rows.iter() {
        let (tool, input) = row_parts(row)?;
        let expected = row
            .as_object()
            .and_then(|o| o.get(&"digest"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| click_error("'digest'"))?;
        let actual = tool_digest(tool, input).map_err(|e| click_error(&e))?;
        if actual != expected {
            eline(&format!(
                "mismatch: {tool} expected {expected}, computed {actual}"
            ));
            mismatched = true;
        }
    }
    if mismatched {
        return Err(CliExit(1));
    }
    Ok(())
}

pub fn run(check_path: Option<&Path>) -> Result<(), CliExit> {
    if let Some(path) = check_path {
        return check(path);
    }
    let mut stdin = Vec::new();
    std::io::stdin()
        .read_to_end(&mut stdin)
        .map_err(|e| click_error(&format!("stdin: {e}")))?;
    let rows: Value = sonic_rs::from_slice(&stdin)
        .map_err(|e| usage_error(USAGE, HELP_PATH, &format!("invalid JSON on stdin: {e}")))?;
    let rows = rows
        .as_array()
        .ok_or_else(|| click_error("digest input must be a JSON array"))?;
    let mut fixture: Vec<Json> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let (tool, input) = row_parts(row)?;
        let digest = tool_digest(tool, input).map_err(|e| click_error(&e))?;
        fixture.push(Json::Obj(vec![
            ("tool".into(), Json::Str(tool.into())),
            ("input".into(), Json::Value(input.clone())),
            ("digest".into(), Json::Str(digest)),
        ]));
    }
    let mut out = Out::new();
    out.line(&Json::Arr(fixture).dumps_pretty())?;
    out.finish()
}
