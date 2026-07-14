//! The harness notification-delivery queue, replayed from a session's events —
//! ported from `cc_transcript/notifications.py`.
//!
//! Replays every `enqueue`/`dequeue`/`remove`/`popAll` `queue-operation` audit
//! record to model which notifications are still queued, which were delivered, and
//! which ever passed through.

use crate::generated::protocol::{
    TASK_NOTIFICATION_MARKER, TOOL_USE_ID_PREFIX, TOOL_USE_ID_SUFFIX,
};
use crate::types::{AttachmentDetail, Entry};
use crate::value::field_str;

/// tool_use_marker (notifications.py): the `<tool-use-id>…</tool-use-id>` wrapper a
/// notification carries for the tool call it reports.
pub fn tool_use_marker(tool_use_id: &str) -> String {
    format!("{TOOL_USE_ID_PREFIX}{tool_use_id}{TOOL_USE_ID_SUFFIX}")
}

/// delivered_text (notifications.py): the text of a delivered notification — a user
/// turn carrying the task-notification marker, or a queued-command attachment's
/// prompt — else None.
fn delivered_text(entry: &Entry) -> Option<String> {
    match entry {
        Entry::User(user) => {
            let text = user.content.text();
            text.contains(TASK_NOTIFICATION_MARKER).then_some(text)
        }
        Entry::Attachment(att) if att.attachment_type == "queued_command" => match &att.detail {
            AttachmentDetail::QueuedCommand(q) => Some(q.prompt.clone().unwrap_or_default()),
            _ => None,
        },
        _ => None,
    }
}

/// replay_queue (notifications.py): folds the transcript's `queue-operation` records
/// into `(still-queued, ever-enqueued)`. Each `enqueue` appends its content to a FIFO
/// and to the enqueued log; `dequeue`/`remove` drop the head; `popAll` subtracts every
/// queued item whose text is a substring of the operation's content.
pub fn replay_queue(entries: &[Entry]) -> (Vec<String>, Vec<String>) {
    let mut queued: Vec<String> = Vec::new();
    let mut enqueued: Vec<String> = Vec::new();
    for entry in entries {
        let Entry::Other(other) = entry else { continue };
        if other.ty != "queue-operation" {
            continue;
        }
        let content = || field_str(&other.raw, "content").unwrap_or("").to_string();
        match field_str(&other.raw, "operation") {
            Some("enqueue") => {
                let item = content();
                enqueued.push(item.clone());
                queued.push(item);
            }
            Some("dequeue" | "remove") if !queued.is_empty() => {
                queued.remove(0);
            }
            Some("popAll") => {
                let content = content();
                queued.retain(|item| !content.contains(item.as_str()));
            }
            _ => {}
        }
    }
    (queued, enqueued)
}

/// The modeled state of a session's harness notification-delivery queue
/// (notifications.py Notifications).
#[derive(Debug, Clone, PartialEq)]
pub struct Notifications {
    pub queued: Vec<String>,
    pub delivered: Vec<String>,
    pub enqueued: Vec<String>,
}

impl Notifications {
    /// from_events (notifications.py): replays the queue over `entries`, in order.
    pub fn from_entries(entries: &[Entry]) -> Self {
        let (queued, enqueued) = replay_queue(entries);
        Notifications {
            queued,
            delivered: entries.iter().filter_map(delivered_text).collect(),
            enqueued,
        }
    }

    /// completed (notifications.py): the call's notification reached the agent — it was
    /// delivered, or it was enqueued yet no longer sits undelivered in the queue.
    pub fn completed(&self, tool_use_id: &str) -> bool {
        let marker = tool_use_marker(tool_use_id);
        self.delivered.iter().any(|text| text.contains(&marker))
            || (self.enqueued.iter().any(|text| text.contains(&marker))
                && !self.queued.iter().any(|text| text.contains(&marker)))
    }

    /// pending (notifications.py): the call's notification is still queued for delivery.
    pub fn pending(&self, tool_use_id: &str) -> bool {
        let marker = tool_use_marker(tool_use_id);
        self.queued.iter().any(|text| text.contains(&marker))
    }

    /// has_pending (notifications.py): any queued item is an undelivered task notification.
    pub fn has_pending(&self) -> bool {
        self.queued
            .iter()
            .any(|text| text.contains(TASK_NOTIFICATION_MARKER))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_bytes;

    fn parse(lines: &[&str]) -> Vec<Entry> {
        parse_bytes(lines.join("\n").as_bytes(), |_| true).unwrap()
    }

    fn notif(tool_use_id: &str) -> String {
        format!(
            "{TASK_NOTIFICATION_MARKER}background task finished {}</task-notification>",
            tool_use_marker(tool_use_id)
        )
    }

    fn queue_op(operation: &str, content: &str) -> String {
        format!(
            r#"{{"type":"queue-operation","operation":"{operation}","content":{}}}"#,
            escape(content)
        )
    }

    fn user(text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":{}}}}}"#,
            escape(text)
        )
    }

    fn escape(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    #[test]
    fn enqueue_only_is_pending_not_completed() {
        let n = Notifications::from_entries(&parse(&[&queue_op("enqueue", &notif("toolu_bg"))]));
        assert!(n.has_pending());
        assert!(n.pending("toolu_bg"));
        assert!(!n.completed("toolu_bg"));
    }

    #[test]
    fn dequeue_then_user_delivery_completes() {
        let n = Notifications::from_entries(&parse(&[
            &queue_op("enqueue", &notif("toolu_bg")),
            &queue_op("dequeue", ""),
            &user(&notif("toolu_bg")),
        ]));
        assert!(n.completed("toolu_bg"));
        assert!(!n.pending("toolu_bg"));
        assert!(!n.has_pending());
    }

    #[test]
    fn popall_of_user_message_spares_the_notification() {
        let n = Notifications::from_entries(&parse(&[
            &queue_op("enqueue", &notif("toolu_bg")),
            &queue_op("enqueue", "please run the suite"),
            &queue_op("popAll", "please run the suite"),
        ]));
        assert_eq!(n.queued, vec![notif("toolu_bg")]);
        assert_eq!(
            n.enqueued,
            vec![notif("toolu_bg"), "please run the suite".to_string()]
        );
        assert!(n.pending("toolu_bg"));
    }

    #[test]
    fn remove_without_delivery_counts_as_dropped_completed() {
        let n = Notifications::from_entries(&parse(&[
            &queue_op("enqueue", &notif("toolu_bg")),
            &queue_op("remove", ""),
        ]));
        assert!(n.completed("toolu_bg"));
        assert!(!n.has_pending());
    }
}
