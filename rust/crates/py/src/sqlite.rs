//! Shared py-side SQLite mapping for the native store engines (`corrections`, `feedback`):
//! `LedgerError` -> the faithful `sqlite3`/`OSError` types callers
//! branch on, cell <-> Python value projection, and `sqlite3.Row`-shaped row dicts.

use pyo3::exceptions::{PyMemoryError, PyOverflowError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyFloat, PyInt, PyString};
use pyo3::IntoPyObjectExt;

use cc_transcript_core::sqlite::{LedgerError, SqlCell, SqlRow, SqliteErrorClass};

pub fn ledger_err(py: Python<'_>, error: LedgerError) -> PyErr {
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

pub fn sqlite3_error(
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

pub fn cell_to_py<'py>(py: Python<'py>, cell: SqlCell) -> PyResult<Bound<'py, PyAny>> {
    match cell {
        SqlCell::Null => Ok(py.None().into_bound(py)),
        SqlCell::Int(i) => i.into_bound_py_any(py),
        SqlCell::Real(f) => f.into_bound_py_any(py),
        SqlCell::Text(s) => Ok(PyString::new(py, &s).into_any()),
        SqlCell::Blob(b) => Ok(PyBytes::new(py, &b).into_any()),
    }
}

// Parity: pysqlite bind_param over the 1-based `index` — int past i64 raises
// OverflowError (never a lossy REAL); an unsupported type raises ProgrammingError.
pub fn py_to_cell(index: usize, obj: &Bound<'_, PyAny>) -> PyResult<SqlCell> {
    if obj.is_none() {
        return Ok(SqlCell::Null);
    }
    if let Ok(bytes) = obj.cast::<PyBytes>() {
        return Ok(SqlCell::Blob(bytes.as_bytes().to_vec()));
    }
    if let Ok(text) = obj.cast::<PyString>() {
        return Ok(SqlCell::Text(text.to_str()?.to_string()));
    }
    if obj.cast::<PyInt>().is_ok() {
        return match obj.extract::<i64>() {
            Ok(int) => Ok(SqlCell::Int(int)),
            Err(_) => Err(PyOverflowError::new_err(
                "Python int too large to convert to SQLite INTEGER",
            )),
        };
    }
    if let Ok(real) = obj.cast::<PyFloat>() {
        return Ok(SqlCell::Real(real.value()));
    }
    Err(sqlite3_error(
        obj.py(),
        "ProgrammingError",
        &format!(
            "Error binding parameter {index}: type '{}' is not supported",
            obj.get_type().name()?
        ),
        None,
        None,
    ))
}

pub fn cells(params: &[Bound<'_, PyAny>]) -> PyResult<Vec<SqlCell>> {
    params
        .iter()
        .enumerate()
        .map(|(i, obj)| py_to_cell(i + 1, obj))
        .collect()
}

// A read row as dict(sqlite3.Row): a name resolves to the FIRST column matching it
// ASCII-case-insensitively (the inverse of the JSON last-wins trap).
pub fn row_to_dict<'py>(py: Python<'py>, row: &SqlRow) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (name, _) in row {
        let first = row
            .iter()
            .position(|(other, _)| other.eq_ignore_ascii_case(name))
            .unwrap();
        dict.set_item(name, cell_to_py(py, row[first].1.clone())?)?;
    }
    Ok(dict)
}

pub fn rows_to_dicts<'py>(py: Python<'py>, rows: Vec<SqlRow>) -> PyResult<Vec<Bound<'py, PyDict>>> {
    rows.iter().map(|row| row_to_dict(py, row)).collect()
}
