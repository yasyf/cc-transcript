use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyType;

pub static USER_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static ASSISTANT_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static SYSTEM_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_INFO_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static STOP_HOOK_SUMMARY_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static PRESERVED_SEGMENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static PRESERVED_MESSAGES_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static COMPACT_BOUNDARY_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static TURN_DURATION_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static MODEL_REFUSAL_FALLBACK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static OTHER_SYSTEM_DETAIL_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static MODE_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static OTHER_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static ATTACHMENT_EVENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_SUCCESS_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_BLOCKING_ERROR_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_NON_BLOCKING_ERROR_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_CANCELLED_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static HOOK_ADDITIONAL_CONTEXT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static ASYNC_HOOK_RESPONSE_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static QUEUED_COMMAND_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static OTHER_ATTACHMENT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static ENTRY_META_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static ATTRIBUTION_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static API_ERROR_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static TEXT_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static THINKING_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static TOOL_USE_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static TOOL_RESULT_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static FALLBACK_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static OTHER_BLOCK_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static USAGE_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static CACHE_CREATION_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static SERVER_TOOL_USE_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static PRINT_RESULT_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static MODEL_USAGE_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static INIT_INFO_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static MCP_SERVER_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static PLUGIN_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub static PRINT_MESSAGE_CLS: PyOnceLock<Py<PyType>> = PyOnceLock::new();

pub fn models_type<'py>(
    py: Python<'py>,
    cell: &'static PyOnceLock<Py<PyType>>,
    name: &str,
) -> PyResult<Bound<'py, PyType>> {
    let cached = cell.get_or_try_init(py, || -> PyResult<Py<PyType>> {
        Ok(py
            .import("cc_transcript.models")?
            .getattr(name)?
            .cast_into::<PyType>()?
            .unbind())
    })?;
    Ok(cached.bind(py).clone())
}
