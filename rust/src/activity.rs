use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;

use crate::types::{matches_names, Entry, ToolResultBlock, ToolUseBlock, UserEntry};
use crate::value::{field, field_str};

const NOTIFICATION_MARKER: &str = "<task-notification>";

// tools.py expand_tool_names("Agent|Task|Bash") / ("Agent|Task"), pre-expanded
// here because the alias table never crosses the language boundary.
static BACKGROUND_TOOLS: Lazy<HashSet<String>> =
    Lazy::new(|| HashSet::from(["Agent", "Task", "Bash", "Execute"].map(String::from)));
static TASK_TOOLS: Lazy<HashSet<String>> =
    Lazy::new(|| HashSet::from(["Agent", "Task"].map(String::from)));

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
            // tools.py expand_tool_names("AskUserQuestion|ExitPlanMode"), pre-expanded
            // here because the alias table never crosses the language boundary.
            human_facing_tools: HashSet::from(
                ["AskUserQuestion", "ExitPlanMode", "ExitSpecMode"].map(String::from),
            ),
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
/// non-blank text. Compact-summary exclusion matches captain-hook, which
/// consumes this probe since the 10.2.0 shared-classifier fix.
fn opens_turn(user: &UserEntry) -> bool {
    !(user.meta.is_meta || user.meta.is_sidechain || user.meta.is_compact_summary || user.interrupted())
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
/// Agent/Task/Bash, or a subagentless Agent/Task — every name matched alias-
/// and MCP-prefix-aware (tools.py matches_names).
fn ephemeral_wait(tool_use: &ToolUseBlock, waiting_tools: &HashSet<String>) -> Option<PendingKind> {
    if matches_names(&tool_use.name, waiting_tools) {
        return Some(PendingKind::WaitingTool);
    }
    if matches_names(&tool_use.name, &BACKGROUND_TOOLS) && tool_use.run_in_background == Some(true) {
        return Some(PendingKind::Background);
    }
    if matches_names(&tool_use.name, &TASK_TOOLS) && tool_use.subagent_type.is_none() {
        return Some(PendingKind::SubagentlessTask);
    }
    None
}

/// conditions.py ``pending_async``: an async Agent/Task launch or a live
/// Workflow whose completion notification has not reached the agent.
fn pending_async(
    tool_use: &ToolUseBlock,
    result: Option<&ToolResultBlock>,
    notifications: &Notifications,
) -> Option<PendingKind> {
    match tool_use.name.as_str() {
        "Agent" | "Task"
            if result.is_some_and(|r| r.is_async) && !notifications.completed(&tool_use.id) =>
        {
            Some(PendingKind::PendingAsyncTask)
        }
        "Workflow" if !notifications.completed(&tool_use.id) => {
            Some(PendingKind::PendingAsyncWorkflow)
        }
        _ => None,
    }
}

/// notifications.py ``Notifications``: the harness delivery queue replayed
/// from the transcript's ``queue-operation`` audit records.
struct Notifications {
    queued: Vec<String>,
    delivered: Vec<String>,
    enqueued: Vec<String>,
}

impl Notifications {
    fn from_entries(entries: &[Entry]) -> Self {
        let mut queued: Vec<String> = Vec::new();
        let mut delivered: Vec<String> = Vec::new();
        let mut enqueued: Vec<String> = Vec::new();
        for entry in entries {
            if let Entry::Other(other) = entry {
                if other.ty == "queue-operation" {
                    match field_str(&other.raw, "operation") {
                        Some("enqueue") => {
                            let content =
                                field_str(&other.raw, "content").unwrap_or_default().to_string();
                            enqueued.push(content.clone());
                            queued.push(content);
                        }
                        Some("dequeue" | "remove") if !queued.is_empty() => {
                            queued.remove(0);
                        }
                        Some("popAll") => {
                            let content = field_str(&other.raw, "content").unwrap_or_default();
                            queued.retain(|item| !content.contains(item.as_str()));
                        }
                        _ => {}
                    }
                }
            }
            if let Some(text) = delivered_text(entry) {
                delivered.push(text);
            }
        }
        Self { queued, delivered, enqueued }
    }

    /// notifications.py ``Notifications.completed``: delivered, or enqueued at
    /// some point yet no longer sitting undelivered in the queue.
    fn completed(&self, tool_use_id: &str) -> bool {
        let marker = format!("<tool-use-id>{tool_use_id}</tool-use-id>");
        let holds = |texts: &[String]| texts.iter().any(|text| text.contains(&marker));
        holds(&self.delivered) || (holds(&self.enqueued) && !holds(&self.queued))
    }

    /// notifications.py ``Notifications.has_pending``: any queued item is an
    /// undelivered task notification.
    fn has_pending(&self) -> bool {
        self.queued.iter().any(|text| text.contains(NOTIFICATION_MARKER))
    }
}

/// notifications.py ``delivered_text``: a user turn carrying a task
/// notification, or a ``queued_command`` attachment replayed to the model.
fn delivered_text(entry: &Entry) -> Option<String> {
    match entry {
        Entry::User(user) => {
            let text = user.content.text();
            text.contains(NOTIFICATION_MARKER).then_some(text)
        }
        Entry::Other(other) if other.ty == "attachment" => {
            let attachment = field(&other.raw, "attachment")?;
            (field_str(attachment, "type") == Some("queued_command"))
                .then(|| field_str(attachment, "prompt").unwrap_or_default().to_string())
        }
        _ => None,
    }
}

/// The session-activity oracle over parsed entries: captain-hook's
/// ``is_waiting`` verdict (post-d2e07cc, minus its hook-side Stop-payload
/// layer) over undelivered notifications, ephemeral waits, and pending async
/// launches, plus the mid-tool flag and the contributing tool calls. An
/// undelivered notification alone sets ``is_waiting`` with no pending item —
/// a resumed session's orphan has no launch to point at.
pub fn session_activity(entries: &[Entry], opts: &ActivityOpts) -> SessionActivity {
    let results: HashMap<&str, &ToolResultBlock> = entries
        .iter()
        .flat_map(Entry::tool_results)
        .map(|result| (result.tool_use_id.as_str(), result))
        .collect();
    let notifications = Notifications::from_entries(entries);
    let turn_start = current_turn_start(entries);

    let mut is_waiting = notifications.has_pending();
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
                .or_else(|| pending_async(tool_use, result, &notifications));
            let unmatched = in_current_turn
                && result.is_none()
                && !matches_names(&tool_use.name, &opts.human_facing_tools);
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
        queue_entry("enqueue", content)
    }

    fn queue_entry(operation: &str, content: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"queue-operation","operation":"{operation}","content":{}}}"#,
            sonic_rs::to_string(&content).unwrap()
        ))
    }

    fn attachment(prompt: &str) -> Entry {
        parse(&format!(
            r#"{{"type":"attachment","attachment":{{"type":"queued_command","prompt":{}}}}}"#,
            sonic_rs::to_string(&prompt).unwrap()
        ))
    }

    fn delivered_notification(id: &str) -> Vec<Entry> {
        vec![queue_op(&notification(id)), queue_entry("dequeue", ""), user(&notification(id))]
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
    fn workflow_delivered_notification_clears_waiting() {
        let mut entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
        ];
        entries.extend(delivered_notification("wf1"));
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn enqueued_undelivered_notification_keeps_waiting() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf1")),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
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
    fn removed_notification_counts_completed() {
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notification("wf1")),
            queue_entry("remove", ""),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn popall_drains_command_but_not_notification() {
        let notif = notification("wf1");
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notif),
            queue_op("run the tests please"),
            queue_entry("popAll", "run the tests please"),
        ];
        let notifications = Notifications::from_entries(&entries);
        assert_eq!(
            notifications.queued,
            vec![notif],
            "popAll drains the matching command yet leaves the notification queued"
        );
        assert!(notifications.has_pending());
        let activity = activity(&entries);
        assert!(activity.is_waiting);
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
    fn popall_drains_the_notification_completes() {
        let notif = notification("wf1");
        let entries = vec![
            user("run the workflow"),
            tool_use("Workflow", "wf1", r#"{"script":"return 1"}"#),
            tool_result("wf1"),
            queue_op(&notif),
            queue_entry("popAll", &notif),
        ];
        let notifications = Notifications::from_entries(&entries);
        assert!(
            notifications.queued.is_empty(),
            "popAll subtracts the notification itself out of the queue"
        );
        assert!(
            notifications.completed("wf1"),
            "an enqueued-then-drained notification counts as completed"
        );
        assert!(!notifications.has_pending());
        let activity = activity(&entries);
        assert!(!activity.is_waiting, "draining the notification closes the workflow's async wait");
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn orphan_undelivered_notification_is_waiting() {
        let entries = vec![user("hi"), queue_op(&notification("tu_ghost"))];
        let activity = activity(&entries);
        assert!(activity.is_waiting, "a queued task notification holds the session on its own");
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
    }

    #[test]
    fn attachment_delivery_completes() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            result_entry("a1", false, true),
            attachment(&notification("a1")),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting, "the queued_command attachment alone closes the async task");
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:07Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity.last_event_epoch, Some(expected));
    }

    #[test]
    fn plain_user_delivery_completes() {
        let entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            result_entry("a1", false, true),
            user(&notification("a1")),
        ];
        let activity = activity(&entries);
        assert!(!activity.is_waiting, "a user turn carrying the notification closes the async task");
        assert!(!activity.mid_tool);
        assert!(activity.pending.is_empty());
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:07Z")
            .unwrap()
            .timestamp();
        assert_eq!(activity.last_event_epoch, Some(expected));
    }

    #[test]
    fn compact_summary_user_does_not_open_turn() {
        let entries = vec![
            user("build it"),
            tool_use("Bash", "b1", r#"{"command":"make","run_in_background":true}"#),
            tool_result("b1"),
            user_with("compact recap", r#""isCompactSummary":true,"#),
        ];
        let activity = activity(&entries);
        assert!(
            activity.is_waiting,
            "auto-compaction must not retire a running background task"
        );
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
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
    fn background_execute_alias_in_current_turn_is_waiting() {
        let entries = vec![
            user("build it"),
            tool_use("Execute", "e1", r#"{"command":"make","run_in_background":true}"#),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
        assert_eq!(activity.pending[0].kind, PendingKind::Background);
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
    fn async_agent_delivered_notification_clears_waiting() {
        let mut entries = vec![
            user("go"),
            tool_use("Agent", "a1", r#"{"subagent_type":"Explore","prompt":"look"}"#),
            result_entry("a1", false, true),
            user("while that runs, plan"),
        ];
        entries.extend(delivered_notification("a1"));
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
    fn mcp_prefixed_waiting_tool_is_waiting() {
        let entries = vec![
            user("ping the pool"),
            tool_use("mcp__pool__SendMessage", "s1", r#"{"text":"hi"}"#),
        ];
        let activity = activity(&entries);
        assert!(activity.is_waiting);
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
    fn mcp_prefixed_human_facing_tool_is_not_mid_tool() {
        let entries = vec![
            user("choose"),
            tool_use("mcp__someserver__AskUserQuestion", "q1", r#"{"questions":[]}"#),
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
