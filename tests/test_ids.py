from __future__ import annotations

import json
import math

import pytest

from cc_transcript.ids import EventRef, EventUuid, SessionId, canonical_json, tool_digest

ES_NUMBER_CASES = [
    pytest.param(0.0, "0", id="zero"),
    pytest.param(-0.0, "0", id="negative-zero"),
    pytest.param(1.0, "1", id="one"),
    pytest.param(-1.5, "-1.5", id="negative-fraction"),
    pytest.param(0.5, "0.5", id="half"),
    pytest.param(0.05, "0.05", id="five-hundredths"),
    pytest.param(100.0, "100", id="trailing-zeros-dropped"),
    pytest.param(123.456, "123.456", id="plain-decimal"),
    pytest.param(333333333.3333333, "333333333.3333333", id="rfc8785-repeating"),
    pytest.param(1e16, "10000000000000000", id="e16-expands"),
    pytest.param(1e20, "100000000000000000000", id="e20-expands"),
    pytest.param(1e21, "1e+21", id="e21-exponent"),
    pytest.param(9.999999999999997e22, "9.999999999999997e+22", id="rfc8785-large"),
    pytest.param(0.000001, "0.000001", id="e-6-expands"),
    pytest.param(1e-7, "1e-7", id="e-7-exponent"),
    pytest.param(1.5e-7, "1.5e-7", id="e-7-mantissa"),
    pytest.param(5e-324, "5e-324", id="subnormal"),
]


@pytest.mark.parametrize(("value", "expected"), ES_NUMBER_CASES)
def test_es_number_serialization(value: float, expected: str) -> None:
    assert canonical_json(value).decode() == expected


@pytest.mark.parametrize("value", [math.nan, math.inf, -math.inf], ids=["nan", "inf", "-inf"])
def test_non_finite_numbers_rejected(value: float) -> None:
    with pytest.raises(ValueError, match="cannot canonicalize"):
        canonical_json(value)


def test_integers_serialize_exactly() -> None:
    assert canonical_json({"a": 1, "b": -42, "c": 2**53 - 1}) == b'{"a":1,"b":-42,"c":9007199254740991}'


def test_integer_beyond_double_precision_rejected() -> None:
    with pytest.raises(ValueError, match="double precision"):
        canonical_json(2**53 + 1)


def test_keys_sort_by_utf16_code_units() -> None:
    emoji, euro, ffff = "\U0001f600", "€", "￿"
    result = canonical_json({ffff: 1, emoji: 2, euro: 3}).decode()
    assert result == f'{{"{euro}":3,"{emoji}":2,"{ffff}":1}}'


def test_string_escaping_matches_json_stringify() -> None:
    assert canonical_json({"k": 'a"b\\c\n\t\x1fé'}) == b'{"k":"a\\"b\\\\c\\n\\t\\u001f\xc3\xa9"}'


def test_nested_structures_canonicalize() -> None:
    value = {"b": [1, {"y": None, "x": True}], "a": "s"}
    assert canonical_json(value) == b'{"a":"s","b":[1,{"x":true,"y":null}]}'


def test_canonical_json_is_valid_json() -> None:
    value = {"edits": [{"old_string": "a", "new_string": "b"}], "file_path": "/tmp/x.py", "n": 1.5}
    assert json.loads(canonical_json(value)) == value


def test_tool_digest_is_stable_across_key_order() -> None:
    a = tool_digest("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
    b = tool_digest("Edit", {"new_string": "y", "old_string": "x", "file_path": "a.py"})
    assert a == b
    assert len(a) == 64 and int(a, 16) >= 0


def test_tool_digest_varies_by_name_and_content() -> None:
    base = tool_digest("Edit", {"file_path": "a.py"})
    assert base != tool_digest("Write", {"file_path": "a.py"})
    assert base != tool_digest("Edit", {"file_path": "b.py"})


def test_event_ref_defaults_tool_use_id_to_none() -> None:
    ref = EventRef(session_id=SessionId("s"), event_uuid=EventUuid("e"))
    assert ref.tool_use_id is None
