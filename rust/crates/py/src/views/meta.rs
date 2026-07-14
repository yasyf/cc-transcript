use chrono::{DateTime, FixedOffset};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use cc_transcript_core::types::Question;

use crate::views::dunder::view_dunders;
use crate::views::store::{EventRef, UsageHost};

/// Envelope metadata shared by the conversational transcript events.
///
/// Attributes:
///     uuid: The entry's unique identifier.
///     parent_uuid: The parent entry's id, or None for roots.
///     session_id: The session this entry belongs to.
///     timestamp: The entry's timezone-aware timestamp.
///     cwd: The working directory recorded for the entry.
///     git_branch: The git branch recorded for the entry.
///     cc_version: The Claude Code version that wrote the entry.
///     is_sidechain: Whether the entry belongs to a subagent sidechain.
///     is_meta: Whether the entry is a meta entry injected by the client.
///     entrypoint: The entrypoint that produced the entry, e.g. ``cli``.
///     is_compact_summary: Whether the entry is a compaction summary.
///     is_visible_in_transcript_only: Whether the entry is transcript-only.
///     user_type: The ``userType`` recorded for the entry, e.g. ``external``, or None when absent.
///     slug: The session slug recorded for the entry, or None when absent.
#[pyclass(name = "EntryMeta", module = "cc_transcript.models", frozen)]
pub(crate) struct EntryMetaView {
    pub r: EventRef,
}

#[pymethods]
impl EntryMetaView {
    #[getter]
    fn uuid(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.meta().uuid.clone())
    }

    #[getter]
    fn parent_uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().parent_uuid.clone())
    }

    #[getter]
    fn session_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.r.meta().session_id.clone())
    }

    #[getter]
    fn timestamp(&self, _py: Python<'_>) -> PyResult<DateTime<FixedOffset>> {
        Ok(self.r.meta().timestamp)
    }

    #[getter]
    fn cwd(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().cwd.clone())
    }

    #[getter]
    fn git_branch(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().git_branch.clone())
    }

    #[getter]
    fn cc_version(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().version.clone())
    }

    #[getter]
    fn is_sidechain(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.meta().is_sidechain)
    }

    #[getter]
    fn is_meta(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.meta().is_meta)
    }

    #[getter]
    fn entrypoint(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().entrypoint.clone())
    }

    #[getter]
    fn is_compact_summary(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.meta().is_compact_summary)
    }

    #[getter]
    fn is_visible_in_transcript_only(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.r.meta().is_visible_in_transcript_only)
    }

    #[getter]
    fn user_type(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().user_type.clone())
    }

    #[getter]
    fn slug(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.r.meta().slug.clone())
    }
}

view_dunders!(
    EntryMetaView,
    "EntryMeta",
    fields = [
        uuid,
        parent_uuid,
        session_id,
        timestamp,
        cwd,
        git_branch,
        cc_version,
        is_sidechain,
        is_meta,
        entrypoint,
        is_compact_summary,
        is_visible_in_transcript_only,
        user_type,
        slug,
    ]
);

/// The plugin, skill, or MCP tool an assistant turn is attributed to.
///
/// Present on an :class:`AssistantEvent` only when the entry carries at least one
/// of the four attribution fields; each component is independently optional.
///
/// Attributes:
///     plugin: The plugin the turn is attributed to, or None.
///     skill: The skill the turn is attributed to, or None.
///     mcp_server: The MCP server the turn is attributed to, or None.
///     mcp_tool: The MCP tool the turn is attributed to, or None.
#[pyclass(name = "Attribution", module = "cc_transcript.models", frozen)]
pub(crate) struct AttributionView {
    pub r: EventRef,
}

impl AttributionView {
    fn attribution(&self) -> &cc_transcript_core::types::Attribution {
        self.r
            .assistant()
            .attribution
            .as_ref()
            .expect("attribution view over an attributed assistant entry")
    }
}

#[pymethods]
impl AttributionView {
    #[getter]
    fn plugin(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.attribution().plugin.clone())
    }

    #[getter]
    fn skill(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.attribution().skill.clone())
    }

    #[getter]
    fn mcp_server(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.attribution().mcp_server.clone())
    }

    #[getter]
    fn mcp_tool(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.attribution().mcp_tool.clone())
    }
}

view_dunders!(
    AttributionView,
    "Attribution",
    fields = [plugin, skill, mcp_server, mcp_tool]
);

/// The upstream API error an assistant turn failed with.
///
/// Present on an :class:`AssistantEvent` only when the entry's
/// ``isApiErrorMessage`` flag is set; each component is independently optional.
///
/// Attributes:
///     error: The error kind the API reported, e.g. ``rate_limit``, or None.
///     status: The HTTP status of the failed request, e.g. ``429``, or None.
///     details: The free-text error detail, or None.
#[pyclass(name = "ApiError", module = "cc_transcript.models", frozen)]
pub(crate) struct ApiErrorView {
    pub r: EventRef,
}

impl ApiErrorView {
    fn api_error(&self) -> &cc_transcript_core::types::ApiError {
        self.r
            .assistant()
            .api_error
            .as_ref()
            .expect("api-error view over a failed assistant entry")
    }
}

#[pymethods]
impl ApiErrorView {
    #[getter]
    fn error(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.api_error().error.clone())
    }

    #[getter]
    fn status(&self, _py: Python<'_>) -> PyResult<Option<i64>> {
        Ok(self.api_error().status)
    }

    #[getter]
    fn details(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.api_error().details.clone())
    }
}

view_dunders!(ApiErrorView, "ApiError", fields = [error, status, details]);

/// One AskUserQuestion round lifted from a tool-use input's ``questions`` array.
///
/// Attributes:
///     question: The prompt text shown to the user.
///     header: The round's short header, or None when the input omits one.
///     multi_select: Whether the round accepted more than one selection.
///     labels: The option labels offered, in presentation order.
#[pyclass(name = "Question", module = "cc_transcript.models", frozen)]
pub(crate) struct QuestionView {
    pub q: Question,
}

#[pymethods]
impl QuestionView {
    #[new]
    #[pyo3(signature = (question, header, multi_select, labels))]
    fn new(
        question: String,
        header: Option<String>,
        multi_select: bool,
        labels: Vec<String>,
    ) -> Self {
        QuestionView {
            q: Question {
                question,
                header,
                multi_select,
                labels,
            },
        }
    }

    #[getter]
    fn question(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.q.question.clone())
    }

    #[getter]
    fn header(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.q.header.clone())
    }

    #[getter]
    fn multi_select(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.q.multi_select)
    }

    #[getter]
    fn labels<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.q.labels)
    }
}

view_dunders!(
    QuestionView,
    "Question",
    fields = [question, header, multi_select, labels]
);

/// The split of cache-creation input tokens by TTL bucket.
///
/// Attributes:
///     ephemeral_5m_input_tokens: Cache-creation tokens written to the 5-minute TTL bucket.
///     ephemeral_1h_input_tokens: Cache-creation tokens written to the 1-hour TTL bucket.
#[pyclass(name = "CacheCreation", module = "cc_transcript.models", frozen)]
pub(crate) struct CacheCreationView {
    pub host: UsageHost,
}

impl CacheCreationView {
    fn cache_creation(&self) -> &cc_transcript_core::types::CacheCreation {
        self.host
            .usage()
            .cache_creation
            .as_ref()
            .expect("cache-creation view over a usage with a cache split")
    }
}

#[pymethods]
impl CacheCreationView {
    #[getter]
    fn ephemeral_5m_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.cache_creation().ephemeral_5m_input_tokens)
    }

    #[getter]
    fn ephemeral_1h_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.cache_creation().ephemeral_1h_input_tokens)
    }
}

view_dunders!(
    CacheCreationView,
    "CacheCreation",
    fields = [ephemeral_5m_input_tokens, ephemeral_1h_input_tokens]
);

/// Server-side tool invocation counts billed within a turn.
///
/// Attributes:
///     web_search_requests: The number of server-side web-search requests.
///     web_fetch_requests: The number of server-side web-fetch requests.
#[pyclass(name = "ServerToolUse", module = "cc_transcript.models", frozen)]
pub(crate) struct ServerToolUseView {
    pub host: UsageHost,
}

impl ServerToolUseView {
    fn server_tool_use(&self) -> &cc_transcript_core::types::ServerToolUse {
        self.host
            .usage()
            .server_tool_use
            .as_ref()
            .expect("server-tool-use view over a usage with server tools")
    }
}

#[pymethods]
impl ServerToolUseView {
    #[getter]
    fn web_search_requests(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.server_tool_use().web_search_requests)
    }

    #[getter]
    fn web_fetch_requests(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.server_tool_use().web_fetch_requests)
    }
}

view_dunders!(
    ServerToolUseView,
    "ServerToolUse",
    fields = [web_search_requests, web_fetch_requests]
);

/// Token usage and cache accounting for a single assistant turn or a -p (print mode) result.
///
/// Exposes both the flat cache_creation_input_tokens and the per-TTL cache_creation
/// split, faithfully and without opinion.
///
/// Attributes:
///     input_tokens: The number of input tokens consumed by the turn.
///     output_tokens: The number of output tokens produced by the turn.
///     cache_read_input_tokens: The number of input tokens served from the cache.
///     cache_creation_input_tokens: The flat total of input tokens written to the cache.
///     cache_creation: The per-TTL split of cache-creation tokens, when present.
///     service_tier: The service tier that billed the turn, when present.
///     inference_geo: The inference geography that served the turn, when present.
///     server_tool_use: Server-side tool invocation counts, when present.
#[pyclass(name = "Usage", module = "cc_transcript.models", frozen)]
pub(crate) struct UsageView {
    pub host: UsageHost,
}

#[pymethods]
impl UsageView {
    #[getter]
    fn input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.host.usage().input_tokens)
    }

    #[getter]
    fn output_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.host.usage().output_tokens)
    }

    #[getter]
    fn cache_read_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.host.usage().cache_read_input_tokens)
    }

    #[getter]
    fn cache_creation_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.host.usage().cache_creation_input_tokens)
    }

    #[getter]
    fn cache_creation(&self, _py: Python<'_>) -> PyResult<Option<CacheCreationView>> {
        Ok(self
            .host
            .usage()
            .cache_creation
            .as_ref()
            .map(|_| CacheCreationView {
                host: self.host.clone(),
            }))
    }

    #[getter]
    fn service_tier(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.host.usage().service_tier.clone())
    }

    #[getter]
    fn inference_geo(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.host.usage().inference_geo.clone())
    }

    #[getter]
    fn server_tool_use(&self, _py: Python<'_>) -> PyResult<Option<ServerToolUseView>> {
        Ok(self
            .host
            .usage()
            .server_tool_use
            .as_ref()
            .map(|_| ServerToolUseView {
                host: self.host.clone(),
            }))
    }
}

view_dunders!(
    UsageView,
    "Usage",
    fields = [
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        cache_creation,
        service_tier,
        inference_geo,
        server_tool_use,
    ]
);
