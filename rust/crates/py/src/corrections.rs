//! pyo3 exposure of the core correction-ledger engine (`cc_transcript_core::corrections`)
//! for the parity suite and the future Rust CLI: a handle mirroring
//! `cc_transcript.corrections.CorrectionLog`.
//!
//! Detail is delegated to Python's own `json` module — `dict(detail)` normalizes or raises
//! and `json.dumps`/`json.loads` reproduce NaN/Infinity and lone-surrogate output exactly.
//! Rows project column-by-column from their SQLite storage class, mirroring Python's dynamic
//! `row[col]`. Every core call runs with the GIL released (`py.detach`) so a blocked writer
//! never convoys other threads; errors raise the same `sqlite3`/`OSError` types callers
//! branch on, carrying `sqlite_errorcode`/`sqlite_errorname` and `errno`/`filename` payloads.

use std::path::Path;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use cc_transcript_core::corrections::{Correction, CorrectionLog, SqlRow};

use crate::sqlite::{cell_to_py, ledger_err, rows_to_dicts};

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(unsendable)]
pub struct RustCorrectionLog {
    log: CorrectionLog,
}

// A pointer to the thread-pinned (`unsendable`) engine. detach runs the closure
// synchronously on the calling thread, so this Send wrapper never actually crosses threads.
struct Pinned(*const CorrectionLog);
unsafe impl Send for Pinned {}

impl Pinned {
    fn log(&self) -> &CorrectionLog {
        unsafe { &*self.0 }
    }
}

// Runs a core call with the GIL released so a blocked writer never convoys other threads.
fn detached<T: Send>(
    py: Python<'_>,
    log: &CorrectionLog,
    call: impl FnOnce(&CorrectionLog) -> T + Send,
) -> T {
    let pinned = Pinned(log);
    py.detach(move || call(pinned.log()))
}

// Mirror row_to_record: drop the DB `id` (asdict omits it) and read detail as
// json.loads(detail_json); every other column projects by its storage class.
fn corrections_project<'py>(
    py: Python<'py>,
    rows: Vec<SqlRow>,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let json_loads = py.import("json")?.getattr("loads")?;
    rows.into_iter()
        .map(|row| {
            let dict = PyDict::new(py);
            for (name, value) in row {
                match name.as_str() {
                    "id" => {}
                    "detail_json" => {
                        dict.set_item("detail", json_loads.call1((cell_to_py(py, value)?,))?)?;
                    }
                    _ => dict.set_item(name, cell_to_py(py, value)?)?,
                }
            }
            Ok(dict)
        })
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl RustCorrectionLog {
    #[new]
    fn new(py: Python<'_>, path: &str) -> PyResult<Self> {
        py.detach(|| CorrectionLog::open(Path::new(path)))
            .map(|log| Self { log })
            .map_err(|e| ledger_err(py, e))
    }

    #[pyo3(signature = (
        ts_ms, session_id, source, anchor_uuid, incorrect_digest, incorrect_file,
        incorrect_old, incorrect_new, correction_origin, correction_file, correction_old,
        correction_new, correction_commit, correction_text, overlap, detail,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn append(
        &self,
        py: Python<'_>,
        ts_ms: i64,
        session_id: String,
        source: String,
        anchor_uuid: String,
        incorrect_digest: Option<String>,
        incorrect_file: String,
        incorrect_old: String,
        incorrect_new: String,
        correction_origin: Option<String>,
        correction_file: Option<String>,
        correction_old: Option<String>,
        correction_new: Option<String>,
        correction_commit: Option<String>,
        correction_text: Option<String>,
        overlap: f64,
        detail: Bound<'_, PyAny>,
    ) -> PyResult<()> {
        // Mirror CorrectionLog.append: dict(detail) normalizes or raises (non-mapping),
        // json.dumps emits NaN/Infinity/lone-surrogates like the Python reference writer.
        let normalized = py.import("builtins")?.getattr("dict")?.call1((detail,))?;
        let detail_json: String = py
            .import("json")?
            .getattr("dumps")?
            .call1((normalized,))?
            .extract()?;
        let record = Correction {
            ts_ms,
            session_id,
            source,
            anchor_uuid,
            incorrect_digest,
            incorrect_file,
            incorrect_old,
            incorrect_new,
            correction_origin,
            correction_file,
            correction_old,
            correction_new,
            correction_commit,
            correction_text,
            overlap,
            detail_json,
        };
        detached(py, &self.log, |log| log.append(&record)).map_err(|e| ledger_err(py, e))
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn for_session<'py>(
        &self,
        py: Python<'py>,
        session_id: &str,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = detached(py, &self.log, |log| log.for_session(session_id))
            .map_err(|e| ledger_err(py, e))?;
        corrections_project(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn for_repo<'py>(&self, py: Python<'py>, repo: &str) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows =
            detached(py, &self.log, |log| log.for_repo(repo)).map_err(|e| ledger_err(py, e))?;
        corrections_project(py, rows)
    }

    #[pyo3(signature = (ts_ms, source=None))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn since<'py>(
        &self,
        py: Python<'py>,
        ts_ms: i64,
        source: Option<&str>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = detached(py, &self.log, |log| log.since(ts_ms, source))
            .map_err(|e| ledger_err(py, e))?;
        corrections_project(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn for_anchor<'py>(
        &self,
        py: Python<'py>,
        session_id: &str,
        anchor_uuid: &str,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = detached(py, &self.log, |log| log.for_anchor(session_id, anchor_uuid))
            .map_err(|e| ledger_err(py, e))?;
        corrections_project(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn by_digest<'py>(
        &self,
        py: Python<'py>,
        session_id: &str,
        incorrect_digest: &str,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = detached(py, &self.log, |log| {
            log.by_digest(session_id, incorrect_digest)
        })
        .map_err(|e| ledger_err(py, e))?;
        corrections_project(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn sql<'py>(&self, py: Python<'py>, statement: &str) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows =
            detached(py, &self.log, |log| log.sql(statement)).map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }
}
