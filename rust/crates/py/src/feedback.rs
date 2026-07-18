//! pyo3 exposure of the native feedback-store engine (`cc_transcript_core::feedback`): the
//! `RustFeedbackStore` handle the `mining.store.FeedbackStore` facade composes over. Every
//! call releases the GIL (`py.detach`); errors raise the faithful `sqlite3` types via
//! `crate::sqlite`, and the open-time v8/v9 guard raises `VerdictSchemaError`. The handle
//! mirrors `sqlite3.Connection`'s lifecycle discipline: `close()` drops the connection
//! (idempotently), a closed handle raises `ProgrammingError`, and cross-thread use raises
//! the same-thread `ProgrammingError` CPython's `check_same_thread` produces.

use std::path::Path;
use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use cc_transcript_core::feedback::{FeedbackConfig, FeedbackEngine, Migration};
use cc_transcript_core::sqlite::{LedgerError, SqlCell};

use crate::sqlite::{cells, ledger_err, rows_to_dicts, sqlite3_error};

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass]
pub struct RustFeedbackStore {
    engine: Mutex<Option<FeedbackEngine>>,
    thread_ident: u64,
}

// Parity: threading.get_ident() — the ident CPython's check_same_thread message prints.
fn thread_ident(py: Python<'_>) -> PyResult<u64> {
    py.import("threading")?
        .getattr("get_ident")?
        .call0()?
        .extract()
}

fn closed_err(py: Python<'_>) -> PyErr {
    sqlite3_error(
        py,
        "ProgrammingError",
        "Cannot operate on a closed database.",
        None,
        None,
    )
}

impl RustFeedbackStore {
    // Parity: pysqlite_check_thread — same exception type and message shape.
    fn check_thread(&self, py: Python<'_>) -> PyResult<()> {
        let current = thread_ident(py)?;
        if current != self.thread_ident {
            return Err(sqlite3_error(
                py,
                "ProgrammingError",
                &format!(
                    "SQLite objects created in a thread can only be used in that same thread. \
                     The object was created in thread id {} and this is thread id {}.",
                    self.thread_ident, current
                ),
                None,
                None,
            ));
        }
        Ok(())
    }

    fn call<T: Send>(
        &self,
        py: Python<'_>,
        call: impl FnOnce(&FeedbackEngine) -> Result<T, LedgerError> + Send,
    ) -> PyResult<T> {
        self.check_thread(py)?;
        let engine = &self.engine;
        match py.detach(move || engine.lock().unwrap().as_ref().map(call)) {
            Some(result) => result.map_err(|e| ledger_err(py, e)),
            None => Err(closed_err(py)),
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
        py: Python<'_>,
        path: &str,
        extra_ddl: Vec<String>,
        event_columns: Vec<String>,
        migrations: Vec<(String, String, String, Option<String>)>,
        verdict_table: String,
        accepted_column: String,
        summary_column: String,
        event_filter: Option<String>,
        readonly: bool,
        busy_timeout_ms: Option<i64>,
    ) -> PyResult<Self> {
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
        let thread_ident = thread_ident(py)?;
        py.detach(|| FeedbackEngine::open(Path::new(path), config))
            .map(|engine| Self {
                engine: Mutex::new(Some(engine)),
                thread_ident,
            })
            .map_err(|e| ledger_err(py, e))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        self.check_thread(py)?;
        let engine = &self.engine;
        py.detach(move || drop(engine.lock().unwrap().take()));
        Ok(())
    }

    #[pyo3(signature = (statement, params=None))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn sql<'py>(
        &self,
        py: Python<'py>,
        statement: &str,
        params: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let cells = cells(&params.unwrap_or_default())?;
        let rows = self.call(py, |e| e.sql(statement, &cells))?;
        rows_to_dicts(py, rows)
    }

    #[pyo3(signature = (statement, params=None))]
    fn execute(
        &self,
        py: Python<'_>,
        statement: &str,
        params: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<i64> {
        let cells = cells(&params.unwrap_or_default())?;
        self.call(py, |e| e.execute(statement, &cells))
    }

    fn executemany(
        &self,
        py: Python<'_>,
        statement: &str,
        seq: Vec<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<i64> {
        let rows: Vec<Vec<SqlCell>> = seq.iter().map(|r| cells(r)).collect::<PyResult<_>>()?;
        self.call(py, |e| e.executemany(statement, &rows))
    }

    fn executescript(&self, py: Python<'_>, script: &str) -> PyResult<()> {
        self.call(py, |e| e.executescript(script))
    }

    fn last_insert_rowid(&self, py: Python<'_>) -> PyResult<i64> {
        self.call(py, |e| Ok(e.last_insert_rowid()))
    }

    fn load_extension(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        self.call(py, |e| e.load_extension(path))
    }

    fn begin_immediate(&self, py: Python<'_>) -> PyResult<()> {
        self.call(py, |e| e.begin_immediate())
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.call(py, |e| e.commit())
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        self.call(py, |e| e.rollback())
    }

    #[pyo3(signature = (rows, extras=None))]
    fn insert_candidates(
        &self,
        py: Python<'_>,
        rows: Vec<Vec<Bound<'_, PyAny>>>,
        extras: Option<Vec<Vec<Bound<'_, PyAny>>>>,
    ) -> PyResult<Vec<String>> {
        let row_cells: Vec<Vec<SqlCell>> =
            rows.iter().map(|r| cells(r)).collect::<PyResult<_>>()?;
        let extra_cells: Option<Vec<Vec<SqlCell>>> = match extras {
            Some(ex) => Some(ex.iter().map(|r| cells(r)).collect::<PyResult<_>>()?),
            None => None,
        };
        self.call(py, |e| {
            e.insert_candidates(&row_cells, extra_cells.as_deref())
        })
    }

    fn record_file(&self, py: Python<'_>, path: &str, mtime: f64) -> PyResult<()> {
        self.call(py, |e| e.record_file(path, mtime))
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn file_mtimes<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self.call(py, |e| e.file_mtimes())?;
        rows_to_dicts(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "tuple[int, int, list[dict[str, typing.Any]]]", imports = ("typing",)))]
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<(i64, i64, Vec<Bound<'py, PyDict>>)> {
        let (total, files, by_source) = self.call(py, |e| e.stats())?;
        Ok((total, files, rows_to_dicts(py, by_source)?))
    }

    #[pyo3(signature = (source_kind=None, limit=20))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn recent<'py>(
        &self,
        py: Python<'py>,
        source_kind: Option<&str>,
        limit: i64,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self.call(py, |e| e.recent(source_kind, limit))?;
        rows_to_dicts(py, rows)
    }

    #[pyo3(signature = (source_kind=None))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn events<'py>(
        &self,
        py: Python<'py>,
        source_kind: Option<&str>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self.call(py, |e| e.events(source_kind))?;
        rows_to_dicts(py, rows)
    }

    fn dedup_keys(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        self.call(py, |e| e.dedup_keys())
    }

    #[pyo3(signature = (
        dedup_key, role, prompt_version, model, category, accepted, summary, confidence,
        rationale, canonical_key, fidelity, judged_at,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn record_verdict(
        &self,
        py: Python<'_>,
        dedup_key: &str,
        role: &str,
        prompt_version: i64,
        model: &str,
        category: &str,
        accepted: bool,
        summary: &str,
        confidence: f64,
        rationale: &str,
        canonical_key: Option<&str>,
        fidelity: &str,
        judged_at: &str,
    ) -> PyResult<bool> {
        self.call(py, |e| {
            e.record_verdict(
                dedup_key,
                role,
                prompt_version,
                model,
                category,
                accepted,
                summary,
                confidence,
                rationale,
                canonical_key,
                fidelity,
                judged_at,
            )
        })
    }

    #[pyo3(signature = (role, prompt_version, refresh_summary=false, limit=None, offset=None))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn unjudged<'py>(
        &self,
        py: Python<'py>,
        role: &str,
        prompt_version: i64,
        refresh_summary: bool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self.call(py, |e| {
            e.unjudged(role, prompt_version, refresh_summary, limit, offset)
        })?;
        rows_to_dicts(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn judged<'py>(
        &self,
        py: Python<'py>,
        role: &str,
        prompt_version: i64,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self.call(py, |e| e.judged(role, prompt_version))?;
        rows_to_dicts(py, rows)
    }
}
