use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::PyTuple;
use sonic_rs::Value;

use cc_transcript_core::types::{joined_text, Entry};

use crate::views::attachment::attachment_detail_view;
use crate::views::blocks::{assistant_block_views, user_block_views};
use crate::views::convert::{json_to_py, opt_json};
use crate::views::dunder::{frozen_copy, view_dunders};
use crate::views::meta::{ApiErrorView, AttributionView, EntryMetaView, UsageView};
use crate::views::store::{BlockHost, EventRef, UsageHost};
use crate::views::system::system_detail_view;

fn envelope_hash(py: Python<'_>, r: &EventRef) -> PyResult<isize> {
    let meta = r.meta();
    PyTuple::new(py, [meta.session_id.as_str(), meta.uuid.as_str()])?.hash()
}

/// A user turn.
///
/// Attributes:
///     meta: The entry envelope metadata.
///     text: The joined text of the turn.
///     blocks: The parsed content blocks, including tool results.
///     interrupted: Whether the turn is a user interruption.
///     is_agent_injected: Whether the turn is an agent-injected relay banner —
///         a teammate-message digest, scheduled-task banner, or foreign-agent
///         header — rather than an authored prompt.
///     prompt_id: The client-assigned id of the prompt this turn belongs to, or None.
///     prompt_source: How the prompt was submitted, e.g. ``typed``, ``queued``,
///         ``system``, or ``sdk``, or None when absent.
///     queue_priority: The queue priority recorded for a queued prompt, or None.
///     image_paste_ids: The paste ids of images attached to the turn, or None when
///         the turn carries no image-paste marker.
///     source_tool_use_id: The id of the tool-use that produced this turn, when the
///         turn originates from a tool result, else None.
///     source_tool_assistant_uuid: The uuid of the assistant entry whose tool produced
///         this turn, else None.
///     mcp_meta: The verbatim ``mcpMeta`` payload attached to the turn, or None.
///     permission_mode: The permission mode in effect for the turn, or None.
///     interrupted_message_id: The API message id (``msg_...``) of the assistant
///         turn this interruption cut short, or None. Set from the raw
///         ``interruptedMessageId`` field; it names the interrupted assistant's
///         API message, not a transcript event uuid.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "UserEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct UserEventView {
    pub r: EventRef,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl UserEventView {
    #[getter]
    fn meta(&self, _py: Python<'_>) -> PyResult<EntryMetaView> {
        Ok(EntryMetaView { r: self.r.clone() })
    }

    #[getter]
    fn text(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.user().content.text())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "tuple[cc_transcript.models.ContentBlock, ...]",
        imports = ("cc_transcript.models",)
    ))]
    fn blocks<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        user_block_views(py, &BlockHost::Entry(self.r.clone()))
    }

    #[getter]
    fn interrupted(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.user().interrupted())
    }

    #[getter]
    fn is_agent_injected(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.user().is_agent_injected())
    }

    #[getter]
    fn prompt_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().prompt_id.clone())
    }

    #[getter]
    fn prompt_source(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().prompt_source.clone())
    }

    #[getter]
    fn queue_priority(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().queue_priority.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "tuple[int, ...] | None",
        imports = ()
    ))]
    fn image_paste_ids<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.r.user().image_paste_ids {
            Some(ids) => Ok(PyTuple::new(py, ids.iter().copied())?.into_any()),
            None => Ok(py.None().into_bound(py)),
        }
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.ToolUseId | None", imports = ("cc_transcript.ids",)))]
    fn source_tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().source_tool_use_id.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.EventUuid | None", imports = ("cc_transcript.ids",)))]
    fn source_tool_assistant_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().source_tool_assistant_uuid.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "collections.abc.Mapping[str, typing.Any] | None",
        imports = ("collections.abc", "typing")
    ))]
    fn mcp_meta<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.r.user().mcp_meta.as_ref())
    }

    #[getter]
    fn permission_mode(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().permission_mode.clone())
    }

    #[getter]
    fn interrupted_message_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.user().interrupted_message_id.clone())
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        envelope_hash(py, &self.r)
    }
}

view_dunders!(
    UserEventView,
    "UserEvent",
    fields = [
        meta,
        text,
        blocks,
        interrupted,
        is_agent_injected,
        prompt_id,
        prompt_source,
        queue_priority,
        image_paste_ids,
        source_tool_use_id,
        source_tool_assistant_uuid,
        mcp_meta,
        permission_mode,
        interrupted_message_id,
    ],
    hash = manual
);

/// An assistant turn.
///
/// Attributes:
///     meta: The entry envelope metadata.
///     model: The model that produced the turn, e.g. ``<synthetic>``.
///     text: The joined text of the turn.
///     blocks: The parsed content blocks, including thinking and tool uses.
///     stop_reason: The model's stop reason, when present.
///     usage: Token usage for the turn, or None when the entry carries no usage
///         (older transcripts, API-error messages).
///     request_id: The API request id that produced the turn, or None.
///     forked_from: The id of the message this turn was forked from, or None.
///     attribution: The plugin/skill/MCP attribution for the turn, or None when the
///         entry carries no attribution field.
///     api_error: The upstream API error the turn failed with, or None when the
///         entry is not an API-error message.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "AssistantEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct AssistantEventView {
    pub r: EventRef,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl AssistantEventView {
    #[getter]
    fn meta(&self, _py: Python<'_>) -> PyResult<EntryMetaView> {
        Ok(EntryMetaView { r: self.r.clone() })
    }

    #[getter]
    fn model(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.assistant().model.clone())
    }

    #[getter]
    fn text(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(joined_text(&self.r.assistant().blocks))
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "tuple[cc_transcript.models.ContentBlock, ...]",
        imports = ("cc_transcript.models",)
    ))]
    fn blocks<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        assistant_block_views(py, &BlockHost::Entry(self.r.clone()))
    }

    #[getter]
    fn stop_reason(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.assistant().stop_reason.clone())
    }

    #[getter]
    fn usage(&self, _py: Python<'_>) -> PyResult<Option<UsageView>> {
        Ok(self.r.assistant().usage.as_ref().map(|_| UsageView {
            host: UsageHost::Entry(self.r.clone()),
        }))
    }

    #[getter]
    fn request_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.assistant().request_id.clone())
    }

    #[getter]
    fn forked_from(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.assistant().forked_from.clone())
    }

    #[getter]
    fn attribution(&self, _py: Python<'_>) -> PyResult<Option<AttributionView>> {
        Ok(self
            .r
            .assistant()
            .attribution
            .as_ref()
            .map(|_| AttributionView { r: self.r.clone() }))
    }

    #[getter]
    fn api_error(&self, _py: Python<'_>) -> PyResult<Option<ApiErrorView>> {
        Ok(self
            .r
            .assistant()
            .api_error
            .as_ref()
            .map(|_| ApiErrorView { r: self.r.clone() }))
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        envelope_hash(py, &self.r)
    }
}

view_dunders!(
    AssistantEventView,
    "AssistantEvent",
    fields = [
        meta,
        model,
        text,
        blocks,
        stop_reason,
        usage,
        request_id,
        forked_from,
        attribution,
        api_error
    ],
    hash = manual
);

/// A system entry, such as a hook summary or notice.
///
/// Attributes:
///     meta: The entry envelope metadata.
///     subtype: The system entry's subtype.
///     content: The entry's text content, when present.
///     level: The entry's severity level, when present.
///     detail: The typed detail for the subtype — a :class:`StopHookSummary`,
///         :class:`CompactBoundary`, :class:`TurnDuration`, or
///         :class:`ModelRefusalFallback` when recognized, else an
///         :class:`OtherSystemDetail` carrying the full payload. Always set.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "SystemEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct SystemEventView {
    pub r: EventRef,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl SystemEventView {
    #[getter]
    fn meta(&self, _py: Python<'_>) -> PyResult<EntryMetaView> {
        Ok(EntryMetaView { r: self.r.clone() })
    }

    #[getter]
    fn subtype(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.system().subtype.clone())
    }

    #[getter]
    fn content(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.system().content.clone())
    }

    #[getter]
    fn level(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.system().level.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "cc_transcript.models.SystemDetail",
        imports = ("cc_transcript.models",)
    ))]
    fn detail<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        system_detail_view(py, &self.r)
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        envelope_hash(py, &self.r)
    }
}

view_dunders!(
    SystemEventView,
    "SystemEvent",
    fields = [meta, subtype, content, level, detail],
    hash = manual
);

/// A mode or permission-mode change marker.
///
/// These entries carry only a session id on disk — no uuid, timestamp, or
/// other envelope fields — so they hold a :attr:`session_id` directly rather
/// than an :class:`EntryMeta`.
///
/// Attributes:
///     session_id: The session whose mode changed.
///     channel: Which mode channel changed.
///     value: The new mode value.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "ModeEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct ModeEventView {
    pub r: EventRef,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl ModeEventView {
    #[getter]
    #[gen_stub(override_return_type(type_repr = "cc_transcript.ids.SessionId", imports = ("cc_transcript.ids",)))]
    fn session_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.mode().session_id.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal[\"mode\", \"permission-mode\"]", imports = ("typing",)))]
    fn channel(&self, _py: Python<'_>) -> PyResult<&'static str> {
        Ok(self.r.mode().channel.as_str())
    }

    #[getter]
    fn value(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.mode().value.clone())
    }
}

view_dunders!(
    ModeEventView,
    "ModeEvent",
    fields = [session_id, channel, value]
);

/// Any recognized entry without a guaranteed conversational envelope.
///
/// Covers ai-title, last-prompt, summary, queue-operation,
/// file-history-snapshot, and similar entry types whose shape carries no
/// :class:`EntryMeta`. Attachments are typed separately as
/// :class:`AttachmentEvent`.
///
/// Attributes:
///     type: The entry's ``type`` field.
///     raw: The entry's full decoded payload.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "OtherEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct OtherEventView {
    pub r: EventRef,
}

impl OtherEventView {
    fn parts(&self) -> (&str, &Value) {
        let other = self.r.other();
        (&other.ty, &other.raw)
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl OtherEventView {
    #[getter(r#type)]
    fn event_type(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.parts().0.to_string())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "collections.abc.Mapping[str, typing.Any]",
        imports = ("collections.abc", "typing")
    ))]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.parts().1)
    }

    #[classattr]
    fn __match_args__(py: Python<'_>) -> PyResult<Py<PyTuple>> {
        Ok(PyTuple::new(py, ["type", "raw"])?.unbind())
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        crate::views::dunder::repr_pairs(
            "OtherEvent",
            &[
                ("type", self.event_type(py)?.into_pyobject(py)?.into_any()),
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
                ("type", self.event_type(py)?.into_pyobject(py)?.into_any()),
                ("raw", self.raw(py)?),
            ],
            &[
                ("type", o.event_type(py)?.into_pyobject(py)?.into_any()),
                ("raw", o.raw(py)?),
            ],
        )
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.event_type(py)?.into_pyobject(py)?.into_any().hash()
    }
}

frozen_copy!(OtherEventView);

/// A harness attachment record — a hook firing, a queued command, or an
/// informational injection — carrying the full envelope it was written with.
///
/// Recognized attachment types carry their typed :class:`AttachmentDetail`;
/// every other type carries the full record verbatim under
/// :class:`OtherAttachment`, so no attachment is lossy.
///
/// Attributes:
///     meta: The entry envelope metadata.
///     attachment_type: The raw ``attachment.type`` string, e.g. ``hook_success``.
///     detail: The typed attachment payload.
#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "AttachmentEvent", module = "cc_transcript.models", frozen)]
pub(crate) struct AttachmentEventView {
    pub r: EventRef,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl AttachmentEventView {
    #[getter]
    fn meta(&self, _py: Python<'_>) -> PyResult<EntryMetaView> {
        Ok(EntryMetaView { r: self.r.clone() })
    }

    #[getter]
    fn attachment_type(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.attachment().attachment_type.clone())
    }

    #[getter]
    #[gen_stub(override_return_type(
        type_repr = "cc_transcript.models.AttachmentDetail",
        imports = ("cc_transcript.models",)
    ))]
    fn detail<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        attachment_detail_view(py, &self.r)
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        envelope_hash(py, &self.r)
    }
}

view_dunders!(
    AttachmentEventView,
    "AttachmentEvent",
    fields = [meta, attachment_type, detail],
    hash = manual
);

pub(crate) fn event_view<'py>(
    py: Python<'py>,
    entries: &Arc<Vec<Entry>>,
    idx: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let r = EventRef {
        entries: Arc::clone(entries),
        idx,
    };
    match r.entry() {
        Entry::User(_) => Ok(Bound::new(py, UserEventView { r })?.into_any()),
        Entry::Assistant(_) => Ok(Bound::new(py, AssistantEventView { r })?.into_any()),
        Entry::System(_) => Ok(Bound::new(py, SystemEventView { r })?.into_any()),
        Entry::Mode(_) => Ok(Bound::new(py, ModeEventView { r })?.into_any()),
        Entry::Other(_) => Ok(Bound::new(py, OtherEventView { r })?.into_any()),
        Entry::Attachment(_) => Ok(Bound::new(py, AttachmentEventView { r })?.into_any()),
    }
}
