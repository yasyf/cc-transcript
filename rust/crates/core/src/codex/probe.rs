use std::collections::HashSet;

use crate::activity::{ActivityOpts, PendingItem, PendingKind, SessionActivity};
use crate::codex::types::{CodexEntry, CodexItem, CodexSession, EventMsg, ResponseItemPayload};

const WAIT_TOOLS: &[&str] = &["wait", "wait_agent"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    Uninitialized,
    NoInstrumentation,
    Open {
        turn_id: Option<String>,
    },
    Completed {
        turn_id: Option<String>,
    },
    Aborted {
        turn_id: Option<String>,
        reason: Option<String>,
    },
}

impl Lifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lifecycle::Uninitialized => "uninitialized",
            Lifecycle::NoInstrumentation => "no_instrumentation",
            Lifecycle::Open { .. } => "open",
            Lifecycle::Completed { .. } => "completed",
            Lifecycle::Aborted { .. } => "aborted",
        }
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Lifecycle::Open { turn_id }
            | Lifecycle::Completed { turn_id }
            | Lifecycle::Aborted { turn_id, .. } => turn_id.as_deref(),
            Lifecycle::Uninitialized | Lifecycle::NoInstrumentation => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnState {
    Open,
    Completed,
    Aborted(Option<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct CodexProbe {
    pub lifecycle: Lifecycle,
    pub pending: Vec<PendingItem>,
    pub last_event_epoch: Option<i64>,
}

pub fn session_probe(session: &CodexSession) -> CodexProbe {
    let turns = fold_turns(session);
    let answered = answered_calls(session);
    let lifecycle = classify(session, &turns);
    let pending = pending_calls(session, &turns, &lifecycle, &answered);
    CodexProbe {
        lifecycle,
        pending,
        last_event_epoch: session
            .entries
            .iter()
            .filter_map(|entry| entry.timestamp)
            .map(|ts| ts.timestamp())
            .max(),
    }
}

pub fn codex_session_activity(session: &CodexSession, _opts: &ActivityOpts) -> SessionActivity {
    let probe = session_probe(session);
    let is_waiting = probe
        .pending
        .iter()
        .any(|item| WAIT_TOOLS.contains(&item.name.as_str()));
    let mid_tool = !probe.pending.is_empty();
    SessionActivity {
        is_waiting,
        mid_tool,
        last_event_epoch: probe.last_event_epoch,
        pending: probe.pending,
    }
}

fn fold_turns(session: &CodexSession) -> Vec<(Option<String>, TurnState)> {
    let mut turns: Vec<(Option<String>, TurnState)> = Vec::new();
    for entry in &session.entries {
        let CodexItem::EventMsg(em) = &entry.item else {
            continue;
        };
        match em {
            EventMsg::TaskStarted { turn_id, .. } => start_turn(&mut turns, turn_id.clone()),
            EventMsg::TaskComplete { turn_id, .. } => {
                close_turn(&mut turns, turn_id.clone(), TurnState::Completed)
            }
            EventMsg::TurnAborted {
                turn_id, reason, ..
            } => close_turn(
                &mut turns,
                turn_id.clone(),
                TurnState::Aborted(reason.clone()),
            ),
            _ => {}
        }
    }
    turns
}

fn start_turn(turns: &mut Vec<(Option<String>, TurnState)>, turn_id: Option<String>) {
    if let Some(id) = &turn_id {
        if let Some(slot) = turns
            .iter_mut()
            .find(|(t, _)| t.as_deref() == Some(id.as_str()))
        {
            slot.1 = TurnState::Open;
            return;
        }
    }
    turns.push((turn_id, TurnState::Open));
}

fn close_turn(
    turns: &mut Vec<(Option<String>, TurnState)>,
    turn_id: Option<String>,
    state: TurnState,
) {
    if let Some(id) = &turn_id {
        if let Some(slot) = turns
            .iter_mut()
            .find(|(t, _)| t.as_deref() == Some(id.as_str()))
        {
            slot.1 = state;
            return;
        }
        turns.push((turn_id, state));
        return;
    }
    if let Some(slot) = turns.iter_mut().rev().find(|(_, s)| *s == TurnState::Open) {
        slot.1 = state;
        return;
    }
    turns.push((None, state));
}

fn classify(session: &CodexSession, turns: &[(Option<String>, TurnState)]) -> Lifecycle {
    if let Some(terminal) = last_terminal_line(session) {
        let started_after = session.entries.iter().any(|entry| {
            entry.line_index > terminal
                && matches!(
                    entry.item,
                    CodexItem::EventMsg(EventMsg::TaskStarted { .. })
                )
        });
        if !started_after
            && session
                .entries
                .iter()
                .any(|entry| post_terminal_activity(entry, terminal, turns))
        {
            return Lifecycle::NoInstrumentation;
        }
    }
    match turns.last() {
        Some((turn_id, TurnState::Open)) => Lifecycle::Open {
            turn_id: turn_id.clone(),
        },
        Some((turn_id, TurnState::Completed)) => Lifecycle::Completed {
            turn_id: turn_id.clone(),
        },
        Some((turn_id, TurnState::Aborted(reason))) => Lifecycle::Aborted {
            turn_id: turn_id.clone(),
            reason: reason.clone(),
        },
        None if session.entries.iter().any(has_content) => Lifecycle::NoInstrumentation,
        None => Lifecycle::Uninitialized,
    }
}

fn post_terminal_activity(
    entry: &CodexEntry,
    terminal: usize,
    turns: &[(Option<String>, TurnState)],
) -> bool {
    if entry.line_index <= terminal {
        return false;
    }
    match &entry.item {
        CodexItem::EventMsg(EventMsg::UserMessage { .. }) => true,
        CodexItem::ResponseItem(ri)
            if matches!(
                &ri.payload,
                ResponseItemPayload::Message {
                    role: Some(role),
                    ..
                } if role == "user"
            ) || call_ref(&ri.payload).is_some() =>
        {
            ri.turn_id
                .as_deref()
                .is_none_or(|id| turn_state(turns, id).is_none())
        }
        _ => false,
    }
}

fn has_content(entry: &CodexEntry) -> bool {
    matches!(
        entry.item,
        CodexItem::ResponseItem(_) | CodexItem::EventMsg(_) | CodexItem::Compacted(_)
    )
}

fn answered_calls(session: &CodexSession) -> HashSet<String> {
    session
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            CodexItem::ResponseItem(ri) => output_call_id(&ri.payload),
            _ => None,
        })
        .cloned()
        .collect()
}

fn pending_calls(
    session: &CodexSession,
    turns: &[(Option<String>, TurnState)],
    lifecycle: &Lifecycle,
    answered: &HashSet<String>,
) -> Vec<PendingItem> {
    let terminal = last_terminal_line(session);
    let positional_start = positional_start_line(session, lifecycle);
    session
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            CodexItem::ResponseItem(ri) => {
                call_ref(&ri.payload).map(|call| (entry.line_index, ri.turn_id.as_deref(), call))
            }
            _ => None,
        })
        .filter(|(line, turn_id, _)| match turn_id {
            Some(id) => match turn_state(turns, id) {
                Some(TurnState::Open) => true,
                Some(TurnState::Completed | TurnState::Aborted(_)) => false,
                None => {
                    matches!(lifecycle, Lifecycle::NoInstrumentation)
                        && terminal.is_none_or(|cutoff| *line > cutoff)
                }
            },
            None => match lifecycle {
                Lifecycle::Open { .. } => positional_start.is_some_and(|start| *line > start),
                Lifecycle::NoInstrumentation => terminal.is_none_or(|cutoff| *line > cutoff),
                _ => false,
            },
        })
        .map(|(_, _, call)| call)
        .filter(|(call_id, _)| !answered.contains(call_id))
        .map(|(call_id, name)| PendingItem {
            tool_use_id: Some(call_id),
            name,
            kind: PendingKind::MidTool,
        })
        .collect()
}

fn turn_state<'a>(
    turns: &'a [(Option<String>, TurnState)],
    turn_id: &str,
) -> Option<&'a TurnState> {
    turns
        .iter()
        .find(|(id, _)| id.as_deref() == Some(turn_id))
        .map(|(_, state)| state)
}

fn positional_start_line(session: &CodexSession, lifecycle: &Lifecycle) -> Option<usize> {
    let Lifecycle::Open { turn_id } = lifecycle else {
        return None;
    };
    let starts = session.entries.iter().filter(|entry| {
        matches!(
            &entry.item,
            CodexItem::EventMsg(EventMsg::TaskStarted { turn_id: id, .. }) if id == turn_id
        )
    });
    if turn_id.is_some() {
        starts.map(|entry| entry.line_index).min()
    } else {
        starts.map(|entry| entry.line_index).max()
    }
}

fn last_terminal_line(session: &CodexSession) -> Option<usize> {
    session
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.item,
                CodexItem::EventMsg(EventMsg::TaskComplete { .. } | EventMsg::TurnAborted { .. })
            )
        })
        .map(|entry| entry.line_index)
        .max()
}

fn call_ref(payload: &ResponseItemPayload) -> Option<(String, String)> {
    match payload {
        ResponseItemPayload::FunctionCall { name, call_id, .. }
        | ResponseItemPayload::CustomToolCall { name, call_id, .. } => call_id
            .clone()
            .map(|id| (id, name.clone().unwrap_or_default())),
        ResponseItemPayload::ToolSearchCall { call_id, .. } => {
            call_id.clone().map(|id| (id, "tool_search".to_string()))
        }
        _ => None,
    }
}

fn output_call_id(payload: &ResponseItemPayload) -> Option<&String> {
    match payload {
        ResponseItemPayload::FunctionCallOutput { call_id, .. }
        | ResponseItemPayload::CustomToolCallOutput { call_id, .. }
        | ResponseItemPayload::ToolSearchOutput { call_id, .. } => call_id.as_ref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::codex::parse_codex_bytes;

    fn testdata_dir() -> PathBuf {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/testdata/codex"
        ))
        .to_path_buf()
    }

    fn session(tag: &str) -> CodexSession {
        let suffix = format!("{tag}.jsonl");
        let entry = std::fs::read_dir(testdata_dir())
            .expect("codex testdata dir")
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().ends_with(&suffix))
            .unwrap_or_else(|| panic!("fixture {tag} not found"));
        parse_codex_bytes(&std::fs::read(entry.path()).expect("read fixture"))
    }

    fn call_ids(items: &[PendingItem]) -> Vec<&str> {
        items
            .iter()
            .map(|item| item.tool_use_id.as_deref().unwrap())
            .collect()
    }

    fn inline_session(lines: &[&str]) -> CodexSession {
        parse_codex_bytes(format!("{}\n", lines.join("\n")).as_bytes())
    }

    #[test]
    fn open_turn_dangling_call_is_mid_tool() {
        let probe = session_probe(&session("050a"));
        assert!(matches!(
            probe.lifecycle,
            Lifecycle::Open { turn_id: Some(_) }
        ));
        assert_eq!(probe.last_event_epoch, Some(1784220090));
        assert_eq!(
            call_ids(&probe.pending),
            vec!["call_Demo0005dang0005dang0005x"]
        );
        assert!(probe.pending.iter().all(|p| p.kind == PendingKind::MidTool));

        let activity = codex_session_activity(&session("050a"), &ActivityOpts::default());
        assert!(activity.mid_tool);
        assert!(!activity.is_waiting);
        assert_eq!(activity.pending.len(), 1);
    }

    #[test]
    fn abort_clears_mid_tool() {
        let probe = session_probe(&session("050b"));
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Aborted {
                turn_id: Some("019f6821-0aa1-7000-8000-0000000000e2".to_string()),
                reason: Some("interrupted".to_string()),
            }
        );
        assert!(probe.pending.is_empty());
        assert_eq!(probe.last_event_epoch, Some(1784220121));

        let activity = codex_session_activity(&session("050b"), &ActivityOpts::default());
        assert!(!activity.mid_tool);
        assert!(!activity.is_waiting);
    }

    #[test]
    fn latest_of_two_completed_turns_wins() {
        let probe = session_probe(&session("050c"));
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Completed {
                turn_id: Some("019f6822-0bb2-7000-8000-0000000000f2".to_string()),
            }
        );
        assert!(probe.pending.is_empty());
        assert_eq!(probe.last_event_epoch, Some(1784220185));

        let activity = codex_session_activity(&session("050c"), &ActivityOpts::default());
        assert!(!activity.mid_tool);
        assert!(!activity.is_waiting);
    }

    #[test]
    fn compaction_does_not_close_the_turn() {
        let probe = session_probe(&session("050d"));
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Completed {
                turn_id: Some("019f6823-0aa1-7000-8000-0000000000c9".to_string()),
            }
        );
        assert!(probe.pending.is_empty());
        assert_eq!(probe.last_event_epoch, Some(1784220242));
    }

    #[test]
    fn no_lifecycle_events_is_no_instrumentation() {
        let probe = session_probe(&session("101"));
        assert_eq!(probe.lifecycle, Lifecycle::NoInstrumentation);
        assert!(probe.pending.is_empty());
        assert_eq!(probe.last_event_epoch, Some(1768881608));

        let activity = codex_session_activity(&session("101"), &ActivityOpts::default());
        assert!(!activity.mid_tool);
        assert!(!activity.is_waiting);
    }

    #[test]
    fn completed_single_turn() {
        let probe = session_probe(&session("303"));
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Completed {
                turn_id: Some("019f67f0-0aa1-7000-8000-0000000000c1".to_string()),
            }
        );
        assert!(probe.pending.is_empty());
    }

    #[test]
    fn last_event_epoch_is_envelope_not_content() {
        let s = session("303");
        let probe = session_probe(&s);
        let content_max = s
            .entries
            .iter()
            .filter(|e| matches!(&e.item, CodexItem::ResponseItem(_)))
            .filter_map(|e| e.timestamp)
            .map(|t| t.timestamp())
            .max();
        assert_eq!(probe.last_event_epoch, Some(1784218935));
        assert_eq!(content_max, Some(1784218801));
        assert!(probe.last_event_epoch > content_max);
    }

    #[test]
    fn child_file_turn_bracket_completes() {
        let probe = session_probe(&session("404"));
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Completed {
                turn_id: Some("019f6800-0bb2-7000-8000-0000000000d2".to_string()),
            }
        );
        assert!(probe.pending.is_empty());
        assert_eq!(probe.last_event_epoch, Some(1784218937));
    }

    #[test]
    fn post_abort_activity_without_task_started_is_open() {
        let session = inline_session(&[
            r#"{"type":"session_meta","payload":{"cli_version":"0.97.0"}}"#,
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"A","reason":"interrupted"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"again"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"new-call","internal_chat_message_metadata_passthrough":{"turn_id":"B"}}}"#,
        ]);
        let probe = session_probe(&session);
        assert_eq!(probe.lifecycle, Lifecycle::NoInstrumentation);
        assert_eq!(call_ids(&probe.pending), ["new-call"]);
        assert!(codex_session_activity(&session, &ActivityOpts::default()).mid_tool);
    }

    #[test]
    fn closed_turn_call_is_not_attributed_by_position() {
        let session = inline_session(&[
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"B"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"late-a","internal_chat_message_metadata_passthrough":{"turn_id":"A"}}}"#,
        ]);
        let probe = session_probe(&session);
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Open {
                turn_id: Some("B".to_string())
            }
        );
        assert!(probe.pending.is_empty());
        assert!(!codex_session_activity(&session, &ActivityOpts::default()).mid_tool);
    }

    #[test]
    fn no_instrumentation_pending_is_set_difference() {
        let session = inline_session(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"A","internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"B","internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"B","output":"done","internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}"#,
        ]);
        let probe = session_probe(&session);
        assert_eq!(probe.lifecycle, Lifecycle::NoInstrumentation);
        assert_eq!(call_ids(&probe.pending), ["A"]);
    }

    #[test]
    fn duplicate_task_started_folds_by_turn_id() {
        let session = inline_session(&[
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"call","internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T"}}"#,
        ]);
        let probe = session_probe(&session);
        assert_eq!(
            probe.lifecycle,
            Lifecycle::Completed {
                turn_id: Some("T".to_string())
            }
        );
        assert!(probe.pending.is_empty());
    }

    #[test]
    fn unanswered_tool_search_call_is_mid_tool() {
        let session = inline_session(&[
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#,
            r#"{"type":"response_item","payload":{"type":"tool_search_call","call_id":"search","arguments":{},"internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}"#,
        ]);
        let probe = session_probe(&session);
        assert_eq!(call_ids(&probe.pending), ["search"]);
        assert_eq!(probe.pending[0].name, "tool_search");
        assert!(codex_session_activity(&session, &ActivityOpts::default()).mid_tool);
    }
}
