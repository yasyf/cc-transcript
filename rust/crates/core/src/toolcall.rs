//! The typed tool-call hierarchy, ported from `cc_transcript/tools.py`.
//!
//! Parity: `tools.py`. Required fields are validated to strings (parse fails
//! otherwise); every other field mirrors Python's untyped `dict.get`, storing the
//! value verbatim (any JSON type) with absent and JSON null both folding to None.
//! `raw` is retained on every variant (Python keeps it: rendering, raw-input
//! queries, and AskUserQuestionResult.questions all read it); it is `compare=False`
//! Python-side, and the content digest lives in `ids`, not here.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::parse::parse_questions;
use crate::types::Question;
use crate::value::{field, field_str, normalized_owned};

// Parity: tools.py TOOL_ALIASES (canonical -> the harness display aliases). Bash also
// carries `exec_command`, codex's shell-call spelling, so a spec naming Bash matches a
// lowered codex shell call. Multi-valued so one canonical tool expands to several names.
fn tool_aliases(name: &str) -> &'static [&'static str] {
    match name {
        "Bash" => &["Execute", "exec_command"],
        "Write" => &["Create"],
        "Agent" => &["Task"],
        "WebFetch" => &["FetchUrl"],
        "ExitPlanMode" => &["ExitSpecMode"],
        _ => &[],
    }
}

// Parity: tools.py TOOL_ALIASES_REVERSE (alias -> canonical name); `exec_command`
// reverse-resolves to Bash so a lowered codex shell call drives the Bash dispatch arm.
fn tool_alias_reverse(name: &str) -> Option<&'static str> {
    match name {
        "Execute" | "exec_command" => Some("Bash"),
        "Create" => Some("Write"),
        "Task" => Some("Agent"),
        "FetchUrl" => Some("WebFetch"),
        "ExitSpecMode" => Some("ExitPlanMode"),
        _ => None,
    }
}

// The codex tools whose ToolUse input is a verbatim string (JSON args, free-form code,
// or a patch envelope) rather than a decoded object mapping; their arms read it directly.
const CODEX_VERBATIM_TOOLS: [&str; 5] = [
    "exec_command",
    "exec",
    "apply_patch",
    "update_plan",
    "write_stdin",
];

// The built-in tools whose parse arms require an object input; a non-object raises
// NonMapping. Mirrors the canonical arms of `parse_tool_call_strict`.
fn requires_object_input(canonical: &str) -> bool {
    matches!(
        canonical,
        "Bash"
            | "Edit"
            | "MultiEdit"
            | "Write"
            | "Read"
            | "NotebookEdit"
            | "Grep"
            | "Glob"
            | "Agent"
            | "Workflow"
            | "Skill"
            | "TaskCreate"
            | "TaskUpdate"
            | "ExitPlanMode"
    )
}

/// The payload key names an MCP tool's span-edit lowering reads — never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEditMap {
    pub path: String,
    pub content: String,
    pub delete: Option<String>,
}

/// A registered MCP tool's behavior: the built-in gate it aliases, plus an
/// optional span-edit lowering addressed by payload key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolSpec {
    pub behaves_like: String,
    pub span_edit: Option<SpanEditMap>,
}

// Process-local: the standalone Rust CLI never populates it (an embedding-driven
// registry), so parsing there keeps the pre-registry OtherCall behavior.
static MCP_REGISTRY: LazyLock<RwLock<HashMap<String, McpToolSpec>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Registers `tool` (a bare MCP segment) with `spec`; last write wins.
pub fn register_mcp_tool(tool: String, spec: McpToolSpec) {
    MCP_REGISTRY
        .write()
        .expect("mcp registry lock")
        .insert(tool, spec);
}

/// Unregisters `tool`, returning whether it was registered.
pub fn unregister_mcp_tool(tool: &str) -> bool {
    MCP_REGISTRY
        .write()
        .expect("mcp registry lock")
        .remove(tool)
        .is_some()
}

/// Resolves a bare MCP tool segment to the built-in edit gate it behaves like.
pub fn mcp_tool_alias(tool: &str) -> Option<String> {
    MCP_REGISTRY
        .read()
        .expect("mcp registry lock")
        .get(tool)
        .map(|spec| spec.behaves_like.clone())
}

// The span-edit lowering registered for a bare MCP tool segment, if any.
fn registered_span_edit(tool: &str) -> Option<SpanEditMap> {
    MCP_REGISTRY
        .read()
        .expect("mcp registry lock")
        .get(tool)
        .and_then(|spec| spec.span_edit.clone())
}

// Parity: tools.py READ_VERBS.
const READ_VERBS: [&str; 10] = [
    "get", "list", "search", "read", "view", "fetch", "query", "describe", "show", "find",
];

fn json_false() -> Value {
    sonic_rs::from_str("false").expect("literal false parses")
}

// Parity: tools.py `raw.get(key)` — absent/null -> None, else the value verbatim.
fn opt(input: &Value, key: &str) -> Option<Value> {
    field(input, key).filter(|v| !v.is_null()).cloned()
}

// Parity: tools.py key_of — the first key whose value is non-null, verbatim.
fn opt_keys(input: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter()
        .find_map(|k| field(input, k).filter(|v| !v.is_null()).cloned())
}

// Parity: tools.py `raw.get(key, False)` — absent -> False, else verbatim (no coercion:
// EditCall.replace_all keeps 1/"yes"; only EditSpan applies bool()).
fn get_or_false(input: &Value, key: &str) -> Value {
    field(input, key).cloned().unwrap_or_else(json_false)
}

// Parity: SkillResult `tuple(tools) if isinstance(tools, list) else None`.
fn opt_array(input: &Value, key: &str) -> Option<Value> {
    field(input, key)
        .filter(|v| v.as_array().is_some())
        .cloned()
}

// Parity: Python `x is not None` over a get — present and non-null.
fn present(input: &Value, key: &str) -> bool {
    field(input, key).is_some_and(|v| !v.is_null())
}

// Parity: Python bool() truthiness (EditSpan.replace_all's coercion).
fn truthy(v: &Value) -> bool {
    if v.is_null() {
        return false;
    }
    if let Some(b) = v.as_bool() {
        return b;
    }
    if let Some(f) = v.as_f64() {
        return f != 0.0;
    }
    if let Some(s) = v.as_str() {
        return !s.is_empty();
    }
    if let Some(a) = v.as_array() {
        return !a.is_empty();
    }
    if let Some(o) = v.as_object() {
        return !o.is_empty();
    }
    true
}

// Parity: Python type(value).__name__ for the value's JSON kind.
fn py_type_name(v: &Value) -> &'static str {
    if v.is_null() {
        return "NoneType";
    }
    if v.as_bool().is_some() {
        return "bool";
    }
    if let Some(n) = v.as_raw_number() {
        return if n.as_str().bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
            "float"
        } else {
            "int"
        };
    }
    if v.as_str().is_some() {
        return "str";
    }
    if v.as_array().is_some() {
        return "list";
    }
    "dict"
}

/// A known tool's input did not match its expected shape (tools.py ToolInputError).
/// Variants carry enough to rebuild Python's message at the pyo3 boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInputError {
    /// Non-mapping input: `{name} input must be a mapping, got {py_type}`.
    NonMapping(&'static str),
    /// Absent/null required key (Python KeyError): `... missing or malformed: '{key}'`.
    MissingKey(String),
    /// Wrong-type required key (Python TypeError): `... {key} must be a str, got {py_type}`.
    WrongType { key: String, py_type: &'static str },
    /// An edits-iteration/span error whose Python message is repr-dependent.
    Malformed(String),
}

impl std::fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolInputError::NonMapping(py_type) => {
                write!(f, "input must be a mapping, got {py_type}")
            }
            ToolInputError::MissingKey(key) => write!(f, "input missing or malformed: '{key}'"),
            ToolInputError::WrongType { key, py_type } => {
                write!(
                    f,
                    "input missing or malformed: {key} must be a str, got {py_type}"
                )
            }
            ToolInputError::Malformed(detail) => write!(f, "input missing or malformed: {detail}"),
        }
    }
}

// Parity: tools.py required_str over a single key.
fn req_str(input: &Value, key: &str) -> Result<String, ToolInputError> {
    match field(input, key) {
        Some(v) if !v.is_null() => {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| ToolInputError::WrongType {
                    key: key.to_string(),
                    py_type: py_type_name(v),
                })
        }
        _ => Err(ToolInputError::MissingKey(key.to_string())),
    }
}

// Parity: tools.py required_str over key alternates; the message cites keys[0].
fn req_str_keys(input: &Value, keys: &[&str]) -> Result<String, ToolInputError> {
    for key in keys {
        if let Some(v) = field(input, key) {
            if !v.is_null() {
                return v
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ToolInputError::WrongType {
                        key: keys[0].to_string(),
                        py_type: py_type_name(v),
                    });
            }
        }
    }
    Err(ToolInputError::MissingKey(keys[0].to_string()))
}

/// A before/after content pair lowered from an edit-shaped tool call (tools.py Hunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old: String,
    pub new: String,
}

/// One replacement within a MultiEdit call, in application order (tools.py EditSpan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSpan {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

impl EditSpan {
    // Parity: EditSpan.from_raw — both strings required; replace_all takes bool().
    fn from_raw(span: &Value) -> Result<EditSpan, ToolInputError> {
        match (field_str(span, "old_string"), field_str(span, "new_string")) {
            (Some(old), Some(new)) => Ok(EditSpan {
                old: old.to_string(),
                new: new.to_string(),
                replace_all: field(span, "replace_all").map(truthy).unwrap_or(false),
            }),
            _ => Err(ToolInputError::Malformed("edit span".to_string())),
        }
    }
}

// Parity: `tuple(EditSpan.from_raw(s) for s in raw["edits"])`. A list yields items; an
// empty string/dict yields nothing; any other value fails as Python iteration would.
fn edit_spans(edits: &Value) -> Result<Vec<EditSpan>, ToolInputError> {
    if let Some(arr) = edits.as_array() {
        return arr.iter().map(EditSpan::from_raw).collect();
    }
    if let Some(obj) = edits.as_object() {
        return if obj.iter().next().is_none() {
            Ok(Vec::new())
        } else {
            Err(ToolInputError::Malformed("edit span".to_string()))
        };
    }
    if let Some(s) = edits.as_str() {
        return if s.is_empty() {
            Ok(Vec::new())
        } else {
            Err(ToolInputError::Malformed("edit span".to_string()))
        };
    }
    Err(ToolInputError::Malformed("edits not iterable".to_string()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct BashCall {
    pub name: String,
    pub raw: Value,
    pub command: String,
    pub timeout: Option<Value>,
    pub description: Option<Value>,
    pub run_in_background: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditCall {
    pub name: String,
    pub raw: Value,
    pub file_path: String,
    pub old: String,
    pub new: String,
    pub replace_all: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MultiEditCall {
    pub name: String,
    pub raw: Value,
    pub file_path: String,
    pub edits: Vec<EditSpan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WriteCall {
    pub name: String,
    pub raw: Value,
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadCall {
    pub name: String,
    pub raw: Value,
    pub file_path: String,
    pub offset: Option<Value>,
    pub limit: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotebookEditCall {
    pub name: String,
    pub raw: Value,
    pub notebook_path: String,
    pub new_source: String,
    pub cell_id: Option<Value>,
    pub edit_mode: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrepCall {
    pub name: String,
    pub raw: Value,
    pub pattern: String,
    pub path: Option<Value>,
    pub glob: Option<Value>,
    pub file_type: Option<Value>,
    pub output_mode: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobCall {
    pub name: String,
    pub raw: Value,
    pub pattern: String,
    pub path: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskCall {
    pub name: String,
    pub raw: Value,
    pub prompt: String,
    pub agent_type: Option<Value>,
    pub model: Option<Value>,
    pub agent_name: Option<Value>,
    pub run_in_background: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowCall {
    pub name: String,
    pub raw: Value,
    pub script: Option<Value>,
    pub script_path: Option<Value>,
    pub workflow_name: Option<Value>,
    pub args: Option<Value>,
    pub resume_from_run_id: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillCall {
    pub name: String,
    pub raw: Value,
    pub skill: String,
    pub args: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskCreateCall {
    pub name: String,
    pub raw: Value,
    pub subject: String,
    pub description: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskUpdateCall {
    pub name: String,
    pub raw: Value,
    pub task_id: String,
    pub status: Option<Value>,
    pub subject: Option<Value>,
    pub description: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExitPlanModeCall {
    pub name: String,
    pub raw: Value,
    pub plan: String,
}

/// A codex code-mode `exec` call: `source` is its free-form program, kept verbatim
/// and never JSON-decoded; `raw` is the original string input.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeModeCall {
    pub name: String,
    pub raw: Value,
    pub source: String,
}

/// How one file of an apply_patch envelope is edited (tools.py PatchEdit.kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchEditKind {
    Add,
    Update,
    Delete,
}

impl PatchEditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PatchEditKind::Add => "add",
            PatchEditKind::Update => "update",
            PatchEditKind::Delete => "delete",
        }
    }
}

/// One file's edit within a codex apply_patch envelope. `hunks` is empty for a
/// deletion and holds one addition hunk for an added file; `move_path` is the
/// rename target when the file is moved.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchEdit {
    pub file_path: String,
    pub kind: PatchEditKind,
    pub move_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// A codex apply_patch call: one `PatchEdit` per file in the envelope. A malformed
/// envelope yields no edits (never an error); `raw` is the original envelope string.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyPatchCall {
    pub name: String,
    pub raw: Value,
    pub edits: Vec<PatchEdit>,
}

/// A codex update_plan call: `plan` is the plan-step array and `explanation` the
/// optional narration, decoded from the JSON-string arguments; `raw` is that string.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdatePlanCall {
    pub name: String,
    pub raw: Value,
    pub plan: Option<Value>,
    pub explanation: Option<String>,
}

/// A codex write_stdin call: `chars` is the text written to the target session's
/// stdin and `session_id` its identifier, decoded from the JSON-string arguments;
/// `raw` is that string.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteStdinCall {
    pub name: String,
    pub raw: Value,
    pub chars: Option<Value>,
    pub session_id: i64,
    pub yield_time_ms: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// An in-place file edit addressed by an opaque locator — a registered MCP tool
/// whose spec carries a span-edit lowering. The payload carries no pre-image;
/// registrations are process-local (the standalone Rust CLI never sees them — the
/// intended semantic for an embedding-driven registry).
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEditCall {
    pub name: String,
    pub raw: Value,
    pub file_path: String,
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtherCall {
    pub name: String,
    pub raw: Value,
    pub error: Option<String>,
}

/// The typed tool-call hierarchy (tools.py ToolCall union).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    Bash(BashCall),
    Edit(EditCall),
    MultiEdit(MultiEditCall),
    Write(WriteCall),
    Read(ReadCall),
    NotebookEdit(NotebookEditCall),
    Grep(GrepCall),
    Glob(GlobCall),
    Task(TaskCall),
    Workflow(WorkflowCall),
    Skill(SkillCall),
    TaskCreate(TaskCreateCall),
    TaskUpdate(TaskUpdateCall),
    ExitPlanMode(ExitPlanModeCall),
    CodeMode(CodeModeCall),
    ApplyPatch(ApplyPatchCall),
    UpdatePlan(UpdatePlanCall),
    WriteStdin(WriteStdinCall),
    SpanEdit(SpanEditCall),
    Other(OtherCall),
}

impl ToolCall {
    /// The Python dataclass name for this variant.
    pub fn type_name(&self) -> &'static str {
        match self {
            ToolCall::Bash(_) => "BashCall",
            ToolCall::Edit(_) => "EditCall",
            ToolCall::MultiEdit(_) => "MultiEditCall",
            ToolCall::Write(_) => "WriteCall",
            ToolCall::Read(_) => "ReadCall",
            ToolCall::NotebookEdit(_) => "NotebookEditCall",
            ToolCall::Grep(_) => "GrepCall",
            ToolCall::Glob(_) => "GlobCall",
            ToolCall::Task(_) => "TaskCall",
            ToolCall::Workflow(_) => "WorkflowCall",
            ToolCall::Skill(_) => "SkillCall",
            ToolCall::TaskCreate(_) => "TaskCreateCall",
            ToolCall::TaskUpdate(_) => "TaskUpdateCall",
            ToolCall::ExitPlanMode(_) => "ExitPlanModeCall",
            ToolCall::CodeMode(_) => "CodeModeCall",
            ToolCall::ApplyPatch(_) => "ApplyPatchCall",
            ToolCall::UpdatePlan(_) => "UpdatePlanCall",
            ToolCall::WriteStdin(_) => "WriteStdinCall",
            ToolCall::SpanEdit(_) => "SpanEditCall",
            ToolCall::Other(_) => "OtherCall",
        }
    }

    /// The tool name (tools.py ToolCallBase.name).
    pub fn name(&self) -> &str {
        match self {
            ToolCall::Bash(c) => &c.name,
            ToolCall::Edit(c) => &c.name,
            ToolCall::MultiEdit(c) => &c.name,
            ToolCall::Write(c) => &c.name,
            ToolCall::Read(c) => &c.name,
            ToolCall::NotebookEdit(c) => &c.name,
            ToolCall::Grep(c) => &c.name,
            ToolCall::Glob(c) => &c.name,
            ToolCall::Task(c) => &c.name,
            ToolCall::Workflow(c) => &c.name,
            ToolCall::Skill(c) => &c.name,
            ToolCall::TaskCreate(c) => &c.name,
            ToolCall::TaskUpdate(c) => &c.name,
            ToolCall::ExitPlanMode(c) => &c.name,
            ToolCall::CodeMode(c) => &c.name,
            ToolCall::ApplyPatch(c) => &c.name,
            ToolCall::UpdatePlan(c) => &c.name,
            ToolCall::WriteStdin(c) => &c.name,
            ToolCall::SpanEdit(c) => &c.name,
            ToolCall::Other(c) => &c.name,
        }
    }

    /// The raw input mapping (tools.py ToolCallBase.raw).
    pub fn raw(&self) -> &Value {
        match self {
            ToolCall::Bash(c) => &c.raw,
            ToolCall::Edit(c) => &c.raw,
            ToolCall::MultiEdit(c) => &c.raw,
            ToolCall::Write(c) => &c.raw,
            ToolCall::Read(c) => &c.raw,
            ToolCall::NotebookEdit(c) => &c.raw,
            ToolCall::Grep(c) => &c.raw,
            ToolCall::Glob(c) => &c.raw,
            ToolCall::Task(c) => &c.raw,
            ToolCall::Workflow(c) => &c.raw,
            ToolCall::Skill(c) => &c.raw,
            ToolCall::TaskCreate(c) => &c.raw,
            ToolCall::TaskUpdate(c) => &c.raw,
            ToolCall::ExitPlanMode(c) => &c.raw,
            ToolCall::CodeMode(c) => &c.raw,
            ToolCall::ApplyPatch(c) => &c.raw,
            ToolCall::UpdatePlan(c) => &c.raw,
            ToolCall::WriteStdin(c) => &c.raw,
            ToolCall::SpanEdit(c) => &c.raw,
            ToolCall::Other(c) => &c.raw,
        }
    }

    /// Parity: tools.py hunks_of.
    pub fn hunks(&self) -> Vec<Hunk> {
        match self {
            ToolCall::Edit(c) => vec![Hunk {
                old: c.old.clone(),
                new: c.new.clone(),
            }],
            ToolCall::MultiEdit(c) => c
                .edits
                .iter()
                .map(|s| Hunk {
                    old: s.old.clone(),
                    new: s.new.clone(),
                })
                .collect(),
            ToolCall::Write(c) => vec![Hunk {
                old: String::new(),
                new: c.content.clone(),
            }],
            ToolCall::NotebookEdit(c) => vec![Hunk {
                old: String::new(),
                new: c.new_source.clone(),
            }],
            _ => Vec::new(),
        }
    }

    /// Parity: tools.py file_path_of.
    pub fn file_path(&self) -> Option<&str> {
        match self {
            ToolCall::Edit(c) => Some(&c.file_path),
            ToolCall::MultiEdit(c) => Some(&c.file_path),
            ToolCall::Write(c) => Some(&c.file_path),
            ToolCall::Read(c) => Some(&c.file_path),
            ToolCall::NotebookEdit(c) => Some(&c.notebook_path),
            ToolCall::SpanEdit(c) => Some(&c.file_path),
            _ => None,
        }
    }

    /// Parity: tools.py file_paths_of. Every file a call targets: one entry per
    /// patched file for apply_patch, else the singular projection as a 0/1-element vec.
    pub fn file_paths(&self) -> Vec<&str> {
        match self {
            ToolCall::ApplyPatch(c) => c.edits.iter().map(|e| e.file_path.as_str()).collect(),
            _ => self.file_path().into_iter().collect(),
        }
    }

    /// Parity: tools.py edits_of. Every `(file_path, hunks)` a call lowers to: one
    /// entry per patched file for apply_patch (empty hunks for a deletion), else the
    /// singular `(file_path, hunks)` when both are non-empty, as a 0/1-element vec.
    pub fn edits(&self) -> Vec<(&str, Vec<Hunk>)> {
        match self {
            ToolCall::ApplyPatch(c) => c
                .edits
                .iter()
                .map(|e| (e.file_path.as_str(), e.hunks.clone()))
                .collect(),
            _ => match (self.file_path(), self.hunks()) {
                (Some(path), hunks) if !hunks.is_empty() => vec![(path, hunks)],
                _ => Vec::new(),
            },
        }
    }
}

fn bash_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    // A string input is codex exec_command `{"cmd": ...}`: decode and read `cmd` (workdir
    // et al. stay only in `raw`). An object is a native Bash reading `command`.
    let command = match raw.as_str() {
        Some(text) => req_str(
            &sonic_rs::from_str(text)
                .map_err(|_| ToolInputError::Malformed("exec_command arguments".to_string()))?,
            "cmd",
        )?,
        None if raw.as_object().is_some() => req_str(raw, "command")?,
        None => return Err(ToolInputError::NonMapping(py_type_name(raw))),
    };
    Ok(ToolCall::Bash(BashCall {
        name: name.to_string(),
        raw: raw.clone(),
        command,
        timeout: opt(raw, "timeout"),
        description: opt(raw, "description"),
        run_in_background: opt(raw, "run_in_background"),
    }))
}

// codex `exec` code-mode input is free-form program text, kept verbatim (never decoded).
fn code_mode_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::CodeMode(CodeModeCall {
        name: name.to_string(),
        raw: raw.clone(),
        source: raw
            .as_str()
            .ok_or_else(|| ToolInputError::NonMapping(py_type_name(raw)))?
            .to_string(),
    }))
}

// codex apply_patch input is a patch-envelope string; a malformed envelope yields no edits.
fn apply_patch_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    let text = raw
        .as_str()
        .ok_or_else(|| ToolInputError::NonMapping(py_type_name(raw)))?;
    Ok(ToolCall::ApplyPatch(ApplyPatchCall {
        name: name.to_string(),
        raw: raw.clone(),
        edits: parse_patch_envelope(text).unwrap_or_default(),
    }))
}

// codex update_plan carries JSON-string arguments `{"plan": [...], "explanation": ...}`.
fn update_plan_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    let decoded = decode_string_arguments(raw)?;
    Ok(ToolCall::UpdatePlan(UpdatePlanCall {
        name: name.to_string(),
        raw: raw.clone(),
        plan: opt_list(&decoded, "plan")?,
        explanation: opt_str(&decoded, "explanation")?,
    }))
}

// codex write_stdin carries JSON-string arguments `{"chars": ..., "session_id": ...}`.
fn write_stdin_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    let decoded = decode_string_arguments(raw)?;
    Ok(ToolCall::WriteStdin(WriteStdinCall {
        name: name.to_string(),
        raw: raw.clone(),
        chars: opt(&decoded, "chars"),
        session_id: req_i64(&decoded, "session_id")?,
        yield_time_ms: opt_i64(&decoded, "yield_time_ms")?,
        max_output_tokens: opt_i64(&decoded, "max_output_tokens")?,
    }))
}

// Decode a codex tool's JSON-string arguments into their object form.
fn decode_string_arguments(raw: &Value) -> Result<Value, ToolInputError> {
    let text = raw
        .as_str()
        .ok_or_else(|| ToolInputError::NonMapping(py_type_name(raw)))?;
    let decoded: Value = sonic_rs::from_str(text)
        .map_err(|_| ToolInputError::Malformed("json arguments".to_string()))?;
    if decoded.as_object().is_none() {
        return Err(ToolInputError::NonMapping(py_type_name(&decoded)));
    }
    Ok(decoded)
}

fn opt_list(input: &Value, key: &str) -> Result<Option<Value>, ToolInputError> {
    match field(input, key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) if value.as_array().is_some() => Ok(Some(value.clone())),
        Some(value) => Err(ToolInputError::Malformed(format!(
            "{key} must be a list, got {}",
            py_type_name(value)
        ))),
    }
}

fn opt_str(input: &Value, key: &str) -> Result<Option<String>, ToolInputError> {
    match field(input, key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value.as_str().map(str::to_string).map(Some).ok_or_else(|| {
            ToolInputError::Malformed(format!("{key} must be a str, got {}", py_type_name(value)))
        }),
    }
}

fn req_i64(input: &Value, key: &str) -> Result<i64, ToolInputError> {
    match field(input, key) {
        Some(value) if !value.is_null() => value.as_i64().ok_or_else(|| {
            ToolInputError::Malformed(format!("{key} must be an int, got {}", py_type_name(value)))
        }),
        _ => Err(ToolInputError::MissingKey(key.to_string())),
    }
}

fn opt_i64(input: &Value, key: &str) -> Result<Option<i64>, ToolInputError> {
    match field(input, key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value.as_i64().map(Some).ok_or_else(|| {
            ToolInputError::Malformed(format!("{key} must be an int, got {}", py_type_name(value)))
        }),
    }
}

// One file's edit accumulated while walking a patch envelope; body lines group by `@@`.
struct PendingEdit {
    file_path: String,
    kind: PatchEditKind,
    move_path: Option<String>,
    groups: Vec<Vec<String>>,
}

impl PendingEdit {
    fn new(file_path: String, kind: PatchEditKind) -> PendingEdit {
        PendingEdit {
            file_path,
            kind,
            move_path: None,
            groups: vec![Vec::new()],
        }
    }

    fn push_body(&mut self, line: &str) {
        if self.kind == PatchEditKind::Update && line.starts_with("@@") {
            self.groups.push(Vec::new());
        } else {
            self.groups.last_mut().unwrap().push(line.to_string());
        }
    }

    fn finish(self) -> PatchEdit {
        let build = match self.kind {
            PatchEditKind::Delete => Vec::new(),
            PatchEditKind::Add => self.groups.iter().filter_map(|g| add_hunk(g)).collect(),
            PatchEditKind::Update => self
                .groups
                .iter()
                .filter(|g| !g.is_empty())
                .map(|g| update_hunk(g))
                .collect(),
        };
        PatchEdit {
            file_path: self.file_path,
            kind: self.kind,
            move_path: self.move_path,
            hunks: build,
        }
    }
}

// A Update hunk's before/after: context lines (space prefix) join both sides, `-` the
// old, `+` the new; a bare/blank line is shared context.
fn update_hunk(group: &[String]) -> Hunk {
    let (mut old, mut new): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    for line in group {
        match line.as_bytes().first() {
            Some(b'-') => old.push(&line[1..]),
            Some(b'+') => new.push(&line[1..]),
            Some(b' ') => {
                old.push(&line[1..]);
                new.push(&line[1..]);
            }
            _ => {
                old.push(line);
                new.push(line);
            }
        }
    }
    Hunk {
        old: old.join("\n"),
        new: new.join("\n"),
    }
}

// An Add file's one addition hunk: empty old side, `+`-stripped lines as the new side.
fn add_hunk(group: &[String]) -> Option<Hunk> {
    (!group.is_empty()).then(|| Hunk {
        old: String::new(),
        new: group
            .iter()
            .map(|l| l.strip_prefix('+').unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

fn parse_patch_envelope(text: &str) -> Result<Vec<PatchEdit>, ()> {
    let mut lines = text.lines();
    if lines.next() != Some("*** Begin Patch") {
        return Err(());
    }
    let mut edits: Vec<PatchEdit> = Vec::new();
    let mut current: Option<PendingEdit> = None;
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("*** ") else {
            match current.as_mut() {
                Some(edit) if edit.kind != PatchEditKind::Delete => edit.push_body(line),
                _ => return Err(()),
            }
            continue;
        };
        if let Some(path) = rest.strip_prefix("Move to:") {
            let path = path.trim();
            match current.as_mut() {
                Some(edit) if edit.kind == PatchEditKind::Update && !path.is_empty() => {
                    edit.move_path = Some(path.to_string());
                }
                _ => return Err(()),
            }
            continue;
        }
        if let Some(edit) = current.take() {
            edits.push(edit.finish());
        }
        match rest {
            "End Patch" => {
                if lines.any(|line| !line.is_empty()) {
                    return Err(());
                }
                return Ok(edits);
            }
            _ if rest.starts_with("Update File:") => {
                current = Some(PendingEdit::new(
                    marker_path(rest).ok_or(())?,
                    PatchEditKind::Update,
                ));
            }
            _ if rest.starts_with("Add File:") => {
                current = Some(PendingEdit::new(
                    marker_path(rest).ok_or(())?,
                    PatchEditKind::Add,
                ));
            }
            _ if rest.starts_with("Delete File:") => {
                current = Some(PendingEdit::new(
                    marker_path(rest).ok_or(())?,
                    PatchEditKind::Delete,
                ));
            }
            _ => return Err(()),
        }
    }
    Err(())
}

fn marker_path(rest: &str) -> Option<String> {
    rest.split_once(':')
        .map(|(_, p)| p.trim())
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

fn edit_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Edit(EditCall {
        name: name.to_string(),
        raw: raw.clone(),
        file_path: req_str(raw, "file_path")?,
        old: req_str(raw, "old_string")?,
        new: req_str(raw, "new_string")?,
        replace_all: get_or_false(raw, "replace_all"),
    }))
}

fn multiedit_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    let file_path = req_str(raw, "file_path")?;
    let edits = match field(raw, "edits") {
        Some(v) => edit_spans(v)?,
        None => return Err(ToolInputError::MissingKey("edits".to_string())),
    };
    Ok(ToolCall::MultiEdit(MultiEditCall {
        name: name.to_string(),
        raw: raw.clone(),
        file_path,
        edits,
    }))
}

fn write_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Write(WriteCall {
        name: name.to_string(),
        raw: raw.clone(),
        file_path: req_str(raw, "file_path")?,
        content: req_str(raw, "content")?,
    }))
}

fn read_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Read(ReadCall {
        name: name.to_string(),
        raw: raw.clone(),
        file_path: req_str(raw, "file_path")?,
        offset: opt(raw, "offset"),
        limit: opt(raw, "limit"),
    }))
}

fn notebook_edit_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::NotebookEdit(NotebookEditCall {
        name: name.to_string(),
        raw: raw.clone(),
        notebook_path: req_str(raw, "notebook_path")?,
        new_source: req_str(raw, "new_source")?,
        cell_id: opt(raw, "cell_id"),
        edit_mode: opt(raw, "edit_mode"),
    }))
}

fn grep_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Grep(GrepCall {
        name: name.to_string(),
        raw: raw.clone(),
        pattern: req_str(raw, "pattern")?,
        path: opt(raw, "path"),
        glob: opt(raw, "glob"),
        file_type: opt(raw, "type"),
        output_mode: opt(raw, "output_mode"),
    }))
}

fn glob_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Glob(GlobCall {
        name: name.to_string(),
        raw: raw.clone(),
        pattern: req_str(raw, "pattern")?,
        path: opt(raw, "path"),
    }))
}

fn task_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Task(TaskCall {
        name: name.to_string(),
        raw: raw.clone(),
        prompt: req_str(raw, "prompt")?,
        agent_type: opt_keys(raw, &["subagent_type", "agent_type"]),
        model: opt(raw, "model"),
        agent_name: opt(raw, "name"),
        run_in_background: opt(raw, "run_in_background"),
    }))
}

fn workflow_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Workflow(WorkflowCall {
        name: name.to_string(),
        raw: raw.clone(),
        script: opt(raw, "script"),
        script_path: opt_keys(raw, &["scriptPath", "script_path"]),
        workflow_name: opt(raw, "name"),
        args: opt(raw, "args"),
        resume_from_run_id: opt_keys(raw, &["resumeFromRunId", "resume_from_run_id"]),
    }))
}

fn skill_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Skill(SkillCall {
        name: name.to_string(),
        raw: raw.clone(),
        skill: req_str(raw, "skill")?,
        args: opt(raw, "args"),
    }))
}

fn task_create_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::TaskCreate(TaskCreateCall {
        name: name.to_string(),
        raw: raw.clone(),
        subject: req_str(raw, "subject")?,
        description: opt(raw, "description"),
    }))
}

fn task_update_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::TaskUpdate(TaskUpdateCall {
        name: name.to_string(),
        raw: raw.clone(),
        task_id: req_str_keys(raw, &["taskId", "task_id"])?,
        status: opt(raw, "status"),
        subject: opt(raw, "subject"),
        description: opt(raw, "description"),
    }))
}

fn exit_plan_mode_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::ExitPlanMode(ExitPlanModeCall {
        name: name.to_string(),
        raw: raw.clone(),
        plan: req_str(raw, "plan")?,
    }))
}

fn other_call(name: &str, raw: Value, error: Option<String>) -> ToolCall {
    ToolCall::Other(OtherCall {
        name: name.to_string(),
        raw,
        error,
    })
}

// A registered MCP span-edit tool lowers to SpanEditCall; else OtherCall. A
// mapped-and-truthy delete key means deletion (new=None); else content is required.
fn span_edit_or_other(name: &str, input: &Value) -> Result<ToolCall, ToolInputError> {
    let Some((_, tool)) = mcp_parts(name) else {
        return Ok(other_call(name, input.clone(), None));
    };
    let Some(map) = registered_span_edit(tool) else {
        return Ok(other_call(name, input.clone(), None));
    };
    let file_path = req_str(input, &map.path)?;
    let new = match &map.delete {
        Some(delete) if field(input, delete).is_some_and(truthy) => None,
        _ => Some(req_str(input, &map.content)?),
    };
    Ok(ToolCall::SpanEdit(SpanEditCall {
        name: name.to_string(),
        raw: input.clone(),
        file_path,
        new,
    }))
}

/// Parity: tools.py parse_tool_call, on_error='raise'.
pub fn parse_tool_call_strict(name: &str, input: &Value) -> Result<ToolCall, ToolInputError> {
    let canonical = tool_alias_reverse(name).unwrap_or(name);
    // Non-object input: a built-in requires an object (NonMapping); an untyped name with a
    // verbatim string is a codex-style call (Other); a list/scalar stays NonMapping.
    if input.as_object().is_none() && !CODEX_VERBATIM_TOOLS.contains(&name) {
        return match () {
            _ if requires_object_input(canonical) => {
                Err(ToolInputError::NonMapping(py_type_name(input)))
            }
            _ if input.as_str().is_some() => Ok(other_call(name, input.clone(), None)),
            _ => Err(ToolInputError::NonMapping(py_type_name(input))),
        };
    }
    match canonical {
        "Bash" => bash_from_raw(name, input),
        "Edit" => edit_from_raw(name, input),
        "MultiEdit" => multiedit_from_raw(name, input),
        "Write" => write_from_raw(name, input),
        "Read" => read_from_raw(name, input),
        "NotebookEdit" => notebook_edit_from_raw(name, input),
        "Grep" => grep_from_raw(name, input),
        "Glob" => glob_from_raw(name, input),
        "Agent" => task_from_raw(name, input),
        "Workflow" => workflow_from_raw(name, input),
        "Skill" => skill_from_raw(name, input),
        "TaskCreate" => task_create_from_raw(name, input),
        "TaskUpdate" => task_update_from_raw(name, input),
        "ExitPlanMode" => exit_plan_mode_from_raw(name, input),
        "exec" => code_mode_from_raw(name, input),
        "apply_patch" => apply_patch_from_raw(name, input),
        "update_plan" => update_plan_from_raw(name, input),
        "write_stdin" => write_stdin_from_raw(name, input),
        _ => span_edit_or_other(name, input),
    }
}

/// Parity: tools.py parse_tool_call, on_error='other'. A malformed call degrades to an
/// OtherCall over the original input verbatim (raw is the digest substrate).
pub fn parse_tool_call(name: &str, input: &Value) -> ToolCall {
    parse_tool_call_strict(name, input)
        .unwrap_or_else(|err| other_call(name, input.clone(), Some(err.to_string())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAnnotation {
    pub preview: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BashResult {
    pub name: String,
    pub raw: Value,
    pub stdout: Option<Value>,
    pub stderr: Option<Value>,
    pub interrupted: Value,
    pub is_image: Value,
    pub no_output_expected: Value,
    pub background_task_id: Option<Value>,
    pub return_code_interpretation: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditResult {
    pub name: String,
    pub raw: Value,
    pub file_path: Option<Value>,
    pub old_string: Option<Value>,
    pub new_string: Option<Value>,
    pub replace_all: Value,
    pub user_modified: Value,
    pub stale_recovered: Value,
    pub structured_patch: Option<Value>,
    pub original_file: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WriteResult {
    pub name: String,
    pub raw: Value,
    pub content: Option<Value>,
    pub file_path: Option<Value>,
    pub original_file: Option<Value>,
    pub structured_patch: Option<Value>,
    pub user_modified: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadResult {
    pub name: String,
    pub raw: Value,
    pub file: Option<Value>,
    pub file_type: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskResult {
    pub name: String,
    pub raw: Value,
    pub agent_id: Option<Value>,
    pub agent_type: Option<Value>,
    pub status: Option<Value>,
    pub total_duration_ms: Option<Value>,
    pub total_tokens: Option<Value>,
    pub total_tool_use_count: Option<Value>,
    pub tool_stats: Option<Value>,
    pub usage: Option<Value>,
    pub content: Option<Value>,
    pub prompt: Option<Value>,
    pub resolved_model: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskLaunchResult {
    pub name: String,
    pub raw: Value,
    pub agent_id: Option<Value>,
    pub output_file: Option<Value>,
    pub is_async: Value,
    pub can_read_output_file: Value,
    pub description: Option<Value>,
    pub prompt: Option<Value>,
    pub status: Option<Value>,
    pub resolved_model: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillResult {
    pub name: String,
    pub raw: Value,
    pub command_name: Option<Value>,
    pub success: Value,
    pub allowed_tools: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AskUserQuestionResult {
    pub name: String,
    pub raw: Value,
    pub answers: Vec<(String, String)>,
    pub annotations: Vec<(String, QuestionAnnotation)>,
}

impl AskUserQuestionResult {
    /// Parity: AskUserQuestionResult.questions — the rounds echoed in the payload.
    pub fn questions(&self) -> Vec<Question> {
        parse_questions(&self.raw).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextResult {
    pub name: String,
    pub raw: Value,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtherResult {
    pub name: String,
    pub raw: Value,
}

/// The typed tool-result hierarchy (tools.py ToolResult union).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResult {
    Bash(BashResult),
    Edit(EditResult),
    Write(WriteResult),
    Read(ReadResult),
    Task(TaskResult),
    TaskLaunch(TaskLaunchResult),
    Skill(SkillResult),
    AskUserQuestion(AskUserQuestionResult),
    Text(TextResult),
    Other(OtherResult),
}

impl ToolResult {
    /// The tool name (tools.py ToolResultBase.name).
    pub fn name(&self) -> &str {
        match self {
            ToolResult::Bash(r) => &r.name,
            ToolResult::Edit(r) => &r.name,
            ToolResult::Write(r) => &r.name,
            ToolResult::Read(r) => &r.name,
            ToolResult::Task(r) => &r.name,
            ToolResult::TaskLaunch(r) => &r.name,
            ToolResult::Skill(r) => &r.name,
            ToolResult::AskUserQuestion(r) => &r.name,
            ToolResult::Text(r) => &r.name,
            ToolResult::Other(r) => &r.name,
        }
    }

    /// The Python dataclass name for this variant.
    pub fn type_name(&self) -> &'static str {
        match self {
            ToolResult::Bash(_) => "BashResult",
            ToolResult::Edit(_) => "EditResult",
            ToolResult::Write(_) => "WriteResult",
            ToolResult::Read(_) => "ReadResult",
            ToolResult::Task(_) => "TaskResult",
            ToolResult::TaskLaunch(_) => "TaskLaunchResult",
            ToolResult::Skill(_) => "SkillResult",
            ToolResult::AskUserQuestion(_) => "AskUserQuestionResult",
            ToolResult::Text(_) => "TextResult",
            ToolResult::Other(_) => "OtherResult",
        }
    }

    /// The record-level `toolUseResult` payload (tools.py ToolResultBase.raw).
    pub fn raw(&self) -> &Value {
        match self {
            ToolResult::Bash(r) => &r.raw,
            ToolResult::Edit(r) => &r.raw,
            ToolResult::Write(r) => &r.raw,
            ToolResult::Read(r) => &r.raw,
            ToolResult::Task(r) => &r.raw,
            ToolResult::TaskLaunch(r) => &r.raw,
            ToolResult::Skill(r) => &r.raw,
            ToolResult::AskUserQuestion(r) => &r.raw,
            ToolResult::Text(r) => &r.raw,
            ToolResult::Other(r) => &r.raw,
        }
    }
}

fn bash_result(name: &str, raw: &Value) -> ToolResult {
    ToolResult::Bash(BashResult {
        name: name.to_string(),
        raw: raw.clone(),
        stdout: opt(raw, "stdout"),
        stderr: opt(raw, "stderr"),
        interrupted: get_or_false(raw, "interrupted"),
        is_image: get_or_false(raw, "isImage"),
        no_output_expected: get_or_false(raw, "noOutputExpected"),
        background_task_id: opt(raw, "backgroundTaskId"),
        return_code_interpretation: opt(raw, "returnCodeInterpretation"),
    })
}

fn edit_result(name: &str, raw: &Value) -> ToolResult {
    ToolResult::Edit(EditResult {
        name: name.to_string(),
        raw: raw.clone(),
        file_path: opt(raw, "filePath"),
        old_string: opt(raw, "oldString"),
        new_string: opt(raw, "newString"),
        replace_all: get_or_false(raw, "replaceAll"),
        user_modified: get_or_false(raw, "userModified"),
        stale_recovered: get_or_false(raw, "staleRecovered"),
        structured_patch: opt(raw, "structuredPatch"),
        original_file: opt(raw, "originalFile"),
    })
}

fn write_result(name: &str, raw: &Value) -> ToolResult {
    ToolResult::Write(WriteResult {
        name: name.to_string(),
        raw: raw.clone(),
        content: opt(raw, "content"),
        file_path: opt(raw, "filePath"),
        original_file: opt(raw, "originalFile"),
        structured_patch: opt(raw, "structuredPatch"),
        user_modified: get_or_false(raw, "userModified"),
    })
}

fn read_result(name: &str, raw: &Value) -> ToolResult {
    ToolResult::Read(ReadResult {
        name: name.to_string(),
        raw: raw.clone(),
        file: opt(raw, "file"),
        file_type: opt(raw, "type"),
    })
}

// Parity: TaskResultBase.from_raw — terminal run -> TaskResult, launch -> TaskLaunchResult.
fn task_result(name: &str, raw: &Value) -> ToolResult {
    if present(raw, "totalDurationMs") || present(raw, "usage") {
        ToolResult::Task(TaskResult {
            name: name.to_string(),
            raw: raw.clone(),
            agent_id: opt(raw, "agentId"),
            agent_type: opt(raw, "agentType"),
            status: opt(raw, "status"),
            total_duration_ms: opt(raw, "totalDurationMs"),
            total_tokens: opt(raw, "totalTokens"),
            total_tool_use_count: opt(raw, "totalToolUseCount"),
            tool_stats: opt(raw, "toolStats"),
            usage: opt(raw, "usage"),
            content: opt(raw, "content"),
            prompt: opt(raw, "prompt"),
            resolved_model: opt(raw, "resolvedModel"),
        })
    } else if present(raw, "outputFile") {
        ToolResult::TaskLaunch(TaskLaunchResult {
            name: name.to_string(),
            raw: raw.clone(),
            agent_id: opt(raw, "agentId"),
            output_file: opt(raw, "outputFile"),
            is_async: get_or_false(raw, "isAsync"),
            can_read_output_file: get_or_false(raw, "canReadOutputFile"),
            description: opt(raw, "description"),
            prompt: opt(raw, "prompt"),
            status: opt(raw, "status"),
            resolved_model: opt(raw, "resolvedModel"),
        })
    } else {
        ToolResult::Other(OtherResult {
            name: name.to_string(),
            raw: raw.clone(),
        })
    }
}

fn skill_result(name: &str, raw: &Value) -> ToolResult {
    ToolResult::Skill(SkillResult {
        name: name.to_string(),
        raw: raw.clone(),
        command_name: opt(raw, "commandName"),
        success: get_or_false(raw, "success"),
        allowed_tools: opt_array(raw, "allowedTools"),
    })
}

fn ask_user_question_result(name: &str, raw: &Value) -> ToolResult {
    let answers = field(raw, "answers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let annotations = field(raw, "annotations")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter(|(_, v)| v.as_object().is_some())
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        QuestionAnnotation {
                            preview: field_str(v, "preview").map(str::to_string),
                            notes: field_str(v, "notes").map(str::to_string),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    ToolResult::AskUserQuestion(AskUserQuestionResult {
        name: name.to_string(),
        raw: raw.clone(),
        answers,
        annotations,
    })
}

/// Parity: tools.py parse_tool_result. Total — a result lift never errors. The one owner
/// of last-wins key normalization for tool_use_result, which parse-boundary dedup skips.
pub fn parse_tool_result(name: &str, payload: &Value) -> ToolResult {
    let payload = &normalized_owned(payload);
    if let Some(text) = payload.as_str() {
        return ToolResult::Text(TextResult {
            name: name.to_string(),
            raw: payload.clone(),
            text: text.to_string(),
        });
    }
    if payload.as_object().is_none() {
        return ToolResult::Other(OtherResult {
            name: name.to_string(),
            raw: payload.clone(),
        });
    }
    match tool_alias_reverse(name).unwrap_or(name) {
        "Bash" => bash_result(name, payload),
        "Edit" => edit_result(name, payload),
        "Write" => write_result(name, payload),
        "Read" => read_result(name, payload),
        "Agent" => task_result(name, payload),
        "Skill" => skill_result(name, payload),
        "AskUserQuestion" => ask_user_question_result(name, payload),
        _ => ToolResult::Other(OtherResult {
            name: name.to_string(),
            raw: payload.clone(),
        }),
    }
}

/// Parity: tools.py mcp_parts. The single owner of the MCP-name split.
pub fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
}

/// Parity: tools.py expand_tool_names.
pub fn expand_tool_names(spec: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for name in spec.split('|') {
        let canonical = tool_alias_reverse(name).unwrap_or(name);
        set.insert(name.to_string());
        set.insert(canonical.to_string());
        set.extend(
            tool_aliases(canonical)
                .iter()
                .map(|alias| alias.to_string()),
        );
    }
    let registered: Vec<String> = MCP_REGISTRY
        .read()
        .expect("mcp registry lock")
        .iter()
        .filter(|(_, spec)| set.contains(&spec.behaves_like))
        .map(|(name, _)| name.clone())
        .collect();
    set.extend(registered);
    set
}

/// Parity: tools.py tool_name_matches — the expand_tool_names set feeds the shared
/// types::matches_names primitive, which closes over the native bare-MCP aliases.
pub fn tool_name_matches(actual: &str, spec: &str) -> bool {
    crate::types::matches_names(actual, &expand_tool_names(spec))
}

/// Parity: tools.py mcp_access.
pub fn mcp_access(tool: &str) -> &'static str {
    let lowered = tool.to_lowercase();
    if READ_VERBS.iter().any(|verb| lowered.starts_with(verb))
        || lowered.split('_').any(|t| READ_VERBS.contains(&t))
    {
        "read"
    } else {
        "write"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(json: &str) -> Value {
        sonic_rs::from_str(json).unwrap()
    }

    #[test]
    fn bash_preserves_loose_values() {
        match parse_tool_call(
            "Bash",
            &obj(r#"{"command":"ls","timeout":1.5,"description":5}"#),
        ) {
            ToolCall::Bash(c) => {
                assert_eq!(c.timeout, Some(obj("1.5")));
                assert_eq!(c.description, Some(obj("5")));
                assert_eq!(c.run_in_background, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn edit_replace_all_no_coercion() {
        match parse_tool_call(
            "Edit",
            &obj(r#"{"file_path":"a","old_string":"x","new_string":"y","replace_all":1}"#),
        ) {
            ToolCall::Edit(c) => assert_eq!(c.replace_all, obj("1")),
            other => panic!("{other:?}"),
        }
        match parse_tool_call(
            "Edit",
            &obj(r#"{"file_path":"a","old_string":"x","new_string":"y"}"#),
        ) {
            ToolCall::Edit(c) => assert_eq!(c.replace_all, obj("false")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn key_of_first_non_null_wins() {
        match parse_tool_call(
            "Agent",
            &obj(r#"{"prompt":"p","subagent_type":5,"agent_type":"Explore"}"#),
        ) {
            ToolCall::Task(c) => assert_eq!(c.agent_type, Some(obj("5"))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn multiedit_empty_iterables_yield_empty_edits() {
        for edits in [r#""""#, "{}", "[]"] {
            let input = obj(&format!(r#"{{"file_path":"a","edits":{edits}}}"#));
            match parse_tool_call("MultiEdit", &input) {
                ToolCall::MultiEdit(c) => assert!(c.edits.is_empty(), "edits={edits}"),
                other => panic!("edits={edits}: {other:?}"),
            }
        }
        for edits in [r#""ab""#, r#"{"k":"v"}"#, "5", "null"] {
            let input = obj(&format!(r#"{{"file_path":"a","edits":{edits}}}"#));
            assert!(
                matches!(parse_tool_call("MultiEdit", &input), ToolCall::Other(_)),
                "edits={edits}"
            );
        }
    }

    #[test]
    fn strict_error_variants() {
        assert_eq!(
            parse_tool_call_strict("Bash", &obj("5")),
            Err(ToolInputError::NonMapping("int"))
        );
        assert_eq!(
            parse_tool_call_strict("Bash", &obj("{}")),
            Err(ToolInputError::MissingKey("command".to_string()))
        );
        assert_eq!(
            parse_tool_call_strict("Bash", &obj(r#"{"command":5}"#)),
            Err(ToolInputError::WrongType {
                key: "command".to_string(),
                py_type: "int"
            })
        );
    }

    #[test]
    fn ask_user_question_derives_questions() {
        let payload = obj(
            r#"{"questions":[{"question":"Q1","options":[{"label":"A"},{"x":1}]},{"noq":1}],"answers":{"Q1":"A","Q2":5}}"#,
        );
        match parse_tool_result("AskUserQuestion", &payload) {
            ToolResult::AskUserQuestion(r) => {
                assert_eq!(r.answers, vec![("Q1".to_string(), "A".to_string())]);
                let questions = r.questions();
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].labels, vec!["A".to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn name_helpers() {
        assert_eq!(mcp_parts("mcp__semble__search"), Some(("semble", "search")));
        assert!(tool_name_matches("Execute", "Bash|Grep"));
        assert_eq!(mcp_access("ccx_read"), "read");
        assert_eq!(mcp_access("deploy"), "write");

        register_mcp_tool(
            "syn_helper_edit".to_string(),
            McpToolSpec {
                behaves_like: "Edit".to_string(),
                span_edit: None,
            },
        );
        assert_eq!(mcp_tool_alias("syn_helper_edit"), Some("Edit".to_string()));
        assert!(tool_name_matches(
            "mcp__cc-context__syn_helper_edit",
            "Edit"
        ));
        assert!(unregister_mcp_tool("syn_helper_edit"));
        assert_eq!(mcp_tool_alias("syn_helper_edit"), None);
        assert!(!tool_name_matches(
            "mcp__cc-context__syn_helper_edit",
            "Edit"
        ));
    }

    #[test]
    fn registered_span_edit_lowers_from_mapped_keys() {
        register_mcp_tool(
            "syn_lower_edit".to_string(),
            McpToolSpec {
                behaves_like: "Edit".to_string(),
                span_edit: Some(SpanEditMap {
                    path: "path".to_string(),
                    content: "content".to_string(),
                    delete: Some("delete".to_string()),
                }),
            },
        );
        match parse_tool_call(
            "mcp__cc-context__syn_lower_edit",
            &obj(r#"{"path":"/x.py","content":"body"}"#),
        ) {
            ToolCall::SpanEdit(c) => {
                assert_eq!(c.file_path, "/x.py");
                assert_eq!(c.new, Some("body".to_string()));
            }
            other => panic!("{other:?}"),
        }
        match parse_tool_call(
            "mcp__cc-context__syn_lower_edit",
            &obj(r#"{"path":"/x.py","content":"body","delete":true}"#),
        ) {
            ToolCall::SpanEdit(c) => assert_eq!(c.new, None),
            other => panic!("{other:?}"),
        }
        // Missing path degrades to OtherCall; content required when not deleting.
        assert!(matches!(
            parse_tool_call(
                "mcp__cc-context__syn_lower_edit",
                &obj(r#"{"content":"body"}"#)
            ),
            ToolCall::Other(_)
        ));
        assert!(matches!(
            parse_tool_call(
                "mcp__cc-context__syn_lower_edit",
                &obj(r#"{"path":"/x.py"}"#)
            ),
            ToolCall::Other(_)
        ));
        assert!(unregister_mcp_tool("syn_lower_edit"));
    }

    #[test]
    fn registered_behaves_like_gates_but_lowers_to_other() {
        register_mcp_tool(
            "syn_gate_only".to_string(),
            McpToolSpec {
                behaves_like: "Write".to_string(),
                span_edit: None,
            },
        );
        assert!(expand_tool_names("Write").contains("syn_gate_only"));
        assert!(matches!(
            parse_tool_call("mcp__cc-context__syn_gate_only", &obj(r#"{"path":"/x"}"#)),
            ToolCall::Other(_)
        ));
        assert!(unregister_mcp_tool("syn_gate_only"));
        assert!(!expand_tool_names("Write").contains("syn_gate_only"));
    }

    #[test]
    fn unregistered_mcp_name_neither_matches_nor_lowers() {
        assert!(!tool_name_matches(
            "mcp__cc-context__syn_unknown",
            "Edit|Write"
        ));
        assert!(matches!(
            parse_tool_call("mcp__cc-context__syn_unknown", &obj(r#"{"path":"/x"}"#)),
            ToolCall::Other(_)
        ));
    }

    fn str_val(s: &str) -> Value {
        Value::from(s)
    }

    #[test]
    fn exec_command_string_parses_as_bash() {
        let raw = str_val(r#"{"cmd":"ls /tmp","workdir":"/tmp"}"#);
        match parse_tool_call("exec_command", &raw) {
            ToolCall::Bash(c) => {
                assert_eq!(c.command, "ls /tmp");
                assert_eq!(c.name, "exec_command");
                assert_eq!(
                    c.raw.as_str(),
                    Some(r#"{"cmd":"ls /tmp","workdir":"/tmp"}"#)
                );
                assert_eq!(c.timeout, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exec_command_reverse_aliases_to_bash_and_expands() {
        let set = expand_tool_names("Bash");
        assert!(set.contains("Bash") && set.contains("Execute") && set.contains("exec_command"));
        assert!(tool_name_matches("exec_command", "Bash"));
        assert_eq!(
            expand_tool_names("Execute"),
            std::collections::HashSet::from([
                "Bash".to_string(),
                "Execute".to_string(),
                "exec_command".to_string(),
            ])
        );
        assert!(tool_name_matches("exec_command", "Execute"));
    }

    #[test]
    fn exec_string_parses_as_code_mode() {
        let src = "python3 -c 'print(1)'";
        match parse_tool_call("exec", &str_val(src)) {
            ToolCall::CodeMode(c) => {
                assert_eq!(c.source, src);
                assert_eq!(c.raw.as_str(), Some(src));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn apply_patch_multi_file_envelope_lowers_every_edit() {
        let envelope = concat!(
            "*** Begin Patch\n",
            "*** Update File: src/a.py\n",
            "*** Move to: src/b.py\n",
            "@@ def f():\n",
            " ctx\n",
            "-old\n",
            "+new\n",
            "*** Add File: src/c.py\n",
            "+line1\n",
            "+line2\n",
            "*** Delete File: src/d.py\n",
            "*** End Patch\n",
        );
        match parse_tool_call("apply_patch", &str_val(envelope)) {
            ToolCall::ApplyPatch(c) => {
                assert_eq!(c.edits.len(), 3);
                assert_eq!(c.edits[0].file_path, "src/a.py");
                assert_eq!(c.edits[0].kind, PatchEditKind::Update);
                assert_eq!(c.edits[0].move_path.as_deref(), Some("src/b.py"));
                assert_eq!(
                    c.edits[0].hunks,
                    vec![Hunk {
                        old: "ctx\nold".into(),
                        new: "ctx\nnew".into()
                    }]
                );
                assert_eq!(c.edits[1].file_path, "src/c.py");
                assert_eq!(c.edits[1].kind, PatchEditKind::Add);
                assert_eq!(
                    c.edits[1].hunks,
                    vec![Hunk {
                        old: String::new(),
                        new: "line1\nline2".into()
                    }]
                );
                assert_eq!(c.edits[2].file_path, "src/d.py");
                assert_eq!(c.edits[2].kind, PatchEditKind::Delete);
                assert!(c.edits[2].hunks.is_empty());
                assert_eq!(c.raw.as_str(), Some(envelope));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn apply_patch_malformed_envelope_yields_no_edits_with_raw() {
        let junk = "not a patch at all";
        match parse_tool_call("apply_patch", &str_val(junk)) {
            ToolCall::ApplyPatch(c) => {
                assert!(c.edits.is_empty());
                assert_eq!(c.raw.as_str(), Some(junk));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn apply_patch_rejects_partial_malformed_envelopes() {
        for envelope in [
            concat!(
                "*** Begin Patch\n",
                "*** Update File: src/a.py\n",
                "@@\n",
                "-old\n",
                "+new\n",
                "*** Bogus Directive\n",
                "*** End Patch\n",
            ),
            concat!(
                "*** Begin Patch\n",
                "*** Update File: src/a.py\n",
                "@@\n",
                "-old\n",
                "+new\n",
            ),
        ] {
            match parse_tool_call("apply_patch", &str_val(envelope)) {
                ToolCall::ApplyPatch(c) => {
                    assert!(c.edits.is_empty());
                    assert_eq!(c.raw.as_str(), Some(envelope));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn apply_patch_file_paths_and_edits_cover_every_file() {
        let envelope = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.py\n",
            "@@\n",
            "-x\n",
            "+y\n",
            "*** Delete File: gone.py\n",
            "*** End Patch\n",
        );
        let call = parse_tool_call("apply_patch", &str_val(envelope));
        assert_eq!(call.file_paths(), vec!["a.py", "gone.py"]);
        let edits = call.edits();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].0, "a.py");
        assert_eq!(edits[1].0, "gone.py");
        assert!(edits[1].1.is_empty());
        assert_eq!(call.file_path(), None);
        assert!(call.hunks().is_empty());
    }

    #[test]
    fn update_plan_and_write_stdin_decode_arguments() {
        match parse_tool_call(
            "update_plan",
            &str_val(r#"{"plan":[{"step":"a","status":"pending"}],"explanation":"why"}"#),
        ) {
            ToolCall::UpdatePlan(c) => {
                assert!(c.plan.as_ref().and_then(|v| v.as_array()).is_some());
                assert_eq!(c.explanation.as_deref(), Some("why"));
            }
            other => panic!("{other:?}"),
        }
        match parse_tool_call(
            "write_stdin",
            &str_val(
                r#"{"chars":"y\n","session_id":42,"yield_time_ms":1000,"max_output_tokens":2000}"#,
            ),
        ) {
            ToolCall::WriteStdin(c) => {
                assert_eq!(c.chars.as_ref().and_then(|v| v.as_str()), Some("y\n"));
                assert_eq!(c.session_id, 42);
                assert_eq!(c.yield_time_ms, Some(1000));
                assert_eq!(c.max_output_tokens, Some(2000));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typed_codex_calls_reject_wrong_input_types() {
        for name in ["apply_patch", "exec"] {
            for input in [obj("[]"), obj("{}"), obj("null"), obj("7")] {
                assert_eq!(
                    parse_tool_call_strict(name, &input),
                    Err(ToolInputError::NonMapping(py_type_name(&input)))
                );
            }
        }
        assert_eq!(
            parse_tool_call_strict("exec_command", &obj("[]")),
            Err(ToolInputError::NonMapping("list"))
        );
    }

    #[test]
    fn typed_codex_argument_fields_are_strict() {
        assert!(matches!(
            parse_tool_call_strict("update_plan", &str_val(r#"{"plan":{}}"#)),
            Err(ToolInputError::Malformed(detail)) if detail.contains("plan must be a list")
        ));
        assert!(matches!(
            parse_tool_call_strict("update_plan", &str_val(r#"{"explanation":7}"#)),
            Err(ToolInputError::Malformed(detail)) if detail.contains("explanation must be a str")
        ));
        for raw in [
            r#"{"chars":"y\n"}"#,
            r#"{"chars":"y\n","session_id":"42"}"#,
            r#"{"chars":"y\n","session_id":42,"yield_time_ms":"1000"}"#,
            r#"{"chars":"y\n","session_id":42,"max_output_tokens":"2000"}"#,
        ] {
            assert!(parse_tool_call_strict("write_stdin", &str_val(raw)).is_err());
        }
    }

    #[test]
    fn non_codex_name_with_string_input_raises_non_mapping() {
        assert_eq!(
            parse_tool_call_strict("Read", &str_val("/etc/hosts")),
            Err(ToolInputError::NonMapping("str"))
        );
        match parse_tool_call("Read", &str_val("/etc/hosts")) {
            ToolCall::Other(c) => {
                assert_eq!(c.raw.as_str(), Some("/etc/hosts"));
                assert!(c.error.as_deref().unwrap().contains("must be a mapping"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn untyped_codex_string_call_degrades_to_other_without_error() {
        match parse_tool_call("send_message", &str_val(r#"{"message":"hi"}"#)) {
            ToolCall::Other(c) => {
                assert!(c.error.is_none());
                assert_eq!(c.raw.as_str(), Some(r#"{"message":"hi"}"#));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lenient_non_mapping_preserves_raw_value() {
        match parse_tool_call("Edit", &obj("[1,2,3]")) {
            ToolCall::Other(c) => {
                assert_eq!(c.raw, obj("[1,2,3]"));
                assert!(c.error.as_deref().unwrap().contains("must be a mapping"));
            }
            other => panic!("{other:?}"),
        }
    }
}
