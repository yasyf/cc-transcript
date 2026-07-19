//! Python exception translation shared by typed tool-call view parsing.

use pyo3::prelude::*;

use cc_transcript_core::toolcall::ToolInputError;

// Raise cc_transcript.tools.ToolInputError with Python's exact message.
pub(crate) fn tool_input_error(py: Python<'_>, name: &str, err: &ToolInputError) -> PyErr {
    let msg = format!("{name} {err}");
    match py
        .import("cc_transcript.tools")
        .and_then(|m| m.getattr("ToolInputError"))
        .and_then(|cls| cls.call1((msg,)))
    {
        Ok(exc) => PyErr::from_value(exc),
        Err(e) => e,
    }
}
