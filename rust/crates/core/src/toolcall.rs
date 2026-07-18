//! The typed tool-call hierarchy, ported from `cc_transcript/tools.py`.
//!
//! Parity: `tools.py`. Required fields are validated to strings (parse fails
//! otherwise); every other field mirrors Python's untyped `dict.get`, storing the
//! value verbatim (any JSON type) with absent and JSON null both folding to None.
//! `raw` is retained on every variant (Python keeps it: rendering, raw-input
//! queries, and AskUserQuestionResult.questions all read it); it is `compare=False`
//! Python-side, and the content digest lives in `ids`, not here.

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::parse::parse_questions;
use crate::types::Question;
use crate::value::{field, field_str, normalized_owned};

// Parity: tools.py TOOL_ALIASES (name -> harness display alias).
fn tool_alias(name: &str) -> Option<&'static str> {
    match name {
        "Bash" => Some("Execute"),
        "Write" => Some("Create"),
        "Agent" => Some("Task"),
        "WebFetch" => Some("FetchUrl"),
        "ExitPlanMode" => Some("ExitSpecMode"),
        _ => None,
    }
}

// Parity: tools.py TOOL_ALIASES_REVERSE (alias -> canonical name).
fn tool_alias_reverse(name: &str) -> Option<&'static str> {
    match name {
        "Execute" => Some("Bash"),
        "Create" => Some("Write"),
        "Task" => Some("Agent"),
        "FetchUrl" => Some("WebFetch"),
        "ExitSpecMode" => Some("ExitPlanMode"),
        _ => None,
    }
}

/// Bare MCP write-tool aliases used by built-in edit-gate matching.
pub const MCP_TOOL_ALIASES: [(&str, &str); 2] =
    [("ccx_code_edit", "Edit"), ("ccx_code_replace", "Write")];

/// Resolves a bare MCP write tool to its built-in edit gate.
pub fn mcp_tool_alias(tool: &str) -> Option<&'static str> {
    MCP_TOOL_ALIASES
        .iter()
        .find_map(|(bare, builtin)| (*bare == tool).then_some(*builtin))
}

// Parity: tools.py READ_VERBS.
const READ_VERBS: [&str; 10] = [
    "get", "list", "search", "read", "view", "fetch", "query", "describe", "show", "find",
];

fn json_false() -> Value {
    sonic_rs::from_str("false").expect("literal false parses")
}

fn empty_object() -> Value {
    sonic_rs::from_str("{}").expect("literal empty object parses")
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

#[derive(Debug, Clone, PartialEq)]
pub struct OtherCall {
    pub name: String,
    pub raw: Value,
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
            _ => None,
        }
    }
}

fn bash_from_raw(name: &str, raw: &Value) -> Result<ToolCall, ToolInputError> {
    Ok(ToolCall::Bash(BashCall {
        name: name.to_string(),
        raw: raw.clone(),
        command: req_str(raw, "command")?,
        timeout: opt(raw, "timeout"),
        description: opt(raw, "description"),
        run_in_background: opt(raw, "run_in_background"),
    }))
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

fn other_call(name: &str, raw: Value) -> ToolCall {
    ToolCall::Other(OtherCall {
        name: name.to_string(),
        raw,
    })
}

/// Parity: tools.py parse_tool_call, on_error='raise'.
pub fn parse_tool_call_strict(name: &str, input: &Value) -> Result<ToolCall, ToolInputError> {
    if input.as_object().is_none() {
        return Err(ToolInputError::NonMapping(py_type_name(input)));
    }
    match tool_alias_reverse(name).unwrap_or(name) {
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
        _ => Ok(other_call(name, input.clone())),
    }
}

/// Parity: tools.py parse_tool_call, on_error='other'. A non-mapping input degrades to
/// OtherCall over an empty mapping; a malformed known tool to OtherCall over the input.
pub fn parse_tool_call(name: &str, input: &Value) -> ToolCall {
    if input.as_object().is_none() {
        return other_call(name, empty_object());
    }
    parse_tool_call_strict(name, input).unwrap_or_else(|_| other_call(name, input.clone()))
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
    let mut set: std::collections::HashSet<String> = spec.split('|').map(str::to_string).collect();
    let aliases: Vec<String> = set
        .iter()
        .flat_map(|n| [tool_alias(n), tool_alias_reverse(n)])
        .flatten()
        .map(str::to_string)
        .collect();
    set.extend(aliases);
    let bares: Vec<String> = MCP_TOOL_ALIASES
        .iter()
        .filter(|(_, builtin)| set.contains(*builtin))
        .map(|(bare, _)| (*bare).to_string())
        .collect();
    set.extend(bares);
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
        assert!(tool_name_matches("mcp__cc-context__ccx_code_edit", "Edit"));
        assert_eq!(mcp_tool_alias("ccx_code_edit"), Some("Edit"));
        assert_eq!(mcp_access("ccx_read"), "read");
        assert_eq!(mcp_access("deploy"), "write");
    }
}
