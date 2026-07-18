use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyFrozenSet, PyTuple};
use sonic_rs::Value;

use cc_transcript_core::ids;
use cc_transcript_core::toolcall::{self, EditSpan, ToolCall};
use cc_transcript_core::value::normalize_last_wins;

use crate::toolcall::tool_input_error;
use crate::views::convert::{json_to_py, opt_json};
use crate::views::dunder::view_dunders;

/// Common shape of every typed tool call.
///
/// Attributes:
///     name: The tool name exactly as invoked (aliases are not normalized —
///         the digest must match what the hook saw).
///     raw: The verbatim input mapping; the only digest substrate.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "ToolCallBase",
    module = "cc_transcript.tools",
    frozen,
    subclass
)]
pub(crate) struct ToolCallBaseView {
    pub call: Arc<ToolCall>,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl ToolCallBaseView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.call.name().to_string())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "collections.abc.Mapping[str, typing.Any]", imports = ("collections.abc", "typing")))]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.call.raw())
    }

    /// The cross-language content digest of this call.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolDigest", imports = ("cc_transcript.ids",)))]
    fn digest(&self, _py: Python<'_>) -> PyResult<String> {
        ids::tool_digest(self.call.name(), self.call.raw()).map_err(PyValueError::new_err)
    }
}

view_dunders!(
    ToolCallBaseView,
    "ToolCallBase",
    fields = [name],
    match_args = []
);

macro_rules! call_variant {
    ($view:ident, $variant:ident, $core:ident) => {
        impl $view {
            fn c(&self) -> &toolcall::$core {
                match &*self.call {
                    ToolCall::$variant(c) => c,
                    _ => unreachable!(concat!(stringify!($view), " over another variant")),
                }
            }
        }
    };
}

/// A Bash/Execute shell invocation.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "BashCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct BashCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(BashCallView, Bash, BashCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl BashCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().command.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "int | None", imports = ()))]
    fn timeout<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().timeout.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().description.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "bool | None", imports = ()))]
    fn run_in_background<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().run_in_background.as_ref())
    }

    /// The command parsed into a :class:`~cc_transcript.command.CommandLine`.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.command.CommandLine", imports = ("cc_transcript.command",)))]
    fn command_line<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("cc_transcript.command")?
            .getattr("parse_command_line")?
            .call1((self.c().command.clone(),))
    }
}

view_dunders!(
    BashCallView,
    "BashCall",
    fields = [name, command, timeout, description, run_in_background],
    match_args = []
);

/// An Edit replacement of ``old`` with ``new`` in one file.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "EditCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct EditCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(EditCallView, Edit, EditCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl EditCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file_path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().file_path.clone())
    }

    #[getter]
    fn old(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().old.clone())
    }

    #[getter]
    fn new(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().new.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "bool", imports = ()))]
    fn replace_all<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().replace_all)
    }
}

view_dunders!(
    EditCallView,
    "EditCall",
    fields = [name, file_path, old, new, replace_all],
    match_args = []
);

/// One replacement within a MultiEdit call, in application order.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "EditSpan", module = "cc_transcript.tools", frozen)]
pub(crate) struct EditSpanView {
    pub span: EditSpan,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl EditSpanView {
    #[new]
    #[pyo3(signature = (old, new, replace_all = false))]
    fn py_new(old: String, new: String, replace_all: bool) -> Self {
        EditSpanView {
            span: EditSpan {
                old,
                new,
                replace_all,
            },
        }
    }

    #[getter]
    fn old(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.span.old.clone())
    }

    #[getter]
    fn new(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.span.new.clone())
    }

    #[getter]
    fn replace_all(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.span.replace_all)
    }
}

view_dunders!(EditSpanView, "EditSpan", fields = [old, new, replace_all]);

/// A MultiEdit applying ``edits`` to one file, in order.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "MultiEditCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct MultiEditCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(MultiEditCallView, MultiEdit, MultiEditCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl MultiEditCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file_path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().file_path.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[cc_transcript.tools.EditSpan, ...]", imports = ("cc_transcript.tools",)))]
    fn edits<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(
            py,
            self.c()
                .edits
                .iter()
                .map(|span| Bound::new(py, EditSpanView { span: span.clone() }))
                .collect::<PyResult<Vec<_>>>()?,
        )
    }
}

view_dunders!(
    MultiEditCallView,
    "MultiEditCall",
    fields = [name, file_path, edits],
    match_args = []
);

/// A Write/Create of a whole file.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "WriteCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct WriteCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(WriteCallView, Write, WriteCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl WriteCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file_path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().file_path.clone())
    }

    #[getter]
    fn content(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().content.clone())
    }
}

view_dunders!(
    WriteCallView,
    "WriteCall",
    fields = [name, file_path, content],
    match_args = []
);

/// A Read of a file, optionally windowed.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "ReadCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct ReadCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(ReadCallView, Read, ReadCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl ReadCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file_path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().file_path.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "int | None", imports = ()))]
    fn offset<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().offset.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "int | None", imports = ()))]
    fn limit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().limit.as_ref())
    }
}

view_dunders!(
    ReadCallView,
    "ReadCall",
    fields = [name, file_path, offset, limit],
    match_args = []
);

/// A NotebookEdit replacing a cell's source.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "NotebookEditCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct NotebookEditCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(NotebookEditCallView, NotebookEdit, NotebookEditCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl NotebookEditCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn notebook_path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().notebook_path.clone())
    }

    #[getter]
    fn new_source(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().new_source.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn cell_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().cell_id.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn edit_mode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().edit_mode.as_ref())
    }
}

view_dunders!(
    NotebookEditCallView,
    "NotebookEditCall",
    fields = [name, notebook_path, new_source, cell_id, edit_mode],
    match_args = []
);

/// A Grep content search.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "GrepCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct GrepCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(GrepCallView, Grep, GrepCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl GrepCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn pattern(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().pattern.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None"))]
    fn path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().path.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn glob<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().glob.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn file_type<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().file_type.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn output_mode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().output_mode.as_ref())
    }
}

view_dunders!(
    GrepCallView,
    "GrepCall",
    fields = [name, pattern, path, glob, file_type, output_mode],
    match_args = []
);

/// A Glob file-pattern search.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "GlobCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct GlobCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(GlobCallView, Glob, GlobCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl GlobCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn pattern(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().pattern.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().path.as_ref())
    }
}

view_dunders!(
    GlobCallView,
    "GlobCall",
    fields = [name, pattern, path],
    match_args = []
);

/// An Agent/Task subagent dispatch.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "TaskCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct TaskCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(TaskCallView, Task, TaskCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl TaskCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn prompt(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().prompt.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn agent_type<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().agent_type.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn model<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().model.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn agent_name<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().agent_name.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "bool | None", imports = ()))]
    fn run_in_background<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().run_in_background.as_ref())
    }
}

view_dunders!(
    TaskCallView,
    "TaskCall",
    fields = [
        name,
        prompt,
        agent_type,
        model,
        agent_name,
        run_in_background
    ],
    match_args = []
);

/// A Workflow dynamic-orchestration dispatch.
///
/// Attributes:
///     script: The inline workflow script, when passed directly.
///     script_path: Path to a script file on disk, when passed instead of
///         ``script``.
///     workflow_name: A predefined workflow's name (``raw["name"]`` — distinct
///         from :attr:`ToolCallBase.name`, the tool name).
///     args: The value exposed to the script as its ``args`` global.
///     resume_from_run_id: A prior run to resume from.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "WorkflowCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct WorkflowCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(WorkflowCallView, Workflow, WorkflowCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl WorkflowCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn script<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().script.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn script_path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().script_path.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn workflow_name<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().workflow_name.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Any", imports = ("typing",)))]
    fn args<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().args.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn resume_from_run_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().resume_from_run_id.as_ref())
    }
}

view_dunders!(
    WorkflowCallView,
    "WorkflowCall",
    fields = [
        name,
        script,
        script_path,
        workflow_name,
        args,
        resume_from_run_id
    ],
    match_args = []
);

/// A Skill invocation.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "SkillCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct SkillCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(SkillCallView, Skill, SkillCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl SkillCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn skill(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().skill.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn args<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().args.as_ref())
    }
}

view_dunders!(
    SkillCallView,
    "SkillCall",
    fields = [name, skill, args],
    match_args = []
);

/// A TaskCreate tracker entry.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "TaskCreateCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct TaskCreateCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(TaskCreateCallView, TaskCreate, TaskCreateCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl TaskCreateCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn subject(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().subject.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().description.as_ref())
    }
}

view_dunders!(
    TaskCreateCallView,
    "TaskCreateCall",
    fields = [name, subject, description],
    match_args = []
);

/// A TaskUpdate tracker change.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "TaskUpdateCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct TaskUpdateCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(TaskUpdateCallView, TaskUpdate, TaskUpdateCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl TaskUpdateCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn task_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().task_id.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().status.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn subject<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().subject.as_ref())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "str | None", imports = ()))]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().description.as_ref())
    }
}

view_dunders!(
    TaskUpdateCallView,
    "TaskUpdateCall",
    fields = [name, task_id, status, subject, description],
    match_args = []
);

/// An ExitPlanMode/ExitSpecMode plan submission.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "ExitPlanModeCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct ExitPlanModeCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(ExitPlanModeCallView, ExitPlanMode, ExitPlanModeCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl ExitPlanModeCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn plan(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().plan.clone())
    }
}

view_dunders!(
    ExitPlanModeCallView,
    "ExitPlanModeCall",
    fields = [name, plan],
    match_args = []
);

/// A tool the platform does not type: unknown names, MCP tools, and — under
/// ``on_error='other'`` — known tools whose input failed to parse.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "OtherCall", module = "cc_transcript.tools", extends = ToolCallBaseView, frozen)]
pub(crate) struct OtherCallView {
    pub call: Arc<ToolCall>,
}

call_variant!(OtherCallView, Other, OtherCall);

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl OtherCallView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }
}

view_dunders!(OtherCallView, "OtherCall", fields = [name], match_args = []);

/// A before/after content pair lowered from an edit-shaped tool call.
///
/// Attributes:
///     old: The content replaced; empty for pure additions such as Write.
///     new: The content written.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "Hunk", module = "cc_transcript.tools", frozen)]
pub(crate) struct HunkView {
    pub old: String,
    pub new: String,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HunkView {
    #[new]
    fn py_new(old: String, new: String) -> Self {
        HunkView { old, new }
    }

    #[getter]
    fn old(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.old.clone())
    }

    #[getter]
    fn new(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.new.clone())
    }
}

view_dunders!(HunkView, "Hunk", fields = [old, new]);

pub(crate) fn call_view<'py>(py: Python<'py>, call: Arc<ToolCall>) -> PyResult<Bound<'py, PyAny>> {
    let base = ToolCallBaseView {
        call: Arc::clone(&call),
    };
    let init = PyClassInitializer::from(base);
    match &*call {
        ToolCall::Bash(_) => {
            Ok(Bound::new(py, init.add_subclass(BashCallView { call }))?.into_any())
        }
        ToolCall::Edit(_) => {
            Ok(Bound::new(py, init.add_subclass(EditCallView { call }))?.into_any())
        }
        ToolCall::MultiEdit(_) => {
            Ok(Bound::new(py, init.add_subclass(MultiEditCallView { call }))?.into_any())
        }
        ToolCall::Write(_) => {
            Ok(Bound::new(py, init.add_subclass(WriteCallView { call }))?.into_any())
        }
        ToolCall::Read(_) => {
            Ok(Bound::new(py, init.add_subclass(ReadCallView { call }))?.into_any())
        }
        ToolCall::NotebookEdit(_) => {
            Ok(Bound::new(py, init.add_subclass(NotebookEditCallView { call }))?.into_any())
        }
        ToolCall::Grep(_) => {
            Ok(Bound::new(py, init.add_subclass(GrepCallView { call }))?.into_any())
        }
        ToolCall::Glob(_) => {
            Ok(Bound::new(py, init.add_subclass(GlobCallView { call }))?.into_any())
        }
        ToolCall::Task(_) => {
            Ok(Bound::new(py, init.add_subclass(TaskCallView { call }))?.into_any())
        }
        ToolCall::Workflow(_) => {
            Ok(Bound::new(py, init.add_subclass(WorkflowCallView { call }))?.into_any())
        }
        ToolCall::Skill(_) => {
            Ok(Bound::new(py, init.add_subclass(SkillCallView { call }))?.into_any())
        }
        ToolCall::TaskCreate(_) => {
            Ok(Bound::new(py, init.add_subclass(TaskCreateCallView { call }))?.into_any())
        }
        ToolCall::TaskUpdate(_) => {
            Ok(Bound::new(py, init.add_subclass(TaskUpdateCallView { call }))?.into_any())
        }
        ToolCall::ExitPlanMode(_) => {
            Ok(Bound::new(py, init.add_subclass(ExitPlanModeCallView { call }))?.into_any())
        }
        ToolCall::Other(_) => {
            Ok(Bound::new(py, init.add_subclass(OtherCallView { call }))?.into_any())
        }
    }
}

pub(crate) fn parse_call_view(py: Python<'_>, name: &str, input: &Value) -> PyResult<Py<PyAny>> {
    match toolcall::parse_tool_call_strict(name, input) {
        Ok(call) => Ok(call_view(py, Arc::new(call))?.unbind()),
        Err(err) => Err(tool_input_error(py, name, &err)),
    }
}

/// Parse a tool's name and raw input (as a JSON document) into the typed view
/// hierarchy; the ``cc_transcript.tools`` facade owns the public signature.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (name, input_json, on_error=None))]
#[gen_stub(override_return_type(type_repr = "cc_transcript.tools.ToolCall", imports = ("cc_transcript.tools",)))]
pub(crate) fn toolcall_parse_view<'py>(
    py: Python<'py>,
    name: &str,
    input_json: &str,
    on_error: Option<&str>,
) -> PyResult<Bound<'py, PyAny>> {
    let mut input: Value = sonic_rs::from_str(input_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    normalize_last_wins(&mut input);
    match on_error.unwrap_or("raise") {
        "raise" => match toolcall::parse_tool_call_strict(name, &input) {
            Ok(call) => call_view(py, Arc::new(call)),
            Err(err) => Err(tool_input_error(py, name, &err)),
        },
        _ => call_view(py, Arc::new(toolcall::parse_tool_call(name, &input))),
    }
}

/// Lower an edit-shaped call to before/after hunks; ``()`` for the rest.
///
/// MultiEdit yields one hunk per span in application order — never just the
/// first. Write and NotebookEdit are pure additions with an empty old side.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "tuple[cc_transcript.tools.Hunk, ...]", imports = ("cc_transcript.tools",)))]
pub(crate) fn hunks_of<'py>(
    py: Python<'py>,
    #[gen_stub(override_type(type_repr = "cc_transcript.tools.ToolCall | cc_transcript.tools.FallbackCall", imports = ("cc_transcript.tools",)))]
    call: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyTuple>> {
    let base = call.cast::<ToolCallBaseView>()?.get();
    PyTuple::new(
        py,
        base.call
            .hunks()
            .into_iter()
            .map(|h| {
                Bound::new(
                    py,
                    HunkView {
                        old: h.old,
                        new: h.new,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?,
    )
}

/// The file a call targets, when it targets one.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn file_path_of(
    #[gen_stub(override_type(type_repr = "cc_transcript.tools.ToolCall | cc_transcript.tools.FallbackCall", imports = ("cc_transcript.tools",)))]
    call: &Bound<'_, PyAny>,
) -> PyResult<Option<String>> {
    Ok(call
        .cast::<ToolCallBaseView>()?
        .get()
        .call
        .file_path()
        .map(str::to_string))
}

/// Expand a pipe-separated tool spec to include alias and MCP bare spellings.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "frozenset[str]", imports = ()))]
pub(crate) fn expand_tool_names<'py>(
    py: Python<'py>,
    spec: &str,
) -> PyResult<Bound<'py, PyFrozenSet>> {
    PyFrozenSet::new(py, toolcall::expand_tool_names(spec))
}

/// Whether ``actual`` is one of ``names``, exactly or as an MCP tool suffix.
///
/// True when ``actual`` is in ``names``, or when it splits as
/// ``mcp__<server>__<tool>`` on the first two ``__`` and ``<tool>`` — or its
/// native built-in edit-gate equivalent — is in ``names``. That alias closes
/// the edit-gate bypass where cc-context's ``ccx_code_edit`` /
/// ``ccx_code_replace`` write through the MCP under names no ``Edit``/``Write``
/// gate would catch. Display-name aliases from
/// :data:`~cc_transcript.tools.TOOL_ALIASES` are not closed over — ``names`` is
/// taken verbatim; pre-expand with :func:`expand_tool_names` for those.
///
/// Example:
///     >>> matches_names("mcp__github__Grep", {"Grep"})
///     True
///     >>> matches_names("mcp__cc-context__ccx_code_edit", {"Edit"})
///     True
///     >>> matches_names("Execute", {"Bash"})
///     False
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn matches_names(
    actual: &str,
    #[gen_stub(override_type(type_repr = "collections.abc.Container[str]", imports = ("collections.abc",)))]
    names: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if names.contains(actual)? {
        return Ok(true);
    }
    match toolcall::mcp_parts(actual) {
        Some((_, tool)) => Ok(names.contains(tool)?
            || match toolcall::mcp_tool_alias(tool) {
                Some(builtin) => names.contains(builtin)?,
                None => false,
            }),
        None => Ok(false),
    }
}

/// Whether ``actual`` matches a pipe spec, honoring aliases and MCP suffixes.
///
/// Example:
///     >>> tool_name_matches("Execute", "Bash|Grep")
///     True
///     >>> tool_name_matches("mcp__github__Grep", "Grep")
///     True
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn tool_name_matches(actual: &str, spec: &str) -> PyResult<bool> {
    Ok(toolcall::tool_name_matches(actual, spec))
}

/// Split an ``mcp__server__tool`` name into ``(server, tool)``, else ``None``.
///
/// Example:
///     >>> mcp_parts("mcp__semble__search")
///     ('semble', 'search')
///     >>> mcp_parts("Bash") is None
///     True
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "tuple[str, str] | None"))]
pub(crate) fn mcp_parts(name: &str) -> PyResult<Option<(String, String)>> {
    Ok(toolcall::mcp_parts(name).map(|(server, tool)| (server.to_string(), tool.to_string())))
}

/// Classify an MCP tool segment as ``"read"`` or ``"write"`` by its verbs.
///
/// Returns ``"read"`` when ``tool`` starts with, or has an underscore-delimited
/// token equal to, a read verb (``get``, ``list``, ``search``, …); otherwise
/// ``"write"``. The token check catches namespaced names like ``ccx_read``.
///
/// Example:
///     >>> mcp_access("search")
///     'read'
///     >>> mcp_access("ccx_read")
///     'read'
///     >>> mcp_access("deploy")
///     'write'
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[gen_stub(override_return_type(type_repr = "typing.Literal[\"read\", \"write\"]", imports = ("typing",)))]
pub(crate) fn mcp_access(tool: &str) -> PyResult<&'static str> {
    Ok(toolcall::mcp_access(tool))
}

pub(crate) fn add_classes(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ToolCallBaseView>()?;
    m.add_class::<BashCallView>()?;
    m.add_class::<EditCallView>()?;
    m.add_class::<MultiEditCallView>()?;
    m.add_class::<WriteCallView>()?;
    m.add_class::<ReadCallView>()?;
    m.add_class::<NotebookEditCallView>()?;
    m.add_class::<GrepCallView>()?;
    m.add_class::<GlobCallView>()?;
    m.add_class::<TaskCallView>()?;
    m.add_class::<WorkflowCallView>()?;
    m.add_class::<SkillCallView>()?;
    m.add_class::<TaskCreateCallView>()?;
    m.add_class::<TaskUpdateCallView>()?;
    m.add_class::<ExitPlanModeCallView>()?;
    m.add_class::<OtherCallView>()?;
    m.add_class::<EditSpanView>()?;
    m.add_class::<HunkView>()?;
    m.add_function(pyo3::wrap_pyfunction!(toolcall_parse_view, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(hunks_of, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(file_path_of, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(expand_tool_names, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(matches_names, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(tool_name_matches, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(mcp_parts, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(mcp_access, m)?)?;
    Ok(())
}
