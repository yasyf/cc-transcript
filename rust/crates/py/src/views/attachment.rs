use pyo3::prelude::*;
use pyo3::types::PyTuple;
use sonic_rs::Value;

use cc_transcript_core::types::{
    AsyncHookResponse, AttachmentDetail, DeferredToolsDelta, HookAdditionalContext,
    HookBlockingError, HookCancelled, HookNonBlockingError, HookSuccess, QueuedCommand,
};

use crate::views::convert::{json_to_py, opt_json};
use crate::views::dunder::view_dunders;
use crate::views::store::EventRef;

/// A hook that fired and exited cleanly, attached to the turn it ran on.
///
/// Attributes:
///     hook_name: The hook matcher that fired, e.g. ``PostToolUse:Bash``, or None.
///     hook_event: The lifecycle event that triggered it, e.g. ``PostToolUse``, or None.
///     tool_use_id: The tool-use the hook ran against, or None for lifecycle hooks.
///     command: The hook command line, or None.
///     content: The additional context the hook injected, or None.
///     stdout: The hook's captured stdout, or None.
///     stderr: The hook's captured stderr, or None.
///     exit_code: The hook's exit status, or None.
///     duration_ms: The hook's wall-clock duration in milliseconds, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "HookSuccess", module = "cc_transcript.models", frozen)]
pub(crate) struct HookSuccessView {
    pub r: EventRef,
}

impl HookSuccessView {
    fn hook_success(&self) -> &HookSuccess {
        match &self.r.attachment().detail {
            AttachmentDetail::HookSuccess(h) => h,
            _ => unreachable!("hook-success view over a non-hook-success attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HookSuccessView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().hook_event.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().tool_use_id.clone())
    }

    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().command.clone())
    }

    #[getter]
    fn content(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().content.clone())
    }

    #[getter]
    fn stdout(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().stdout.clone())
    }

    #[getter]
    fn stderr(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_success().stderr.clone())
    }

    #[getter]
    fn exit_code(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_success().exit_code)
    }

    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_success().duration_ms)
    }
}

view_dunders!(
    HookSuccessView,
    "HookSuccess",
    fields = [
        hook_name,
        hook_event,
        tool_use_id,
        command,
        content,
        stdout,
        stderr,
        exit_code,
        duration_ms,
    ]
);

/// A hook that blocked the turn, carrying the structured blocking payload.
///
/// Attributes:
///     hook_name: The hook matcher that fired, or None.
///     hook_event: The lifecycle event that triggered it, or None.
///     tool_use_id: The tool-use the hook ran against, or None.
///     blocking_error: The verbatim ``blockingError`` payload, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "HookBlockingError", module = "cc_transcript.models", frozen)]
pub(crate) struct HookBlockingErrorView {
    pub r: EventRef,
}

impl HookBlockingErrorView {
    fn hook_blocking_error(&self) -> &HookBlockingError {
        match &self.r.attachment().detail {
            AttachmentDetail::HookBlockingError(h) => h,
            _ => unreachable!("hook-blocking-error view over a non-hook-blocking-error attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HookBlockingErrorView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_blocking_error().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_blocking_error().hook_event.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_blocking_error().tool_use_id.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "collections.abc.Mapping[str, typing.Any] | None", imports = ("collections.abc", "typing")))]
    fn blocking_error<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.hook_blocking_error().blocking_error.as_ref())
    }
}

view_dunders!(
    HookBlockingErrorView,
    "HookBlockingError",
    fields = [hook_name, hook_event, tool_use_id, blocking_error]
);

/// A hook that failed without blocking the turn.
///
/// Attributes:
///     hook_name: The hook matcher that fired, or None.
///     hook_event: The lifecycle event that triggered it, or None.
///     tool_use_id: The tool-use the hook ran against, or None.
///     command: The hook command line, or None.
///     stdout: The hook's captured stdout, or None.
///     stderr: The hook's captured stderr, or None.
///     exit_code: The hook's exit status, or None.
///     duration_ms: The hook's wall-clock duration in milliseconds, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "HookNonBlockingError", module = "cc_transcript.models", frozen)]
pub(crate) struct HookNonBlockingErrorView {
    pub r: EventRef,
}

impl HookNonBlockingErrorView {
    fn hook_non_blocking_error(&self) -> &HookNonBlockingError {
        match &self.r.attachment().detail {
            AttachmentDetail::HookNonBlockingError(h) => h,
            _ => unreachable!(
                "hook-non-blocking-error view over a non-hook-non-blocking-error attachment"
            ),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HookNonBlockingErrorView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().hook_event.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().tool_use_id.clone())
    }

    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().command.clone())
    }

    #[getter]
    fn stdout(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().stdout.clone())
    }

    #[getter]
    fn stderr(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_non_blocking_error().stderr.clone())
    }

    #[getter]
    fn exit_code(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_non_blocking_error().exit_code)
    }

    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_non_blocking_error().duration_ms)
    }
}

view_dunders!(
    HookNonBlockingErrorView,
    "HookNonBlockingError",
    fields = [
        hook_name,
        hook_event,
        tool_use_id,
        command,
        stdout,
        stderr,
        exit_code,
        duration_ms,
    ]
);

/// A hook the harness cancelled, typically on timeout.
///
/// Attributes:
///     hook_name: The hook matcher that fired, or None.
///     hook_event: The lifecycle event that triggered it, or None.
///     tool_use_id: The tool-use the hook ran against, or None.
///     command: The hook command line, or None.
///     duration_ms: How long it ran before cancellation, or None.
///     timed_out: Whether the cancellation was a timeout, or None.
///     timeout_ms: The configured timeout in milliseconds, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "HookCancelled", module = "cc_transcript.models", frozen)]
pub(crate) struct HookCancelledView {
    pub r: EventRef,
}

impl HookCancelledView {
    fn hook_cancelled(&self) -> &HookCancelled {
        match &self.r.attachment().detail {
            AttachmentDetail::HookCancelled(h) => h,
            _ => unreachable!("hook-cancelled view over a non-hook-cancelled attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HookCancelledView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_cancelled().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_cancelled().hook_event.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_cancelled().tool_use_id.clone())
    }

    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_cancelled().command.clone())
    }

    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_cancelled().duration_ms)
    }

    #[getter]
    fn timed_out(&self, _py: Python<'_>) -> PyResult<Option<bool>> {
        Ok(self.hook_cancelled().timed_out)
    }

    #[getter]
    fn timeout_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_cancelled().timeout_ms)
    }
}

view_dunders!(
    HookCancelledView,
    "HookCancelled",
    fields = [
        hook_name,
        hook_event,
        tool_use_id,
        command,
        duration_ms,
        timed_out,
        timeout_ms,
    ]
);

/// Context a hook injected into the turn without blocking it.
///
/// Attributes:
///     hook_name: The hook matcher that fired, or None.
///     hook_event: The lifecycle event that triggered it, or None.
///     tool_use_id: The tool-use the hook ran against, or None.
///     content: The injected context lines, in order.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "HookAdditionalContext",
    module = "cc_transcript.models",
    frozen
)]
pub(crate) struct HookAdditionalContextView {
    pub r: EventRef,
}

impl HookAdditionalContextView {
    fn hook_additional_context(&self) -> &HookAdditionalContext {
        match &self.r.attachment().detail {
            AttachmentDetail::HookAdditionalContext(h) => h,
            _ => unreachable!(
                "hook-additional-context view over a non-hook-additional-context attachment"
            ),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl HookAdditionalContextView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_additional_context().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_additional_context().hook_event.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.hook_additional_context().tool_use_id.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.hook_additional_context().content)
    }
}

view_dunders!(
    HookAdditionalContextView,
    "HookAdditionalContext",
    fields = [hook_name, hook_event, tool_use_id, content]
);

/// The result of an asynchronously-executed hook, matched back by process id.
///
/// Attributes:
///     hook_name: The hook matcher that fired, or None.
///     hook_event: The lifecycle event that triggered it, or None.
///     process_id: The async execution's process id, or None.
///     stdout: The hook's captured stdout, or None.
///     stderr: The hook's captured stderr, or None.
///     exit_code: The hook's exit status, or None.
///     response: The verbatim ``response`` payload, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "AsyncHookResponse", module = "cc_transcript.models", frozen)]
pub(crate) struct AsyncHookResponseView {
    pub r: EventRef,
}

impl AsyncHookResponseView {
    fn async_hook_response(&self) -> &AsyncHookResponse {
        match &self.r.attachment().detail {
            AttachmentDetail::AsyncHookResponse(h) => h,
            _ => unreachable!("async-hook-response view over a non-async-hook-response attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl AsyncHookResponseView {
    #[getter]
    fn hook_name(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.async_hook_response().hook_name.clone())
    }

    #[getter]
    fn hook_event(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.async_hook_response().hook_event.clone())
    }

    #[getter]
    fn process_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.async_hook_response().process_id.clone())
    }

    #[getter]
    fn stdout(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.async_hook_response().stdout.clone())
    }

    #[getter]
    fn stderr(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.async_hook_response().stderr.clone())
    }

    #[getter]
    fn exit_code(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.async_hook_response().exit_code)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "collections.abc.Mapping[str, typing.Any] | None", imports = ("collections.abc", "typing")))]
    fn response<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.async_hook_response().response.as_ref())
    }
}

view_dunders!(
    AsyncHookResponseView,
    "AsyncHookResponse",
    fields = [hook_name, hook_event, process_id, stdout, stderr, exit_code, response,]
);

/// A user command queued for delivery to the agent, replayed as an attachment.
///
/// Attributes:
///     prompt: The queued command's prompt text, or None when it carries no
///         plain-string prompt (e.g. an image-paste payload).
///     command_mode: How the command was queued, e.g. ``prompt`` or
///         ``task-notification``, or None.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "QueuedCommand", module = "cc_transcript.models", frozen)]
pub(crate) struct QueuedCommandView {
    pub r: EventRef,
}

impl QueuedCommandView {
    fn queued_command(&self) -> &QueuedCommand {
        match &self.r.attachment().detail {
            AttachmentDetail::QueuedCommand(q) => q,
            _ => unreachable!("queued-command view over a non-queued-command attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl QueuedCommandView {
    #[getter]
    fn prompt(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.queued_command().prompt.clone())
    }

    #[getter]
    fn command_mode(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.queued_command().command_mode.clone())
    }
}

view_dunders!(
    QueuedCommandView,
    "QueuedCommand",
    fields = [prompt, command_mode]
);

/// Tools added to and removed from the deferred tool inventory.
///
/// Attributes:
///     added_count: Number of added tools.
///     removed_count: Number of removed tools.
///     added_names: Added tool names, in transcript order.
///     removed_names: Removed tool names, in transcript order.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "DeferredToolsDelta",
    module = "cc_transcript.models",
    extends = OtherAttachmentView,
    frozen
)]
pub(crate) struct DeferredToolsDeltaView {
    pub r: EventRef,
}

impl DeferredToolsDeltaView {
    fn deferred_tools_delta(&self) -> &DeferredToolsDelta {
        match &self.r.attachment().detail {
            AttachmentDetail::DeferredToolsDelta(delta) => delta,
            _ => unreachable!("deferred-tools-delta view over another attachment"),
        }
    }

    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.deferred_tools_delta().raw)
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl DeferredToolsDeltaView {
    #[getter]
    fn added_count(&self, _py: Python<'_>) -> PyResult<usize> {
        Ok(self.deferred_tools_delta().added_names.len())
    }

    #[getter]
    fn removed_count(&self, _py: Python<'_>) -> PyResult<usize> {
        Ok(self.deferred_tools_delta().removed_names.len())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn added_names<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.deferred_tools_delta().added_names)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[str, ...]"))]
    fn removed_names<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.deferred_tools_delta().removed_names)
    }
}

view_dunders!(
    DeferredToolsDeltaView,
    "DeferredToolsDelta",
    fields = [raw, added_count, removed_count, added_names, removed_names]
);

/// Any attachment whose type has no typed detail, carried verbatim.
///
/// Covers the many informational attachment types (skill/tool/agent listings,
/// reminders, plan-mode markers, file references, and anything future) whose
/// shape is not further decomposed, mirroring :class:`OtherSystemDetail`.
///
/// Attributes:
///     raw: The attachment entry's full decoded payload.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "OtherAttachment",
    module = "cc_transcript.models",
    frozen,
    subclass
)]
pub(crate) struct OtherAttachmentView {
    pub r: EventRef,
}

impl OtherAttachmentView {
    fn other(&self) -> &Value {
        match &self.r.attachment().detail {
            AttachmentDetail::DeferredToolsDelta(delta) => &delta.raw,
            AttachmentDetail::Other(raw) => raw,
            _ => unreachable!("other view over a modeled attachment"),
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl OtherAttachmentView {
    #[getter]
    #[gen_stub(override_return_type(type_repr = "collections.abc.Mapping[str, typing.Any]", imports = ("collections.abc", "typing")))]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.other())
    }
}

view_dunders!(OtherAttachmentView, "OtherAttachment", fields = [raw]);

pub(crate) fn attachment_detail_view<'py>(
    py: Python<'py>,
    r: &EventRef,
) -> PyResult<Bound<'py, PyAny>> {
    match &r.attachment().detail {
        AttachmentDetail::HookSuccess(_) => {
            Ok(Bound::new(py, HookSuccessView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::HookBlockingError(_) => {
            Ok(Bound::new(py, HookBlockingErrorView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::HookNonBlockingError(_) => {
            Ok(Bound::new(py, HookNonBlockingErrorView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::HookCancelled(_) => {
            Ok(Bound::new(py, HookCancelledView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::HookAdditionalContext(_) => {
            Ok(Bound::new(py, HookAdditionalContextView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::AsyncHookResponse(_) => {
            Ok(Bound::new(py, AsyncHookResponseView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::QueuedCommand(_) => {
            Ok(Bound::new(py, QueuedCommandView { r: r.clone() })?.into_any())
        }
        AttachmentDetail::DeferredToolsDelta(_) => {
            let base = OtherAttachmentView { r: r.clone() };
            let init = PyClassInitializer::from(base);
            Ok(Bound::new(
                py,
                init.add_subclass(DeferredToolsDeltaView { r: r.clone() }),
            )?
            .into_any())
        }
        AttachmentDetail::Other(_) => {
            Ok(Bound::new(py, OtherAttachmentView { r: r.clone() })?.into_any())
        }
    }
}
