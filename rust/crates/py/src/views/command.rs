use std::collections::BTreeMap;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyType};
use pyo3::IntoPyObjectExt;

use cc_transcript_core::command::{dequote, Command, CommandLine, Redirect};

use crate::views::dunder::view_dunders;

/// A shell redirect parsed from a bash command (e.g. ``> file.txt``, ``2>&1``).
///
/// Attributes:
///     op: The redirect operator (``>``, ``>>``, ``2>&1`` yields ``>&``).
///     target: The redirect target word.
///     fd: The leading file descriptor number, or None when unspecified.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "Redirect",
    module = "cc_transcript.command",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub(crate) struct RedirectView {
    pub r: Redirect,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl RedirectView {
    #[new]
    #[pyo3(signature = (op, target, fd=None))]
    fn new(op: String, target: String, fd: Option<i64>) -> Self {
        RedirectView {
            r: Redirect { op, target, fd },
        }
    }

    #[getter]
    fn op(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.op.clone())
    }

    #[getter]
    fn target(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.target.clone())
    }

    #[getter]
    fn fd(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.r.fd)
    }
}

view_dunders!(RedirectView, "Redirect", fields = [op, target, fd]);

/// A single parsed shell command with executable, arguments, env vars, and redirects.
///
/// Use ``Command.parse(raw)`` to parse a command string, or access via ``CommandLine``.
///
/// Attributes:
///     raw: The command's source text.
///     executable: The command name, or "" when nothing parsed.
///     args: The arguments after the executable.
///     env: The leading ``VAR=val`` assignments, as name/value pairs.
///     redirects: The command's file redirects.
///     span: The command's byte span in the line, or None when a redirect
///         absorbed a trailing word; excluded from equality and repr.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "Command",
    module = "cc_transcript.command",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub(crate) struct CommandView {
    pub cmd: Command,
}

impl CommandView {
    fn str_value(&self) -> String {
        let argv = self.cmd.argv();
        if argv.is_empty() {
            self.cmd.raw.clone()
        } else {
            argv.join(" ")
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl CommandView {
    #[new]
    #[pyo3(signature = (raw, executable, args, env=None, redirects=None, span=None))]
    fn new(
        raw: String,
        executable: String,
        args: Vec<String>,
        env: Option<Vec<(String, String)>>,
        redirects: Option<Vec<RedirectView>>,
        span: Option<(usize, usize)>,
    ) -> Self {
        CommandView {
            cmd: Command {
                raw,
                executable,
                args,
                env: env.unwrap_or_default(),
                redirects: redirects
                    .unwrap_or_default()
                    .into_iter()
                    .map(|rv| rv.r)
                    .collect(),
                span,
            },
        }
    }

    #[classmethod]
    fn parse(_cls: &Bound<'_, PyType>, raw: &str) -> Option<CommandView> {
        CommandLine::parse(raw)
            .primary()
            .cloned()
            .map(|cmd| CommandView { cmd })
    }

    #[getter]
    fn raw(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.cmd.raw.clone())
    }

    #[getter]
    fn executable(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.cmd.executable.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn args<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(py, &self.cmd.args)?.into_bound_py_any(py)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[tuple[str, str], ...]"))]
    fn env<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(
            py,
            self.cmd
                .env
                .iter()
                .map(|(name, value)| pyo3::types::PyTuple::new(py, [name, value]))
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .into_bound_py_any(py)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[cc_transcript.command.Redirect, ...]", imports = ("cc_transcript.command",)))]
    fn redirects<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(
            py,
            self.cmd
                .redirects
                .iter()
                .map(|r| RedirectView { r: r.clone() }),
        )?
        .into_bound_py_any(py)
    }

    #[getter]
    fn span(&self, _py: Python<'_>) -> PyResult<Option<(usize, usize)>> {
        Ok(self.cmd.span)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn argv<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(py, self.cmd.argv())?.into_bound_py_any(py)
    }

    #[getter]
    fn program(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.cmd.program().to_string())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "dict[str, str]"))]
    fn env_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (name, value) in &self.cmd.env {
            dict.set_item(name, value)?;
        }
        Ok(dict)
    }

    // Parity: command.py Command.unwrapped returns self when nothing strips (identity preserved).
    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.command.Command", imports = ("cc_transcript.command",)))]
    fn unwrapped<'py>(slf: Bound<'py, Self>, py: Python<'py>) -> PyResult<Bound<'py, Self>> {
        let unwrapped = slf.borrow().cmd.unwrapped();
        if unwrapped == slf.borrow().cmd {
            Ok(slf)
        } else {
            Bound::new(py, CommandView { cmd: unwrapped })
        }
    }

    #[getter]
    fn prefix(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.cmd.prefix())
    }

    #[pyo3(signature = (*argv))]
    fn runs(&self, argv: Vec<String>) -> bool {
        self.cmd
            .runs(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn matches(&self, py: Python<'_>, pattern: &str) -> PyResult<bool> {
        py.import("re")?
            .call_method1("search", (pattern, self.str_value()))?
            .is_truthy()
    }

    #[pyo3(signature = (*patterns))]
    fn has_arg(&self, py: Python<'_>, patterns: Vec<String>) -> PyResult<bool> {
        let re = py.import("re")?;
        for pattern in &patterns {
            for arg in &self.cmd.args {
                if re.call_method1("search", (pattern, arg))?.is_truthy()? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn __str__(&self) -> String {
        self.str_value()
    }

    fn __contains__(&self, item: &str) -> bool {
        self.str_value().contains(item)
    }

    fn __bool__(&self) -> bool {
        !self.cmd.executable.is_empty()
    }
}

view_dunders!(
    CommandView,
    "Command",
    fields = [raw, executable, args, env, redirects]
);

/// One command of a ``CommandLine`` with its position and joining context.
///
/// Attributes:
///     line: The command line this occurrence belongs to.
///     index: This command's index into ``line.parts``.
///     command: The command at this occurrence's index.
///     prev_op: The operator joining the previous command; None at index 0.
///     next_op: The operator joining this command to the next; None for the final command.
///     piped: Whether this command sits on either side of a pipe.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "Occurrence", module = "cc_transcript.command", frozen)]
pub(crate) struct OccurrenceView {
    pub line: Arc<CommandLine>,
    pub index: usize,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl OccurrenceView {
    #[new]
    fn new(line: CommandLineView, index: usize) -> Self {
        OccurrenceView {
            line: Arc::clone(&line.line),
            index,
        }
    }

    #[getter]
    fn line(&self, _py: Python<'_>) -> PyResult<CommandLineView> {
        Ok(CommandLineView {
            line: Arc::clone(&self.line),
        })
    }

    #[getter]
    fn index(&self, _py: Python<'_>) -> PyResult<usize> {
        Ok(self.index)
    }

    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<CommandView> {
        Ok(CommandView {
            cmd: self.line.parts[self.index].0.clone(),
        })
    }

    #[getter]
    fn prev_op(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.line.prev_op(self.index).map(str::to_string))
    }

    #[getter]
    fn next_op(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.line.next_op(self.index).map(str::to_string))
    }

    #[getter]
    fn piped(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.line.piped(self.index))
    }
}

view_dunders!(OccurrenceView, "Occurrence", fields = [line, index]);

/// Predicate helpers for inspecting a parsed ``CommandLine``. Obtain one via ``CommandLine.q``.
///
/// Attributes:
///     line: The command line these predicates run over.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "CommandLineQuery", module = "cc_transcript.command", frozen)]
pub(crate) struct CommandLineQueryView {
    pub line: Arc<CommandLine>,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl CommandLineQueryView {
    #[new]
    fn new(line: CommandLineView) -> Self {
        CommandLineQueryView {
            line: Arc::clone(&line.line),
        }
    }

    #[getter]
    fn line(&self, _py: Python<'_>) -> PyResult<CommandLineView> {
        Ok(CommandLineView {
            line: Arc::clone(&self.line),
        })
    }

    #[pyo3(signature = (*argv))]
    fn runs(&self, argv: Vec<String>) -> bool {
        self.line
            .q()
            .runs(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn has_subcommand(&self, name: &str) -> bool {
        self.line.q().has_subcommand(name)
    }

    fn any_command(
        &self,
        #[gen_stub(override_type(type_repr = "collections.abc.Callable[[cc_transcript.command.Command], bool]", imports = ("collections.abc", "cc_transcript.command")))]
        pred: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        for cmd in self.line.commands() {
            if pred
                .call1((CommandView { cmd: cmd.clone() },))?
                .is_truthy()?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn uses_redirect(&self) -> bool {
        self.line.q().uses_redirect()
    }

    fn contains_token(&self, token: &str) -> bool {
        self.line.q().contains_token(token)
    }
}

view_dunders!(CommandLineQueryView, "CommandLineQuery", fields = [line]);

/// A full parsed bash command line, potentially containing multiple commands joined by operators.
///
/// Use ``CommandLine.parse(raw)`` (or the cached ``parse_command_line``) to parse. Access
/// individual commands via ``.commands`` or the final command via ``.primary``.
///
/// Attributes:
///     raw: The line's source text.
///     parts: Each command paired with the operator that follows it (None for the last).
///     commands: The parsed commands, in line order.
///     primary: The final command, or None when nothing parsed.
///     head: The first command, or None when nothing parsed.
///     prefixes: The permission-style prefix of each command, absent prefixes dropped.
///     q: The predicate helper for this line.
///     occurrences: One Occurrence per part, in line order.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "CommandLine",
    module = "cc_transcript.command",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub(crate) struct CommandLineView {
    pub line: Arc<CommandLine>,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl CommandLineView {
    #[new]
    fn new(
        raw: String,
        #[gen_stub(override_type(type_repr = "tuple[tuple[cc_transcript.command.Command, str | None], ...]", imports = ("cc_transcript.command",)))]
        parts: Vec<(CommandView, Option<String>)>,
    ) -> Self {
        CommandLineView {
            line: Arc::new(CommandLine {
                raw,
                parts: parts.into_iter().map(|(cv, op)| (cv.cmd, op)).collect(),
            }),
        }
    }

    #[classmethod]
    fn parse(_cls: &Bound<'_, PyType>, raw: &str) -> CommandLineView {
        CommandLineView {
            line: Arc::new(CommandLine::parse(raw)),
        }
    }

    #[staticmethod]
    fn dequote(text: &str) -> String {
        dequote(text).to_string()
    }

    #[getter]
    fn raw(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.line.raw.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[tuple[cc_transcript.command.Command, str | None], ...]", imports = ("cc_transcript.command",)))]
    fn parts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(
            py,
            self.line
                .parts
                .iter()
                .map(|(cmd, op)| {
                    pyo3::types::PyTuple::new(
                        py,
                        [
                            CommandView { cmd: cmd.clone() }.into_bound_py_any(py)?,
                            op.clone().into_bound_py_any(py)?,
                        ],
                    )
                })
                .collect::<PyResult<Vec<_>>>()?,
        )?
        .into_bound_py_any(py)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[cc_transcript.command.Command, ...]", imports = ("cc_transcript.command",)))]
    fn commands<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(
            py,
            self.line
                .parts
                .iter()
                .map(|(cmd, _)| CommandView { cmd: cmd.clone() }),
        )?
        .into_bound_py_any(py)
    }

    #[getter]
    fn primary(&self, _py: Python<'_>) -> PyResult<Option<CommandView>> {
        Ok(self.line.primary().cloned().map(|cmd| CommandView { cmd }))
    }

    #[getter]
    fn head(&self, _py: Python<'_>) -> PyResult<Option<CommandView>> {
        Ok(self.line.head().cloned().map(|cmd| CommandView { cmd }))
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn prefixes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(py, self.line.prefixes())?.into_bound_py_any(py)
    }

    #[getter]
    fn q(&self, _py: Python<'_>) -> PyResult<CommandLineQueryView> {
        Ok(CommandLineQueryView {
            line: Arc::clone(&self.line),
        })
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[cc_transcript.command.Occurrence, ...]", imports = ("cc_transcript.command",)))]
    fn occurrences<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3::types::PyTuple::new(
            py,
            (0..self.line.parts.len()).map(|index| OccurrenceView {
                line: Arc::clone(&self.line),
                index,
            }),
        )?
        .into_bound_py_any(py)
    }

    fn splice(&self, replacements: BTreeMap<usize, String>) -> PyResult<String> {
        self.line
            .splice(&replacements)
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    fn rewrite_occurrences(
        &self,
        #[gen_stub(override_type(type_repr = "collections.abc.Callable[[cc_transcript.command.Occurrence], str | None]", imports = ("collections.abc", "cc_transcript.command")))]
        to: &Bound<'_, PyAny>,
    ) -> PyResult<Option<String>> {
        let mut replacements: BTreeMap<usize, String> = BTreeMap::new();
        for index in 0..self.line.parts.len() {
            let occ = OccurrenceView {
                line: Arc::clone(&self.line),
                index,
            };
            let result = to.call1((occ,))?;
            if !result.is_none() {
                replacements.insert(index, result.extract::<String>()?);
            }
        }
        if replacements.is_empty() {
            return Ok(None);
        }
        self.line
            .splice(&replacements)
            .map(Some)
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    #[gen_stub(override_return_type(type_repr = "collections.abc.Iterator[cc_transcript.command.Command]", imports = ("collections.abc", "cc_transcript.command")))]
    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let commands: Vec<CommandView> = self
            .line
            .parts
            .iter()
            .map(|(cmd, _)| CommandView { cmd: cmd.clone() })
            .collect();
        Ok(PyList::new(py, commands)?.into_any().try_iter()?.into_any())
    }

    fn __len__(&self) -> usize {
        self.line.parts.len()
    }

    fn __str__(&self) -> String {
        self.line.raw.clone()
    }

    fn __contains__(&self, item: &str) -> bool {
        self.line.raw.contains(item)
    }

    fn __bool__(&self) -> bool {
        !self.line.parts.is_empty()
    }
}

view_dunders!(CommandLineView, "CommandLine", fields = [raw, parts]);
