//! pyo3 exposure of the core typed tool-call hierarchy: parse a tool name + raw
//! input (or result payload) into a projected dict for the Python parity suite.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use sonic_rs::Value;

use crate::event::json_to_py;
use cc_transcript_core::toolcall::{
    self, EditSpan, QuestionAnnotation, ToolCall, ToolInputError, ToolResult,
};
use cc_transcript_core::types::Question;
use cc_transcript_core::value::normalize_last_wins;

fn tagged<'py>(py: Python<'py>, cls: &str) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("cls", cls)?;
    Ok(d)
}

fn some_json<'py>(py: Python<'py>, value: &Option<Value>) -> PyResult<Bound<'py, PyAny>> {
    match value {
        Some(v) => json_to_py(py, v),
        None => Ok(py.None().into_bound(py)),
    }
}

fn edit_span<'py>(py: Python<'py>, span: &EditSpan) -> PyResult<Bound<'py, PyDict>> {
    let d = tagged(py, "EditSpan")?;
    d.set_item("old", &span.old)?;
    d.set_item("new", &span.new)?;
    d.set_item("replace_all", span.replace_all)?;
    Ok(d)
}

fn question_dict<'py>(py: Python<'py>, q: &Question) -> PyResult<Bound<'py, PyDict>> {
    let d = tagged(py, "Question")?;
    d.set_item("question", &q.question)?;
    d.set_item("header", q.header.as_deref())?;
    d.set_item("multi_select", q.multi_select)?;
    d.set_item("labels", q.labels.as_slice())?;
    Ok(d)
}

fn annotation_dict<'py>(py: Python<'py>, a: &QuestionAnnotation) -> PyResult<Bound<'py, PyDict>> {
    let d = tagged(py, "QuestionAnnotation")?;
    d.set_item("preview", a.preview.as_deref())?;
    d.set_item("notes", a.notes.as_deref())?;
    Ok(d)
}

fn call_to_dict<'py>(py: Python<'py>, call: &ToolCall) -> PyResult<Bound<'py, PyDict>> {
    let d = tagged(py, call.type_name())?;
    d.set_item("raw", json_to_py(py, call.raw())?)?;
    match call {
        ToolCall::Bash(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("command", &c.command)?;
            d.set_item("timeout", some_json(py, &c.timeout)?)?;
            d.set_item("description", some_json(py, &c.description)?)?;
            d.set_item("run_in_background", some_json(py, &c.run_in_background)?)?;
        }
        ToolCall::Edit(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("file_path", &c.file_path)?;
            d.set_item("old", &c.old)?;
            d.set_item("new", &c.new)?;
            d.set_item("replace_all", json_to_py(py, &c.replace_all)?)?;
        }
        ToolCall::MultiEdit(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("file_path", &c.file_path)?;
            let edits = PyList::empty(py);
            for span in &c.edits {
                edits.append(edit_span(py, span)?)?;
            }
            d.set_item("edits", edits)?;
        }
        ToolCall::Write(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("file_path", &c.file_path)?;
            d.set_item("content", &c.content)?;
        }
        ToolCall::Read(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("file_path", &c.file_path)?;
            d.set_item("offset", some_json(py, &c.offset)?)?;
            d.set_item("limit", some_json(py, &c.limit)?)?;
        }
        ToolCall::NotebookEdit(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("notebook_path", &c.notebook_path)?;
            d.set_item("new_source", &c.new_source)?;
            d.set_item("cell_id", some_json(py, &c.cell_id)?)?;
            d.set_item("edit_mode", some_json(py, &c.edit_mode)?)?;
        }
        ToolCall::Grep(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("pattern", &c.pattern)?;
            d.set_item("path", some_json(py, &c.path)?)?;
            d.set_item("glob", some_json(py, &c.glob)?)?;
            d.set_item("file_type", some_json(py, &c.file_type)?)?;
            d.set_item("output_mode", some_json(py, &c.output_mode)?)?;
        }
        ToolCall::Glob(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("pattern", &c.pattern)?;
            d.set_item("path", some_json(py, &c.path)?)?;
        }
        ToolCall::Task(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("prompt", &c.prompt)?;
            d.set_item("agent_type", some_json(py, &c.agent_type)?)?;
            d.set_item("model", some_json(py, &c.model)?)?;
            d.set_item("agent_name", some_json(py, &c.agent_name)?)?;
            d.set_item("run_in_background", some_json(py, &c.run_in_background)?)?;
        }
        ToolCall::Workflow(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("script", some_json(py, &c.script)?)?;
            d.set_item("script_path", some_json(py, &c.script_path)?)?;
            d.set_item("workflow_name", some_json(py, &c.workflow_name)?)?;
            d.set_item("args", some_json(py, &c.args)?)?;
            d.set_item("resume_from_run_id", some_json(py, &c.resume_from_run_id)?)?;
        }
        ToolCall::Skill(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("skill", &c.skill)?;
            d.set_item("args", some_json(py, &c.args)?)?;
        }
        ToolCall::TaskCreate(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("subject", &c.subject)?;
            d.set_item("description", some_json(py, &c.description)?)?;
        }
        ToolCall::TaskUpdate(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("task_id", &c.task_id)?;
            d.set_item("status", some_json(py, &c.status)?)?;
            d.set_item("subject", some_json(py, &c.subject)?)?;
            d.set_item("description", some_json(py, &c.description)?)?;
        }
        ToolCall::ExitPlanMode(c) => {
            d.set_item("name", &c.name)?;
            d.set_item("plan", &c.plan)?;
        }
        ToolCall::Other(c) => {
            d.set_item("name", &c.name)?;
        }
    }
    Ok(d)
}

fn result_to_dict<'py>(py: Python<'py>, result: &ToolResult) -> PyResult<Bound<'py, PyDict>> {
    let d = tagged(py, result.type_name())?;
    d.set_item("raw", json_to_py(py, result.raw())?)?;
    match result {
        ToolResult::Bash(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("stdout", some_json(py, &r.stdout)?)?;
            d.set_item("stderr", some_json(py, &r.stderr)?)?;
            d.set_item("interrupted", json_to_py(py, &r.interrupted)?)?;
            d.set_item("is_image", json_to_py(py, &r.is_image)?)?;
            d.set_item("no_output_expected", json_to_py(py, &r.no_output_expected)?)?;
            d.set_item("background_task_id", some_json(py, &r.background_task_id)?)?;
            d.set_item(
                "return_code_interpretation",
                some_json(py, &r.return_code_interpretation)?,
            )?;
        }
        ToolResult::Edit(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("file_path", some_json(py, &r.file_path)?)?;
            d.set_item("old_string", some_json(py, &r.old_string)?)?;
            d.set_item("new_string", some_json(py, &r.new_string)?)?;
            d.set_item("replace_all", json_to_py(py, &r.replace_all)?)?;
            d.set_item("user_modified", json_to_py(py, &r.user_modified)?)?;
            d.set_item("stale_recovered", json_to_py(py, &r.stale_recovered)?)?;
            d.set_item("structured_patch", some_json(py, &r.structured_patch)?)?;
            d.set_item("original_file", some_json(py, &r.original_file)?)?;
        }
        ToolResult::Write(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("content", some_json(py, &r.content)?)?;
            d.set_item("file_path", some_json(py, &r.file_path)?)?;
            d.set_item("original_file", some_json(py, &r.original_file)?)?;
            d.set_item("structured_patch", some_json(py, &r.structured_patch)?)?;
            d.set_item("user_modified", json_to_py(py, &r.user_modified)?)?;
        }
        ToolResult::Read(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("file", some_json(py, &r.file)?)?;
            d.set_item("type", some_json(py, &r.file_type)?)?;
        }
        ToolResult::Task(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("agent_id", some_json(py, &r.agent_id)?)?;
            d.set_item("agent_type", some_json(py, &r.agent_type)?)?;
            d.set_item("status", some_json(py, &r.status)?)?;
            d.set_item("total_duration_ms", some_json(py, &r.total_duration_ms)?)?;
            d.set_item("total_tokens", some_json(py, &r.total_tokens)?)?;
            d.set_item(
                "total_tool_use_count",
                some_json(py, &r.total_tool_use_count)?,
            )?;
            d.set_item("tool_stats", some_json(py, &r.tool_stats)?)?;
            d.set_item("usage", some_json(py, &r.usage)?)?;
            d.set_item("content", some_json(py, &r.content)?)?;
            d.set_item("prompt", some_json(py, &r.prompt)?)?;
            d.set_item("resolved_model", some_json(py, &r.resolved_model)?)?;
        }
        ToolResult::TaskLaunch(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("agent_id", some_json(py, &r.agent_id)?)?;
            d.set_item("output_file", some_json(py, &r.output_file)?)?;
            d.set_item("is_async", json_to_py(py, &r.is_async)?)?;
            d.set_item(
                "can_read_output_file",
                json_to_py(py, &r.can_read_output_file)?,
            )?;
            d.set_item("description", some_json(py, &r.description)?)?;
            d.set_item("prompt", some_json(py, &r.prompt)?)?;
            d.set_item("status", some_json(py, &r.status)?)?;
            d.set_item("resolved_model", some_json(py, &r.resolved_model)?)?;
        }
        ToolResult::Skill(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("command_name", some_json(py, &r.command_name)?)?;
            d.set_item("success", json_to_py(py, &r.success)?)?;
            d.set_item("allowed_tools", some_json(py, &r.allowed_tools)?)?;
        }
        ToolResult::AskUserQuestion(r) => {
            d.set_item("name", &r.name)?;
            let answers = PyDict::new(py);
            for (k, v) in &r.answers {
                answers.set_item(k, v)?;
            }
            d.set_item("answers", answers)?;
            let annotations = PyDict::new(py);
            for (k, a) in &r.annotations {
                annotations.set_item(k, annotation_dict(py, a)?)?;
            }
            d.set_item("annotations", annotations)?;
            let questions = PyList::empty(py);
            for q in &r.questions() {
                questions.append(question_dict(py, q)?)?;
            }
            d.set_item("questions", questions)?;
        }
        ToolResult::Text(r) => {
            d.set_item("name", &r.name)?;
            d.set_item("text", &r.text)?;
        }
        ToolResult::Other(r) => {
            d.set_item("name", &r.name)?;
        }
    }
    Ok(d)
}

// Raise cc_transcript.tools.ToolInputError with Python's exact message.
fn tool_input_error(py: Python<'_>, name: &str, err: &ToolInputError) -> PyErr {
    let msg = match err {
        ToolInputError::NonMapping(t) => format!("{name} input must be a mapping, got {t}"),
        ToolInputError::MissingKey(k) => format!("{name} input missing or malformed: '{k}'"),
        ToolInputError::WrongType { key, py_type } => {
            format!("{name} input missing or malformed: {key} must be a str, got {py_type}")
        }
        ToolInputError::Malformed(detail) => format!("{name} input missing or malformed: {detail}"),
    };
    match py
        .import("cc_transcript.tools")
        .and_then(|m| m.getattr("ToolInputError"))
        .and_then(|cls| cls.call1((msg,)))
    {
        Ok(exc) => PyErr::from_value(exc),
        Err(e) => e,
    }
}

#[pyfunction]
#[pyo3(signature = (name, input_json, on_error=None))]
pub fn toolcall_parse<'py>(
    py: Python<'py>,
    name: &str,
    input_json: &str,
    on_error: Option<&str>,
) -> PyResult<Bound<'py, PyDict>> {
    let mut input: Value = sonic_rs::from_str(input_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    // parse_tool_call stays pure over an already-deduped input (parse_entry normalizes the
    // transcript path); this standalone gateway bypasses it, so normalize here.
    normalize_last_wins(&mut input);
    let call = match on_error.unwrap_or("raise") {
        "raise" => toolcall::parse_tool_call_strict(name, &input)
            .map_err(|e| tool_input_error(py, name, &e))?,
        _ => toolcall::parse_tool_call(name, &input),
    };
    call_to_dict(py, &call)
}

#[pyfunction]
pub fn toolresult_parse<'py>(
    py: Python<'py>,
    name: &str,
    payload_json: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let payload: Value = sonic_rs::from_str(payload_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    result_to_dict(py, &toolcall::parse_tool_result(name, &payload))
}
