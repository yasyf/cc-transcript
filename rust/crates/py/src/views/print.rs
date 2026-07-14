use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use cc_transcript_core::types::{
    joined_text, InitInfo, McpServer, ModelUsage, Plugin, PrintBody, PrintMessage,
};

use crate::views::blocks::{assistant_block_views, user_block_views};
use crate::views::convert::{json_to_py, opt_json};
use crate::views::dunder::view_dunders;
use crate::views::meta::UsageView;
use crate::views::store::{BlockHost, PrintRef, UsageHost};

fn init_of(p: &PrintRef) -> &InitInfo {
    p.0.init
        .as_ref()
        .expect("init view over a result that has one")
}

/// Per-model token usage and cost from a -p (print mode) result's modelUsage map.
///
/// Attributes:
///     input_tokens: The number of input tokens consumed by the model.
///     output_tokens: The number of output tokens produced by the model.
///     cache_read_input_tokens: The number of input tokens served from the cache.
///     cache_creation_input_tokens: The flat total of input tokens written to the cache.
///     web_search_requests: The number of server-side web-search requests billed to the model.
///     cost_usd: The cost in USD attributed to the model.
///     context_window: The model's context window size in tokens.
///     max_output_tokens: The model's maximum output token budget.
#[pyclass(name = "ModelUsage", module = "cc_transcript.models", frozen)]
pub(crate) struct ModelUsageView {
    pub p: PrintRef,
    pub idx: usize,
}

impl ModelUsageView {
    fn model_usage(&self) -> &ModelUsage {
        &self.p.0.model_usage[self.idx].1
    }
}

#[pymethods]
impl ModelUsageView {
    #[getter]
    fn input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().input_tokens)
    }

    #[getter]
    fn output_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().output_tokens)
    }

    #[getter]
    fn cache_read_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().cache_read_input_tokens)
    }

    #[getter]
    fn cache_creation_input_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().cache_creation_input_tokens)
    }

    #[getter]
    fn web_search_requests(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().web_search_requests)
    }

    #[getter]
    fn cost_usd(&self, _py: Python<'_>) -> PyResult<f64> {
        Ok(self.model_usage().cost_usd)
    }

    #[getter]
    fn context_window(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().context_window)
    }

    #[getter]
    fn max_output_tokens(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.model_usage().max_output_tokens)
    }
}

view_dunders!(
    ModelUsageView,
    "ModelUsage",
    fields = [
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        web_search_requests,
        cost_usd,
        context_window,
        max_output_tokens,
    ]
);

/// An MCP server entry from the -p init element.
///
/// Attributes:
///     name: The configured name of the MCP server.
///     status: The connection status reported for the server.
#[pyclass(name = "McpServer", module = "cc_transcript.models", frozen)]
pub(crate) struct McpServerView {
    pub p: PrintRef,
    pub idx: usize,
}

impl McpServerView {
    fn server(&self) -> &McpServer {
        &init_of(&self.p).mcp_servers[self.idx]
    }
}

#[pymethods]
impl McpServerView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.server().name.clone())
    }

    #[getter]
    fn status(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.server().status.clone())
    }
}

view_dunders!(McpServerView, "McpServer", fields = [name, status]);

/// A plugin entry from the -p init element.
///
/// Attributes:
///     name: The plugin's name.
///     path: The filesystem path the plugin was loaded from.
///     source: The source the plugin was installed from.
#[pyclass(name = "Plugin", module = "cc_transcript.models", frozen)]
pub(crate) struct PluginView {
    pub p: PrintRef,
    pub idx: usize,
}

impl PluginView {
    fn plugin(&self) -> &Plugin {
        &init_of(&self.p).plugins[self.idx]
    }
}

#[pymethods]
impl PluginView {
    #[getter]
    fn name(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.plugin().name.clone())
    }

    #[getter]
    fn path(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.plugin().path.clone())
    }

    #[getter]
    fn source(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.plugin().source.clone())
    }
}

view_dunders!(PluginView, "Plugin", fields = [name, path, source]);

/// The session init snapshot from a -p system/init element.
///
/// Attributes:
///     mcp_servers: The MCP servers configured for the session.
///     plugins: The plugins loaded for the session.
///     tools: The tool names available to the session.
///     skills: The skill names available to the session.
#[pyclass(name = "InitInfo", module = "cc_transcript.models", frozen)]
pub(crate) struct InitInfoView {
    pub p: PrintRef,
}

impl InitInfoView {
    fn init(&self) -> &InitInfo {
        init_of(&self.p)
    }
}

#[pymethods]
impl InitInfoView {
    #[getter]
    fn mcp_servers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let views = (0..self.init().mcp_servers.len())
            .map(|idx| {
                Bound::new(
                    py,
                    McpServerView {
                        p: self.p.clone(),
                        idx,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        PyTuple::new(py, views)
    }

    #[getter]
    fn plugins<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let views = (0..self.init().plugins.len())
            .map(|idx| {
                Bound::new(
                    py,
                    PluginView {
                        p: self.p.clone(),
                        idx,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        PyTuple::new(py, views)
    }

    #[getter]
    fn tools<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.init().tools)
    }

    #[getter]
    fn skills<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, &self.init().skills)
    }
}

view_dunders!(
    InitInfoView,
    "InitInfo",
    fields = [mcp_servers, plugins, tools, skills]
);

/// A conversational message lifted from a -p (print mode) result.
///
/// Unlike on-disk events it carries no EntryMeta — the -p element shape lacks
/// timestamp/parentUuid — so it holds only role, model, text, blocks, and the ids
/// that are present.
///
/// Attributes:
///     role: The author of the message, either "user" or "assistant".
///     model: The model that produced the message, when present.
///     text: The flattened text of the message.
///     blocks: The structured content blocks of the message.
///     uuid: The message's event uuid, when present.
///     session_id: The session the message belongs to.
#[pyclass(name = "PrintMessage", module = "cc_transcript.models", frozen)]
pub(crate) struct PrintMessageView {
    pub p: PrintRef,
    pub msg: usize,
}

impl PrintMessageView {
    fn message(&self) -> &PrintMessage {
        &self.p.0.messages[self.msg]
    }
}

#[pymethods]
impl PrintMessageView {
    #[getter]
    fn role(&self, _py: Python<'_>) -> PyResult<&'static str> {
        Ok(match &self.message().body {
            PrintBody::Assistant { .. } => "assistant",
            PrintBody::User(_) => "user",
        })
    }

    #[getter]
    fn model(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(match &self.message().body {
            PrintBody::Assistant { model, .. } => model.clone(),
            PrintBody::User(_) => None,
        })
    }

    #[getter]
    fn text(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(match &self.message().body {
            PrintBody::Assistant { blocks, .. } => joined_text(blocks),
            PrintBody::User(content) => content.text(),
        })
    }

    #[getter]
    fn blocks<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let host = BlockHost::Print(Arc::clone(&self.p.0), self.msg);
        match &self.message().body {
            PrintBody::Assistant { .. } => assistant_block_views(py, &host),
            PrintBody::User(_) => user_block_views(py, &host),
        }
    }

    #[getter]
    fn uuid(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.message().uuid.clone())
    }

    #[getter]
    fn session_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.message().session_id.clone())
    }
}

view_dunders!(
    PrintMessageView,
    "PrintMessage",
    fields = [role, model, text, blocks, uuid, session_id]
);

/// A parsed 'claude -p --output-format json' result.
///
/// Holds the billing/usage/structured-output payload, the init snapshot, and the
/// conversational messages. Reuses the shared Usage model; not a TranscriptEvent.
///
/// Attributes:
///     total_cost_usd: The total cost in USD for the run.
///     model_usage: Per-model usage and cost, keyed by model name.
///     usage: The aggregate token usage for the run.
///     structured_output: The structured output payload, when present.
///     num_turns: The number of turns in the run.
///     is_error: Whether the run ended in an error.
///     result: The final result text, when present.
///     session_id: The session the run belongs to.
///     fast_mode_state: The fast-mode state reported for the run, when present.
///     stop_reason: The reason the run stopped, when present.
///     permission_denials: The permission denials recorded during the run.
///     init: The session init snapshot, when present.
///     messages: The conversational messages of the run.
#[pyclass(name = "PrintResult", module = "cc_transcript.models", frozen)]
pub(crate) struct PrintResultView {
    pub p: PrintRef,
}

#[pymethods]
impl PrintResultView {
    #[getter]
    fn total_cost_usd(&self, _py: Python<'_>) -> PyResult<f64> {
        Ok(self.p.0.total_cost_usd)
    }

    #[getter]
    fn model_usage<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (idx, (model, _)) in self.p.0.model_usage.iter().enumerate() {
            dict.set_item(
                model,
                ModelUsageView {
                    p: self.p.clone(),
                    idx,
                },
            )?;
        }
        Ok(dict)
    }

    #[getter]
    fn usage(&self, _py: Python<'_>) -> PyResult<UsageView> {
        Ok(UsageView {
            host: UsageHost::Print(Arc::clone(&self.p.0)),
        })
    }

    #[getter]
    fn structured_output<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        opt_json(py, self.p.0.structured_output.as_ref())
    }

    #[getter]
    fn num_turns(&self, _py: Python<'_>) -> PyResult<i64> {
        Ok(self.p.0.num_turns)
    }

    #[getter]
    fn is_error(&self, _py: Python<'_>) -> PyResult<bool> {
        Ok(self.p.0.is_error)
    }

    #[getter]
    fn result(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.p.0.result.clone())
    }

    #[getter]
    fn session_id(&self, _py: Python<'_>) -> PyResult<String> {
        Ok(self.p.0.session_id.clone())
    }

    #[getter]
    fn fast_mode_state(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.p.0.fast_mode_state.clone())
    }

    #[getter]
    fn stop_reason(&self, _py: Python<'_>) -> PyResult<Option<String>> {
        Ok(self.p.0.stop_reason.clone())
    }

    #[getter]
    fn permission_denials<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let denials = self
            .p
            .0
            .permission_denials
            .iter()
            .map(|value| json_to_py(py, value))
            .collect::<PyResult<Vec<_>>>()?;
        PyTuple::new(py, denials)
    }

    #[getter]
    fn init(&self, _py: Python<'_>) -> PyResult<Option<InitInfoView>> {
        Ok(self
            .p
            .0
            .init
            .as_ref()
            .map(|_| InitInfoView { p: self.p.clone() }))
    }

    #[getter]
    fn messages<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let views = (0..self.p.0.messages.len())
            .map(|msg| {
                Bound::new(
                    py,
                    PrintMessageView {
                        p: self.p.clone(),
                        msg,
                    },
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        PyTuple::new(py, views)
    }
}

view_dunders!(
    PrintResultView,
    "PrintResult",
    fields = [
        total_cost_usd,
        model_usage,
        usage,
        structured_output,
        num_turns,
        is_error,
        result,
        session_id,
        fast_mode_state,
        stop_reason,
        permission_denials,
        init,
        messages,
    ]
);

pub(crate) fn print_result_view(
    py: Python<'_>,
    result: cc_transcript_core::types::PrintResult,
) -> PyResult<Bound<'_, PyAny>> {
    Ok(Bound::new(
        py,
        PrintResultView {
            p: PrintRef(Arc::new(result)),
        },
    )?
    .into_any())
}
