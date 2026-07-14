use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};
use pyo3::IntoPyObjectExt;
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use cc_transcript_core::parse::ParseError;

// Orphan rule: ParseError (core) and PyErr (pyo3) are both foreign here, so the
// conversion is a function rather than a From impl.
pub(crate) fn parse_err(err: ParseError) -> PyErr {
    match err {
        ParseError::Key(key) => PyKeyError::new_err(format!("'{key}'")),
        ParseError::Value(msg) => PyValueError::new_err(msg),
    }
}

pub(crate) fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value.get_type() {
        JsonType::Null => Ok(py.None().into_bound(py)),
        JsonType::Boolean => value.as_bool().unwrap().into_bound_py_any(py),
        JsonType::Number => match (value.as_i64(), value.as_u64()) {
            (Some(i), _) => i.into_bound_py_any(py),
            (_, Some(u)) => u.into_bound_py_any(py),
            // sonic-rs stores the rest as f64, collapsing `-0` (orjson int 0)
            // and `-0.0` (orjson float -0.0) into +0.0; re-parse the raw text
            // with orjson's semantics.
            _ => number_from_raw(py, value),
        },
        JsonType::String => Ok(PyString::new(py, value.as_str().unwrap()).into_any()),
        JsonType::Array => {
            let list = PyList::empty(py);
            for item in value.as_array().unwrap() {
                list.append(json_to_py(py, item)?)?;
            }
            Ok(list.into_any())
        }
        JsonType::Object => {
            let dict = PyDict::new(py);
            for (k, v) in value.as_object().unwrap() {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

// orjson semantics over raw text: a frac/exp marker is float; else int via i64/u64,
// or an exact big int via Python int() beyond u64.
fn number_from_raw<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let raw = value
        .as_raw_number()
        .expect("a parsed JSON number carries raw text under arbitrary_precision");
    let text = raw.as_str();
    if !text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        return match (text.parse::<i64>(), text.parse::<u64>()) {
            (Ok(i), _) => i.into_bound_py_any(py),
            (_, Ok(u)) => u.into_bound_py_any(py),
            _ => py.import("builtins")?.getattr("int")?.call1((text,)),
        };
    }
    text.parse::<f64>()
        .expect("JSON number text parses as f64")
        .into_bound_py_any(py)
}

pub(crate) fn opt_json<'py>(py: Python<'py>, value: Option<&Value>) -> PyResult<Bound<'py, PyAny>> {
    match value {
        Some(value) => json_to_py(py, value),
        None => Ok(py.None().into_bound(py)),
    }
}
