//! pyo3 exposure of the core transcript-discovery walk and session resolution
//! for the Python parity suite: each function takes and returns path strings.

use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;
use std::path::Path;

use cc_transcript_core::discovery;

fn to_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_find_transcripts(root: &str) -> Vec<String> {
    discovery::find_transcripts(Path::new(root))
        .iter()
        .map(|p| to_str(p))
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_find_transcript(root: &str, session_id: &str) -> Option<String> {
    discovery::find_transcript(Path::new(root), session_id).map(|p| to_str(&p))
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (directory, name_contains=None, limit=None, known_mtimes=None))]
pub fn discovery_find_in(
    directory: &str,
    name_contains: Option<String>,
    limit: Option<usize>,
    #[gen_stub(override_type(type_repr = "dict[str, float] | None"))] known_mtimes: Option<
        HashMap<String, f64>,
    >,
) -> Vec<(String, f64)> {
    discovery::find_in(
        Path::new(directory),
        name_contains.as_deref(),
        limit,
        known_mtimes.as_ref(),
    )
    .into_iter()
    .map(|(path, mtime)| (to_str(&path), mtime))
    .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_subagent_paths(path: &str) -> Vec<String> {
    discovery::subagent_paths(Path::new(path))
        .iter()
        .map(|p| to_str(p))
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "dict[str, str]"))]
pub fn discovery_subagent_transcripts<'py>(
    py: Python<'py>,
    path: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    for (tool_use_id, entry) in discovery::subagent_transcripts(Path::new(path)) {
        out.set_item(tool_use_id, to_str(&entry))?;
    }
    Ok(out)
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub fn discovery_is_subagent_path(path: &str) -> bool {
    discovery::is_subagent_path(Path::new(path))
}
