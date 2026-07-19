//! The py side of the actor: an op runs GIL-free on the actor thread, then one
//! `Python::attach` block materializes its result or exception and resolves the caller's
//! asyncio future via `loop.call_soon_threadsafe`. `Complete.__call__` runs on the loop
//! thread and drops the payload for a cancelled future — aiosqlite's cancellation contract.
//!
//! `deliver` swallows every delivery failure, not just a closed-loop RuntimeError: the op
//! already ran, so the completion is moot and the actor thread must outlive any loop's refusal.

use std::sync::Mutex;

use pyo3::prelude::*;

use cc_transcript_core::actor::{Actor, Job};
use cc_transcript_core::sqlite::LedgerError;

use crate::sqlite::{ledger_err, sqlite3_error};

pub fn running_loop(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    py.import("asyncio")?.getattr("get_running_loop")?.call0()
}

pub fn closed_error(py: Python<'_>) -> PyErr {
    sqlite3_error(
        py,
        "ProgrammingError",
        "Cannot operate on a closed database.",
        None,
        None,
    )
}

pub fn none(py: Python<'_>, _: ()) -> PyResult<Py<PyAny>> {
    Ok(py.None())
}

#[pyclass]
struct Complete {
    future: Py<PyAny>,
    payload: Mutex<Option<PyResult<Py<PyAny>>>>,
}

impl Complete {
    fn new(future: Py<PyAny>, payload: PyResult<Py<PyAny>>) -> Self {
        Complete {
            future,
            payload: Mutex::new(Some(payload)),
        }
    }
}

#[pymethods]
impl Complete {
    fn __call__(&self, py: Python<'_>) -> PyResult<()> {
        let future = self.future.bind(py);
        if future.call_method0("done")?.extract::<bool>()? {
            return Ok(());
        }
        match self.payload.lock().unwrap().take() {
            Some(Ok(value)) => future.call_method1("set_result", (value,)).map(drop),
            Some(Err(err)) => future
                .call_method1("set_exception", (err.into_value(py),))
                .map(drop),
            None => Ok(()),
        }
    }
}

fn deliver(
    py: Python<'_>,
    event_loop: &Py<PyAny>,
    future: Py<PyAny>,
    payload: PyResult<Py<PyAny>>,
) {
    let complete = Py::new(py, Complete::new(future, payload)).expect("Complete allocates");
    // The op already ran, so a loop that refuses the completion makes it moot; swallow every
    // failure rather than unwind — an unwind here has no catch and would strand the actor.
    let _ = event_loop
        .bind(py)
        .call_method1("call_soon_threadsafe", (complete,));
}

pub fn on_open_callback(
    event_loop: Py<PyAny>,
    future: Py<PyAny>,
) -> impl FnOnce(Result<(), LedgerError>) + Send + 'static {
    move |result| {
        Python::attach(|py| {
            let payload = result
                .map(|()| py.None())
                .map_err(|err| ledger_err(py, err));
            deliver(py, &event_loop, future, payload);
        })
    }
}

pub fn done_callback(
    event_loop: Py<PyAny>,
    future: Py<PyAny>,
) -> Box<dyn FnOnce() + Send + 'static> {
    Box::new(move || {
        Python::attach(|py| deliver(py, &event_loop, future, Ok(py.None())));
    })
}

pub fn submit<E, T>(
    py: Python<'_>,
    actor: &Actor<E>,
    op: impl FnOnce(&E) -> Result<T, LedgerError> + Send + 'static,
    convert: impl FnOnce(Python<'_>, T) -> PyResult<Py<PyAny>> + Send + 'static,
) -> PyResult<Py<PyAny>>
where
    E: 'static,
    T: Send + 'static,
{
    let event_loop = running_loop(py)?;
    let future = event_loop.call_method0("create_future")?.unbind();
    let event_loop = event_loop.unbind();
    let scheduled = future.clone_ref(py);
    let job: Job<E> = Box::new(move |engine: Option<&E>| {
        // Run the op GIL-free on the actor thread; attach only to map the result and deliver.
        let result = engine.map(op);
        Python::attach(|py| {
            let payload = match result {
                Some(Ok(value)) => convert(py, value),
                Some(Err(err)) => Err(ledger_err(py, err)),
                None => Err(closed_error(py)),
            };
            deliver(py, &event_loop, scheduled, payload);
        });
    });
    match actor.submit(job) {
        Ok(()) => Ok(future),
        Err(_) => unreachable!("actor thread ended while the handle was open"),
    }
}
