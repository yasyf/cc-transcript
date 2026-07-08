use std::collections::{HashMap, HashSet};

use crate::types::{Entry, ToolResultBlock, ToolUseBlock, UserEntry};
use crate::value::field_str;

pub struct ActivityOpts {
    pub waiting_tools: HashSet<String>,
    pub human_facing_tools: HashSet<String>,
}

impl Default for ActivityOpts {
    fn default() -> Self {
        Self {
            waiting_tools: HashSet::from(
                ["Monitor", "ScheduleWakeup", "SendMessage", "TeamCreate"].map(String::from),
            ),
            human_facing_tools: HashSet::from(["AskUserQuestion", "ExitPlanMode"].map(String::from)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    WaitingTool,
    Background,
    SubagentlessTask,
    PendingAsyncTask,
    PendingAsyncWorkflow,
    MidTool,
}

impl PendingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingKind::WaitingTool => "waiting_tool",
            PendingKind::Background => "background",
            PendingKind::SubagentlessTask => "subagentless_task",
            PendingKind::PendingAsyncTask => "pending_async_task",
            PendingKind::PendingAsyncWorkflow => "pending_async_workflow",
            PendingKind::MidTool => "mid_tool",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PendingItem {
    pub tool_use_id: Option<String>,
    pub name: String,
    pub kind: PendingKind,
}

#[derive(Debug)]
pub struct SessionActivity {
    pub is_waiting: bool,
    pub mid_tool: bool,
    pub pending: Vec<PendingItem>,
    pub last_event_epoch: Option<i64>,
}

/// Whether a user entry opens a turn (activity.py ``native_user_classifier``):
/// a real prompt — non-meta, non-sidechain, not an interruption marker, with
/// non-blank text.
fn opens_turn(user: &UserEntry) -> bool {
    !(user.meta.is_meta || user.meta.is_sidechain || user.interrupted())
        && !user.content.text().trim().is_empty()
}

/// The index opening the current turn: the last turn-opening user entry, or 0
/// when no prompt qualifies (activity.py ``from_events`` turn 0).
fn current_turn_start(entries: &[Entry]) -> usize {
    entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::User(user) if opens_turn(user)))
        .unwrap_or(0)
}

/// conditions.py ``ephemeral_wait``: a waiting tool, a backgrounded
/// Agent/Task/Bash, or a subagentless Agent/Task.
fn ephemeral_wait(tool_use: &ToolUseBlock, waiting_tools: &HashSet<String>) -> Option<PendingKind> {
    if waiting_tools.contains(&tool_use.name) {
        return Some(PendingKind::WaitingTool);
    }
    match tool_use.name.as_str() {
        "Agent" | "Task" | "Bash" if tool_use.run_in_background == Some(true) => {
            Some(PendingKind::Background)
        }
        "Agent" | "Task" if tool_use.subagent_type.is_none() => Some(PendingKind::SubagentlessTask),
        _ => None,
    }
}

/// conditions.py ``pending_async``: an async Agent/Task launch or a live
/// Workflow with no completion notification yet.
fn pending_async(
    tool_use: &ToolUseBlock,
    result: Option<&ToolResultBlock>,
    completions: &[&str],
) -> Option<PendingKind> {
    match tool_use.name.as_str() {
        "Agent" | "Task"
            if result.is_some_and(|r| r.is_async) && !completed(completions, &tool_use.id) =>
        {
            Some(PendingKind::PendingAsyncTask)
        }
        "Workflow" if !completed(completions, &tool_use.id) => Some(PendingKind::PendingAsyncWorkflow),
        _ => None,
    }
}

fn completion_contents(entries: &[Entry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Other(other) if other.ty == "queue-operation" => field_str(&other.raw, "content"),
            _ => None,
        })
        .collect()
}

fn completed(contents: &[&str], id: &str) -> bool {
    let marker = format!("<tool-use-id>{id}</tool-use-id>");
    contents.iter().any(|content| content.contains(&marker))
}

/// The session-activity oracle over parsed entries: captain-hook's
/// ``is_waiting`` verdict over ephemeral waits and pending async launches,
/// the mid-tool flag, and the contributing tool calls.
pub fn session_activity(entries: &[Entry], opts: &ActivityOpts) -> SessionActivity {
    let results: HashMap<&str, &ToolResultBlock> = entries
        .iter()
        .flat_map(Entry::tool_results)
        .map(|result| (result.tool_use_id.as_str(), result))
        .collect();
    let completions = completion_contents(entries);
    let turn_start = current_turn_start(entries);

    let mut is_waiting = false;
    let mut mid_tool = false;
    let mut pending: Vec<PendingItem> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        for tool_use in entry.tool_uses() {
            let result = results.get(tool_use.id.as_str()).copied();
            if result.is_some_and(|r| r.is_error) {
                continue;
            }
            let in_current_turn = index >= turn_start;
            let waiting_kind = in_current_turn
                .then(|| ephemeral_wait(tool_use, &opts.waiting_tools))
                .flatten()
                .or_else(|| pending_async(tool_use, result, &completions));
            let unmatched = in_current_turn
                && result.is_none()
                && !opts.human_facing_tools.contains(&tool_use.name);
            is_waiting |= waiting_kind.is_some();
            mid_tool |= unmatched;
            let Some(kind) = waiting_kind.or(unmatched.then_some(PendingKind::MidTool)) else {
                continue;
            };
            if seen.insert(tool_use.id.as_str()) {
                pending.push(PendingItem {
                    tool_use_id: Some(tool_use.id.clone()),
                    name: tool_use.name.clone(),
                    kind,
                });
            }
        }
    }
    SessionActivity {
        is_waiting,
        mid_tool,
        pending,
        last_event_epoch: entries
            .iter()
            .filter_map(Entry::meta)
            .map(|meta| meta.timestamp.timestamp())
            .max(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_entry;

    fn parse(raw: &str) -> Entry {
        parse_entry(sonic_rs::from_str(raw).unwrap()).unwrap()
    }

    fn user(text: &str) -> Entry {
        user_with(text, "")
    }

    fn user_with(text: &str, flags: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"user","uuid":"u","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z",{flags}"message":{{"content":{}}}}}"#,
            sonic_rs::to_string(&text).unwrap()
        ))
    }

    fn tool_use(name: &str, id: &str, input: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"assistant","uuid":"a","sessionId":"s1","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"m","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
        ))
    }

    fn tool_result(id: &str) -> Entry {
        result_entry(id, false, false)
    }

    fn result_entry(id: &str, is_error: bool, is_async: bool) -> Entry {
        parse(&format!(
            r#"{{"type":"user","uuid":"r","sessionId":"s1","timestamp":"2026-01-02T03:04:07Z","toolUseResult":{{"isAsync":{is_async}}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"done","is_error":{is_error}}}]}}}}"#
        ))
    }

    fn queue_op(content: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"queue-operation","operation":"enqueue","content":{}}}"#,
            sonic_rs::to_string(&content).unwrap()
        ))
    }

    fn notification(id: &str) -> String {
        format!(
            "<task-notification><task-id>t</task-id><tool-use-id>{id}</tool-use-id><status>completed</status></task-notification>"
        )
    }

    fn activity(entries: &[Entry]) -> SessionActivity {
        session_activity(entries, &ActivityOpts::default())
    }

    #[test]
    fn pending_workflow_is_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(!activity.mid_tool);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("wf1".to_string()),
                name: "Workflow".to_string(),
                kind: PendingKind::PendingAsyncWorkflow
            }]
        );
    }

    #[test]
    fn workflow_completion_marker_clears_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf1")),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn unrelated_completion_marker_keeps_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf_other")),
        ];
        assert!(activity(&entries).is_waiting);
    }

    #[test]
    fn errored_workflow_is_not_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            result_entry("wf1", true, false),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn background_bash_in_current_turn_is_waiting() {
        let entries = vec![
            user("build it"),
            tool_use("Bash", "b1", r#"{"command":"make","run_in_background":true}"#),
            tool_result("b1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(!activity.mid_tool);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn background_bash_in_previous_turn_is_not_waiting() {
        let entries = vec![
            user("build it"),
            tool_use("Bash", "b1", r#"{"command":"make","run_in_background":true}"#),
            tool_result("b1"),
            user("now do something else"),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn subagentless_agent_is_waiting() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"prompt":"look around"}"#),
            tool_result("a1"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::SubagentlessTask);
    }

    #[test]
    fn typed_agent_with_sync_result_is_not_waiting() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            tool_result("a1"),
        ];
        assert!(!activity(&entries).is_waiting);
    }

    #[test]
    fn async_agent_in_previous_turn_still_pending() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            result_entry("a1", false, true),
            user("while that runs, plan"),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::PendingAsyncTask);
    }

    #[test]
    fn async_agent_completion_marker_clears_waiting() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            result_entry("a1", false, true),
            user("while that runs, plan"),
            queue_op(&notification("a1")),
        ];
        assert!(!activity(&entries).is_waiting);
    }

    #[test]
    fn waiting_tool_in_current_turn_is_waiting() {
        let entries = vec![user("watch it"), tool_use("Monitor", "m1", r#"{"until":"done"}"#)];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert!(activity.mid_tool, "an unmatched Monitor is also mid-tool");
        assert_eq!(activity.pending[0].kind, PendingKind::WaitingTool);
    }

    #[test]
    fn pending_ask_user_question_is_quiet() {
        let entries = vec![
            user("choose"),
            tool_use("AskUserQuestion", "q1", r#"{"questions":[]}"#),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn unmatched_bash_in_current_turn_is_mid_tool() {
        let entries = vec![user("list"), tool_use("Bash", "b1", r#"{"command":"ls"}"#)];
        let activity = activity(&entries);
        assert!(activity.mid_tool);
        assert!(!activity.is_waiting);
        assert_eq!(
            activity.pending,
            [PendingItem {
                tool_use_id: Some("b1".to_string()),
                name: "Bash".to_string(),
                kind: PendingKind::MidTool
            }]
        );
    }

    #[test]
    fn unmatched_bash_in_previous_turn_is_not_mid_tool() {
        let entries = vec![
            user("list"),
            tool_use("Bash", "b1", r#"{"command":"ls"}"#),
            user("moving on"),
        ];
        let activity = activity(&entries);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn meta_sidechain_and_interrupt_users_do_not_open_turns() {
        let entries = vec![
            user("build it"),
            tool_use("Bash", "b1", r#"{"command":"make","run_in_background":true}"#),
            tool_result("b1"),
            user_with("injected context", r#""isMeta":true,"#),
            user_with("sidechain prompt", r#""isSidechain":true,"#),
            user("[Request interrupted by user]"),
            user("   "),
        ];
        assert!(
            activity(&entries).is_waiting,
            "no later entry opens a turn, so the background Bash stays current"
        );
    }

    #[test]
    fn contributing_call_dedupes_by_tool_use_id() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"prompt":"x","run_in_background":true}"#),
            result_entry("a1", false, true),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending.len(), 1);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
    }

    #[test]
    fn last_event_epoch_is_max_meta_timestamp() {
        let entries = vec![
            user("hi"),
            tool_use("Bash", "b1", r#"{"command":"ls"}"#),
            queue_op("no meta here"),
        ];
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:06Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity(&entries).last_event_epoch, Some(expected));
    }

    #[test]
    fn empty_session_is_idle() {
        let activity = activity(&[]);
        assert!(!activity.is_waiting);
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        assert_eq!(activity.last_event_epoch, None);
    }
}
