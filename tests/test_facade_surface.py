"""The public facade surface must not lose names the pre-inversion modules exported.

The v14 inversion turned ``models`` and ``tools`` into thin facades over the native
views; the object-model rewrite silently dropped several runtime bindings existing
direct imports depend on. These frozensets are the ``d936cee6`` public API surface of
each module (locals, module constants, and ``cc_transcript.*`` re-exports — stdlib
imports excluded). P4 curates the final surface; until then no old name may be missing.
"""

from __future__ import annotations

import pytest

from cc_transcript import codex, models, tools

MODELS_SURFACE = frozenset(
    {
        "ApiError", "AssistantEvent", "AsyncHookResponse", "AttachmentDetail", "AttachmentEvent",
        "Attribution", "CacheCreation", "CcVersion", "CompactBoundary", "ContentBlock", "EntryMeta",
        "EventUuid", "FallbackBlock", "HookAdditionalContext", "HookBlockingError", "HookCancelled",
        "HookInfo", "HookNonBlockingError", "HookSuccess", "InitInfo", "McpServer", "ModeEvent",
        "ModelRefusalFallback", "ModelUsage", "OtherAttachment", "OtherBlock", "OtherEvent",
        "OtherSystemDetail", "Plugin", "PreservedMessages", "PreservedSegment", "PrintMessage",
        "PrintResult", "Question", "QueuedCommand", "ServerToolUse", "SessionId", "StopHookSummary",
        "SystemDetail", "SystemEvent", "TextBlock", "ThinkingBlock", "ToolCall", "ToolDigest",
        "ToolResultBlock", "ToolUseBlock", "ToolUseId", "TranscriptEvent", "TurnDuration", "Usage",
        "UserEvent", "parse_tool_call", "thinking_chars", "tool_digest", "tool_uses",
    }
)

TOOLS_SURFACE = frozenset(
    {
        "ApplyPatchCall", "AskUserQuestionResult", "BashCall", "BashResult", "CodeModeCall",
        "EditCall", "EditResult", "EditSpan",
        "ExitPlanModeCall", "GlobCall", "GrepCall", "Hunk", "MultiEditCall", "NotebookEditCall",
        "OtherCall", "OtherResult", "PatchEdit", "QuestionAnnotation", "ReadCall", "ReadResult",
        "SkillCall",
        "SkillResult", "TOOL_ALIASES", "TaskCall", "TaskCreateCall", "TaskLaunchResult",
        "TaskResult", "TaskResultBase", "TaskUpdateCall", "TextResult", "ToolCall", "ToolCallBase",
        "ToolDigest", "ToolInputError", "ToolResult", "ToolResultBase", "ToolResultError",
        "UpdatePlanCall", "WriteStdinCall", "edits_of", "file_paths_of",
        "WorkflowCall", "WriteCall", "WriteResult", "expand_tool_names", "file_path_of", "hunks_of",
        "matches_names", "mcp_access", "mcp_parts", "parse_tool_call", "parse_tool_result",
        "tool_digest", "tool_name_matches",
    }
)


CODEX_SURFACE = frozenset(
    {
        "SESSIONS_ROOT", "CodexPendingItem", "CodexRollout", "CodexSessionInfo", "CodexUsage",
        "Lifecycle", "children_of", "discover", "find_transcript", "session_info", "sessions_root",
    }
)


@pytest.mark.parametrize(
    ("module", "surface"),
    [
        pytest.param(models, MODELS_SURFACE, id="models"),
        pytest.param(tools, TOOLS_SURFACE, id="tools"),
        pytest.param(codex, CODEX_SURFACE, id="codex"),
    ],
)
def test_facade_keeps_pre_inversion_surface(module: object, surface: frozenset[str]) -> None:
    assert surface - set(dir(module)) == set()
