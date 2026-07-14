from __future__ import annotations

import dataclasses
import hashlib
from collections.abc import Mapping
from datetime import datetime
from typing import Any

from cc_transcript.models import AssistantEvent, ToolResultBlock, ToolUseBlock, UserEvent

GOLDEN_VERSION = "cc-transcript.views-golden/1"
STR_CAP = 2048
SAMPLE_CAP = 60


def capped_str(s: str) -> str | dict[str, Any]:
    if len(s) <= STR_CAP:
        return s
    return {"__strhash__": hashlib.sha256(s.encode()).hexdigest(), "__len__": len(s)}


def project(obj: Any, *, reprs: bool = False) -> Any:
    match obj:
        case None | bool() | int() | float():
            return obj
        case str():
            return capped_str(obj)
        case datetime():
            return {"__dt__": obj.isoformat()}
        case tuple() | list():
            return {"__seq__": type(obj).__name__, "items": [project(item, reprs=reprs) for item in obj]}
        case Mapping():
            return {"__mapping__": {key: project(value, reprs=reprs) for key, value in obj.items()}}
        case _ if dataclasses.is_dataclass(obj):
            return project_dataclass(obj, reprs=reprs)
        case _:
            raise TypeError(f"unprojectable {type(obj).__name__}")


def project_dataclass(obj: Any, *, reprs: bool) -> dict[str, Any]:
    from cc_transcript.tools import AskUserQuestionResult, ToolCallBase, hunks_of

    node: dict[str, Any] = {"__class__": type(obj).__name__}
    for field in dataclasses.fields(obj):
        node[field.name] = project(getattr(obj, field.name), reprs=reprs)
    if isinstance(obj, ToolUseBlock):
        node["digest"] = attr_or_raises(obj, "digest", reprs=reprs)
        node["file_path"] = project(obj.file_path, reprs=reprs)
        node["questions"] = project(obj.questions, reprs=reprs)
        try:
            call = obj.call
        except ValueError as error:
            node["call"] = {"__raises__": type(error).__name__}
        else:
            node["call"] = project(call, reprs=reprs) | {"__hunks__": project(hunks_of(call), reprs=reprs)}
    if isinstance(obj, ToolCallBase):
        node["digest"] = attr_or_raises(obj, "digest", reprs=reprs)
    if isinstance(obj, AskUserQuestionResult):
        node["questions"] = project(obj.questions, reprs=reprs)
    if reprs:
        node["__repr__"] = capped_str(repr(obj))
    return node


def attr_or_raises(obj: Any, name: str, *, reprs: bool) -> Any:
    try:
        value = getattr(obj, name)
    except ValueError as error:
        return {"__raises__": type(error).__name__}
    return project(value, reprs=reprs)


def project_tool_result(name: str, payload: Any, *, reprs: bool) -> dict[str, Any]:
    from cc_transcript.tools import ToolResultError, parse_tool_result

    try:
        result = parse_tool_result(name, payload)
    except ToolResultError:
        return {"__raises__": "ToolResultError"}
    return project(result, reprs=reprs)


def tool_result_names(events: list[Any]) -> dict[str, str]:
    return {
        block.id: block.name
        for event in events
        if isinstance(event, (UserEvent, AssistantEvent))
        for block in event.blocks
        if isinstance(block, ToolUseBlock)
    }


def sample_indices(count: int, cap: int = SAMPLE_CAP) -> list[int]:
    if count <= cap:
        return list(range(count))
    edge = cap // 4
    body = cap - 2 * edge
    mids = {edge + (i * (count - 2 * edge)) // body for i in range(body)}
    return sorted(set(range(edge)) | mids | set(range(count - edge, count)))


def file_record(events: list[Any], *, mode: str) -> dict[str, Any]:
    reprs = mode == "full"
    indices = range(len(events)) if mode == "full" else sample_indices(len(events))
    names = tool_result_names(events)
    events_node: dict[str, Any] = {}
    for i in indices:
        event = events[i]
        node: dict[str, Any] = {"proj": project(event, reprs=reprs)}
        if results := {
            str(bi): project_tool_result(names.get(block.tool_use_id, "Unknown"), block.tool_use_result, reprs=reprs)
            for bi, block in enumerate(event.blocks if isinstance(event, (UserEvent, AssistantEvent)) else ())
            if isinstance(block, ToolResultBlock)
        }:
            node["results"] = results
        events_node[str(i)] = node
    return {"count": len(events), "mode": mode, "events": events_node}


def replay_file(record: Mapping[str, Any], events: list[Any]) -> None:
    assert len(events) == record["count"], f"event count {len(events)} != {record['count']}"
    names = tool_result_names(events)
    for key, node in record["events"].items():
        event = events[int(key)]
        compare(node["proj"], event, f"$[{key}]")
        for bkey, expected in node.get("results", {}).items():
            block = event.blocks[int(bkey)]
            compare_tool_result(expected, names.get(block.tool_use_id, "Unknown"), block, f"$[{key}].results[{bkey}]")


def compare_tool_result(expected: Any, name: str, block: Any, path: str) -> None:
    from cc_transcript.tools import ToolResultError, parse_tool_result

    if isinstance(expected, dict) and "__raises__" in expected:
        try:
            parse_tool_result(name, block.tool_use_result)
        except ToolResultError:
            return
        raise AssertionError(f"{path}: expected ToolResultError")
    compare(expected, parse_tool_result(name, block.tool_use_result), path)


def compare(golden: Any, live: Any, path: str = "$") -> None:
    match golden:
        case {"__class__": cls}:
            assert type(live).__name__ == cls, f"{path}: {type(live).__name__} != {cls}"
            for key, sub in golden.items():
                match key:
                    case "__class__":
                        pass
                    case "__repr__":
                        compare_leaf_str(sub, repr(live), f"{path}!repr")
                    case "__hunks__":
                        compare_hunks(sub, live, f"{path}!hunks")
                    case _ if isinstance(sub, dict) and "__raises__" in sub:
                        expect_raises(live, key, sub["__raises__"], f"{path}.{key}")
                    case _:
                        compare(sub, getattr(live, key), f"{path}.{key}")
        case {"__dt__": iso}:
            assert isinstance(live, datetime), f"{path}: {type(live).__name__} != datetime"
            assert live.isoformat() == iso, f"{path}: {live.isoformat()} != {iso}"
        case {"__seq__": kind, "items": items}:
            assert type(live).__name__ == kind, f"{path}: {type(live).__name__} != {kind}"
            assert len(live) == len(items), f"{path}: len {len(live)} != {len(items)}"
            for i, item in enumerate(items):
                compare(item, live[i], f"{path}[{i}]")
        case {"__mapping__": mapping}:
            # v14: memoized dict fields (ToolUseBlock.input, ToolResultBlock.tool_use_result)
            # are a read-only dict subclass, so compare by value/keys, not exact dict type.
            assert isinstance(live, Mapping), f"{path}: {type(live).__name__} is not a Mapping"
            assert list(live.keys()) == list(mapping.keys()), f"{path}: keys {list(live)} != {list(mapping)}"
            for key, sub in mapping.items():
                compare(sub, live[key], f"{path}[{key!r}]")
        case {"__strhash__": _}:
            compare_leaf_str(golden, live, path)
        case {"__raises__": exc}:
            raise AssertionError(f"{path}: expected {exc}, got {type(live).__name__}")
        case bool():
            assert type(live) is bool and live == golden, f"{path}: {live!r} != {golden!r}"
        case int():
            assert type(live) is int and live == golden, f"{path}: {live!r} != {golden!r}"
        case float():
            assert type(live) is float and repr(live) == repr(golden), f"{path}: {live!r} != {golden!r}"
        case str():
            assert type(live) is str and live == golden, f"{path}: {live!r} != {golden!r}"
        case None:
            assert live is None, f"{path}: {type(live).__name__} is not None"
        case _:
            raise AssertionError(f"{path}: unhandled golden node {golden!r}")


def compare_leaf_str(golden: Any, live: Any, path: str) -> None:
    assert type(live) is str, f"{path}: {type(live).__name__} != str"
    match golden:
        case {"__strhash__": digest, "__len__": length}:
            assert len(live) == length, f"{path}: len {len(live)} != {length}"
            live_digest = hashlib.sha256(live.encode()).hexdigest()
            assert live_digest == digest, f"{path}: sha256 {live_digest} != {digest}"
        case str():
            assert live == golden, f"{path}: {live!r} != {golden!r}"
        case _:
            raise AssertionError(f"{path}: unhandled string node {golden!r}")


def expect_raises(live: Any, key: str, exc_name: str, path: str) -> None:
    try:
        getattr(live, key)
    except ValueError as error:
        assert type(error).__name__ == exc_name, f"{path}: raised {type(error).__name__} != {exc_name}"
        return
    raise AssertionError(f"{path}: expected {exc_name}")


def compare_hunks(expected: Any, call: Any, path: str) -> None:
    from cc_transcript.tools import hunks_of

    compare(expected, hunks_of(call), path)
