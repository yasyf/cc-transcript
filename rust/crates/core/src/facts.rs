//! The single tool-call analytics substrate (facts.py).
//!
//! A pure projection of a lifted session's tool activity: [`tool_facts`] flattens every
//! [`ToolUse`] into a [`ToolFact`] — command prefixes, MCP server/tool/access, file path,
//! error and denial state, and duration — and the aggregators ([`command_prefix_counts`],
//! [`mcp_summary`]) roll those facts up. Gated behind `command`: prefix parsing needs the
//! tree-sitter command module.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

use crate::activity::{lift_session, ToolUse};
use crate::command;
use crate::protocol::{embedded_user_text, DENIAL_KIND_USER_REJECTED};
use crate::toolcall::{mcp_access, mcp_parts, ToolCall};
use crate::types::{Entry, ToolResultBlock};

/// One tool call flattened for analytics, lifted from a parsed session (facts.py `ToolFact`).
#[derive(Debug, Clone)]
pub struct ToolFact {
    pub ts: DateTime<FixedOffset>,
    pub session_id: String,
    pub path: String,
    pub tool_use_id: String,
    pub tool: String,
    pub command_prefixes: Vec<String>,
    pub command: Option<String>,
    pub mcp_server: Option<String>,
    pub mcp_tool: Option<String>,
    pub mcp_access: Option<String>,
    pub file_path: Option<String>,
    pub is_error: bool,
    pub denied: bool,
    pub denial_kind: Option<String>,
    pub user_said: Option<String>,
    pub duration_ms: Option<i64>,
}

/// A per-server MCP usage summary (facts.py `mcp_summary` values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSummary {
    pub read: usize,
    pub write: usize,
    pub total: usize,
    pub tools: Vec<(String, usize)>,
}

/// Denial flag and embedded user-rejection text carried by a tool result.
fn denial_fields(result: Option<&ToolResultBlock>) -> (bool, Option<String>) {
    match result {
        Some(block) if block.denial_kind.as_deref() == Some(DENIAL_KIND_USER_REJECTED) => {
            (true, embedded_user_text(&block.content))
        }
        _ => (false, None),
    }
}

/// Splits an MCP tool name into its server, tool, and access parts.
fn mcp_split(name: &str) -> (Option<String>, Option<String>, Option<String>) {
    match mcp_parts(name) {
        Some((server, tool)) => (
            Some(server.to_string()),
            Some(tool.to_string()),
            Some(mcp_access(tool).to_string()),
        ),
        None => (None, None, None),
    }
}

/// Flattens one lifted [`ToolUse`] into a [`ToolFact`].
fn fact_of(use_: &ToolUse, session_id: &str, path: &str) -> ToolFact {
    let call = &use_.call;
    let name = call.name();
    let (mcp_server, mcp_tool, mcp_access) = mcp_split(name);
    let (command, command_prefixes) = match call {
        ToolCall::Bash(bash) => (Some(bash.command.clone()), command::prefixes(&bash.command)),
        _ => (None, Vec::new()),
    };
    let (denied, user_said) = denial_fields(use_.result);
    ToolFact {
        ts: use_.ts,
        session_id: session_id.to_string(),
        path: path.to_string(),
        tool_use_id: use_.tool_use_id.to_string(),
        tool: name.to_string(),
        command_prefixes,
        command,
        mcp_server,
        mcp_tool,
        mcp_access,
        file_path: call.file_path().map(str::to_string),
        is_error: use_.result.is_some_and(|r| r.is_error),
        denied,
        denial_kind: use_.result.and_then(|r| r.denial_kind.clone()),
        user_said,
        duration_ms: use_.duration_ms(),
    }
}

/// One [`ToolFact`] per tool call across the lifted session, in turn then call order
/// (facts.py `tool_facts`).
pub fn tool_facts(session_id: &str, path: &str, entries: &[Entry]) -> Vec<ToolFact> {
    lift_session(session_id, entries)
        .turns
        .iter()
        .flat_map(|turn| turn.tool_uses.iter())
        .map(|use_| fact_of(use_, session_id, path))
        .collect()
}

/// Counts `items` preserving first-seen order, ordered by descending count with stable
/// ties (Python `Counter.most_common`).
fn counter_most_common(items: impl Iterator<Item = String>) -> Vec<(String, usize)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items {
        match counts.get_mut(&item) {
            Some(count) => *count += 1,
            None => {
                order.push(item.clone());
                counts.insert(item, 1);
            }
        }
    }
    let mut result: Vec<(String, usize)> = order
        .into_iter()
        .map(|key| (counts[&key], key))
        .map(|(count, key)| (key, count))
        .collect();
    result.sort_by(|a, b| b.1.cmp(&a.1));
    result
}

/// Every Bash command prefix across `facts`, most frequent first (facts.py `command_prefix_counts`).
pub fn command_prefix_counts<'a>(
    facts: impl IntoIterator<Item = &'a ToolFact>,
) -> Vec<(String, usize)> {
    counter_most_common(
        facts
            .into_iter()
            .flat_map(|fact| fact.command_prefixes.iter().cloned()),
    )
}

/// MCP usage per server across `facts`, servers ordered by descending total then name,
/// each `tools` map by descending frequency (facts.py `mcp_summary`).
pub fn mcp_summary<'a>(
    facts: impl IntoIterator<Item = &'a ToolFact>,
) -> Vec<(String, McpServerSummary)> {
    let mut server_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for fact in facts {
        if let (Some(server), Some(tool), Some(access)) =
            (&fact.mcp_server, &fact.mcp_tool, &fact.mcp_access)
        {
            match groups.get_mut(server) {
                Some(calls) => calls.push((tool.clone(), access.clone())),
                None => {
                    server_order.push(server.clone());
                    groups.insert(server.clone(), vec![(tool.clone(), access.clone())]);
                }
            }
        }
    }
    let mut out: Vec<(String, McpServerSummary)> = server_order
        .into_iter()
        .map(|server| {
            let calls = &groups[&server];
            let read = calls.iter().filter(|(_, a)| a == "read").count();
            let write = calls.iter().filter(|(_, a)| a == "write").count();
            let tools = counter_most_common(calls.iter().map(|(tool, _)| tool.clone()));
            (
                server,
                McpServerSummary {
                    read,
                    write,
                    total: calls.len(),
                    tools,
                },
            )
        })
        .collect();
    out.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_bytes;

    fn parse(raw: &str) -> Vec<Entry> {
        parse_bytes(raw.as_bytes(), |_| true).expect("parses")
    }

    #[test]
    fn facts_project_prefixes_mcp_and_denial() {
        let entries = parse(concat!(
            r#"{"type":"user","uuid":"u0","sessionId":"s","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"go"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a0","sessionId":"s","timestamp":"2026-01-01T00:00:01.000Z","message":{"model":"m","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"git push && ls"}},{"type":"tool_use","id":"t2","name":"mcp__semble__search","input":{"query":"x"}},{"type":"tool_use","id":"t3","name":"mcp__semble__search","input":{"query":"y"}}]}}"#,
            "\n",
        ));
        let facts = tool_facts("s", "/p.jsonl", &entries);
        assert_eq!(facts.len(), 3);
        assert_eq!(
            facts[0].command_prefixes,
            vec!["git push".to_string(), "ls".to_string()]
        );
        assert_eq!(facts[1].mcp_server.as_deref(), Some("semble"));
        assert_eq!(facts[1].mcp_access.as_deref(), Some("read"));
        let counts = command_prefix_counts(&facts);
        assert_eq!(
            counts,
            vec![("git push".to_string(), 1), ("ls".to_string(), 1)]
        );
        let mcp = mcp_summary(&facts);
        assert_eq!(mcp.len(), 1);
        assert_eq!(mcp[0].0, "semble");
        assert_eq!(mcp[0].1.total, 2);
        assert_eq!(mcp[0].1.read, 2);
        assert_eq!(mcp[0].1.tools, vec![("search".to_string(), 2)]);
    }
}
