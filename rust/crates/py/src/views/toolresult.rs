use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use sonic_rs::{JsonContainerTrait, Value};

use cc_transcript_core::toolcall::{self, QuestionAnnotation, ToolResult};

use crate::views::convert::{json_to_py, opt_json};
use crate::views::dunder::view_dunders;
use crate::views::meta::QuestionView;

/// Common shape of every typed tool result.
///
/// Attributes:
///     name: The tool name exactly as invoked (aliases are not normalized).
///     raw: The verbatim ``toolUseResult`` payload — a mapping for structured
///         results, a plain string for denials, or None when the record carried
///         none. Excluded from equality and repr.
#[pyclass(
    name = "ToolResultBase",
    module = "cc_transcript.tools",
    frozen,
    subclass
)]
pub(crate) struct ToolResultBaseView {
    pub result: Arc<ToolResult>,
}

#[pymethods]
impl ToolResultBaseView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.result.name().to_string())
    }

    #[getter]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.result.raw())
    }
}

view_dunders!(
    ToolResultBaseView,
    "ToolResultBase",
    fields = [name],
    match_args = []
);

macro_rules! result_variant {
    ($view:ident, $variant:ident, $core:ident) => {
        impl $view {
            fn c(&self) -> &toolcall::$core {
                match &*self.result {
                    ToolResult::$variant(r) => r,
                    _ => unreachable!(concat!(stringify!($view), " over another variant")),
                }
            }
        }
    };
}

/// A Bash/Execute execution result.
#[pyclass(name = "BashResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct BashResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(BashResultView, Bash, BashResult);

#[pymethods]
impl BashResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn stdout<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().stdout.as_ref())
    }

    #[getter]
    fn stderr<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().stderr.as_ref())
    }

    #[getter]
    fn interrupted<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().interrupted)
    }

    #[getter]
    fn is_image<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().is_image)
    }

    #[getter]
    fn no_output_expected<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().no_output_expected)
    }

    #[getter]
    fn background_task_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().background_task_id.as_ref())
    }

    #[getter]
    fn return_code_interpretation<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().return_code_interpretation.as_ref())
    }
}

view_dunders!(
    BashResultView,
    "BashResult",
    fields = [
        name,
        stdout,
        stderr,
        interrupted,
        is_image,
        no_output_expected,
        background_task_id,
        return_code_interpretation,
    ],
    match_args = []
);

/// An Edit result: the applied replacement and its structured patch.
///
/// ``structured_patch`` and ``original_file`` are kept verbatim — the patch as
/// the raw hunk list Claude Code emits, the original file as its full text.
#[pyclass(name = "EditResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct EditResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(EditResultView, Edit, EditResult);

#[pymethods]
impl EditResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file_path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().file_path.as_ref())
    }

    #[getter]
    fn old_string<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().old_string.as_ref())
    }

    #[getter]
    fn new_string<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().new_string.as_ref())
    }

    #[getter]
    fn replace_all<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().replace_all)
    }

    #[getter]
    fn user_modified<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().user_modified)
    }

    #[getter]
    fn stale_recovered<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().stale_recovered)
    }

    #[getter]
    fn structured_patch<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().structured_patch.as_ref())
    }

    #[getter]
    fn original_file<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().original_file.as_ref())
    }
}

view_dunders!(
    EditResultView,
    "EditResult",
    fields = [
        name,
        file_path,
        old_string,
        new_string,
        replace_all,
        user_modified,
        stale_recovered,
        structured_patch,
        original_file,
    ],
    match_args = []
);

/// A Write/Create result: the written content and its structured patch.
#[pyclass(name = "WriteResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct WriteResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(WriteResultView, Write, WriteResult);

#[pymethods]
impl WriteResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().content.as_ref())
    }

    #[getter]
    fn file_path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().file_path.as_ref())
    }

    #[getter]
    fn original_file<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().original_file.as_ref())
    }

    #[getter]
    fn structured_patch<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().structured_patch.as_ref())
    }

    #[getter]
    fn user_modified<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().user_modified)
    }
}

view_dunders!(
    WriteResultView,
    "WriteResult",
    fields = [
        name,
        content,
        file_path,
        original_file,
        structured_patch,
        user_modified
    ],
    match_args = []
);

/// A Read result: the file payload and its content type.
///
/// ``file`` is kept verbatim — the raw mapping (``filePath``, ``content``,
/// ``numLines``, …) Claude Code emits for the read window.
#[pyclass(name = "ReadResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct ReadResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(ReadResultView, Read, ReadResult);

#[pymethods]
impl ReadResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn file<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().file.as_ref())
    }

    #[getter(r#type)]
    fn file_type<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().file_type.as_ref())
    }

    #[classattr]
    fn __match_args__(py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(py, Vec::<&str>::new())?.unbind())
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        crate::views::dunder::repr_pairs(
            "ReadResult",
            &[
                ("name", self.name(py)?.into_pyobject(py)?.into_any()),
                ("file", self.file(py)?),
                ("type", self.file_type(py)?),
            ],
        )
    }

    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let Ok(other) = other.cast_exact::<Self>() else {
            return Ok(py.NotImplemented());
        };
        let o = other.get();
        crate::views::dunder::eq_pairs(
            py,
            &[
                ("name", self.name(py)?.into_pyobject(py)?.into_any()),
                ("file", self.file(py)?),
                ("type", self.file_type(py)?),
            ],
            &[
                ("name", o.name(py)?.into_pyobject(py)?.into_any()),
                ("file", o.file(py)?),
                ("type", o.file_type(py)?),
            ],
        )
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        crate::views::dunder::hash_pairs(
            py,
            &[
                ("name", self.name(py)?.into_pyobject(py)?.into_any()),
                ("file", self.file(py)?),
                ("type", self.file_type(py)?),
            ],
        )
    }
}

/// The Agent/Task result family head; :meth:`from_raw` picks the variant.
///
/// A terminal run (``totalDurationMs``/``usage`` present) becomes a
/// :class:`TaskResult`; an in-flight launch (``outputFile`` present) becomes a
/// :class:`TaskLaunchResult`; a payload matching neither shape degrades to
/// :class:`OtherResult` rather than an all-None variant.
#[pyclass(
    name = "TaskResultBase",
    module = "cc_transcript.tools",
    extends = ToolResultBaseView,
    frozen,
    subclass
)]
pub(crate) struct TaskResultBaseView;

/// A completed Agent/Task subagent run.
///
/// ``tool_stats``, ``usage``, and ``content`` are kept verbatim — the raw
/// per-tool stats, usage mapping, and final content-block list the subagent
/// returned.
#[pyclass(name = "TaskResult", module = "cc_transcript.tools", extends = TaskResultBaseView, frozen)]
pub(crate) struct TaskResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(TaskResultView, Task, TaskResult);

#[pymethods]
impl TaskResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn agent_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().agent_id.as_ref())
    }

    #[getter]
    fn agent_type<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().agent_type.as_ref())
    }

    #[getter]
    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().status.as_ref())
    }

    #[getter]
    fn total_duration_ms<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().total_duration_ms.as_ref())
    }

    #[getter]
    fn total_tokens<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().total_tokens.as_ref())
    }

    #[getter]
    fn total_tool_use_count<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().total_tool_use_count.as_ref())
    }

    #[getter]
    fn tool_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().tool_stats.as_ref())
    }

    #[getter]
    fn usage<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().usage.as_ref())
    }

    #[getter]
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().content.as_ref())
    }

    #[getter]
    fn prompt<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().prompt.as_ref())
    }

    #[getter]
    fn resolved_model<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().resolved_model.as_ref())
    }
}

view_dunders!(
    TaskResultView,
    "TaskResult",
    fields = [
        name,
        agent_id,
        agent_type,
        status,
        total_duration_ms,
        total_tokens,
        total_tool_use_count,
        tool_stats,
        usage,
        content,
        prompt,
        resolved_model,
    ],
    match_args = []
);

/// An in-flight Agent/Task launch (async or backgrounded subagent).
#[pyclass(name = "TaskLaunchResult", module = "cc_transcript.tools", extends = TaskResultBaseView, frozen)]
pub(crate) struct TaskLaunchResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(TaskLaunchResultView, TaskLaunch, TaskLaunchResult);

#[pymethods]
impl TaskLaunchResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn agent_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().agent_id.as_ref())
    }

    #[getter]
    fn output_file<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().output_file.as_ref())
    }

    #[getter]
    fn is_async<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().is_async)
    }

    #[getter]
    fn can_read_output_file<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().can_read_output_file)
    }

    #[getter]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().description.as_ref())
    }

    #[getter]
    fn prompt<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().prompt.as_ref())
    }

    #[getter]
    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().status.as_ref())
    }

    #[getter]
    fn resolved_model<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().resolved_model.as_ref())
    }
}

view_dunders!(
    TaskLaunchResultView,
    "TaskLaunchResult",
    fields = [
        name,
        agent_id,
        output_file,
        is_async,
        can_read_output_file,
        description,
        prompt,
        status,
        resolved_model,
    ],
    match_args = []
);

/// A Skill invocation result.
#[pyclass(name = "SkillResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct SkillResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(SkillResultView, Skill, SkillResult);

#[pymethods]
impl SkillResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn command_name<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.c().command_name.as_ref())
    }

    #[getter]
    fn success<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.c().success)
    }

    #[getter]
    fn allowed_tools<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self.c().allowed_tools.as_ref().and_then(|v| v.as_array()) {
            Some(items) => Ok(PyTuple::new(
                py,
                items
                    .iter()
                    .map(|item| json_to_py(py, item))
                    .collect::<PyResult<Vec<_>>>()?,
            )?
            .into_any()),
            None => Ok(py.None().into_bound(py)),
        }
    }
}

view_dunders!(
    SkillResultView,
    "SkillResult",
    fields = [name, command_name, success, allowed_tools],
    match_args = []
);

/// A reviewer's annotation on one answered AskUserQuestion round.
///
/// Attributes:
///     preview: The short preview the picker rendered under the answer, if any.
///     notes: The free-text note the reviewer attached, if any.
#[pyclass(name = "QuestionAnnotation", module = "cc_transcript.tools", frozen)]
pub(crate) struct QuestionAnnotationView {
    pub a: QuestionAnnotation,
}

#[pymethods]
impl QuestionAnnotationView {
    #[new]
    #[pyo3(signature = (preview = None, notes = None))]
    fn py_new(preview: Option<String>, notes: Option<String>) -> Self {
        QuestionAnnotationView {
            a: QuestionAnnotation { preview, notes },
        }
    }

    #[getter]
    fn preview(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.a.preview.clone())
    }

    #[getter]
    fn notes(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.a.notes.clone())
    }
}

view_dunders!(
    QuestionAnnotationView,
    "QuestionAnnotation",
    fields = [preview, notes]
);

/// An AskUserQuestion result: the rounds, the answers, and any annotations.
///
/// Attributes:
///     questions: The rounds echoed in the payload, lifted via
///         :func:`~cc_transcript.models.parse_questions`.
///     answers: A mapping from each round's question text to the chosen answer.
///         Non-string answer values are dropped, mirroring the Rust lift.
///     annotations: A mapping from question text to the reviewer's
///         :class:`QuestionAnnotation`, present only for annotated rounds.
///         Non-string preview/notes leaves read as None, mirroring the Rust lift.
#[pyclass(name = "AskUserQuestionResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct AskUserQuestionResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(
    AskUserQuestionResultView,
    AskUserQuestion,
    AskUserQuestionResult
);

#[pymethods]
impl AskUserQuestionResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn answers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let answers = PyDict::new(py);
        for (question, answer) in &self.c().answers {
            answers.set_item(question, answer)?;
        }
        Ok(answers)
    }

    #[getter]
    fn annotations<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let annotations = PyDict::new(py);
        for (question, annotation) in &self.c().annotations {
            annotations.set_item(
                question,
                Bound::new(
                    py,
                    QuestionAnnotationView {
                        a: annotation.clone(),
                    },
                )?,
            )?;
        }
        Ok(annotations)
    }

    /// The AskUserQuestion rounds echoed in the result payload.
    #[getter]
    fn questions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(
            py,
            self.c()
                .questions()
                .into_iter()
                .map(|q| Bound::new(py, QuestionView { q }))
                .collect::<PyResult<Vec<_>>>()?,
        )
    }
}

view_dunders!(
    AskUserQuestionResultView,
    "AskUserQuestionResult",
    fields = [name, answers, annotations],
    match_args = []
);

/// A plain-string tool result — denials and other unstructured payloads.
#[pyclass(name = "TextResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct TextResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(TextResultView, Text, TextResult);

#[pymethods]
impl TextResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }

    #[getter]
    fn text(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().text.clone())
    }
}

view_dunders!(
    TextResultView,
    "TextResult",
    fields = [name, text],
    match_args = []
);

/// A tool result the platform does not type: unknown tools, untyped tools
/// (such as TodoWrite), and known tools whose payload shape did not match.
#[pyclass(name = "OtherResult", module = "cc_transcript.tools", extends = ToolResultBaseView, frozen)]
pub(crate) struct OtherResultView {
    pub result: Arc<ToolResult>,
}

result_variant!(OtherResultView, Other, OtherResult);

#[pymethods]
impl OtherResultView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.c().name.clone())
    }
}

view_dunders!(
    OtherResultView,
    "OtherResult",
    fields = [name],
    match_args = []
);

pub(crate) fn result_view<'py>(
    py: Python<'py>,
    result: Arc<ToolResult>,
) -> PyResult<Bound<'py, PyAny>> {
    let base = ToolResultBaseView {
        result: Arc::clone(&result),
    };
    let init = PyClassInitializer::from(base);
    match &*result {
        ToolResult::Bash(_) => {
            Ok(Bound::new(py, init.add_subclass(BashResultView { result }))?.into_any())
        }
        ToolResult::Edit(_) => {
            Ok(Bound::new(py, init.add_subclass(EditResultView { result }))?.into_any())
        }
        ToolResult::Write(_) => {
            Ok(Bound::new(py, init.add_subclass(WriteResultView { result }))?.into_any())
        }
        ToolResult::Read(_) => {
            Ok(Bound::new(py, init.add_subclass(ReadResultView { result }))?.into_any())
        }
        ToolResult::Task(_) => Ok(Bound::new(
            py,
            init.add_subclass(TaskResultBaseView)
                .add_subclass(TaskResultView { result }),
        )?
        .into_any()),
        ToolResult::TaskLaunch(_) => Ok(Bound::new(
            py,
            init.add_subclass(TaskResultBaseView)
                .add_subclass(TaskLaunchResultView { result }),
        )?
        .into_any()),
        ToolResult::Skill(_) => {
            Ok(Bound::new(py, init.add_subclass(SkillResultView { result }))?.into_any())
        }
        ToolResult::AskUserQuestion(_) => {
            Ok(Bound::new(py, init.add_subclass(AskUserQuestionResultView { result }))?.into_any())
        }
        ToolResult::Text(_) => {
            Ok(Bound::new(py, init.add_subclass(TextResultView { result }))?.into_any())
        }
        ToolResult::Other(_) => {
            Ok(Bound::new(py, init.add_subclass(OtherResultView { result }))?.into_any())
        }
    }
}

/// Parse a tool's name and ``toolUseResult`` payload (as a JSON document) into
/// the typed view hierarchy; the ``cc_transcript.tools`` facade owns the public
/// signature.
#[pyfunction]
pub(crate) fn toolresult_parse_view<'py>(
    py: Python<'py>,
    name: &str,
    payload_json: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let payload: Value = sonic_rs::from_str(payload_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    result_view(py, Arc::new(toolcall::parse_tool_result(name, &payload)))
}

pub(crate) fn add_classes(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ToolResultBaseView>()?;
    m.add_class::<BashResultView>()?;
    m.add_class::<EditResultView>()?;
    m.add_class::<WriteResultView>()?;
    m.add_class::<ReadResultView>()?;
    m.add_class::<TaskResultBaseView>()?;
    m.add_class::<TaskResultView>()?;
    m.add_class::<TaskLaunchResultView>()?;
    m.add_class::<SkillResultView>()?;
    m.add_class::<AskUserQuestionResultView>()?;
    m.add_class::<TextResultView>()?;
    m.add_class::<OtherResultView>()?;
    m.add_class::<QuestionAnnotationView>()?;
    m.add_function(pyo3::wrap_pyfunction!(toolresult_parse_view, m)?)?;
    Ok(())
}
