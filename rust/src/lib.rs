use pyo3::prelude::*;

// TODO(rust-parity): port the transcript parser from cc-sentiment's crates/transcripts
// and extend it to the full superset event model (tool_result, mode, isMeta, sidechain).

#[pymodule]
fn _parser_rs(_py: Python<'_>, _m: &Bound<'_, PyModule>) -> PyResult<()> {
    Ok(())
}
