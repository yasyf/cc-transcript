use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3::IntoPyObjectExt;

pub(crate) fn repr_pairs(
    name: &str,
    pairs: &[(&'static str, Bound<'_, PyAny>)],
) -> PyResult<String> {
    let mut out = String::with_capacity(16 + 24 * pairs.len());
    out.push_str(name);
    out.push('(');
    for (i, (field, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(field);
        out.push('=');
        out.push_str(value.repr()?.to_str()?);
    }
    out.push(')');
    Ok(out)
}

pub(crate) fn eq_pairs(
    py: Python<'_>,
    a: &[(&'static str, Bound<'_, PyAny>)],
    b: &[(&'static str, Bound<'_, PyAny>)],
) -> PyResult<Py<PyAny>> {
    for ((_, left), (_, right)) in a.iter().zip(b) {
        if !left.eq(right)? {
            return false.into_py_any(py);
        }
    }
    true.into_py_any(py)
}

pub(crate) fn hash_pairs(
    py: Python<'_>,
    pairs: &[(&'static str, Bound<'_, PyAny>)],
) -> PyResult<isize> {
    PyTuple::new(py, pairs.iter().map(|(_, value)| value))?.hash()
}

/// Dataclass-style dunders for a frozen view class, emitted as a second
/// `#[pymethods]` impl (the `multiple-pymethods` feature): `__match_args__`,
/// `__repr__`, structural `__eq__` (exact-class, field-by-field over Python
/// equality), and — unless `hash = manual` — a dataclass-style `__hash__` over
/// the field tuple. `match_args = []` mirrors a `kw_only` dataclass (tools.py
/// hierarchy), whose repr/eq fields still list every compare field in
/// declaration order. Invoke at module level, after the class's own
/// `#[pymethods]` impl.
macro_rules! view_dunders {
    ($cls:ident, $name:literal, fields = [$($f:ident),* $(,)?]) => {
        view_dunders!(@impl $cls, $name, [$($f),*], [$($f),*]);
        view_dunders!(@hash $cls);
    };
    ($cls:ident, $name:literal, fields = [$($f:ident),* $(,)?], hash = manual) => {
        view_dunders!(@impl $cls, $name, [$($f),*], [$($f),*]);
    };
    ($cls:ident, $name:literal, fields = [$($f:ident),* $(,)?], match_args = []) => {
        view_dunders!(@impl $cls, $name, [$($f),*], []);
        view_dunders!(@hash $cls);
    };
    ($cls:ident, $name:literal, fields = [$($f:ident),* $(,)?], match_args = [], hash = manual) => {
        view_dunders!(@impl $cls, $name, [$($f),*], []);
    };
    (@impl $cls:ident, $name:literal, [$($f:ident),*], [$($m:ident),*]) => {
        impl $cls {
            fn dunder_fields<'py>(
                &self,
                py: pyo3::Python<'py>,
            ) -> pyo3::PyResult<Vec<(&'static str, pyo3::Bound<'py, pyo3::PyAny>)>> {
                use pyo3::IntoPyObjectExt;
                Ok(vec![$((stringify!($f), self.$f(py)?.into_bound_py_any(py)?)),*])
            }
        }

        #[pyo3_stub_gen::derive::gen_stub_pymethods]
        #[pyo3::pymethods]
        impl $cls {
            #[classattr]
            fn __match_args__(py: pyo3::Python<'_>) -> pyo3::PyResult<pyo3::Py<pyo3::types::PyTuple>> {
                let names: &[&str] = &[$(stringify!($m)),*];
                Ok(pyo3::types::PyTuple::new(py, names)?.unbind())
            }

            #[gen_stub(skip)]
            fn __repr__(&self, py: pyo3::Python<'_>) -> pyo3::PyResult<String> {
                crate::views::dunder::repr_pairs($name, &self.dunder_fields(py)?)
            }

            #[gen_stub(skip)]
            fn __eq__(
                &self,
                py: pyo3::Python<'_>,
                other: &pyo3::Bound<'_, pyo3::PyAny>,
            ) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
                let Ok(other) = other.cast_exact::<Self>() else {
                    return Ok(py.NotImplemented());
                };
                crate::views::dunder::eq_pairs(py, &self.dunder_fields(py)?, &other.get().dunder_fields(py)?)
            }

            // A frozen view is immutable, so copy and deep-copy are identity.
            #[gen_stub(skip)]
            fn __copy__(slf: pyo3::PyRef<'_, Self>) -> pyo3::PyRef<'_, Self> {
                slf
            }

            #[gen_stub(skip)]
            fn __deepcopy__<'py>(
                slf: pyo3::PyRef<'py, Self>,
                _memo: &pyo3::Bound<'py, pyo3::PyAny>,
            ) -> pyo3::PyRef<'py, Self> {
                slf
            }
        }
    };
    (@hash $cls:ident) => {
        #[pyo3::pymethods]
        impl $cls {
            fn __hash__(&self, py: pyo3::Python<'_>) -> pyo3::PyResult<isize> {
                crate::views::dunder::hash_pairs(py, &self.dunder_fields(py)?)
            }
        }
    };
}

pub(crate) use view_dunders;

/// `__copy__`/`__deepcopy__` returning identity for a frozen view that does not
/// use `view_dunders!` (its dunders are hand-written). Immutable ⇒ copy is self.
macro_rules! frozen_copy {
    ($cls:ident) => {
        #[pyo3::pymethods]
        impl $cls {
            fn __copy__(slf: pyo3::PyRef<'_, Self>) -> pyo3::PyRef<'_, Self> {
                slf
            }

            fn __deepcopy__<'py>(
                slf: pyo3::PyRef<'py, Self>,
                _memo: &pyo3::Bound<'py, pyo3::PyAny>,
            ) -> pyo3::PyRef<'py, Self> {
                slf
            }
        }
    };
}

pub(crate) use frozen_copy;
