//! pyo3 exposure of the native feedback-store engine (`cc_transcript_core::feedback`): the
//! `RustFeedbackStore` handle the `mining.store.FeedbackStore` facade composes over. `__new__`
//! is cheap and stores config; `open()` spawns the actor thread that owns the connection, and
//! every op returns an `asyncio.Future` resolved from that thread. The state machine mirrors
//! `sqlite3.Connection`'s lifecycle: a closed handle raises `ProgrammingError` synchronously,
//! open failure (incl. the v8/v9 `VerdictSchemaError` guard) arrives through `await open()`,
//! and errors raise the faithful `sqlite3` types via `crate::sqlite`.

use std::path::PathBuf;
use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::IntoPyObjectExt;

use cc_transcript_core::actor::Actor;
use cc_transcript_core::feedback::{FeedbackConfig, FeedbackEngine, Migration};
use cc_transcript_core::sqlite::{SqlCell, SqlRow};

use crate::actor_bridge::{
    closed_error, done_callback, none, on_open_callback, running_loop, submit,
};
use crate::sqlite::{cells, rows_to_dicts};

enum State {
    Unopened {
        path: PathBuf,
        config: FeedbackConfig,
    },
    Open(Actor<FeedbackEngine>),
    Closed,
}

fn rows_to_py(py: Python<'_>, rows: Vec<SqlRow>) -> PyResult<Py<PyAny>> {
    rows_to_dicts(py, rows)?.into_py_any(py)
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass]
pub struct RustFeedbackStore {
    state: Mutex<State>,
}

impl RustFeedbackStore {
    fn with_actor(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&Actor<FeedbackEngine>) -> PyResult<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        match &*self.state.lock().unwrap() {
            State::Open(actor) => f(actor),
            State::Closed => Err(closed_error(py)),
            State::Unopened { .. } => unreachable!("operation on an unopened feedback store"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl RustFeedbackStore {
    #[new]
    #[pyo3(signature = (
        path, extra_ddl, event_columns, migrations, verdict_table, accepted_column,
        summary_column, event_filter, readonly=false, busy_timeout_ms=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: String,
        extra_ddl: Vec<String>,
        event_columns: Vec<String>,
        migrations: Vec<(String, String, String, Option<String>)>,
        verdict_table: String,
        accepted_column: String,
        summary_column: String,
        event_filter: Option<String>,
        readonly: bool,
        busy_timeout_ms: Option<i64>,
    ) -> Self {
        let config = FeedbackConfig {
            extra_ddl,
            event_columns,
            migrations: migrations
                .into_iter()
                .map(|(table, column, ddl, backfill)| Migration {
                    table,
                    column,
                    ddl,
                    backfill,
                })
                .collect(),
            verdict_table,
            accepted_column,
            summary_column,
            event_filter,
            readonly,
            busy_timeout_ms: busy_timeout_ms.unwrap_or(5000),
        };
        Self {
            state: Mutex::new(State::Unopened {
                path: PathBuf::from(path),
                config,
            }),
        }
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn open(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let event_loop = running_loop(py)?;
        let future = event_loop.call_method0("create_future")?.unbind();
        let event_loop = event_loop.unbind();
        let mut guard = self.state.lock().unwrap();
        let State::Unopened { path, config } = std::mem::replace(&mut *guard, State::Closed) else {
            unreachable!("open() on an already-opened feedback store");
        };
        let actor = Actor::spawn(
            move || FeedbackEngine::open(&path, config),
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

    #[pyo3(signature = (statement, params=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn sql(
        &self,
        py: Python<'_>,
        statement: String,
        params: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<Py<PyAny>> {
        let cells = cells(&params.unwrap_or_default())?;
        self.with_actor(py, |actor| {
            submit(py, actor, move |e| e.sql(&statement, &cells), rows_to_py)
        })
    }

    #[pyo3(signature = (statement, params=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[int]", imports = ("asyncio",)))]
    fn execute(
        &self,
        py: Python<'_>,
        statement: String,
        params: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<Py<PyAny>> {
        let cells = cells(&params.unwrap_or_default())?;
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.execute(&statement, &cells),
                |py, n| n.into_py_any(py),
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[int]", imports = ("asyncio",)))]
    fn executemany(
        &self,
        py: Python<'_>,
        statement: String,
        seq: Vec<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<Py<PyAny>> {
        let rows: Vec<Vec<SqlCell>> = seq.iter().map(|r| cells(r)).collect::<PyResult<_>>()?;
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.executemany(&statement, &rows),
                |py, n| n.into_py_any(py),
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn executescript(&self, py: Python<'_>, script: String) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, move |e| e.executescript(&script), none)
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[int]", imports = ("asyncio",)))]
    fn last_insert_rowid(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                |e| Ok(e.last_insert_rowid()),
                |py, n| n.into_py_any(py),
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn load_extension(&self, py: Python<'_>, path: String) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, move |e| e.load_extension(&path), none)
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn begin_immediate(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| submit(py, actor, |e| e.begin_immediate(), none))
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn commit(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| submit(py, actor, |e| e.commit(), none))
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn rollback(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| submit(py, actor, |e| e.rollback(), none))
    }

    #[pyo3(signature = (rows, extras=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[str]]", imports = ("asyncio",)))]
    fn insert_candidates(
        &self,
        py: Python<'_>,
        rows: Vec<Vec<Bound<'_, PyAny>>>,
        extras: Option<Vec<Vec<Bound<'_, PyAny>>>>,
    ) -> PyResult<Py<PyAny>> {
        let row_cells: Vec<Vec<SqlCell>> =
            rows.iter().map(|r| cells(r)).collect::<PyResult<_>>()?;
        let extra_cells: Option<Vec<Vec<SqlCell>>> = match extras {
            Some(ex) => Some(ex.iter().map(|r| cells(r)).collect::<PyResult<_>>()?),
            None => None,
        };
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.insert_candidates(&row_cells, extra_cells.as_deref()),
                |py, keys| keys.into_py_any(py),
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[None]", imports = ("asyncio",)))]
    fn record_file(&self, py: Python<'_>, path: String, mtime: f64) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, move |e| e.record_file(&path, mtime), none)
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn file_mtimes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(py, actor, |e| e.file_mtimes(), rows_to_py)
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[tuple[int, int, list[dict[str, typing.Any]]]]", imports = ("asyncio", "typing")))]
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                |e| e.stats(),
                |py, (total, files, by_source)| {
                    (total, files, rows_to_dicts(py, by_source)?).into_py_any(py)
                },
            )
        })
    }

    #[pyo3(signature = (source_kind=None, limit=20))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn recent(
        &self,
        py: Python<'_>,
        source_kind: Option<String>,
        limit: i64,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.recent(source_kind.as_deref(), limit),
                rows_to_py,
            )
        })
    }

    #[pyo3(signature = (source_kind=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn events(&self, py: Python<'_>, source_kind: Option<String>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.events(source_kind.as_deref()),
                rows_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[str]]", imports = ("asyncio",)))]
    fn dedup_keys(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                |e| e.dedup_keys(),
                |py, keys| keys.into_py_any(py),
            )
        })
    }

    #[pyo3(signature = (
        dedup_key, role, prompt_version, model, category, accepted, summary, confidence,
        rationale, canonical_key, fidelity, judged_at,
    ))]
    #[allow(clippy::too_many_arguments)]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[bool]", imports = ("asyncio",)))]
    fn record_verdict(
        &self,
        py: Python<'_>,
        dedup_key: String,
        role: String,
        prompt_version: i64,
        model: String,
        category: String,
        accepted: bool,
        summary: String,
        confidence: f64,
        rationale: String,
        canonical_key: Option<String>,
        fidelity: String,
        judged_at: String,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| {
                    e.record_verdict(
                        &dedup_key,
                        &role,
                        prompt_version,
                        &model,
                        &category,
                        accepted,
                        &summary,
                        confidence,
                        &rationale,
                        canonical_key.as_deref(),
                        &fidelity,
                        &judged_at,
                    )
                },
                |py, changed| changed.into_py_any(py),
            )
        })
    }

    #[pyo3(signature = (role, prompt_version, refresh_summary=false, limit=None, offset=None))]
    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn unjudged(
        &self,
        py: Python<'_>,
        role: String,
        prompt_version: i64,
        refresh_summary: bool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.unjudged(&role, prompt_version, refresh_summary, limit, offset),
                rows_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[dict[str, typing.Any]]]", imports = ("asyncio", "typing")))]
    fn judged(&self, py: Python<'_>, role: String, prompt_version: i64) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.judged(&role, prompt_version),
                rows_to_py,
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[tuple[str, float, list[str]]]]", imports = ("asyncio",)))]
    fn suggest_canonical_keys(
        &self,
        py: Python<'_>,
        query: Vec<u8>,
        prompt_version: i64,
        k: usize,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.suggest_canonical_keys(&query, prompt_version, k),
                |py, ranked| ranked.into_py_any(py),
            )
        })
    }

    #[gen_stub(override_return_type(type_repr = "asyncio.Future[list[tuple[str, str, float]]]", imports = ("asyncio",)))]
    fn near_duplicate_keys(
        &self,
        py: Python<'_>,
        prompt_version: i64,
        threshold: f64,
    ) -> PyResult<Py<PyAny>> {
        self.with_actor(py, |actor| {
            submit(
                py,
                actor,
                move |e| e.near_duplicate_keys(prompt_version, threshold),
                |py, overlaps| overlaps.into_py_any(py),
            )
        })
    }
}
