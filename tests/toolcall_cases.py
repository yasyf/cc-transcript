"""Hand-built edge cases for the tool-call parity golden."""

from __future__ import annotations

from typing import Any

# (tool_name, input) — aliases, MCP names, loose-value preservation, coercion quirks.
EDGE_CALLS: list[tuple[str, dict[str, Any]]] = [
    ("Execute", {"command": "ls -la", "timeout": 30, "run_in_background": True}),
    ("Create", {"file_path": "/a.py", "content": "x = 1\n"}),
    ("Task", {"prompt": "explore", "subagent_type": "Explore", "model": "opus"}),
    ("Agent", {"prompt": "explore", "agent_type": "Explore"}),
    ("ExitSpecMode", {"plan": "the plan"}),
    ("ExitPlanMode", {"plan": "another plan"}),
    ("mcp__github__create_issue", {"title": "bug", "body": "boom"}),
    ("ccx_code_edit", {"file": "/a", "at": "1-2"}),
    ("Bash", {"command": "echo hi", "description": "greet", "extra_key": "ignored"}),
    # Loose optional fields keep their verbatim value (wrong types and all).
    ("Bash", {"command": "ls", "timeout": 1.5, "description": 5, "run_in_background": "yes"}),
    ("Bash", {"command": "ls", "timeout": None}),
    ("Bash", {"command": "big", "timeout": 18446744073709551617}),
    # Alias resolution keeps the first non-null value verbatim, not the first string.
    ("Agent", {"prompt": "p", "subagent_type": 5, "agent_type": "Explore"}),
    ("Workflow", {"scriptPath": 5, "script_path": "/s"}),
    # EditCall.replace_all is not coerced (only EditSpan is): 1 stays 1, null stays None.
    ("Edit", {"file_path": "/a", "old_string": "x", "new_string": "y", "replace_all": True}),
    ("Edit", {"file_path": "/a", "old_string": "x", "new_string": "y", "replace_all": 1}),
    ("Edit", {"file_path": "/a", "old_string": "x", "new_string": "y", "replace_all": None}),
    ("Edit", {"file_path": "/a", "old_string": "x", "new_string": "y", "replace_all": "yes"}),
    (
        "MultiEdit",
        {
            "file_path": "/a",
            "edits": [
                {"old_string": "x", "new_string": "y"},
                {"old_string": "p", "new_string": "q", "replace_all": True},
                {"old_string": "m", "new_string": "n", "replace_all": 1},
                {"old_string": "s", "new_string": "t", "replace_all": None},
            ],
        },
    ),
    # Empty iterables yield empty edits; every other non-list value degrades to Other.
    ("MultiEdit", {"file_path": "/a", "edits": ""}),
    ("MultiEdit", {"file_path": "/a", "edits": {}}),
    ("MultiEdit", {"file_path": "/a", "edits": []}),
    ("MultiEdit", {"file_path": "/a", "edits": "ab"}),
    ("MultiEdit", {"file_path": "/a", "edits": {"k": "v"}}),
    ("MultiEdit", {"file_path": "/a", "edits": 5}),
    ("MultiEdit", {"file_path": "/a", "edits": None}),
    ("MultiEdit", {"file_path": "/a", "edits": [{"old_string": "x"}]}),
    ("NotebookEdit", {"notebook_path": "/n.ipynb", "new_source": "code", "cell_id": "c1", "edit_mode": "replace"}),
    ("Grep", {"pattern": "foo", "type": "py", "output_mode": "content", "glob": "*.rs", "path": "/p", "-i": True}),
    ("Glob", {"pattern": "**/*.py", "path": "/x"}),
    ("Read", {"file_path": "/a.py", "offset": 10, "limit": 200}),
    ("Skill", {"skill": "commit", "args": "--all"}),
    ("TaskCreate", {"subject": "do the thing"}),
    ("TaskUpdate", {"task_id": "7", "status": "completed", "subject": "s"}),
    ("TaskUpdate", {"taskId": "8", "description": "camelCase id key"}),
    ("Workflow", {"script": "print(1)", "args": {"k": [1, 2, 3], "n": None}}),
    ("Workflow", {"scriptPath": "/wf.py", "resumeFromRunId": "run-42", "name": "nightly"}),
    # Malformed under on_error='other' -> OtherCall (raw retained).
    ("Bash", {"description": "no command field"}),
    ("Edit", {"file_path": "/a"}),
]

# (tool_name, payload) — a dict, a denial string, None, or a bare scalar.
EDGE_RESULTS: list[tuple[str, Any]] = [
    ("Bash", "the user doesn't want to proceed with this tool use. The tool use was rejected."),
    ("Bash", {"stdout": "ok", "stderr": "", "interrupted": False, "isImage": False}),
    ("Read", {"file": {"filePath": "/a.py", "content": "x\n", "numLines": 1}, "type": "text"}),
    (
        "Write",
        {
            "content": "x\n",
            "filePath": "/a",
            "originalFile": None,
            "structuredPatch": [{"oldStart": 1, "lines": ["+x"]}],
            "userModified": False,
        },
    ),
    (
        "Edit",
        {
            "filePath": "/a",
            "oldString": "x",
            "newString": "y",
            "replaceAll": False,
            "userModified": True,
            "staleRecovered": True,
            "structuredPatch": [{"lines": ["-x", "+y"]}],
            "originalFile": "x\n",
        },
    ),
    (
        "Agent",
        {
            "agentId": "agent-1",
            "outputFile": "/tmp/out.txt",
            "isAsync": True,
            "canReadOutputFile": True,
            "description": "launch",
            "prompt": "go",
            "status": "running",
        },
    ),
    (
        "Task",
        {
            "agentId": "agent-2",
            "agentType": "Explore",
            "status": "completed",
            "totalDurationMs": 1200,
            "totalTokens": 900,
            "totalToolUseCount": 4,
            "toolStats": {"Read": 3},
            "usage": {"input_tokens": 100, "output_tokens": 20, "big": 18446744073709551617},
            "content": [{"type": "text", "text": "done"}],
            "prompt": "go",
            "resolvedModel": "claude-opus-4-8",
        },
    ),
    # Dispatch turns on non-null-ness: both null -> Other, launch marker -> TaskLaunch.
    ("Agent", {"totalDurationMs": None, "usage": None}),
    ("Agent", {"totalDurationMs": None, "outputFile": "/x"}),
    ("Skill", {"commandName": "/commit", "success": True, "allowedTools": ["Bash", "Edit"]}),
    (
        "AskUserQuestion",
        {
            "questions": [
                {"question": "Q1", "header": "h", "multiSelect": True, "options": [{"label": "A"}, {"nolabel": 1}]},
                {"noquestion": "drop me"},
                {"question": "Q2"},
            ],
            "answers": {"Q1": "A", "Q2": 5},
            "annotations": {"Q1": {"preview": "p", "notes": 7}, "Q2": "not-a-mapping"},
        },
    ),
    # Untyped tools and non-mapping payloads -> OtherResult / TextResult.
    ("TodoWrite", {"todos": []}),
    ("Agent", {"status": "neither terminal nor launch"}),
    ("Read", None),
    ("Bash", 5),
    ("Bash", [1, 2, 3]),
]

# (tool_name, input) whose strict-mode ToolInputError message the Rust binding rebuilds
# exactly: a missing/null required key, a wrong-type key, or a non-mapping input.
STRICT_RAISERS: list[tuple[str, object]] = [
    ("Bash", {}),
    ("Bash", {"command": 5}),
    ("Bash", {"command": 1.5}),
    ("Bash", {"command": True}),
    ("Bash", {"command": None}),
    ("Edit", {"file_path": "/a"}),
    ("Write", {"file_path": "/a"}),
    ("MultiEdit", {"file_path": "/a"}),
    ("TaskUpdate", {"status": "x"}),
    ("TaskUpdate", {"taskId": 5}),
    ("TaskUpdate", {"task_id": 5}),
    ("Bash", "input is not a mapping"),
    ("Bash", 5),
    ("Bash", None),
    ("Bash", [1, 2]),
]
