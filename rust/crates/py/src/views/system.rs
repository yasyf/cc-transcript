use pyo3::prelude::*;
use pyo3::types::PyTuple;
use sonic_rs::Value;

use cc_transcript_core::types::SystemDetail;

use crate::views::convert::json_to_py;
use crate::views::dunder::view_dunders;
use crate::views::store::EventRef;

/// The typed detail of a ``stop_hook_summary`` system entry.
///
/// Attributes:
///     hook_count: The number of hooks that ran.
///     hook_infos: The per-hook command and duration records.
///     hook_errors: The error strings raised by hooks.
///     hook_additional_context: The extra context strings hooks contributed.
///     prevented_continuation: Whether a hook blocked the turn from continuing.
///     stop_reason: The reason the turn stopped, when recorded.
///     has_output: Whether the hooks produced output.
///     tool_use_id: The tool-use id the summary is tied to, when present.
#[pyclass(name = "StopHookSummary", module = "cc_transcript.models", frozen)]
pub(crate) struct StopHookSummaryView {
    pub r: EventRef,
}

impl StopHookSummaryView {
    fn summary(&self) -> &cc_transcript_core::types::StopHookSummary {
        match &self.r.system().detail {
            SystemDetail::StopHookSummary(summary) => summary,
            _ => unreachable!("stop-hook-summary view over a non-stop-hook-summary system entry"),
        }
    }
}

#[pymethods]
impl StopHookSummaryView {
    #[getter]
    fn hook_count(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.summary().hook_count)
    }

    #[getter]
    fn hook_infos<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(
            py,
            (0..self.summary().hook_infos.len())
                .map(|info| {
                    Bound::new(
                        py,
                        HookInfoView {
                            r: self.r.clone(),
                            info,
                        },
                    )
                })
                .collect::<PyResult<Vec<_>>>()?,
        )
    }

    #[getter]
    fn hook_errors<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.summary().hook_errors)
    }

    #[getter]
    fn hook_additional_context<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.summary().hook_additional_context)
    }

    #[getter]
    fn prevented_continuation(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.summary().prevented_continuation)
    }

    #[getter]
    fn stop_reason(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.summary().stop_reason.clone())
    }

    #[getter]
    fn has_output(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.summary().has_output)
    }

    #[getter]
    fn tool_use_id(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.summary().tool_use_id.clone())
    }
}

view_dunders!(
    StopHookSummaryView,
    "StopHookSummary",
    fields = [
        hook_count,
        hook_infos,
        hook_errors,
        hook_additional_context,
        prevented_continuation,
        stop_reason,
        has_output,
        tool_use_id,
    ]
);

/// One hook invocation recorded in a stop-hook summary.
///
/// Attributes:
///     command: The hook command that ran.
///     duration_ms: The hook's wall-clock duration in milliseconds, when recorded.
#[pyclass(name = "HookInfo", module = "cc_transcript.models", frozen)]
pub(crate) struct HookInfoView {
    pub r: EventRef,
    pub info: usize,
}

impl HookInfoView {
    fn hook_info(&self) -> &cc_transcript_core::types::HookInfo {
        match &self.r.system().detail {
            SystemDetail::StopHookSummary(summary) => &summary.hook_infos[self.info],
            _ => unreachable!("hook-info view over a non-stop-hook-summary system entry"),
        }
    }
}

#[pymethods]
impl HookInfoView {
    #[getter]
    fn command(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.hook_info().command.clone())
    }

    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.hook_info().duration_ms)
    }
}

view_dunders!(HookInfoView, "HookInfo", fields = [command, duration_ms]);

/// The head/anchor/tail uuids of the segment preserved across a compaction.
///
/// Attributes:
///     head_uuid: The first preserved event's uuid.
///     anchor_uuid: The anchor event's uuid.
///     tail_uuid: The last preserved event's uuid.
#[pyclass(name = "PreservedSegment", module = "cc_transcript.models", frozen)]
pub(crate) struct PreservedSegmentView {
    pub r: EventRef,
}

impl PreservedSegmentView {
    fn preserved_segment(&self) -> &cc_transcript_core::types::PreservedSegment {
        match &self.r.system().detail {
            SystemDetail::CompactBoundary(boundary) => boundary
                .preserved_segment
                .as_ref()
                .expect("preserved-segment view over a boundary that has one"),
            _ => unreachable!("preserved-segment view over a non-compact-boundary system entry"),
        }
    }
}

#[pymethods]
impl PreservedSegmentView {
    #[getter]
    fn head_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.preserved_segment().head_uuid.clone())
    }

    #[getter]
    fn anchor_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.preserved_segment().anchor_uuid.clone())
    }

    #[getter]
    fn tail_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.preserved_segment().tail_uuid.clone())
    }
}

view_dunders!(
    PreservedSegmentView,
    "PreservedSegment",
    fields = [head_uuid, anchor_uuid, tail_uuid]
);

/// The message uuids preserved across a compaction.
///
/// Attributes:
///     anchor_uuid: The anchor event's uuid.
///     uuids: The uuids of the preserved messages, in order.
///     all_uuids: The uuids of every message considered for preservation,
///         in order.
#[pyclass(name = "PreservedMessages", module = "cc_transcript.models", frozen)]
pub(crate) struct PreservedMessagesView {
    pub r: EventRef,
}

impl PreservedMessagesView {
    fn preserved_messages(&self) -> &cc_transcript_core::types::PreservedMessages {
        match &self.r.system().detail {
            SystemDetail::CompactBoundary(boundary) => boundary
                .preserved_messages
                .as_ref()
                .expect("preserved-messages view over a boundary that has one"),
            _ => unreachable!("preserved-messages view over a non-compact-boundary system entry"),
        }
    }
}

#[pymethods]
impl PreservedMessagesView {
    #[getter]
    fn anchor_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.preserved_messages().anchor_uuid.clone())
    }

    #[getter]
    fn uuids<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.preserved_messages().uuids)
    }

    #[getter]
    fn all_uuids<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.preserved_messages().all_uuids)
    }
}

view_dunders!(
    PreservedMessagesView,
    "PreservedMessages",
    fields = [anchor_uuid, uuids, all_uuids]
);

/// The typed detail of a ``compact_boundary`` system entry.
///
/// Attributes:
///     trigger: What triggered the compaction, such as ``manual`` or ``auto``.
///     pre_tokens: The context token count before compaction.
///     post_tokens: The context token count after compaction.
///     duration_ms: The compaction's wall-clock duration in milliseconds.
///     cumulative_dropped_tokens: The session's running total of tokens dropped
///         by compactions.
///     pre_compact_discovered_tools: The tool names discovered before compaction.
///     preserved_segment: The head/anchor/tail segment preserved, when recorded.
///     preserved_messages: The preserved message uuids, when recorded.
///     logical_parent_uuid: The uuid the post-compaction thread logically
///         continues from.
///     precomputed: Whether the compaction was precomputed, when recorded.
#[pyclass(name = "CompactBoundary", module = "cc_transcript.models", frozen)]
pub(crate) struct CompactBoundaryView {
    pub r: EventRef,
}

impl CompactBoundaryView {
    fn boundary(&self) -> &cc_transcript_core::types::CompactBoundary {
        match &self.r.system().detail {
            SystemDetail::CompactBoundary(boundary) => boundary,
            _ => unreachable!("compact-boundary view over a non-compact-boundary system entry"),
        }
    }
}

#[pymethods]
impl CompactBoundaryView {
    #[getter]
    fn trigger(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.boundary().trigger.clone())
    }

    #[getter]
    fn pre_tokens(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.boundary().pre_tokens)
    }

    #[getter]
    fn post_tokens(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.boundary().post_tokens)
    }

    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.boundary().duration_ms)
    }

    #[getter]
    fn cumulative_dropped_tokens(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.boundary().cumulative_dropped_tokens)
    }

    #[getter]
    fn pre_compact_discovered_tools<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.boundary().pre_compact_discovered_tools)
    }

    #[getter]
    fn preserved_segment(&self, _py: Python<'_>) -> PyResult<Option<PreservedSegmentView>> {
        Ok(self
            .boundary()
            .preserved_segment
            .as_ref()
            .map(|_| PreservedSegmentView { r: self.r.clone() }))
    }

    #[getter]
    fn preserved_messages(&self, _py: Python<'_>) -> PyResult<Option<PreservedMessagesView>> {
        Ok(self
            .boundary()
            .preserved_messages
            .as_ref()
            .map(|_| PreservedMessagesView { r: self.r.clone() }))
    }

    #[getter]
    fn logical_parent_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.boundary().logical_parent_uuid.clone())
    }

    #[getter]
    fn precomputed(&self, _py: Python<'_>) -> PyResult<Option<bool>> {
        Ok(self.boundary().precomputed)
    }
}

view_dunders!(
    CompactBoundaryView,
    "CompactBoundary",
    fields = [
        trigger,
        pre_tokens,
        post_tokens,
        duration_ms,
        cumulative_dropped_tokens,
        pre_compact_discovered_tools,
        preserved_segment,
        preserved_messages,
        logical_parent_uuid,
        precomputed,
    ]
);

/// The typed detail of a ``turn_duration`` system entry.
///
/// Attributes:
///     duration_ms: The turn's wall-clock duration in milliseconds.
///     message_count: The number of messages in the turn.
///     pending_workflow_count: The workflows still pending, when recorded.
///     pending_background_agent_count: The background agents still pending, when
///         recorded.
#[pyclass(name = "TurnDuration", module = "cc_transcript.models", frozen)]
pub(crate) struct TurnDurationView {
    pub r: EventRef,
}

impl TurnDurationView {
    fn turn_duration(&self) -> &cc_transcript_core::types::TurnDuration {
        match &self.r.system().detail {
            SystemDetail::TurnDuration(turn_duration) => turn_duration,
            _ => unreachable!("turn-duration view over a non-turn-duration system entry"),
        }
    }
}

#[pymethods]
impl TurnDurationView {
    #[getter]
    fn duration_ms(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.turn_duration().duration_ms)
    }

    #[getter]
    fn message_count(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.turn_duration().message_count)
    }

    #[getter]
    fn pending_workflow_count(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.turn_duration().pending_workflow_count)
    }

    #[getter]
    fn pending_background_agent_count(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.turn_duration().pending_background_agent_count)
    }
}

view_dunders!(
    TurnDurationView,
    "TurnDuration",
    fields = [
        duration_ms,
        message_count,
        pending_workflow_count,
        pending_background_agent_count,
    ]
);

/// The typed detail of a ``model_refusal_fallback`` system entry.
///
/// Attributes:
///     api_refusal_category: The refusal category the API reported, when present.
///     api_refusal_explanation: The refusal explanation the API reported, when present.
///     trigger: What triggered the fallback.
///     direction: The fallback direction.
///     original_model: The model that refused.
///     fallback_model: The model fallen back to.
///     retracted_message_uuids: The uuids of messages retracted by the fallback.
///     refused_user_message_uuid: The user message uuid that drew the refusal,
///         when present.
#[pyclass(name = "ModelRefusalFallback", module = "cc_transcript.models", frozen)]
pub(crate) struct ModelRefusalFallbackView {
    pub r: EventRef,
}

impl ModelRefusalFallbackView {
    fn fallback(&self) -> &cc_transcript_core::types::ModelRefusalFallback {
        match &self.r.system().detail {
            SystemDetail::ModelRefusalFallback(fallback) => fallback,
            _ => unreachable!(
                "model-refusal-fallback view over a non-model-refusal-fallback system entry"
            ),
        }
    }
}

#[pymethods]
impl ModelRefusalFallbackView {
    #[getter]
    fn api_refusal_category(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().api_refusal_category.clone())
    }

    #[getter]
    fn api_refusal_explanation(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().api_refusal_explanation.clone())
    }

    #[getter]
    fn trigger(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().trigger.clone())
    }

    #[getter]
    fn direction(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().direction.clone())
    }

    #[getter]
    fn original_model(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().original_model.clone())
    }

    #[getter]
    fn fallback_model(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().fallback_model.clone())
    }

    #[getter]
    fn retracted_message_uuids<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.fallback().retracted_message_uuids)
    }

    #[getter]
    fn refused_user_message_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.fallback().refused_user_message_uuid.clone())
    }
}

view_dunders!(
    ModelRefusalFallbackView,
    "ModelRefusalFallback",
    fields = [
        api_refusal_category,
        api_refusal_explanation,
        trigger,
        direction,
        original_model,
        fallback_model,
        retracted_message_uuids,
        refused_user_message_uuid,
    ]
);

/// The catch-all detail for a system entry without a typed subtype.
///
/// Carries the entry's full decoded payload verbatim, so no system entry is
/// lossy regardless of subtype.
///
/// Attributes:
///     raw: The entry's full decoded payload.
#[pyclass(name = "OtherSystemDetail", module = "cc_transcript.models", frozen)]
pub(crate) struct OtherSystemDetailView {
    pub r: EventRef,
}

impl OtherSystemDetailView {
    fn value(&self) -> &Value {
        match &self.r.system().detail {
            SystemDetail::Other(value) => value,
            _ => unreachable!("other-system-detail view over a modeled system detail"),
        }
    }
}

#[pymethods]
impl OtherSystemDetailView {
    #[getter]
    fn raw<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, self.value())
    }
}

view_dunders!(OtherSystemDetailView, "OtherSystemDetail", fields = [raw]);

pub(crate) fn system_detail_view<'py>(
    py: Python<'py>,
    r: &EventRef,
) -> PyResult<Bound<'py, PyAny>> {
    match &r.system().detail {
        SystemDetail::StopHookSummary(_) => {
            Ok(Bound::new(py, StopHookSummaryView { r: r.clone() })?.into_any())
        }
        SystemDetail::CompactBoundary(_) => {
            Ok(Bound::new(py, CompactBoundaryView { r: r.clone() })?.into_any())
        }
        SystemDetail::TurnDuration(_) => {
            Ok(Bound::new(py, TurnDurationView { r: r.clone() })?.into_any())
        }
        SystemDetail::ModelRefusalFallback(_) => {
            Ok(Bound::new(py, ModelRefusalFallbackView { r: r.clone() })?.into_any())
        }
        SystemDetail::Other(_) => {
            Ok(Bound::new(py, OtherSystemDetailView { r: r.clone() })?.into_any())
        }
    }
}
