from __future__ import annotations

from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter

SCHEMA = "cc-transcript.snapshot/1"
MAX_FRAME_BYTES = 1024 * 1024
MAX_PROJECTION_BYTES = 16 * 1024 * 1024
MAX_SOURCE_BYTES = 512 * 1024 * 1024
MAX_RETAINED_BYTES = 1024 * 1024 * 1024
MAX_ENTRY_BYTES = 64 * 1024 * 1024
MAX_LEASES = 256
MAX_LEASE_MS = 30_000
MAX_PREPARATION_MS = 120_000

Token = Annotated[str, StringConstraints(min_length=1, max_length=256)]
Text = Annotated[str, StringConstraints(max_length=MAX_FRAME_BYTES)]
PathText = Annotated[str, StringConstraints(min_length=1, max_length=4096, pattern=r"^/")]
Count = Annotated[int, Field(ge=0, le=2**53 - 1)]
Positive = Annotated[int, Field(gt=0, le=2**53 - 1)]
Decimal = Annotated[str, StringConstraints(pattern=r"^[0-9]+$")]


class WireModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)


class Limits(WireModel):
    max_read_bytes: Annotated[int, Field(gt=0, le=MAX_SOURCE_BYTES)]
    max_events: Annotated[int, Field(gt=0, le=1_000_000)]
    max_items: Annotated[int, Field(gt=0, le=65_536)]
    max_output_bytes: Annotated[int, Field(gt=0, le=MAX_PROJECTION_BYTES)]
    max_discovery_entries: Annotated[int, Field(gt=0, le=1_000_000)]
    max_sources: Annotated[int, Field(gt=0, le=65_536)]


class RemainingWork(WireModel):
    max_read_bytes: Count
    max_events: Count
    max_items: Count
    max_output_bytes: Count
    max_discovery_entries: Count
    max_sources: Count


class Classifier(WireModel):
    id: Token
    version: Token


class Handle(WireModel):
    owner_epoch: Token
    snapshot_id: Token
    generation: Token
    lease_id: Token


class EventRef(WireModel):
    session_id: Token
    event_uuid: Token
    tool_use_id: Token | None


class Description(WireModel):
    handle: Handle
    lease_expires_unix_ms: Positive
    canonical_path: PathText
    source_id: Token
    device: Decimal
    inode: Decimal
    mtime_ns: Decimal
    ctime_ns: Decimal
    provider: Literal["claude", "codex"]
    parser_version: Token
    source_bytes: Count
    committed_bytes: Count
    event_count: Count
    turn_count: Count
    classifier: Classifier
    provisional_tail: bool


class Reservation(WireModel):
    owner_epoch: Token
    reservation_id: Token
    load_id: Token
    created_unix_ms: Positive
    expires_unix_ms: Positive
    absolute_deadline_unix_ms: Positive
    remaining_work: RemainingWork


class CurrentTurn(WireModel):
    kind: Literal["current_turn"]


class Prior(WireModel):
    kind: Literal["prior"]


class Recent(WireModel):
    kind: Literal["recent_events", "recent_messages"]
    count: Count


class ToolBoundary(WireModel):
    kind: Literal["after_last_tool", "before_last_tool"]
    name: Token
    file: Text | None


class EventRange(WireModel):
    kind: Literal["event_range"]
    start: Count
    stop: Count


Selector = Annotated[CurrentTurn | Prior | Recent | ToolBoundary | EventRange, Field(discriminator="kind")]


class GraphHandle(WireModel):
    graph_id: Token
    owner_epoch: Token
    revision: Token
    complete: Literal[True]


class PreparedView(WireModel):
    handle: Handle
    classifier: Classifier
    selectors: Annotated[list[Selector], Field(max_length=64)]
    attachments: Annotated[list[PathText], Field(max_length=0)]


class View(WireModel):
    handle: Handle
    classifier: Classifier
    selectors: Annotated[list[Selector], Field(max_length=64)]
    attachments: Annotated[list[PathText], Field(max_length=1024)]


class ToolPredicate(WireModel):
    kind: Literal["has_tool", "has_read", "has_command_regex"]
    pattern: Text
    subagents: bool


class ListPredicate(WireModel):
    kind: Literal["has_command", "has_edit_to", "has_skill", "has_skill_suffix", "has_pending_tool", "has_read_glob"]
    values: Annotated[list[Text], Field(max_length=256)]
    subagents: bool


class OverridePredicate(WireModel):
    kind: Literal["has_override"]
    token: Text
    invalidated_by: Annotated[list[Token], Field(max_length=256)]
    subagents: bool


class PendingNamedTask(WireModel):
    kind: Literal["pending_named_task"]


class ErrorPredicate(WireModel):
    kind: Literal["has_error", "has_edit"]
    subagents: bool


class WorkflowText(WireModel):
    kind: Literal["workflow_text"]
    mode: Literal["contains", "regex"]
    pattern: Text


class InputRegex(WireModel):
    field: Token
    pattern: Text
    flags: Count


class ToolCount(WireModel):
    kind: Literal["tool_count"]
    name: Token | None
    input_regex: InputRegex | None
    errors: Literal["exclude", "include", "only"]


class MessageCount(WireModel):
    kind: Literal["message_count"]
    role: Literal["user", "assistant", "any"]


class NamedCount(WireModel):
    kind: Literal["unresolved_tools"]
    names: Annotated[list[Token], Field(max_length=256)]


class Scalar(WireModel):
    kind: Literal["user_text", "first_prompt", "event_count", "turn_count", "failures"]


class Page(WireModel):
    kind: Literal[
        "turns",
        "events",
        "files_touched",
        "edited_files",
        "predicate_inputs",
        "deep_predicate_inputs",
        "sidechain_membership",
    ]
    order: Literal["forward", "reverse"]


class ToolCalls(WireModel):
    kind: Literal["tool_calls"]
    order: Literal["forward", "reverse"]
    name: Token | None = None


class DirectSidechains(WireModel):
    kind: Literal["direct_sidechains"]
    order: Literal["forward"]
    dispatch_ids: Annotated[list[Token], Field(max_length=256)]


class Prompts(WireModel):
    kind: Literal["prompts"]
    selection: Literal["first", "last", "current"]
    count: Count


class AssistantText(WireModel):
    kind: Literal["assistant_text"]
    count: Count
    max_per_message: Count


class SignalTexts(WireModel):
    kind: Literal["signal_texts"]
    window: Count | Literal["current_turn"]
    origin: Literal["assistant", "any"]


class RenderBudget(WireModel):
    turn_chars: Count
    tool_chars: Count


class Render(WireModel):
    kind: Literal["render"]
    budget: RenderBudget
    tool_results: bool


Query = Annotated[
    ToolPredicate
    | ListPredicate
    | OverridePredicate
    | PendingNamedTask
    | ErrorPredicate
    | WorkflowText
    | ToolCount
    | MessageCount
    | NamedCount
    | Scalar
    | Page
    | ToolCalls
    | DirectSidechains
    | Prompts
    | AssistantText
    | SignalTexts
    | Render,
    Field(discriminator="kind"),
]


class Envelope(WireModel):
    schema_: Literal["cc-transcript.snapshot/1"] = Field(alias="schema")
    id: Token


class WorkRequest(Envelope):
    deadline_unix_ms: Positive
    limits: Limits


class Acquire(WorkRequest):
    operation: Literal["acquire"]
    path: PathText
    classifier: Classifier


class Resolve(WorkRequest):
    operation: Literal["resolve"]
    session_ids: Annotated[list[Token], Field(min_length=1, max_length=256)]
    roots: Annotated[list[PathText], Field(min_length=1, max_length=64)]
    classifier: Classifier


class Locate(WorkRequest):
    operation: Literal["locate"]
    session_ids: Annotated[list[Token], Field(min_length=1, max_length=1024)]
    roots: Annotated[list[PathText], Field(min_length=1, max_length=64)]


class Resume(Envelope):
    operation: Literal["resume"]
    cursor: Token


class Release(Envelope):
    model_config = ConfigDict(
        json_schema_extra={
            "if": {"properties": {"kind": {"enum": ["lease", "reservation", "graph"]}}},
            "then": {"required": ["owner_epoch"], "properties": {"owner_epoch": {"type": "string"}}},
        }
    )
    operation: Literal["release"]
    owner_epoch: Token | None = None
    kind: Literal["lease", "reservation", "cursor", "graph"]
    token: Token


class Retain(Envelope):
    operation: Literal["retain"]
    handle: Handle


class Discover(WorkRequest):
    operation: Literal["discover"]
    roots: Annotated[list[PathText], Field(min_length=1, max_length=64)]
    checkpoint: Token | None


class Renew(Envelope):
    operation: Literal["renew"]
    handle: Handle


class Describe(Envelope):
    operation: Literal["describe"]
    handle: Handle


class Mine(WorkRequest):
    operation: Literal["mine"]
    view: View
    policy: Classifier


class Capture(WorkRequest):
    operation: Literal["capture"]
    view: View
    anchors: Annotated[list[EventRef], Field(max_length=256)]
    before: Count
    after: Count
    preview_chars: Count


class SessionHandle(WireModel):
    session_id: Token
    handle: Handle


class HydrationBudget(WireModel):
    before: RenderBudget
    trigger: RenderBudget
    after: RenderBudget


class Hydrate(WorkRequest):
    operation: Literal["hydrate"]
    handles: Annotated[list[SessionHandle], Field(max_length=256)]
    windows_json: Annotated[list[Text], Field(max_length=256)]
    render: HydrationBudget


class PreparedInputs(WireModel):
    kind: Literal["deep_predicate_inputs"]
    order: Literal["forward"]


class PreparedToolPredicate(WireModel):
    kind: Literal["has_tool", "has_read", "has_command_regex"]
    pattern: Text
    subagents: Literal[True]


class PreparedListPredicate(WireModel):
    kind: Literal["has_command", "has_edit_to", "has_skill", "has_skill_suffix", "has_read_glob"]
    values: Annotated[list[Text], Field(max_length=256)]
    subagents: Literal[True]


class PreparedOverridePredicate(WireModel):
    kind: Literal["has_override"]
    token: Text
    invalidated_by: Annotated[list[Token], Field(max_length=256)]
    subagents: Literal[True]


class PreparedErrorPredicate(WireModel):
    kind: Literal["has_error", "has_edit"]
    subagents: Literal[True]


PreparedQuery = Annotated[
    PreparedInputs | PreparedToolPredicate | PreparedListPredicate | PreparedOverridePredicate | PreparedErrorPredicate,
    Field(discriminator="kind"),
]


class PrepareGraph(WorkRequest):
    operation: Literal["prepare_graph"]
    view: PreparedView
    thread_ids: Annotated[list[Token], Field(max_length=1024)]
    roots: Annotated[list[PathText], Field(max_length=64)]
    direct_paths: Annotated[list[PathText], Field(max_length=1024)]


class QueryGraph(WorkRequest):
    operation: Literal["query_graph"]
    handle: GraphHandle
    selectors: Annotated[list[Selector], Field(max_length=64)]
    query: PreparedQuery


class WarmRegistered(WorkRequest):
    operation: Literal["warm_registered"]
    classifier: Classifier
    thread_ids: Annotated[list[Token], Field(max_length=1024)]
    roots: Annotated[list[PathText], Field(max_length=64)]
    direct_paths: Annotated[list[PathText], Field(max_length=1024)]
    start_index: Count
    membership_revision: Token | None


class QueryRequest(WorkRequest):
    operation: Literal["query"]
    view: PreparedView
    query: Query


class ActivityProbe(WorkRequest):
    operation: Literal["activity_probe"]
    view: View
    waiting_tools: Annotated[list[Token], Field(max_length=256)]
    human_facing_tools: Annotated[list[Token], Field(max_length=256)]
    tool_registry_generation: Token
    policy_version: Token


class Stats(Envelope):
    operation: Literal["stats"]


Request = Annotated[
    Acquire
    | Resolve
    | Locate
    | Discover
    | Resume
    | Release
    | Retain
    | Renew
    | Describe
    | Mine
    | Capture
    | Hydrate
    | QueryRequest
    | PrepareGraph
    | QueryGraph
    | WarmRegistered
    | ActivityProbe
    | Stats,
    Field(discriminator="operation"),
]


class Usage(WireModel):
    source_opens: Count
    source_bytes_read: Count
    bytes_decoded: Count
    events_parsed: Count
    cold_parses: Count
    append_parses: Count
    activity_lifts: Count
    cache_hits: Count
    inflight_joins: Count
    generations_published: Count
    generations_invalidated: Count
    requests_cancelled: Count
    requests_failed: Count
    output_bytes: Count
    transport_bytes: Count
    nonincremental_lowering_calls: Count
    nonincremental_lowering_source_bytes: Count
    discovery_entries_examined: Count


class Gauges(WireModel):
    retained_entry_capacity_bytes: Count
    retained_index_capacity_bytes: Count
    retained_projection_bytes: Count
    pending_input_capacity_bytes: Count
    retained_total_accounted_bytes: Count
    active_leases: Count
    pending_loads: Count
    live_generations: Count


class PreparedGraphResult(WireModel):
    kind: Literal["prepared_graph"]
    handle: GraphHandle


class WarmedRegistryResult(WireModel):
    kind: Literal["warmed_registry"]
    owner_epoch: Token
    membership_revision: Token
    next_index: Count
    complete: bool
    fact_cache_bytes: Count
    fact_cache_write_bytes: Count
    fact_cache_writes: Count


class Acquired(WireModel):
    kind: Literal["acquired"]
    description: Description


class Loading(WireModel):
    kind: Literal["loading"]
    reservation: Reservation


class Resolution(WireModel):
    session_id: Token
    status: Literal["ok", "missing", "incomplete"]
    description: Description | None


class Resolved(WireModel):
    kind: Literal["resolved"]
    sessions: Annotated[list[Resolution], Field(max_length=256)]


class Location(WireModel):
    session_id: Token
    status: Literal["ok", "missing", "incomplete"]
    path: PathText | None
    revision: Token | None


class Located(WireModel):
    kind: Literal["located"]
    sessions: Annotated[list[Location], Field(max_length=1024)]


class DiscoveryEntry(WireModel):
    path: PathText
    revision: Token
    state: Literal["present", "removed"]
    size: Count
    mtime_ns: Decimal


class Discovered(WireModel):
    kind: Literal["discovered"]
    entries: Annotated[list[DiscoveryEntry], Field(max_length=256)]
    checkpoint: Token | None


class Released(WireModel):
    kind: Literal["released"]
    released: bool


class Renewed(WireModel):
    kind: Literal["renewed"]
    expires_unix_ms: Positive


class Captured(WireModel):
    kind: Literal["captured"]
    windows_json: Annotated[list[Text], Field(max_length=256)]


class HydratedItem(WireModel):
    input_index: Count
    availability: Literal["full", "missing_ref"]
    rendered: Text | None


class Hydrated(WireModel):
    kind: Literal["hydrated"]
    windows: Annotated[list[HydratedItem], Field(max_length=256)]


class ScalarResult(WireModel):
    kind: Literal["scalar"]
    value: bool | Count | Text | None


class StringsResult(WireModel):
    kind: Literal["strings"]
    values: Annotated[list[Text], Field(max_length=256)]


class RecordsResult(WireModel):
    kind: Literal["records"]
    record_schema: Literal[
        "cc-transcript.turn/1",
        "cc-transcript.event/1",
        "cc-transcript.tool-use/1",
        "cc-transcript.file-ref/1",
        "cc-transcript.predicate-inputs/1",
        "cc-transcript.sidechain/1",
        "cc-transcript.mining-signal/1",
    ]
    records_json: Annotated[list[Text], Field(max_length=256)]


class WaitingResult(WireModel):
    kind: Literal["activity_probe"]
    waiting: bool
    reason: Text
    tool_registry_generation: Token


class StatsResult(WireModel):
    kind: Literal["stats"]
    counters: Usage
    gauges: Gauges


Result = Annotated[
    Acquired
    | PreparedGraphResult
    | WarmedRegistryResult
    | Loading
    | Resolved
    | Located
    | Discovered
    | Released
    | Renewed
    | Captured
    | Hydrated
    | ScalarResult
    | StringsResult
    | RecordsResult
    | WaitingResult
    | StatsResult,
    Field(discriminator="kind"),
]


class CompleteResponse(Envelope):
    status: Literal["ok"]
    complete: Literal[True]
    data: Result
    cursor: None
    reason: None
    usage: Usage


class IncompleteResponse(Envelope):
    status: Literal["incomplete"]
    complete: Literal[False]
    data: Result | None
    cursor: Token | None
    reason: Text
    usage: Usage


class FailedResponse(Envelope):
    status: Literal[
        "missing",
        "changed",
        "source_limit",
        "entry_limit",
        "retained_limit",
        "lease_limit",
        "output_limit",
        "deadline",
        "cancelled",
        "missing_ref",
        "parse_error",
        "permission_denied",
        "stale_handle",
        "stale_cursor",
        "invalid_request",
    ]
    complete: Literal[False]
    data: None
    cursor: None
    reason: Text
    usage: Usage


Response = Annotated[CompleteResponse | IncompleteResponse | FailedResponse, Field(discriminator="status")]


REQUEST = TypeAdapter(Request)
RESPONSE = TypeAdapter(Response)


class UserAuthority(WireModel):
    kind: Literal["user"]
    effective_uid: Decimal


class RestrictedAuthority(WireModel):
    kind: Literal["restricted_roots"]
    effective_uid: Decimal
    roots: Annotated[list[PathText], Field(min_length=1, max_length=64)]


Authority = Annotated[UserAuthority | RestrictedAuthority, Field(discriminator="kind")]


class CallContext(WireModel):
    claimant: Token
    admission: Literal["hook", "review"]
    authority: Authority
    registry_generation: Token


class StoreConfig(WireModel):
    max_retained_bytes: Positive = MAX_RETAINED_BYTES
    max_prepared_fact_memory_bytes: Positive = 128 * 1024 * 1024
    max_prepared_disk_bytes: Positive = 2 * 1024 * 1024 * 1024
    max_source_bytes: Positive = MAX_SOURCE_BYTES
    max_entry_bytes: Positive = MAX_ENTRY_BYTES
    max_projection_bytes: Positive = MAX_PROJECTION_BYTES
    max_leases: Positive = MAX_LEASES
    max_lease_ms: Positive = MAX_LEASE_MS
    max_preparation_ms: Positive = MAX_PREPARATION_MS
    max_pending_loads: Positive = 4
    max_read_bytes_per_step: Positive = 8 * 1024 * 1024
    max_events_per_step: Positive = 4096
    max_items_per_page: Positive = 256
    reserved_hook_loads: Positive = 1
    reserved_hook_leases: Positive = 32
    reserved_hook_accounted_bytes: Positive = 512 * 1024 * 1024


class SpanEditDefinition(WireModel):
    path: Token
    content: Token
    delete: Token | None


class ToolDefinition(WireModel):
    name: Token
    behaves_like: Token
    span_edit: SpanEditDefinition | None


TOOL_REGISTRY = TypeAdapter(Annotated[list[ToolDefinition], Field(max_length=256)])


CONTEXT = TypeAdapter(CallContext)
CONFIG = TypeAdapter(StoreConfig)


def main() -> None:
    import argparse
    import json
    from pathlib import Path

    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    for name, adapter in (
        ("request", REQUEST),
        ("response", RESPONSE),
        ("context", CONTEXT),
        ("config", CONFIG),
        ("tool_registry", TOOL_REGISTRY),
    ):
        path = args.directory / f"{name}.schema.json"
        contents = (
            json.dumps(
                {"$schema": "https://json-schema.org/draft/2020-12/schema", **adapter.json_schema(by_alias=True)},
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        if args.check:
            if path.read_text() != contents:
                raise SystemExit(f"generated snapshot schema is stale: {path}")
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)


if __name__ == "__main__":
    main()
