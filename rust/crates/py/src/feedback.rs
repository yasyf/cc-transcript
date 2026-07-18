//! pyo3 exposure of the native feedback-store engine (`cc_transcript_core::feedback`): the
//! `RustFeedbackStore` handle the `mining.store.FeedbackStore` facade composes over. Every
//! call releases the GIL (`py.detach`); errors raise the faithful `sqlite3` types via
//! `crate::sqlite`, and the open-time v8/v9 guard raises `VerdictSchemaError`.

use std::path::Path;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use cc_transcript_core::feedback::{FeedbackConfig, FeedbackEngine, Migration};
use cc_transcript_core::sqlite::SqlCell;

use crate::sqlite::{cells, ledger_err, rows_to_dicts};

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(unsendable)]
pub struct RustFeedbackStore {
    engine: FeedbackEngine,
}

// A pointer to the thread-pinned (`unsendable`) engine. detach runs the closure
// synchronously on the calling thread, so this Send wrapper never actually crosses threads.
struct Pinned(*const FeedbackEngine);
unsafe impl Send for Pinned {}

impl Pinned {
    fn engine(&self) -> &FeedbackEngine {
        unsafe { &*self.0 }
    }
}

fn detached<T: Send>(
    py: Python<'_>,
    engine: &FeedbackEngine,
    call: impl FnOnce(&FeedbackEngine) -> T + Send,
) -> T {
    let pinned = Pinned(engine);
    py.detach(move || call(pinned.engine()))
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
        py.detach(|| FeedbackEngine::open(Path::new(path), config))
            .map(|engine| Self { engine })
            .map_err(|e| ledger_err(py, e))
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
        let rows = detached(py, &self.engine, |e| e.sql(statement, &cells))
            .map_err(|e| ledger_err(py, e))?;
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
        detached(py, &self.engine, |e| e.execute(statement, &cells)).map_err(|e| ledger_err(py, e))
    }

    fn executemany(
        &self,
        py: Python<'_>,
        statement: &str,
        seq: Vec<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<i64> {
        let rows: Vec<Vec<SqlCell>> = seq.iter().map(|r| cells(r)).collect::<PyResult<_>>()?;
        detached(py, &self.engine, |e| e.executemany(statement, &rows))
            .map_err(|e| ledger_err(py, e))
    }

    fn executescript(&self, py: Python<'_>, script: &str) -> PyResult<()> {
        detached(py, &self.engine, |e| e.executescript(script)).map_err(|e| ledger_err(py, e))
    }

    fn last_insert_rowid(&self) -> i64 {
        self.engine.last_insert_rowid()
    }

    fn load_extension(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        detached(py, &self.engine, |e| e.load_extension(path)).map_err(|e| ledger_err(py, e))
    }

    fn begin_immediate(&self, py: Python<'_>) -> PyResult<()> {
        detached(py, &self.engine, |e| e.begin_immediate()).map_err(|e| ledger_err(py, e))
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        detached(py, &self.engine, |e| e.commit()).map_err(|e| ledger_err(py, e))
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        detached(py, &self.engine, |e| e.rollback()).map_err(|e| ledger_err(py, e))
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
        detached(py, &self.engine, |e| {
            e.insert_candidates(&row_cells, extra_cells.as_deref())
        })
        .map_err(|e| ledger_err(py, e))
    }

    fn record_file(&self, py: Python<'_>, path: &str, mtime: f64) -> PyResult<()> {
        detached(py, &self.engine, |e| e.record_file(path, mtime)).map_err(|e| ledger_err(py, e))
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn file_mtimes<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows =
            detached(py, &self.engine, |e| e.file_mtimes()).map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "tuple[int, int, list[dict[str, typing.Any]]]", imports = ("typing",)))]
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<(i64, i64, Vec<Bound<'py, PyDict>>)> {
        let (total, files, by_source) =
            detached(py, &self.engine, |e| e.stats()).map_err(|e| ledger_err(py, e))?;
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
        let rows = detached(py, &self.engine, |e| e.recent(source_kind, limit))
            .map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }

    #[pyo3(signature = (source_kind=None))]
    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn events<'py>(
        &self,
        py: Python<'py>,
        source_kind: Option<&str>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows =
            detached(py, &self.engine, |e| e.events(source_kind)).map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }

    fn dedup_keys(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        detached(py, &self.engine, |e| e.dedup_keys()).map_err(|e| ledger_err(py, e))
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
        detached(py, &self.engine, |e| {
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
        .map_err(|e| ledger_err(py, e))
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
        let rows = detached(py, &self.engine, |e| {
            e.unjudged(role, prompt_version, refresh_summary, limit, offset)
        })
        .map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }

    #[gen_stub(override_return_type(type_repr = "list[dict[str, typing.Any]]", imports = ("typing",)))]
    fn judged<'py>(
        &self,
        py: Python<'py>,
        role: &str,
        prompt_version: i64,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = detached(py, &self.engine, |e| e.judged(role, prompt_version))
            .map_err(|e| ledger_err(py, e))?;
        rows_to_dicts(py, rows)
    }
}
