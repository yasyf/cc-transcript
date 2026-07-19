//! pyo3 exposure of the codex session surface: discovery, resolution, and the
//! per-rollout session-info fold the Python `cc_transcript.codex` facade
//! rehydrates. Rollout paths cross as PathBuf, matching the CC discovery facade.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use cc_transcript_core::activity::PendingItem;
use cc_transcript_core::codex::{
    self, lower, parse_codex_bytes, session_probe, CodexItem, CodexSession, CodexUsageAggregate,
    Lifecycle,
};

struct CodexInfo {
    rollout_thread_id: Option<String>,
    session_id: Option<String>,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
    cwd: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
    model_provider: Option<String>,
    lifecycle: Lifecycle,
    pending: Vec<PendingItem>,
    last_event_epoch: Option<i64>,
    usage: CodexUsageAggregate,
}

fn session_meta_provider(session: &CodexSession) -> Option<String> {
    session
        .entries
        .iter()
        .find_map(|entry| match &entry.item {
            CodexItem::SessionMeta(m) if m.id.as_deref().is_some_and(|id| !id.is_empty()) => {
                Some(m)
            }
            _ => None,
        })
        .and_then(|m| m.model_provider.clone())
}

fn read_info(path: &str) -> std::io::Result<CodexInfo> {
    let session = parse_codex_bytes(&std::fs::read(path)?);
    let probe = session_probe(&session);
    let usage = lower(&session).usage;
    let model_provider = session_meta_provider(&session);
    Ok(CodexInfo {
        rollout_thread_id: session.rollout_thread_id,
        session_id: session.session_id,
        parent_thread_id: session.parent_thread_id,
        forked_from_id: session.forked_from_id,
        cwd: session.cwd,
        originator: session.originator,
        cli_version: session.cli_version,
        model_provider,
        lifecycle: probe.lifecycle,
        pending: probe.pending,
        last_event_epoch: probe.last_event_epoch,
        usage,
    })
}

fn identity_dict<'py>(py: Python<'py>, info: &CodexInfo) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("rollout_thread_id", info.rollout_thread_id.as_deref())?;
    dict.set_item("session_id", info.session_id.as_deref())?;
    dict.set_item("parent_thread_id", info.parent_thread_id.as_deref())?;
    dict.set_item("forked_from_id", info.forked_from_id.as_deref())?;
    dict.set_item("cwd", info.cwd.as_deref())?;
    dict.set_item("originator", info.originator.as_deref())?;
    dict.set_item("cli_version", info.cli_version.as_deref())?;
    dict.set_item("model_provider", info.model_provider.as_deref())?;
    Ok(dict)
}

fn pending_list<'py>(py: Python<'py>, pending: &[PendingItem]) -> PyResult<Bound<'py, PyList>> {
    PyList::new(
        py,
        pending
            .iter()
            .map(|item| {
                let entry = PyDict::new(py);
                entry.set_item("tool_use_id", item.tool_use_id.as_deref())?;
                entry.set_item("name", &item.name)?;
                entry.set_item("kind", item.kind.as_str())?;
                Ok(entry)
            })
            .collect::<PyResult<Vec<_>>>()?,
    )
}

fn usage_dict<'py>(
    py: Python<'py>,
    usage: &CodexUsageAggregate,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    if usage.token_count_events == 0 {
        return Ok(None);
    }
    let dict = PyDict::new(py);
    dict.set_item("input_tokens", usage.input_tokens)?;
    dict.set_item("cached_input_tokens", usage.cached_input_tokens)?;
    dict.set_item("output_tokens", usage.output_tokens)?;
    dict.set_item("reasoning_output_tokens", usage.reasoning_output_tokens)?;
    dict.set_item("total_tokens", usage.total_tokens)?;
    dict.set_item("model_context_window", usage.model_context_window)?;
    dict.set_item("token_count_events", usage.token_count_events)?;
    Ok(Some(dict))
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "dict[str, typing.Any]", imports = ("typing",)))]
pub fn codex_session_info<'py>(py: Python<'py>, path: String) -> PyResult<Bound<'py, PyDict>> {
    let info = py.detach(|| read_info(&path))?;
    let out = PyDict::new(py);
    out.set_item("identity", identity_dict(py, &info)?)?;
    let lifecycle = PyDict::new(py);
    lifecycle.set_item("state", info.lifecycle.as_str())?;
    lifecycle.set_item("turn_id", info.lifecycle.turn_id())?;
    out.set_item("lifecycle", lifecycle)?;
    out.set_item("pending", pending_list(py, &info.pending)?)?;
    out.set_item("last_event_epoch", info.last_event_epoch)?;
    out.set_item("usage", usage_dict(py, &info.usage)?)?;
    Ok(out)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (root=None))]
pub fn codex_discover(root: Option<PathBuf>) -> Vec<(PathBuf, String, bool)> {
    codex::discover(&codex::sessions_root(root.as_deref()))
        .into_iter()
        .map(|rollout| (rollout.path, rollout.session_id, rollout.compressed))
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (session_id, root=None))]
pub fn codex_children_of(session_id: &str, root: Option<String>) -> Vec<(PathBuf, String, bool)> {
    let root = codex::sessions_root(root.as_deref().map(Path::new));
    codex::children_of(session_id, &root)
        .into_iter()
        .map(|rollout| (rollout.path, rollout.session_id, rollout.compressed))
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (session_id, root=None))]
pub fn codex_resolve(session_id: &str, root: Option<PathBuf>) -> Option<PathBuf> {
    codex::resolve(session_id, &codex::sessions_root(root.as_deref()))
}
