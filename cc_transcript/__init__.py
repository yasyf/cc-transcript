"""The cc-family session-activity platform.

One spine: parse Claude Code transcript JSONL into typed events, lift them
into a :class:`~cc_transcript.activity.SessionActivity` of turns, tool calls,
and edits — and every higher capability (context windows, evidence harvest,
queries, the decision ledger, LLM judging) is a pure function or thin store
over that spine.

The package root is lazy (PEP 562): importing ``cc_transcript.ids`` or
``cc_transcript.tools`` pulls the standard library only, so the hook hot path
pays nothing for the parser or any heavy dependency.
"""

from __future__ import annotations

from importlib import import_module
from typing import TYPE_CHECKING

EXPORTS: dict[str, str] = {
    name: module
    for module, names in {
        "cc_transcript.ids": (
            "EventRef",
            "EventUuid",
            "SessionId",
            "ToolDigest",
            "ToolUseId",
            "canonical_json",
            "tool_digest",
        ),
        "cc_transcript.tools": (
            "TOOL_ALIASES",
            "BashCall",
            "EditCall",
            "EditSpan",
            "ExitPlanModeCall",
            "GlobCall",
            "GrepCall",
            "Hunk",
            "MultiEditCall",
            "NotebookEditCall",
            "OtherCall",
            "ReadCall",
            "SkillCall",
            "TaskCall",
            "TaskCreateCall",
            "TaskUpdateCall",
            "ToolCall",
            "ToolCallBase",
            "ToolInputError",
            "WorkflowCall",
            "WriteCall",
            "expand_tool_names",
            "file_path_of",
            "hunks_of",
            "matches_names",
            "mcp_access",
            "mcp_parts",
            "parse_tool_call",
            "tool_name_matches",
        ),
        "cc_transcript.command": (
            "Command",
            "CommandLine",
            "CommandLineQuery",
            "Redirect",
            "command_prefixes",
            "parse_command_line",
        ),
        "cc_transcript.models": (
            "AssistantEvent",
            "CacheCreation",
            "CcVersion",
            "ContentBlock",
            "EntryMeta",
            "FallbackBlock",
            "InitInfo",
            "McpServer",
            "ModeEvent",
            "ModelUsage",
            "OtherBlock",
            "OtherEvent",
            "Plugin",
            "PrintMessage",
            "PrintResult",
            "ServerToolUse",
            "SystemEvent",
            "TextBlock",
            "ThinkingBlock",
            "ToolResultBlock",
            "ToolUseBlock",
            "TranscriptEvent",
            "Usage",
            "UserEvent",
        ),
        "cc_transcript.parser": (
            "TranscriptParser",
            "parse_event",
            "parse_events_async",
            "parse_events_from_bytes",
            "parse_print_result",
        ),
        "cc_transcript.cost": (
            "PRICING",
            "CostBreakdown",
            "ModelPricing",
            "cost_of",
            "cost_of_assistant",
            "resolve_pricing",
        ),
        "cc_transcript.backend": ("Backend", "ParsedTranscript"),
        "cc_transcript.discovery": (
            "CLAUDE_PROJECTS_DIR",
            "TranscriptDiscovery",
            "TranscriptExpiredError",
            "find_transcript",
            "find_transcript_sync",
            "subagent_paths",
        ),
        "cc_transcript.activity": (
            "Edit",
            "SessionActivity",
            "ToolUse",
            "Turn",
            "UserClassifier",
            "hunk_overlap",
            "native_user_classifier",
            "result_index",
        ),
        "cc_transcript.facts": (
            "ToolFact",
            "command_prefix_counts",
            "mcp_summary",
            "tool_facts",
        ),
        "cc_transcript.evidence": (
            "CandidatePair",
            "GitFix",
            "git_corrections",
            "harvest_pairs",
            "match_corrections",
            "record_harvest",
        ),
        "cc_transcript.corrections": (
            "CORRECTIONS_DDL",
            "Correction",
            "CorrectionLog",
            "Origin",
        ),
        "cc_transcript.context": (
            "ContextWindow",
            "Fidelity",
            "HydratedWindow",
            "SchemaError",
            "TurnRef",
            "capture_window",
        ),
        "cc_transcript.render": ("Budget", "render_session", "render_tool_call", "render_turn"),
        "cc_transcript.query": ("FileRef", "Session", "SubagentIndex", "SubagentSession", "ToolCallQuery"),
        "cc_transcript.decisions": ("DECISIONS_DDL", "Action", "Decision", "DecisionLog"),
        "cc_transcript.disktruth": (
            "AttributionRange",
            "DiskTruth",
            "FileAttribution",
            "TreeTurn",
            "export_activity",
            "load_export",
        ),
        "cc_transcript.filterspec": (
            "ASSISTANTS",
            "INTERRUPT_MARKER_RE",
            "JUNK_USER_MESSAGE_RE",
            "RESUME_PHRASE_SET",
            "STRUCTURAL_NOISE_RE",
            "TRIVIAL_ACK_SET",
            "USERS",
            "FilterSpec",
            "annotate_spec",
            "apply_spec",
            "keep",
            "labels_for",
        ),
        "cc_transcript.builders": (
            "NOISE_SPEC",
            "build_spec",
            "drop_compacted",
            "drop_empty",
            "drop_entrypoints",
            "drop_junk",
            "drop_meta_flag",
            "drop_phrases",
            "drop_short",
            "drop_sidechain",
            "drop_synthetic",
            "keep_only",
        ),
        "cc_transcript.messages": (
            "AssistantMessage",
            "BaseMessage",
            "MessageToolCall",
            "TranscriptMessage",
            "UserMessage",
        ),
        "cc_transcript.store": ("FileStateStore",),
    }.items()
    for name in names
}


def __getattr__(name: str) -> object:
    if module := EXPORTS.get(name):
        return getattr(import_module(module), name)
    raise AttributeError(f"module 'cc_transcript' has no attribute {name!r}")


def __dir__() -> list[str]:
    return sorted(EXPORTS)


if TYPE_CHECKING:
    from cc_transcript.activity import (
        Edit as Edit,
    )
    from cc_transcript.activity import (
        SessionActivity as SessionActivity,
    )
    from cc_transcript.activity import (
        ToolUse as ToolUse,
    )
    from cc_transcript.activity import (
        Turn as Turn,
    )
    from cc_transcript.activity import (
        UserClassifier as UserClassifier,
    )
    from cc_transcript.activity import (
        hunk_overlap as hunk_overlap,
    )
    from cc_transcript.activity import (
        native_user_classifier as native_user_classifier,
    )
    from cc_transcript.activity import (
        result_index as result_index,
    )
    from cc_transcript.backend import Backend as Backend
    from cc_transcript.backend import ParsedTranscript as ParsedTranscript
    from cc_transcript.builders import (
        NOISE_SPEC as NOISE_SPEC,
    )
    from cc_transcript.builders import (
        build_spec as build_spec,
    )
    from cc_transcript.builders import (
        drop_compacted as drop_compacted,
    )
    from cc_transcript.builders import (
        drop_empty as drop_empty,
    )
    from cc_transcript.builders import (
        drop_entrypoints as drop_entrypoints,
    )
    from cc_transcript.builders import (
        drop_junk as drop_junk,
    )
    from cc_transcript.builders import (
        drop_meta_flag as drop_meta_flag,
    )
    from cc_transcript.builders import (
        drop_phrases as drop_phrases,
    )
    from cc_transcript.builders import (
        drop_short as drop_short,
    )
    from cc_transcript.builders import (
        drop_sidechain as drop_sidechain,
    )
    from cc_transcript.builders import (
        drop_synthetic as drop_synthetic,
    )
    from cc_transcript.builders import (
        keep_only as keep_only,
    )
    from cc_transcript.command import (
        Command as Command,
    )
    from cc_transcript.command import (
        CommandLine as CommandLine,
    )
    from cc_transcript.command import (
        CommandLineQuery as CommandLineQuery,
    )
    from cc_transcript.command import (
        Redirect as Redirect,
    )
    from cc_transcript.command import (
        command_prefixes as command_prefixes,
    )
    from cc_transcript.command import (
        parse_command_line as parse_command_line,
    )
    from cc_transcript.context import (
        ContextWindow as ContextWindow,
    )
    from cc_transcript.context import (
        Fidelity as Fidelity,
    )
    from cc_transcript.context import (
        HydratedWindow as HydratedWindow,
    )
    from cc_transcript.context import (
        SchemaError as SchemaError,
    )
    from cc_transcript.context import (
        TurnRef as TurnRef,
    )
    from cc_transcript.context import (
        capture_window as capture_window,
    )
    from cc_transcript.corrections import (
        CORRECTIONS_DDL as CORRECTIONS_DDL,
    )
    from cc_transcript.corrections import (
        Correction as Correction,
    )
    from cc_transcript.corrections import (
        CorrectionLog as CorrectionLog,
    )
    from cc_transcript.corrections import (
        Origin as Origin,
    )
    from cc_transcript.cost import (
        PRICING as PRICING,
    )
    from cc_transcript.cost import (
        CostBreakdown as CostBreakdown,
    )
    from cc_transcript.cost import (
        ModelPricing as ModelPricing,
    )
    from cc_transcript.cost import (
        cost_of as cost_of,
    )
    from cc_transcript.cost import (
        cost_of_assistant as cost_of_assistant,
    )
    from cc_transcript.cost import (
        resolve_pricing as resolve_pricing,
    )
    from cc_transcript.decisions import (
        DECISIONS_DDL as DECISIONS_DDL,
    )
    from cc_transcript.decisions import (
        Action as Action,
    )
    from cc_transcript.decisions import (
        Decision as Decision,
    )
    from cc_transcript.decisions import (
        DecisionLog as DecisionLog,
    )
    from cc_transcript.discovery import (
        CLAUDE_PROJECTS_DIR as CLAUDE_PROJECTS_DIR,
    )
    from cc_transcript.discovery import (
        TranscriptDiscovery as TranscriptDiscovery,
    )
    from cc_transcript.discovery import (
        TranscriptExpiredError as TranscriptExpiredError,
    )
    from cc_transcript.discovery import (
        find_transcript as find_transcript,
    )
    from cc_transcript.discovery import (
        find_transcript_sync as find_transcript_sync,
    )
    from cc_transcript.discovery import (
        subagent_paths as subagent_paths,
    )
    from cc_transcript.disktruth import (
        AttributionRange as AttributionRange,
    )
    from cc_transcript.disktruth import (
        DiskTruth as DiskTruth,
    )
    from cc_transcript.disktruth import (
        FileAttribution as FileAttribution,
    )
    from cc_transcript.disktruth import (
        TreeTurn as TreeTurn,
    )
    from cc_transcript.disktruth import (
        export_activity as export_activity,
    )
    from cc_transcript.disktruth import (
        load_export as load_export,
    )
    from cc_transcript.evidence import (
        CandidatePair as CandidatePair,
    )
    from cc_transcript.evidence import (
        GitFix as GitFix,
    )
    from cc_transcript.evidence import (
        git_corrections as git_corrections,
    )
    from cc_transcript.evidence import (
        harvest_pairs as harvest_pairs,
    )
    from cc_transcript.evidence import (
        match_corrections as match_corrections,
    )
    from cc_transcript.evidence import (
        record_harvest as record_harvest,
    )
    from cc_transcript.facts import (
        ToolFact as ToolFact,
    )
    from cc_transcript.facts import (
        command_prefix_counts as command_prefix_counts,
    )
    from cc_transcript.facts import (
        mcp_summary as mcp_summary,
    )
    from cc_transcript.facts import (
        tool_facts as tool_facts,
    )
    from cc_transcript.filterspec import (
        ASSISTANTS as ASSISTANTS,
    )
    from cc_transcript.filterspec import (
        INTERRUPT_MARKER_RE as INTERRUPT_MARKER_RE,
    )
    from cc_transcript.filterspec import (
        JUNK_USER_MESSAGE_RE as JUNK_USER_MESSAGE_RE,
    )
    from cc_transcript.filterspec import (
        RESUME_PHRASE_SET as RESUME_PHRASE_SET,
    )
    from cc_transcript.filterspec import (
        STRUCTURAL_NOISE_RE as STRUCTURAL_NOISE_RE,
    )
    from cc_transcript.filterspec import (
        TRIVIAL_ACK_SET as TRIVIAL_ACK_SET,
    )
    from cc_transcript.filterspec import (
        USERS as USERS,
    )
    from cc_transcript.filterspec import (
        FilterSpec as FilterSpec,
    )
    from cc_transcript.filterspec import (
        annotate_spec as annotate_spec,
    )
    from cc_transcript.filterspec import (
        apply_spec as apply_spec,
    )
    from cc_transcript.filterspec import (
        keep as keep,
    )
    from cc_transcript.filterspec import (
        labels_for as labels_for,
    )
    from cc_transcript.ids import (
        EventRef as EventRef,
    )
    from cc_transcript.ids import (
        EventUuid as EventUuid,
    )
    from cc_transcript.ids import (
        SessionId as SessionId,
    )
    from cc_transcript.ids import (
        ToolDigest as ToolDigest,
    )
    from cc_transcript.ids import (
        ToolUseId as ToolUseId,
    )
    from cc_transcript.ids import (
        canonical_json as canonical_json,
    )
    from cc_transcript.ids import (
        tool_digest as tool_digest,
    )
    from cc_transcript.messages import (
        AssistantMessage as AssistantMessage,
    )
    from cc_transcript.messages import (
        BaseMessage as BaseMessage,
    )
    from cc_transcript.messages import (
        MessageToolCall as MessageToolCall,
    )
    from cc_transcript.messages import (
        TranscriptMessage as TranscriptMessage,
    )
    from cc_transcript.messages import (
        UserMessage as UserMessage,
    )
    from cc_transcript.models import (
        AssistantEvent as AssistantEvent,
    )
    from cc_transcript.models import (
        CacheCreation as CacheCreation,
    )
    from cc_transcript.models import (
        CcVersion as CcVersion,
    )
    from cc_transcript.models import (
        ContentBlock as ContentBlock,
    )
    from cc_transcript.models import (
        EntryMeta as EntryMeta,
    )
    from cc_transcript.models import (
        FallbackBlock as FallbackBlock,
    )
    from cc_transcript.models import (
        InitInfo as InitInfo,
    )
    from cc_transcript.models import (
        McpServer as McpServer,
    )
    from cc_transcript.models import (
        ModeEvent as ModeEvent,
    )
    from cc_transcript.models import (
        ModelUsage as ModelUsage,
    )
    from cc_transcript.models import (
        OtherBlock as OtherBlock,
    )
    from cc_transcript.models import (
        OtherEvent as OtherEvent,
    )
    from cc_transcript.models import (
        Plugin as Plugin,
    )
    from cc_transcript.models import (
        PrintMessage as PrintMessage,
    )
    from cc_transcript.models import (
        PrintResult as PrintResult,
    )
    from cc_transcript.models import (
        ServerToolUse as ServerToolUse,
    )
    from cc_transcript.models import (
        SystemEvent as SystemEvent,
    )
    from cc_transcript.models import (
        TextBlock as TextBlock,
    )
    from cc_transcript.models import (
        ThinkingBlock as ThinkingBlock,
    )
    from cc_transcript.models import (
        ToolResultBlock as ToolResultBlock,
    )
    from cc_transcript.models import (
        ToolUseBlock as ToolUseBlock,
    )
    from cc_transcript.models import (
        TranscriptEvent as TranscriptEvent,
    )
    from cc_transcript.models import (
        Usage as Usage,
    )
    from cc_transcript.models import (
        UserEvent as UserEvent,
    )
    from cc_transcript.parser import (
        TranscriptParser as TranscriptParser,
    )
    from cc_transcript.parser import (
        parse_event as parse_event,
    )
    from cc_transcript.parser import (
        parse_events_async as parse_events_async,
    )
    from cc_transcript.parser import (
        parse_events_from_bytes as parse_events_from_bytes,
    )
    from cc_transcript.parser import (
        parse_print_result as parse_print_result,
    )
    from cc_transcript.query import (
        FileRef as FileRef,
    )
    from cc_transcript.query import (
        Session as Session,
    )
    from cc_transcript.query import (
        SubagentIndex as SubagentIndex,
    )
    from cc_transcript.query import (
        SubagentSession as SubagentSession,
    )
    from cc_transcript.query import (
        ToolCallQuery as ToolCallQuery,
    )
    from cc_transcript.render import (
        Budget as Budget,
    )
    from cc_transcript.render import (
        render_session as render_session,
    )
    from cc_transcript.render import (
        render_tool_call as render_tool_call,
    )
    from cc_transcript.render import (
        render_turn as render_turn,
    )
    from cc_transcript.store import FileStateStore as FileStateStore
    from cc_transcript.tools import (
        TOOL_ALIASES as TOOL_ALIASES,
    )
    from cc_transcript.tools import (
        BashCall as BashCall,
    )
    from cc_transcript.tools import (
        EditCall as EditCall,
    )
    from cc_transcript.tools import (
        EditSpan as EditSpan,
    )
    from cc_transcript.tools import (
        ExitPlanModeCall as ExitPlanModeCall,
    )
    from cc_transcript.tools import (
        GlobCall as GlobCall,
    )
    from cc_transcript.tools import (
        GrepCall as GrepCall,
    )
    from cc_transcript.tools import (
        Hunk as Hunk,
    )
    from cc_transcript.tools import (
        MultiEditCall as MultiEditCall,
    )
    from cc_transcript.tools import (
        NotebookEditCall as NotebookEditCall,
    )
    from cc_transcript.tools import (
        OtherCall as OtherCall,
    )
    from cc_transcript.tools import (
        ReadCall as ReadCall,
    )
    from cc_transcript.tools import (
        SkillCall as SkillCall,
    )
    from cc_transcript.tools import (
        TaskCall as TaskCall,
    )
    from cc_transcript.tools import (
        TaskCreateCall as TaskCreateCall,
    )
    from cc_transcript.tools import (
        TaskUpdateCall as TaskUpdateCall,
    )
    from cc_transcript.tools import (
        ToolCall as ToolCall,
    )
    from cc_transcript.tools import (
        ToolCallBase as ToolCallBase,
    )
    from cc_transcript.tools import (
        ToolInputError as ToolInputError,
    )
    from cc_transcript.tools import (
        WorkflowCall as WorkflowCall,
    )
    from cc_transcript.tools import (
        WriteCall as WriteCall,
    )
    from cc_transcript.tools import (
        expand_tool_names as expand_tool_names,
    )
    from cc_transcript.tools import (
        file_path_of as file_path_of,
    )
    from cc_transcript.tools import (
        hunks_of as hunks_of,
    )
    from cc_transcript.tools import (
        matches_names as matches_names,
    )
    from cc_transcript.tools import (
        mcp_access as mcp_access,
    )
    from cc_transcript.tools import (
        mcp_parts as mcp_parts,
    )
    from cc_transcript.tools import (
        parse_tool_call as parse_tool_call,
    )
    from cc_transcript.tools import (
        tool_name_matches as tool_name_matches,
    )
