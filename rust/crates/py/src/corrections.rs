//! pyo3 exposure of the core correction-ledger engine (`cc_transcript_core::corrections`)
//! for the `cc_transcript.corrections.CorrectionLog` facade: `__new__` stores the path,
//! `open()` spawns the actor thread that owns the connection, and every op returns an
//! `asyncio.Future` resolved from that thread.
//!
//! Detail is delegated to Python's own `json` module — `dict(detail)` normalizes or raises
//! (non-mapping TypeError stays synchronous) and `json.dumps`/`json.loads` reproduce
//! NaN/Infinity and lone-surrogate output exactly. Rows project column-by-column from their
//! SQLite storage class, mirroring Python's dynamic `row[col]`. Errors raise the same
//! `sqlite3`/`OSError` types callers branch on, carrying the `sqlite_errorcode`/`errno`
//! payloads.

use std::path::PathBuf;
use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::IntoPyObjectExt;

use cc_transcript_core::actor::Actor;
use cc_transcript_core::corrections::{Correction, CorrectionLog, SqlRow};

use crate::actor_bridge::{
    closed_error, done_callback, none, on_open_callback, running_loop, submit,
};
use crate::sqlite::{cell_to_py, rows_to_dicts};

enum State {
    Unopened(PathBuf),
    Open(Actor<CorrectionLog>),
    Closed,
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

fn corrections_to_py(py: Python<'_>, rows: Vec<SqlRow>) -> PyResult<Py<PyAny>> {
    corrections_project(py, rows)?.into_py_any(py)
}

fn sql_rows_to_py(py: Python<'_>, rows: Vec<SqlRow>) -> PyResult<Py<PyAny>> {
    rows_to_dicts(py, rows)?.into_py_any(py)
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass]
pub struct RustCorrectionLog {
    state: Mutex<State>,
}

impl RustCorrectionLog {
    fn with_actor(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&Actor<CorrectionLog>) -> PyResult<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        match &*self.state.lock().unwrap() {
            State::Open(actor) => f(actor),
            State::Closed => Err(closed_error(py)),
            State::Unopened(_) => unreachable!("operation on an unopened correction log"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl RustCorrectionLog {
    #[new]
    fn new(path: String) -> Self {
        Self {
            state: Mutex::new(State::Unopened(PathBuf::from(path))),
        }
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn open(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let event_loop = running_loop(py)?;
        let future = event_loop.call_method0("create_future")?.unbind();
        let event_loop = event_loop.unbind();
        let mut guard = self.state.lock().unwrap();
        let State::Unopened(path) = std::mem::replace(&mut *guard, State::Closed) else {
            unreachable!("open() on an already-opened correction log");
        };
        let actor = Actor::spawn(
            move || CorrectionLog::open(&path),
            on_open_callback(event_loop, future.clone_ref(py)),
        );
        *guard = State::Open(actor);
        Ok(future)
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn close(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let event_loop = running_loop(py)?;
        let future = event_loop.call_method0("create_future")?.unbind();
        let event_loop = event_loop.unbind();
        let mut guard = self.state.lock().unwrap();
        match std::mem::replace(&mut *guard, State::Closed) {
            State::Open(actor) => {
                actor.stop(done_callback(event_loop, future.clone_ref(py)));
            }
            _ => {
                future.bind(py).call_method1("set_result", (py.None(),))?;
            }
        }
        Ok(future)
    }

    #[pyo3(signature = (
        ts_ms, session_id, source, anchor_uuid, incorrect_digest, incorrect_file,
        incorrect_old, incorrect_new, correction_origin, correction_file, correction_old,
        correction_new, correction_commit, correction_text, overlap, detail,
    ))]
    #[allow(clippy::too_many_arguments)]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
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
    ) -> PyResult<Py<PyAny>> {
        // Convert before the lock: dict(detail)/json.dumps run arbitrary Python (a re-entrant
        // mapping could call close() and deadlock on the state Mutex). Mirror CorrectionLog.
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
        self.with_actor(py, |actor| {
            submit(py, actor, move |log| log.append(&record), none)
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn for_session(&self, py: Python<'_>, session_id: String) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |log| log.for_session(&session_id),
                corrections_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn for_repo(&self, py: Python<'_>, repo: String) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, move |log| log.for_repo(&repo), corrections_to_py)
        })
    }

    #[pyo3(signature = (ts_ms, source=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn since(&self, py: Python<'_>, ts_ms: i64, source: Option<String>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |log| log.since(ts_ms, source.as_deref()),
                corrections_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn for_anchor(
        &self,
        py: Python<'_>,
        session_id: String,
        anchor_uuid: String,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |log| log.for_anchor(&session_id, &anchor_uuid),
                corrections_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn by_digest(
        &self,
        py: Python<'_>,
        session_id: String,
        incorrect_digest: String,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |log| log.by_digest(&session_id, &incorrect_digest),
                corrections_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn sql(&self, py: Python<'_>, statement: String) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, move |log| log.sql(&statement), sql_rows_to_py)
        })
    }
}
