use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyTuple;
use sonic_rs::Value;

use cc_transcript_core::ids;
use cc_transcript_core::types::{ContentBlock, ToolResultBlock, ToolUseBlock};

use crate::views::convert::{json_to_py, opt_json, read_only};
use crate::views::dunder::{frozen_copy, view_dunders};
use crate::views::meta::QuestionView;
use crate::views::store::{BlockHost, BlockRef};

/// A text content block from a user or assistant message.
///
/// Attributes:
///     text: The block's literal text.
#[pyclass(name = "TextBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct TextBlockView {
    pub r: BlockRef,
}

#[pymethods]
impl TextBlockView {
    #[getter]
    fn text(&self, _py: Python<'_>) -> PyResult<String> {
        match self.r.block() {
            ContentBlock::Text(text) => Ok(text.clone()),
            _ => unreachable!("text view over a non-text block"),
        }
    }
}

view_dunders!(TextBlockView, "TextBlock", fields = [text]);

/// An extended-thinking content block emitted by the assistant.
///
/// Attributes:
///     thinking: The model's thinking text.
#[pyclass(name = "ThinkingBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct ThinkingBlockView {
    pub r: BlockRef,
}

#[pymethods]
impl ThinkingBlockView {
    #[getter]
    fn thinking(&self, _py: Python<'_>) -> PyResult<String> {
        match self.r.block() {
            ContentBlock::Thinking(thinking) => Ok(thinking.clone()),
            _ => unreachable!("thinking view over a non-thinking block"),
        }
    }
}

view_dunders!(ThinkingBlockView, "ThinkingBlock", fields = [thinking]);

/// A marker that the assistant turn fell back from one model to another.
///
/// Claude Code records this when a turn switches models mid-stream; it carries
/// no message content, only the two model names.
///
/// Attributes:
///     from_model: The model the turn started on.
///     to_model: The model the turn fell back to.
#[pyclass(name = "FallbackBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct FallbackBlockView {
    pub r: BlockRef,
}

impl FallbackBlockView {
    fn fallback(&self) -> &cc_transcript_core::types::FallbackBlock {
        match self.r.block() {
            ContentBlock::Fallback(fallback) => fallback,
            _ => unreachable!("fallback view over a non-fallback block"),
        }
    }
}

#[pymethods]
impl FallbackBlockView {
    #[getter]
    fn from_model(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.fallback().from_model.clone())
    }

    #[getter]
    fn to_model(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.fallback().to_model.clone())
    }
}

view_dunders!(
    FallbackBlockView,
    "FallbackBlock",
    fields = [from_model, to_model]
);

/// Any assistant content block whose ``type`` is not yet modeled.
///
/// The escape hatch that keeps an unrecognized block from crashing the parser
/// as Claude Code's transcript format evolves, mirroring :class:`OtherEvent`.
///
/// Attributes:
///     type: The block's ``type`` field.
///     raw: The block's full decoded payload.
#[pyclass(name = "OtherBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct OtherBlockView {
    pub r: BlockRef,
}

impl OtherBlockView {
    fn parts(&self) -> (&str, &Value) {
        match self.r.block() {
            ContentBlock::Other { ty, raw } => (ty, raw),
            _ => unreachable!("other view over a modeled block"),
        }
    }
}

#[pymethods]
impl OtherBlockView {
    #[getter(r#type)]
    fn block_type(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.parts().0.to_string())
    }

    #[getter]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.parts().1)
    }

    #[classattr]
    fn __match_args__(py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(py, ["type", "raw"])?.unbind())
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        crate::views::dunder::repr_pairs(
            "OtherBlock",
            &[
                ("type", self.block_type(py)?.into_pyobject(py)?.into_any()),
                ("raw", self.raw(py)?),
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
                ("type", self.block_type(py)?.into_pyobject(py)?.into_any()),
                ("raw", self.raw(py)?),
            ],
            &[
                ("type", o.block_type(py)?.into_pyobject(py)?.into_any()),
                ("raw", o.raw(py)?),
            ],
        )
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.block_type(py)?.into_pyobject(py)?.into_any().hash()
    }
}

frozen_copy!(OtherBlockView);

/// An assistant request to invoke a tool.
///
/// Attributes:
///     id: The tool-use identifier referenced by the matching result.
///     name: The tool's name.
///     input: The tool's input arguments, preserved verbatim. v14: a read-only
///         :class:`~cc_transcript.models.ReadOnlyDict` — the view is immutable, so
///         the input cannot be mutated out of step with :attr:`call`/:attr:`digest`.
///         It stays a ``dict`` (serializes and canonicalizes like one); only the top
///         level is frozen, so nested containers keep their plain-JSON types.
#[pyclass(name = "ToolUseBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct ToolUseBlockView {
    pub r: BlockRef,
    pub input_cache: PyOnceLock<Py<PyAny>>,
    pub call_cache: PyOnceLock<Py<PyAny>>,
    pub questions_cache: PyOnceLock<Py<PyAny>>,
}

impl ToolUseBlockView {
    fn tool_use(&self) -> &ToolUseBlock {
        match self.r.block() {
            ContentBlock::ToolUse(tool_use) => tool_use,
            _ => unreachable!("tool-use view over a non-tool-use block"),
        }
    }
}

#[pymethods]
impl ToolUseBlockView {
    #[getter]
    fn id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.tool_use().id.clone())
    }

    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.tool_use().name.clone())
    }

    /// The tool's input arguments, as a read-only mapping (v14: immutable view).
    #[getter]
    fn input(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let cached = self.input_cache.get_or_try_init(py, || {
            read_only(py, json_to_py(py, &self.tool_use().input)?).map(Bound::unbind)
        })?;
        Ok(cached.clone_ref(py))
    }

    /// The block parsed into the typed tool-call hierarchy.
    ///
    /// Strict: a known tool whose input is malformed raises
    /// :class:`~cc_transcript.tools.ToolInputError`.
    #[getter]
    fn call(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let cached = self.call_cache.get_or_try_init(py, || {
            let tool_use = self.tool_use();
            crate::views::toolcall::parse_call_view(py, &tool_use.name, &tool_use.input)
        })?;
        Ok(cached.clone_ref(py))
    }

    /// The cross-language content digest of this call.
    #[getter]
    fn digest(&self, _py: Python<'_>) -> PyResult<String> {
        let tool_use = self.tool_use();
        ids::tool_digest(&tool_use.name, &tool_use.input).map_err(PyValueError::new_err)
    }

    /// The raw ``file_path`` input argument when it is a string, else None.
    ///
    /// Mirrors the Rust parse-layer lift in ``rust/crates/core/src/parse.rs``: the value is
    /// read verbatim from the input for every tool, and a non-object input or a
    /// non-string value reads as None. Mining denial evidence consumes this uniform
    /// lift rather than the type-dispatched :func:`~cc_transcript.tools.file_path_of`.
    #[getter]
    fn file_path(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.tool_use().file_path.clone())
    }

    /// The AskUserQuestion rounds lifted from the ``questions`` input array, or None.
    ///
    /// Delegates to :func:`parse_questions`, which mirrors the Rust parse layer; a
    /// non-object input reads as None.
    #[getter]
    fn questions(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let cached =
            self.questions_cache
                .get_or_try_init(py, || match &self.tool_use().questions {
                    None => Ok::<pyo3::Py<pyo3::PyAny>, PyErr>(py.None()),
                    Some(questions) => Ok(PyTuple::new(
                        py,
                        questions
                            .iter()
                            .map(|q| Bound::new(py, QuestionView { q: q.clone() }))
                            .collect::<PyResult<Vec<_>>>()?,
                    )?
                    .unbind()
                    .into_any()),
                })?;
        Ok(cached.clone_ref(py))
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.id(py)?.into_pyobject(py)?.into_any().hash()
    }
}

view_dunders!(
    ToolUseBlockView,
    "ToolUseBlock",
    fields = [id, name, input],
    hash = manual
);

/// The result of a tool invocation, delivered in a user turn.
///
/// Attributes:
///     tool_use_id: The id of the originating tool-use block.
///     content: The result text, flattened from string or block content.
///     is_error: Whether the tool reported a failure.
///     is_async: Whether the originating tool ran asynchronously — computed at
///         the parse layer from :attr:`tool_use_result`'s ``isAsync`` marker,
///         then stored (like :attr:`UserEvent.interrupted`).
///     tool_use_result: The record-level ``toolUseResult`` payload verbatim — a
///         mapping for structured results, a plain string for denials, or None
///         when the record carried none. Pass a tool name and this payload to
///         :func:`~cc_transcript.tools.parse_tool_result` for the typed result.
///         v14: a structured payload is a read-only
///         :class:`~cc_transcript.models.ReadOnlyDict`, like :attr:`ToolUseBlock.input`.
///     denial_kind: The tool-denial kind, computed at the parse layer — the
///         record-level ``toolDenialKind`` (``user-rejected`` for a human
///         rejection, ``permission-rule`` for a hook/guard block) when present,
///         else ``user-rejected`` when this error block carries the legacy denial
///         banner, else None.
#[pyclass(name = "ToolResultBlock", module = "cc_transcript.models", frozen)]
pub(crate) struct ToolResultBlockView {
    pub r: BlockRef,
    pub result_cache: PyOnceLock<Py<PyAny>>,
}

impl ToolResultBlockView {
    fn tool_result(&self) -> &ToolResultBlock {
        match self.r.block() {
            ContentBlock::ToolResult(tool_result) => tool_result,
            _ => unreachable!("tool-result view over a non-tool-result block"),
        }
    }
}

#[pymethods]
impl ToolResultBlockView {
    #[getter]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.tool_result().tool_use_id.clone())
    }

    #[getter]
    fn content(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.tool_result().content.clone())
    }

    #[getter]
    fn is_error(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.tool_result().is_error)
    }

    #[getter]
    fn is_async(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.tool_result().is_async)
    }

    #[getter]
    fn tool_use_result(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let cached = self.result_cache.get_or_try_init(py, || {
            read_only(
                py,
                opt_json(py, self.tool_result().tool_use_result.as_ref())?,
            )
            .map(Bound::unbind)
        })?;
        Ok(cached.clone_ref(py))
    }

    #[getter]
    fn denial_kind(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.tool_result().denial_kind.clone())
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.tool_use_id(py)?.into_pyobject(py)?.into_any().hash()
    }
}

view_dunders!(
    ToolResultBlockView,
    "ToolResultBlock",
    fields = [
        tool_use_id,
        content,
        is_error,
        is_async,
        tool_use_result,
        denial_kind
    ],
    hash = manual
);

pub(crate) fn block_view<'py>(
    py: Python<'py>,
    host: &BlockHost,
    idx: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let r = BlockRef {
        host: host.clone(),
        block: idx,
    };
    match r.block() {
        ContentBlock::Text(_) => Ok(Bound::new(py, TextBlockView { r })?.into_any()),
        ContentBlock::Thinking(_) => Ok(Bound::new(py, ThinkingBlockView { r })?.into_any()),
        ContentBlock::ToolUse(_) => Ok(Bound::new(
            py,
            ToolUseBlockView {
                r,
                input_cache: PyOnceLock::new(),
                call_cache: PyOnceLock::new(),
                questions_cache: PyOnceLock::new(),
            },
        )?
        .into_any()),
        ContentBlock::ToolResult(_) => Ok(Bound::new(
            py,
            ToolResultBlockView {
                r,
                result_cache: PyOnceLock::new(),
            },
        )?
        .into_any()),
        ContentBlock::Fallback(_) => Ok(Bound::new(py, FallbackBlockView { r })?.into_any()),
        ContentBlock::Other { .. } => Ok(Bound::new(py, OtherBlockView { r })?.into_any()),
    }
}

// The Python model orders user blocks text-first, then tool results.
pub(crate) fn user_block_views<'py>(
    py: Python<'py>,
    host: &BlockHost,
) -> PyResult<Bound<'py, PyTuple>> {
    let blocks = host.blocks();
    let ordered = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b, ContentBlock::Text(_)))
        .chain(
            blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| matches!(b, ContentBlock::ToolResult(_))),
        )
        .map(|(idx, _)| block_view(py, host, idx))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(py, ordered)
}

pub(crate) fn assistant_block_views<'py>(
    py: Python<'py>,
    host: &BlockHost,
) -> PyResult<Bound<'py, PyTuple>> {
    let views = (0..host.blocks().len())
        .map(|idx| block_view(py, host, idx))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(py, views)
}
