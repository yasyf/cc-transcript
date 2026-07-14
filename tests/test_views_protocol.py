"""EventList's Sequence contract and the frozen views' copy protocol.

The v14 inversion turned the parse output's ``events`` from a ``list`` into a native
``EventList`` view. This pins the sequence surface it must still satisfy and the
immutable-copy semantics of every frozen view.
"""

from __future__ import annotations

import collections.abc
import copy
import dataclasses
import pickle

import orjson
import pytest

from cc_transcript import _parser_rs
from cc_transcript.ids import EventUuid
from cc_transcript.models import ModeEvent
from tests import testkit


def event_list(*lines: dict) -> object:
    raw = b"\n".join(orjson.dumps(line) for line in lines)
    return _parser_rs.parse_bytes(raw).events


def three_events() -> object:
    return event_list(
        testkit.user_line("u1", "a"),
        testkit.user_line("u2", "b"),
        testkit.user_line("u3", "c"),
    )


def test_eventlist_is_registered_sequence() -> None:
    assert isinstance(three_events(), collections.abc.Sequence)


def test_eventlist_reversed_yields_events_in_reverse() -> None:
    events = three_events()
    assert [event.meta.uuid for event in reversed(events)] == [EventUuid("u3"), EventUuid("u2"), EventUuid("u1")]


def test_eventlist_index_and_count() -> None:
    events = three_events()
    assert events.index(events[1]) == 1
    assert events.count(events[2]) == 1
    with pytest.raises(ValueError):
        events.index("not an event")
    assert events.count("not an event") == 0


def test_eventlist_copy_returns_a_plain_list() -> None:
    events = three_events()
    duplicate = events.copy()
    assert type(duplicate) is list
    assert duplicate == list(events)


def test_eventlist_eq_is_elementwise_against_sequences() -> None:
    events = three_events()
    assert events == list(events)
    assert list(events) == events
    assert events == tuple(events)
    assert events != list(events)[:-1]
    assert events != 3


def test_eventlist_membership_and_indexing_still_work() -> None:
    events = three_events()
    assert events[0] in events
    assert len(events) == 3


def test_view_identity_across_accesses_is_not_guaranteed() -> None:
    # By design: each access materializes a fresh view over the shared parse.
    events = three_events()
    assert events[0] is not events[0]
    assert events[0] == events[0]


def test_frozen_view_copy_is_identity() -> None:
    event = three_events()[0]
    assert copy.copy(event) is event
    assert copy.deepcopy(event) is event
    block = testkit.parse_event(
        testkit.assistant_line("a1", "", blocks=[testkit.tool_use("t1", "Read", {"file_path": "x"})])
    ).blocks[0]
    assert copy.copy(block) is block
    assert copy.deepcopy(block) is block


def test_eventlist_and_transcript_copy_is_identity() -> None:
    transcript = _parser_rs.parse_bytes(orjson.dumps(testkit.user_line("u1", "a")))
    assert copy.copy(transcript) is transcript
    assert copy.deepcopy(transcript) is transcript
    events = transcript.events
    assert copy.copy(events) is events
    assert copy.deepcopy(events) is events


def test_frozen_views_are_not_picklable() -> None:
    event = three_events()[0]
    with pytest.raises((TypeError, pickle.PicklingError)):
        pickle.dumps(event)


def test_views_reject_the_dataclass_protocol() -> None:
    # v14 contract: views are not dataclasses — is_dataclass/fields/asdict/replace do not
    # apply (copy/deepcopy still work per the frozen-view tests above).
    event = testkit.parse_event(testkit.mode_line("normal", session_id="s1"))
    assert not dataclasses.is_dataclass(event)
    with pytest.raises(TypeError):
        dataclasses.fields(event)
    with pytest.raises(TypeError):
        dataclasses.asdict(event)
    with pytest.raises(TypeError):
        dataclasses.replace(event, value="plan")


def test_view_leaves_are_not_subclassable() -> None:
    with pytest.raises(TypeError):
        type("SubMode", (ModeEvent,), {})
