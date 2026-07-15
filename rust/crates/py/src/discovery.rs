//! pyo3 exposure of the core transcript-discovery walk and session resolution
//! for the Python parity suite: paths cross the boundary as PathBuf, so
//! non-UTF-8 (surrogate-escaped) names survive both directions.

use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;
use std::path::PathBuf;

use cc_transcript_core::discovery;

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_find_transcripts(root: PathBuf) -> Vec<PathBuf> {
    discovery::find_transcripts(&root)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_find_transcript(root: PathBuf, session_id: &str) -> Option<PathBuf> {
    discovery::find_transcript(&root, session_id)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (directory, name_contains=None, limit=None, known_mtimes=None))]
pub fn discovery_find_in(
    directory: PathBuf,
    name_contains: Option<String>,
    limit: Option<usize>,
    #[gen_stub(override_type(type_repr = "dict[str, float] | None"))] known_mtimes: Option<
        HashMap<String, f64>,
    >,
) -> Vec<(PathBuf, f64)> {
    discovery::find_in(
        &directory,
        name_contains.as_deref(),
        limit,
        known_mtimes.as_ref(),
    )
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_subagent_paths(path: PathBuf) -> Vec<PathBuf> {
    discovery::subagent_paths(&path)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "dict[str, pathlib.Path]", imports = ("pathlib",)))]
pub fn discovery_subagent_transcripts<'py>(
    py: Python<'py>,
    path: PathBuf,
) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    for (tool_use_id, entry) in discovery::subagent_transcripts(&path) {
        out.set_item(tool_use_id, entry)?;
    }
    Ok(out)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_is_subagent_path(path: PathBuf) -> bool {
    discovery::is_subagent_path(&path)
}
