"""Identity primitives shared by every layer of the platform.

This module is import-light by contract: it depends on the standard library
only, so the hook hot path can import it without paying for the parser or any
heavy dependency. It defines the only session key in the system (the Claude
session UUID), the universal :class:`EventRef` handle, and the single
cross-language tool-content digest.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from hashlib import sha256
from typing import TYPE_CHECKING, NewType

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

SessionId = NewType("SessionId", str)
EventUuid = NewType("EventUuid", str)
ToolUseId = NewType("ToolUseId", str)
ToolDigest = NewType("ToolDigest", str)

MAX_SAFE_INTEGER = 2**53 - 1


@dataclass(frozen=True, slots=True)
class EventRef:
    """A resolvable reference to a transcript event.

    The universal handle for pointing back at full content: every anchor
    parameter across the platform takes an ``EventRef``. Paths are never part
    of a reference — transcripts are located by session UUID.

    Attributes:
        session_id: The Claude session UUID the event belongs to.
        event_uuid: The transcript entry's unique identifier.
        tool_use_id: The tool-use block id, when the reference names a tool
            call rather than a whole entry.
    """

    session_id: SessionId
    event_uuid: EventUuid
    tool_use_id: ToolUseId | None = None


def canonical_json(value: object) -> bytes:
    """Serialize ``value`` per RFC 8785 (JSON Canonicalization Scheme).

    Object keys sort by UTF-16 code units, numbers serialize per ECMAScript
    ``Number::toString``, and strings escape exactly as ``JSON.stringify``.
    Raises ``ValueError`` for NaN, infinities, and integers beyond IEEE-754
    double precision — inputs that cannot canonicalize identically across
    languages.
    """
    return "".join(canonical_parts(value)).encode()


def tool_digest(tool_name: str, tool_input: Mapping[str, Any]) -> ToolDigest:
    """Digest a tool call's content into the cross-language join key.

    Computed over the raw input mapping only — never the tool-use id, which
    hook stdin does not carry — so a hook, the parser, and cc-review's Go port
    produce identical digests for the same call.
    """
    return ToolDigest(sha256(canonical_json({"input": tool_input, "tool": tool_name})).hexdigest())


def canonical_parts(value: object) -> list[str]:
    match value:
        case None:
            return ["null"]
        case bool():
            return ["true" if value else "false"]
        case int():
            if abs(value) > MAX_SAFE_INTEGER:
                raise ValueError(f"integer exceeds IEEE-754 double precision: {value}")
            return [str(value)]
        case float():
            return [es_number(value)]
        case str():
            return [json.dumps(value, ensure_ascii=False)]
        case dict():
            items = sorted(value.items(), key=lambda kv: utf16_key(kv[0]))
            return [
                "{",
                *(
                    part
                    for i, (k, v) in enumerate(items)
                    for part in ([","] if i else []) + [json.dumps(k, ensure_ascii=False), ":", *canonical_parts(v)]
                ),
                "}",
            ]
        case list() | tuple():
            return [
                "[",
                *(part for i, v in enumerate(value) for part in ([","] if i else []) + canonical_parts(v)),
                "]",
            ]
        case _:
            raise ValueError(f"type cannot canonicalize: {type(value).__name__}")


def utf16_key(key: object) -> bytes:
    if not isinstance(key, str):
        raise ValueError(f"object key must be str, got {type(key).__name__}")
    return key.encode("utf-16-be")


def es_number(value: float) -> str:
    if math.isnan(value) or math.isinf(value):
        raise ValueError(f"number cannot canonicalize: {value}")
    if value == 0:
        return "0"
    digits, exponent = shortest_digits(value)
    body = es_layout(digits, exponent)
    return f"-{body}" if value < 0 else body


def shortest_digits(value: float) -> tuple[str, int]:
    mantissa = repr(abs(value))
    exponent = 0
    if "e" in mantissa:
        mantissa, _, raw = mantissa.partition("e")
        exponent = int(raw)
    whole, _, frac = mantissa.partition(".")
    digits = (whole + frac).strip("0") or "0"
    leading = len(whole + frac) - len((whole + frac).lstrip("0"))
    return digits, exponent + len(whole) - leading


def es_layout(digits: str, n: int) -> str:
    k = len(digits)
    if k <= n <= 21:
        return digits + "0" * (n - k)
    if 0 < n <= 21:
        return f"{digits[:n]}.{digits[n:]}"
    if -6 < n <= 0:
        return f"0.{'0' * -n}{digits}"
    head = digits[0] if k == 1 else f"{digits[0]}.{digits[1:]}"
    return f"{head}e{'+' if n > 0 else '-'}{abs(n - 1)}"
