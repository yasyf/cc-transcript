//! Python exception translation shared by typed tool-call view parsing.

use pyo3::prelude::*;

use cc_transcript_core::toolcall::ToolInputError;

// Raise cc_transcript.tools.ToolInputError with Python's exact message.
pub(crate) fn tool_input_error(py: Python<'_>, name: &str, err: &ToolInputError) -> PyErr {
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
