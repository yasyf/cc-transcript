//! swift-bridge surface over the session-activity oracle: reads a transcript
//! file, parses it with the python-free core, and exposes the result as
//! opaque `SessionActivity` / `PendingItem` handles.
//!
//! swift-bridge 0.1.59 cannot bridge transparent structs holding
//! `Vec<struct>` fields (the generated Swift lacks a `Vectorizable`
//! conformance), so both types cross the boundary as opaque Rust types with
//! accessor methods instead of shared structs.

use std::fs;
use std::panic::{self, AssertUnwindSafe};

use _parser_rs::activity::{self, ActivityOpts};
use _parser_rs::parse::{self, ParseError};

#[swift_bridge::bridge]
mod ffi {
    extern "Rust" {
        type SessionActivity;

        fn is_waiting(&self) -> bool;
        fn mid_tool(&self) -> bool;
        fn last_event_epoch(&self) -> Option<i64>;
        fn pending(&self) -> Vec<PendingItem>;
    }

    extern "Rust" {
        type PendingItem;

        fn tool_use_id(&self) -> Option<String>;
        fn name(&self) -> &str;
        fn kind(&self) -> &str;
    }

    extern "Rust" {
        fn session_activity(
            path: String,
            waiting_tools: Vec<String>,
            human_facing_tools: Vec<String>,
        ) -> Result<SessionActivity, String>;
    }
}

#[derive(Debug)]
pub struct SessionActivity(activity::SessionActivity);

impl SessionActivity {
    fn is_waiting(&self) -> bool {
        self.0.is_waiting
    }

    fn mid_tool(&self) -> bool {
        self.0.mid_tool
    }

    fn last_event_epoch(&self) -> Option<i64> {
        self.0.last_event_epoch
    }

    fn pending(&self) -> Vec<PendingItem> {
        self.0
            .pending
            .iter()
            .map(|item| PendingItem {
                tool_use_id: item.tool_use_id.clone(),
                name: item.name.clone(),
                kind: item.kind.as_str(),
            })
            .collect()
    }
}

pub struct PendingItem {
    tool_use_id: Option<String>,
    name: String,
    kind: &'static str,
}

impl PendingItem {
    fn tool_use_id(&self) -> Option<String> {
        self.tool_use_id.clone()
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        self.kind
    }
}

/// Probe `path` (a session transcript, JSONL) for whether the session is
/// waiting on the human. Empty tool lists mean [`ActivityOpts::default`].
/// Panics below this boundary become `Err` rather than crossing the
/// swift-bridge `extern "C"` glue, where rustc's abort-on-unwind shim would
/// take down the consuming Swift process.
pub fn session_activity(
    path: String,
    waiting_tools: Vec<String>,
    human_facing_tools: Vec<String>,
) -> Result<SessionActivity, String> {
    catch_panic(move || {
        let bytes = fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let entries = parse::parse_bytes(&bytes, |_| true).map_err(|e| match e {
            ParseError::Key(key) => format!("missing key '{key}'"),
            ParseError::Value(msg) => msg,
        })?;
        let mut opts = ActivityOpts::default();
        if !waiting_tools.is_empty() {
            opts.waiting_tools = waiting_tools.into_iter().collect();
        }
        if !human_facing_tools.is_empty() {
            opts.human_facing_tools = human_facing_tools.into_iter().collect();
        }
        Ok(SessionActivity(activity::session_activity(&entries, &opts)))
    })
}

// The default panic hook still prints the panic to stderr before the payload
// reaches us here; swapping the hook around the call would mutate
// process-global state from a library, so the stderr line stays.
fn catch_panic<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic");
        Err(format!("panic in session_activity: {msg}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = concat!(
        r#"{"type":"user","uuid":"u","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","message":{"content":"run it"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a","sessionId":"s1","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":"wf1","name":"Workflow","input":{"script":"return 1"}}]}}"#,
        "\n",
        r#"{"type":"user","uuid":"r","sessionId":"s1","timestamp":"2026-01-02T03:04:07Z","toolUseResult":{"isAsync":false},"message":{"content":[{"type":"tool_result","tool_use_id":"wf1","content":"done","is_error":false}]}}"#,
        "\n",
    );

    fn write_transcript(test: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cc_transcript_swift_{test}_{}.jsonl",
            std::process::id()
        ));
        fs::write(&path, TRANSCRIPT).unwrap();
        path
    }

    #[test]
    fn pending_workflow_round_trips() {
        let path = write_transcript("round_trips");
        let activity =
            session_activity(path.to_str().unwrap().to_string(), vec![], vec![]).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(activity.is_waiting());
        assert!(!activity.mid_tool());
        assert_eq!(activity.last_event_epoch(), Some(1767323047));
        let pending = activity.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_use_id(), Some("wf1".to_string()));
        assert_eq!(pending[0].name(), "Workflow");
        assert_eq!(pending[0].kind(), "pending_async_workflow");
    }

    #[test]
    fn custom_waiting_tools_replace_defaults() {
        let path = write_transcript("custom_tools");
        let activity = session_activity(
            path.to_str().unwrap().to_string(),
            vec!["NoSuchTool".to_string()],
            vec!["NoSuchTool".to_string()],
        )
        .unwrap();
        fs::remove_file(&path).unwrap();

        assert!(activity.is_waiting());
        assert_eq!(activity.pending()[0].kind(), "pending_async_workflow");
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = session_activity("/nonexistent/transcript.jsonl".to_string(), vec![], vec![])
            .unwrap_err();
        assert!(err.starts_with("/nonexistent/transcript.jsonl: "), "{err}");
    }

    #[test]
    fn panic_becomes_err() {
        let err = catch_panic::<()>(|| panic!("boom {}", 42)).unwrap_err();
        assert_eq!(err, "panic in session_activity: boom 42");

        let err = catch_panic::<()>(|| panic!("static boom")).unwrap_err();
        assert_eq!(err, "panic in session_activity: static boom");
    }
}
