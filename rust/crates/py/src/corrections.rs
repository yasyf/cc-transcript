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

use pyo3::exceptions::PyMemoryError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString};
use pyo3::IntoPyObjectExt;

use cc_transcript_core::corrections::{
    Correction, CorrectionLog, LedgerError, SqlCell, SqlRow, SqliteErrorClass,
};

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

fn ledger_err(py: Python<'_>, error: LedgerError) -> PyErr {
    match error {
        LedgerError::MultipleStatements => sqlite3_error(
            py,
            "ProgrammingError",
            "You can only execute one statement at a time.",
            None,
            None,
        ),
        // SQLITE_NOMEM maps to the builtin MemoryError (CPython's PyErr_NoMemory), with no
        // sqlite payload; every other class is looked up in the sqlite3 module.
        LedgerError::Sqlite {
            class: SqliteErrorClass::Memory,
            message,
            ..
        } => PyMemoryError::new_err(message),
        LedgerError::Sqlite {
            class,
            message,
            code,
            name,
        } => sqlite3_error(py, class.name(), &message, code, name.as_deref()),
        LedgerError::Io { error, path } => os_error(py, &error, path.to_string_lossy().as_ref()),
    }
}

fn sqlite3_error(
    py: Python<'_>,
    class: &str,
    message: &str,
    code: Option<i32>,
    name: Option<&str>,
) -> PyErr {
    let build = || -> PyResult<Bound<'_, PyAny>> {
        let exc = py.import("sqlite3")?.getattr(class)?.call1((message,))?;
        if let Some(code) = code {
            exc.setattr("sqlite_errorcode", code)?;
            exc.setattr("sqlite_errorname", name.unwrap_or("unknown"))?;
        }
        Ok(exc)
    };
    match build() {
        Ok(exc) => PyErr::from_value(exc),
        Err(err) => err,
    }
}

fn os_error(py: Python<'_>, error: &std::io::Error, filename: &str) -> PyErr {
    // OSError(errno, strerror, filename) dispatches to the errno-specific subclass
    // (FileExistsError, NotADirectoryError, …) and carries the Python-shaped payload.
    let build = || -> PyResult<Bound<'_, PyAny>> {
        let errno = error.raw_os_error().unwrap_or(0);
        let strerror = py.import("os")?.getattr("strerror")?.call1((errno,))?;
        py.import("builtins")?
            .getattr("OSError")?
            .call1((errno, strerror, filename))
    };
    match build() {
        Ok(exc) => PyErr::from_value(exc),
        Err(err) => err,
    }
}

fn cell_to_py<'py>(py: Python<'py>, cell: SqlCell) -> PyResult<Bound<'py, PyAny>> {
    match cell {
        SqlCell::Null => Ok(py.None().into_bound(py)),
        SqlCell::Int(i) => i.into_bound_py_any(py),
        SqlCell::Real(f) => f.into_bound_py_any(py),
        SqlCell::Text(s) => Ok(PyString::new(py, &s).into_any()),
        SqlCell::Blob(b) => Ok(PyBytes::new(py, &b).into_any()),
    }
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
        rows.into_iter()
            .map(|row| {
                // dict(sqlite3.Row): a name resolves to the value of the FIRST column matching
                // it ASCII-case-insensitively (inverse of the JSON last-wins trap).
                let dict = PyDict::new(py);
                for (name, _) in &row {
                    let first = row
                        .iter()
                        .position(|(other, _)| other.eq_ignore_ascii_case(name))
                        .unwrap();
                    dict.set_item(name, cell_to_py(py, row[first].1.clone())?)?;
                }
                Ok(dict)
            })
            .collect()
    }
}
