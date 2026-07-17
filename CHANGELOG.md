# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The object model inverts: the eager dataclasses become lazy views over one native
parse. Stub — factual bullets only; P7 finalizes the prose.

### Changed
- **BREAKING: events, blocks, and tool calls/results are native frozen views, not
  dataclasses.** They are not constructible from Python and not subclassable at the
  leaves. `dataclasses.is_dataclass` is `False`, and `fields`/`asdict`/`replace`, `pickle`,
  and `copy.replace` no longer apply; `copy.copy`/`copy.deepcopy` return the immutable
  object itself. Import paths, class names, field names, `isinstance`, keyword `match`,
  `repr`, and value equality/hash are unchanged.
- **BREAKING: `ToolUseBlock.input` and `ToolResultBlock.tool_use_result` are read-only.**
  A structured mapping is a `ReadOnlyDict` (a `dict` subclass that raises on mutation) — it
  serializes, canonicalizes, and `isinstance`-checks as a `dict`, only the top level is
  frozen, and mutation raises `TypeError`.
- **BREAKING: `Transcript.events` is an `EventList`, not a `list`.** It implements the
  immutable `collections.abc.Sequence` interface and registers as one; `copy()` returns a
  plain `list`. Views materialize fresh on access, so identity across accesses is not
  guaranteed (`events[0] is events[0]` is `False`).
- **Memory model.** One parse owns one shared entry buffer; retaining any view keeps that
  whole parse's entries alive. The live `watch` stream is exempt (one buffer per event).
- **Parse divergences (native backend).** Root-level and print-envelope duplicate keys read
  first-wins (orjson: last-wins); invalid or empty print JSON raises `ValueError` (not
  `orjson.JSONDecodeError`/`StopIteration`); a below-`MINYEAR` timestamp drops the line
  rather than raising; tool input serialized outside the JSON contract (datetime, bytes,
  cycles) raises in strict mode and degrades to a fallback under `on_error='other'`.
- **BREAKING: post-parse spec regexes use the Rust regex dialect.** `keep`, `labels_for`,
  `apply_spec`, and `annotate_spec` compile a `TextMatchesAny` clause in the native core (the
  Rust `regex` crate) — the same engine parse-time filtering always used, so one dialect now
  governs a spec everywhere. Two shifts from the former Python `re` post-parse path: `$` no
  longer matches before a trailing newline (`foo$` misses `"foo\n"`), and constructs Rust
  `regex` has no equivalent for — lookaround (`(?=…)`, `(?<=…)`) and `\Z` — raise `ValueError`
  at match time rather than compiling.
- **BREAKING: `tool_facts(paths, *, max_events)` takes transcript file paths, not parsed
  transcripts.** The tool-fact projection runs entirely in the native core; the facade
  re-parses each path, caps it at `max_events` events, and rehydrates each call into a
  `ToolFact`, re-attaching its source `path`. `command_prefix_counts` and `mcp_summary` keep
  their signatures; the internal `is_denial`/`denial_fields`/`mcp_split`/`fact_of` helpers are gone.
- **BREAKING: `capture_window(raw, anchor, *, before, after, preview_chars)` takes transcript
  bytes, not a `SessionActivity`.** Window capture and typed-preview construction run in the
  native core; the facade re-parses `raw`, captures around `anchor`, and rehydrates the
  `ContextWindow`. `sample_windows(raw, ...)` likewise takes bytes and lifts internally. The
  `ContextWindow`/`TurnRef`/preview dataclasses, `to_json`/`from_json`, `hydrate`, and
  `render_preview` stay Python; the internal `build_previews`/`preview_of_call`/`ask_preview`/
  `turn_ref` builders are gone.
- **`ConversationBucket.bucket_start` is UTC-aligned.** The native bucketer emits each window's
  start as a `tzinfo=UTC` `datetime`, where the deleted Python bucketer preserved the source
  event's original offset. The instant is unchanged — `2026-01-06T09:00:00+05:30` and
  `2026-01-06T03:30:00+00:00` are the same moment — but `tzinfo` and `isoformat()` now render in
  UTC.

### Removed
- Removed the `pricing=` override from `cost_of`/`cost_of_assistant` (native pricing is
  canonical); `PRICING` remains importable.
- Removed the Python raw-bytes renderers now owned by the native core — `compact_line`,
  `haystack`, and `collect_stats`/`Stats`/`render_stats` (with their `truncate`/`human_size`
  helpers). `render_compact_lines`/`render_haystacks`/`render_stats` on the native core are
  canonical; `Budget`, `render_tool_call`, `render_turn`, and `render_session` remain the
  Python object renderers.
- Removed `cc_transcript/backend.py` and its `ParsedTranscript` dataclass — the
  `(path, mtime, events)` container that `tool_facts` and the raw-bytes renderers consumed.
  Both surfaces now take transcript paths or bytes (see Changed), leaving it no consumer.
- Removed the remaining module-visible helpers the native facade flip absorbed. None appeared
  in the curated API reference, but each was an importable module-level name:
  - `render.py`: `display_path`, `transcript_header`, `render_counts`, `render_mcp`,
    `render_histogram`, `fact_dict`, `fact_line`, `denial_dict`, `denial_line`, `event_dict`,
    `view_asdict`, `view_field`, `render_span`, `line_budget`, `stats_dict`, the
    `assistant_payload`/`user_payload`/`event_payload`/`block_payload`/`result_payload`/
    `attachment_text`/`tool_haystack` projections, the `ViewLike` protocol, and the
    `BLANK_TIME`/`SIZE_UNITS`/`TAGS`/`UNCLIPPED`/`VIEW_TYPES`/`WHERE_ALL` constants — the
    CLI-line and record-projection layer the native renderers replaced.
  - `filterspec.py`: `clause_matches`, `predicate_matches`, `normalize_bare`, `meta_flag` — the
    Python spec-evaluation helpers now living in the native `SpecMatcher`.
  - `command.py`: `BASH_PARSER` and the `ASSIGNMENT_RE`/`PIPE_GAP_RE`/`COMPOUND_OPS`/
    `MULTI_LEVEL_TOOLS`/`REDIRECT_OP_TYPES`/`WRAPPER_COMMANDS` parsing tables — bash parsing
    runs in the native splice layer; `Command`, `CommandLine`, `CommandLineQuery`,
    `Occurrence`, and `Redirect` stay importable as native views.
  - `context.py`: `preview_block_parts` and the `ASK_USER_QUESTION` constant — the last
    Python-side preview builder and its tool-name key, alongside the builders already noted
    under Changed.
  - `notifications.py`: `replay_queue`, `delivered_text` — the notification-replay helpers now
    lifted in the native core.
  - `sentiment/buckets.py`: `ConversationBucketer.align_to_bucket` — bucket alignment is
    internal to the native bucketer.
  - `cost.py`: `MTOK` — the per-million-tokens constant folded into the native cost model.

## [13.2.0] - 2026-07-14

### Added
- **The byte-span splice layer on parsed command lines.** `Command.span` is the
  command's byte range in its source line; `CommandLine.occurrences()` yields typed
  `Occurrence` handles (each carrying its command plus `prev_op`/`next_op`/`piped`
  context); `CommandLine.splice(replacements)` rewrites exact byte ranges in the
  original line, raising `ValueError` on overlapping spans or a span-less command;
  and `CommandLine.rewrite_occurrences(to)` maps a callable over every occurrence
  and splices the results. `Occurrence` joins the root exports.

## [13.1.0] - 2026-07-13

Dispatch observability: a heartbeat ledger that records an event reached dispatch at
all, so a silent wiring gap stops reading as a quiet session.

### Added
- **`HeartbeatLog` — a per-`(session, event)` dispatch heartbeat.** The decision ledger
  records only hooks that *fired*, so "this event never dispatched" and "it dispatched and
  matched nothing" are indistinguishable there. A new `dispatch_heartbeats` table (its own
  DDL, sharing `decisions.db`; the Go-byte-compared `decisions` schema is untouched) takes
  an unconditional, upserting `(session, event)` beat — `beat` bumps a count and last-seen
  timestamp, keeping one row per event — so a missing beat for an event a session should
  emit is an unambiguous wiring gap.

## [13.0.0] - 2026-07-12

One engine, and a real one. The Rust extension is now the only executor — the
Python reference backend, every portability gate, and `CC_TRANSCRIPT_DISABLE_RUST`
are gone — and with a single implementation to maintain, proper NLP is
affordable again: v12's `str.isalpha` tokenizer is replaced by an embedded
UDPipe model, and sentiment scoring understands negation. Two breaking themes
(execution model, sentiment behavior); prebuilt wheels still install without a
toolchain.

### Changed
- **BREAKING: `_parser_rs` is a hard requirement; there is no Python executor.**
  The `Backend` protocol, `PythonBackend`, `rust.RustBackend`, the
  `is_portable`/`score_spec_is_portable`/`mining_spec_is_portable` gates, and the
  `CC_TRANSCRIPT_DISABLE_RUST` escape hatch are deleted. `FilterSpec`,
  `ScoreSpec`, and `MiningSpec` execute only in Rust; parsing, the activity
  probe, and command-prefix extraction import the extension directly.
  `TranscriptParser.backend_name()` returns the literal `"rust"`. Cross-backend
  parity suites are replaced by golden regression fixtures
  (`tests/testdata/{filter,score,mining}_golden.json`).
- **BREAKING: the sentiment lexicon tokenizes with a real NLP pipeline.** The
  generated `str.isalpha` Unicode table is gone; tokenization, POS tagging,
  lemmatization, and dependency parsing run over an embedded UDPipe model
  (UD_English-EWT, shipped in the wheel, no download) via `udpipe-rs`. The
  tokenizer splits multiword tokens the way Universal Dependencies does
  (`can't` → `ca` + `n't`), so tokenizer output and the AFINN
  tokenizer-unreachable inventory both change (28 → 26 keys).
- **BREAKING: `Lexicon.has_hit` is negation-aware.** A negated term's surface
  polarity is sign-flipped, so "isn't great" reaches the negative floor and "no
  damage" the positive one. Polarity lookup stays surface-keyed — never the
  lemma (the v12 fix stands); POS and negation only gate and flip. An audit over
  a real-transcript sample (`scripts/audit_lexicon.py`, re-runnable) reclassifies
  ~7.5% of sampled buckets, all negation-driven. POS-based suppression is a
  highlighter concern and does not touch scoring.

### Added
- **`cc_transcript.nlp`** — a public token substrate. `analyze(text)` returns
  frozen `Token`s carrying the surface form, lowercased form, lemma, UPOS tag,
  codepoint span, surface polarity, and a negation flag.
- **`MiningSpec.CallableReviewFormat` runs against the Rust miner** through a
  pyo3 callback: an arbitrary Python extractor invoked single-threaded under the
  held GIL for review-comment shapes no declarative format covers.
- **`scripts/train_udpipe_model.sh`** — the reproducible recipe (UDPipe @
  9158bf7, UD_English-EWT @ r2.18) that produces the shipped model; see `NOTICE`
  for the CC BY-SA 4.0 attribution.

### Removed
- The `str.isalpha` codepoint table (`rust/src/generated/unicode.rs`) and its
  generator, dissolving the tokenizer's Unicode-version drift question.
- The Python spec interpreters — `apply_spec`'s execution internals,
  `py_short_circuit`/`py_post_process`, and the Python mining detectors — and the
  `rust_*_backend` fallback selectors. The post-parse APIs over already-materialized
  events stay: `filterspec.keep`/`apply_spec` (the CLI's `show`/`grep`) and
  `mining.mine`, whose events-in signature is unchanged but which now runs the Rust
  detector pipeline in place of the deleted Python detectors.

## [12.1.1] - 2026-07-11

Row-invariant hardening plus docs coverage for the 12.1.0 features.

### Fixed
- Optional-row guards are replaced by fail-fast asserts in `facts.py`,
  `judge/similar.py`, `mining/store.py`, and `tools.py`; `replace_all` is coerced
  to `bool` at the boundary.

### Documentation
- The docs site now covers the `watch` module and `mining.sample_windows`.

## [12.1.0] - 2026-07-11

Live transcript tailing and seeded context-window sampling.

### Added
- **`cc_transcript.watch`** — live transcript tailing. `watch`/`tick` poll a
  transcript by byte offset and yield each appended event exactly once, over
  `TailState`/`TailCursor`/`WatchEvent`; a `watch` CLI command and discovery
  additions ship with it.
- **`mining.sample_windows`** (with `fold_trigger` and `turn_anchor`) — seeded
  context-window sampling that draws deterministic "did not steer here" negatives.

## [12.0.0] - 2026-07-11

The sentiment lexicon is rebuilt around one principle: AFINN is surface-keyed,
so lookup is surface-keyed too. Both lemmatizers are gone, scoring is
deterministic and identical across backends, and the lexicon carries no
models, no downloads, and no optional extra. One breaking change (the lexicon
API).

### Changed
- **BREAKING: surface-only lexicon lookup with a shared deterministic tokenizer.**
  Both backends lemmatized each token and looked the lemma up in surface-keyed
  tables — a category error against AFINN, which calibrates inflections as
  separate rows. Lookup is now surface-form over the same package-data TSVs on
  both sides (`importlib.resources` in Python, `include_str!` in Rust), behind
  one tokenizer: maximal runs of Unicode letter characters (Python
  `str.isalpha` semantics; Rust matches them exactly through a generated
  660-range table pinned to Unicode 15.1.0), lowercased per run.
  `Lexicon.has_hit` loses its `floor` parameter — the ±3 floor is fixed — and
  `clamp_positive()`/`demote_mild_irritation()` lose theirs with it (no caller
  ever overrode them). The domain-override table grows 38 → 61 rows: audited
  inflection closures for the strong families, the break-family calibration
  (`broke`/`breaks`/`breaking` at -2 versus `broken` at -3, now reliably
  reachable), and the dictionary-sweep additions `dies`, `panicking`, `misled`,
  `crises`, and `quitted`. A 252k-word sweep against both old lemmatizers
  pinned those five as the entire real regression surface.
- **rustfmt is the enforced baseline.** `cargo fmt` over the crate, an edition
  pin in `rust/rustfmt.toml`, and a `cargo fmt --check` gate in CI.

### Removed
- **The `[sentiment]` extra and both lemmatizers.** The spaCy `NLP` class and
  model-download machinery, `Lexicon.ensure_ready`,
  `FilteredEngine.prepare_lexicon`, the `lexicon_available` binding, and the
  Rust UDPipe machinery with its scoring-path mutex, LINDAT mirror workaround,
  and `udpipe-rs`/`dirs` dependencies. The lexicon needs nothing beyond the
  wheel.
- **`scripts/build_lexicon_data.py`.** The TSVs under
  `cc_transcript/sentiment/data/` are the canonical vendored data (AFINN-165
  provenance in the header); `tests/test_lexicon_parity.py` owns their
  validation.

### Fixed
- **Cross-backend scoring nondeterminism.** spaCy and UDPipe picked different
  rows for the same text — "this is broken" crossed the hostile floor on Rust
  but not on Python — and 23% of floor-relevant vocabulary disagreed across
  backends. One deterministic path remains.
- **Strong inflections scored zero.** "lost" and "losing" carry -3 in AFINN,
  but the lemma "lose" has no row, so both backends scored them 0. Surface
  lookup restores them (`lost` appeared 304 times in the audit's
  real-transcript sample).

### Added
- **A hermetic exact-equality lexicon parity suite.** The shared tokenizer
  fixture (Unicode edge cases pinned), the break/broken and lost/losing
  regressions, `has_hit`/`polarity` equality over every override row plus a
  deterministic AFINN sample, and reachability invariants — zero models, zero
  skips.
- **Single-sourced command-prefix pins.** The 32-row battery lives once in
  `rust/data/command_prefix_pins.tsv`, read by both `tests/test_command.py`
  and the Rust test suite.

## [11.0.0] - 2026-07-11

Structured-first protocol parsing: every semantic signal the transcripts carry
as JSON is now read from the structured field, with the legacy text sniff kept
only as the fallback for old transcripts. One breaking change (attachments).

### Changed
- **BREAKING: attachment records are a typed `AttachmentEvent`.** `type:"attachment"`
  records now parse to a new `TranscriptEvent` member carrying a full `EntryMeta`
  envelope and a typed `AttachmentDetail` union — `HookSuccess`,
  `HookBlockingError`, `HookNonBlockingError`, `HookCancelled`,
  `HookAdditionalContext`, `AsyncHookResponse`, `QueuedCommand`, and a non-lossy
  `OtherAttachment(raw)` — instead of landing untyped in `OtherEvent`. Fallout:
  match arms on `OtherEvent` with a raw `type=="attachment"` no longer fire;
  `EventKind` gains `"attachment"`, so `keep_only("other")` no longer retains
  attachments and a `KindIs({"other"})` drop no longer removes them; attachments
  now carry meta, so `session_id_of`, `MetaFlag`/`EntrypointIn` predicates
  (including `drop_sidechain()`), turn stamps, stats spans, and
  `last_event_epoch` observe them, and a malformed attachment line fails the
  file like any other typed record. Malformed payloads degrade identically in
  both backends, pinned by parity fixtures.
- **Denial detection reads `toolDenialKind`.** `ToolResultBlock.denial_kind` is
  computed once at the parse layer — the record-level field when present, else
  `user-rejected` via the legacy banner on error blocks — and `facts.is_denial`,
  `ToolFact`, rendering, and both mining `denial_results` are pure field reads.
  A `permission-rule` hook block is never a user denial and `ToolFact.denied`
  keeps meaning human rejection only; hook blocks that the banner sniff could
  never see now surface via `ToolFact.denial_kind`.
- **AskUserQuestion mining is structured-first.** When a non-error result block's
  `toolUseResult` carries `answers`, mined pairs come from the payload's
  `questions`/`answers`/`annotations` (falling back to the joined tool-use
  questions when the payload omits them); the rendered-banner re-parser remains
  only as the fallback for old transcripts. Both paths yield identical signals
  for the same round, non-string answer and annotation leaves read as absent in
  both languages, and error rounds never mine.

### Added
- **`ToolResultBlock.tool_use_result` and the typed result hierarchy.** The
  record-level `toolUseResult` payload rides on every result block verbatim
  (dict, string, or absent; `is_async` derives from it), and `tools.py` gains a
  stdlib-only per-tool result hierarchy — `BashResult`, `EditResult`,
  `WriteResult`, `ReadResult`, `TaskResult`/`TaskLaunchResult`, `SkillResult`,
  `AskUserQuestionResult`, `TextResult`, `OtherResult` — behind
  `parse_tool_result`, joined by tool name at `activity.ToolUse.typed_result`.
- **`SystemEvent.level` and a typed `detail` union.** `SystemEvent` was the one
  lossy event kind; it now carries `StopHookSummary`, `CompactBoundary`
  (field-complete against the corpus, including `preservedMessages.allUuids`),
  `TurnDuration`, and `ModelRefusalFallback` typed from their subtype payloads,
  with every other subtype preserved whole in `OtherSystemDetail(raw)`.
- **`AssistantEvent.api_error`.** Rate-limit/outage markers
  (`isApiErrorMessage`, `apiErrorStatus`, `error`, `errorDetails`) surface as a
  typed `ApiError` instead of being dropped.
- **`UserEvent.interrupted_message_id`** (the `msg_` API id of the interrupted
  assistant turn), and `UserEvent.interrupted` is now the text marker OR the
  structured field — the marker stays primary since field coverage is partial.
- **Envelope fields.** `EntryMeta.user_type`/`slug`; `UserEvent.prompt_id`,
  `prompt_source`, `queue_priority`, `image_paste_ids`, `source_tool_use_id`,
  `source_tool_assistant_uuid`, `mcp_meta`, `permission_mode`;
  `AssistantEvent.request_id`, `forked_from`, and a typed `Attribution`
  (plugin/skill/MCP provenance).

### Fixed
- **Rust number parsing matches orjson exactly.** sonic-rs collapsed `-0`/`-0.0`
  to positive zero and disagreed with orjson on beyond-u64 integers; the Rust
  backend now preserves sign and magnitude semantics byte-for-byte, pinned by a
  type-and-repr parity test.

## [10.8.0] - 2026-07-11

### Added
- **`unjudged(probe_hydration=)`.** A keyword-only flag on
  `VerdictStoreMixin.unjudged`; passing `False` skips the per-row
  `hydratable()` transcript-discovery probe, for dashboard-style callers
  that only count backlog rows and can tolerate rows whose transcripts are
  no longer discoverable. The default (`True`) preserves every existing
  caller byte-for-byte.

### Changed
- **`find_transcript_sync` memoizes positive hits.** By-UUID lookups used
  to rescan the full projects tree on every call; a module-level memo keyed
  by session id and resolved root now returns warm hits in O(1). Misses are
  never cached (a transcript may appear later), and a cached path that no
  longer exists falls through to a fresh scan. `find_transcript` inherits
  the memo through its sync core.

## [10.7.0] - 2026-07-10

### Added
- **Generated Rust literals.** Every constant the Rust backend used to hand-mirror
  from Python — the CC-protocol marker strings and patterns, the mining source-kind
  and detector ids, the answer separators and confidence floors, and the bash
  command tables — is now rendered by `scripts/build_rust_literals.py` from the
  Python source of truth into committed `rust/src/generated/` modules. One manifest
  drives both the emission and the drift gate: `_parser_rs.embedded_literals()`
  exposes every embedded value, and `tests/test_literals_parity.py` asserts
  whole-dict equality against the Python constants plus byte-for-byte freshness of
  the committed files. The "mirror this in Rust" comment discipline is gone with
  the duplication it policed.
- **`Question` and `ToolUseBlock.questions`.** A typed lift of AskUserQuestion
  rounds onto the event model, mirroring the Rust parse layer field-for-field —
  including its leniency on malformed input — so the mining detectors consume
  typed rounds instead of hand-parsing `input` dicts.
- **`ToolUseBlock.file_path`.** The uniform raw lift of the input's `file_path`
  (a string, else `None`), matching the Rust parse-time field byte-for-byte;
  denial evidence reads it instead of reaching into the raw input.
- **`compile_groups(..., multiline=...)`.** The one group compiler now covers the
  review-format case; see Removed.
- **Toolchain pin test.** `tests/test_toolchain_parity.py` asserts `tree-sitter-bash`
  resolves to the same version in `Cargo.lock` and `uv.lock` (and the core crate
  stays on one minor), so a grammar bump on one side of the bash-parsing parity
  pair fails fast instead of drifting silently.

### Changed
- **One sync-ledger implementation.** `CorrectionLog` and `DecisionLog` now share
  a `SyncLedger` base owning the connection setup, schema-driven append, and
  session queries. Schemas, dedup semantics, and every public query are unchanged.

### Removed
- **`mining.spec.compile_review_format`.** Folded into
  `filterspec.compile_groups(multiline=True)`; the compiled pattern string and
  flags are byte-identical.

### Fixed
- **Sentiment buckets apply the junk filter.** `SENTIMENT_JUNK_GROUPS` existed but
  nothing in the sentiment path consumed it, so `ConversationBucketer` scored CC
  protocol noise — slash-command wrappers, interrupt markers, stop-hook feedback,
  bash-mode echoes — as authored prose. `bucket_events` now drops junk turns
  before `MIN_USER_TURNS` eligibility, the junk set gains the bash-mode command
  echoes, `<local-command-stderr>` joins its sibling wrappers, and the command-echo
  pattern is start-anchored so a turn that merely mentions `<bash-input>`
  mid-sentence stays scored. Sentiment scores over noisy sessions change
  accordingly — a deliberate correction.
- **The interrupt marker folds identically in both engines.** The leading "i" of
  "interrupted" is ASCII-pinned via `(?-i:[Ii])`, closing a dotted/dotless-I
  divergence where Python `re.IGNORECASE` matched U+0130/U+0131 substitutions and
  the Rust regex crate did not — the same discipline `role_reminder` already uses.
- **`--no-default-features` builds need no Python.** `pyo3-build-config` is now an
  optional, feature-gated build dependency, so the Swift consumer's build resolves
  zero pyo3 crates and runs on machines without a Python toolchain.
- **The lexicon spaCy-parity test is hermetic.** It sampled live transcripts from
  the running machine, so pass/fail depended on what happened to be on disk; it now
  runs a frozen adversarial fixture covering both scoring axes and asserts the
  protocol-wrapper strings are junk-filtered before scoring.

## [10.6.0] - 2026-07-10

### Fixed
- **Agent-injection matching is anchored to the start of the text.** Every
  `AGENT_INJECTION_GROUPS` alternative now requires its banner marker at the head of
  the message (leading whitespace tolerated), so a prompt that only mentions a relay
  tag mid-text — for instance one asking about a `<teammate-message>` tag it saw —
  reads as authored, not injected. Such a prompt keeps opening turns and no longer
  distorts the `is_waiting` oracle. Because the `agent_injection` group feeds
  `STRUCTURAL_NOISE_GROUPS`, this also stops `drop_junk("agent_injection")` from
  dropping mention-only prompts — a deliberate, approved tightening.
- **Portable follower classes restore exact Python/Rust parity.** The trailing `\b`
  in each alternative is replaced by an explicit follower class (whitespace,
  delimiter, or end-of-text), and Reminder's `i` is pinned to ASCII via `(?-i:[Ii])`.
  Python `re` and the Rust `regex` crate diverged on `\b` before a combining mark
  (`<teammate-message` + U+0301 matched only in Python) and on Unicode dotted/dotless-I
  case folding; both engines now agree, holding the backend- and activity-parity
  suites in lockstep.

## [10.5.0] - 2026-07-10

### Added
- **`UserEvent.is_agent_injected`.** A parse-time boolean flagging agent-injected
  relay banners — teammate-message digests, scheduled-task banners, and
  foreign-agent headers — computed from the `AGENT_INJECTION_GROUPS` regexes via
  the new `is_agent_injection` helper (search semantics, matching the
  `agent_injection` junk group). Both backends set the field in lockstep: the
  Rust parser mirrors the group alternation in `protocol.rs`, and the parse-time
  value plus the oracle behavior are pinned by the backend- and activity-parity
  suites.

### Fixed
- **Agent-injected relay banners no longer open turns.** A banner folded into a
  turn stops resetting turn segmentation in `SessionActivity` and stops opening a
  turn in the `is_waiting` oracle — the Python `probe_events` twin and the Rust
  `opens_turn` alike, so exact three-way parity holds. This is a deliberate
  oracle-semantics change: an injected banner arriving after a pending tool no
  longer closes the pending-tool waiting window, so a session mid-background-task
  stays `is_waiting` through a relay banner instead of being falsely retired.

## [10.4.0] - 2026-07-10

### Fixed
- **The activity oracle matches tool names the way the platform layer does.**
  Ephemeral-wait classification lowers aliases (an `Execute` background command
  counts as backgrounded Bash), and the waiting-tool and human-facing checks
  strip `mcp__<server>__` prefixes via `matches_names` — an MCP-wrapped
  `SendMessage` matches a configured waiting tool, and an MCP-wrapped
  `AskUserQuestion` classifies as human-facing instead of mid-tool. Both
  backends move in lockstep, parity-pinned, restoring the typed-lowering
  reference semantics captain-hook had before it delegated to the probe.

## [10.3.0] - 2026-07-10

### Fixed
- **cc-context MCP edits no longer bypass edit-tool matching.** `mcp__cc-context__ccx_code_edit`
  and `ccx_code_replace` split to bare names that matched no `Edit`/`Write` gate, so a
  `Tool("Edit"|"Write"|"MultiEdit")` condition never saw edits routed through the cc-context
  MCP. `matches_names` now consults the new `MCP_TOOL_ALIASES` table (`ccx_code_edit` → `Edit`,
  `ccx_code_replace` → `Write`) after the `mcp__` split, `expand_tool_names` reverse-closes the
  bare names into pre-expanded specs — the spec JSON crossing to Rust inherits the fix — and
  plan-reentry mining (`last_edit_index`) uses the suffix-splitting matcher on both sides of the
  parity line.

## [10.2.0] - 2026-07-09

### Fixed
- **The Rust backend no longer drops a whole file over one non-object line.**
  A line that is valid JSON but not an object (a bare scalar or array) is now
  skipped, exactly as the Python backend skips it; previously it errored out of
  the typed parse and `stream_parse` discarded the entire transcript. Valid
  objects that fail the typed parse still fail the file.
- **Compact-summary lines no longer open turns anywhere.** The exclusion moved
  from the activity probe's local check into the shared `native_user_classifier`,
  so `Session.current_turn` and every consumer built on it (context, facts,
  evidence, mining, render) now survives auto-compaction; the probe's observable
  behavior is unchanged.

### Changed
- Mining's event parsing goes through the shared parse layer (`parse_bytes`)
  instead of a bespoke line splitter, confining raw-byte JSON parsing to the
  parse layer.

## [10.1.0] - 2026-07-09

### Changed
- **Delivery-aware oracle completion.** Completion is delivery-aware via the
  `Notifications` queue replay: a pending async Agent/Task/Workflow counts as
  completed only once its notification was delivered (user turn or
  `queued_command` attachment) or drained from the queue — merely enqueued does
  not clear the wait — and any queued-but-undelivered task notification keeps
  `is_waiting` true on its own, resumed-session orphans included. Compact-summary
  user lines do not open turns, so auto-compaction cannot retire a running
  background task; this deliberately leads captain-hook, which still has the
  compaction bug.

## [10.0.0] - 2026-07-09

### Changed (BREAKING)
- **Alias-aware tool-name matching in mining specs.** Mining specs now match
  tool names through `matches_names` over `expand_tool_names`: a spec naming
  `Task` also matches `Agent` and MCP-suffixed spellings, in both the Python
  and Rust backends. Specs that relied on exact-string matching to target a
  single alias now match its siblings, which can change mining results.

### Added
- **Platform contract layer** (`cc_transcript.ids`, `cc_transcript.tools`):
  `SessionId`/`EventRef` identity, RFC 8785 canonical JSON and `tool_digest`
  over the raw input substrate, and the typed `ToolCall` hierarchy —
  first-class `MultiEdit` hunk lowering, alias + MCP name matching, and a
  hook-runtime `on_error` degrade that keeps digests correct. `TaskCall`
  carries `agent_type`, `model`, `run_in_background`, and `agent_name` — the
  `name` a teammate spawn passes to the Agent tool, which is what lets a hook
  tell a teammate start from a plain subagent start.
- **CCTranscript Swift package.** A SwiftPM package over a swift-bridge crate
  exposing the session-activity oracle: root `Package.swift` for git
  consumers, value-type wrappers with no dangling bridged refs, panic-catching
  at the bridge boundary, Sendable summaries, and a macOS CI job covering the
  bridge crate and package freshness.
- **Session-activity oracle.** A Rust session-activity oracle over the typed
  entry model, exposed as a dual-backend (Rust/Python) probe pinned by parity
  tests.

### Fixed
- `key_of`/`required_key` treat explicit JSON `null` as absent instead of
  returning it.
- Wild-data mode degrades non-mapping tool input instead of crashing the lift.
- Typed calls validate required field types at the wild-data boundary, so
  malformed wild data degrades instead of mistyping fields.

## [9.1.0] - 2026-07-07

### Added
- **`Session.notifications`.** A replay of Claude Code's task-notification
  delivery queue from the transcript's `queue-operation` audit records. The
  `Notifications` view separates *enqueued* from *delivered* — a finished
  task's notification can sit queued while the agent is mid-turn:
  `completed(tool_use_id)` is true once the notification was delivered (as a
  user event or a `queued_command` attachment) or otherwise left the queue,
  while `pending(tool_use_id)` and `has_pending` report what is still
  undelivered. FIFO replay covers `enqueue`/`dequeue`/`remove`/`popAll`
  (subtract-by-content, never clear-all); the semantics are pinned by an
  empirical test spec distilled from real transcripts.

## [9.0.1] - 2026-07-06

### Fixed
- A non-JSON judge response now converts to `JudgeError` like every other
  response-validation failure, so the row counts failed and retries on the next
  pass instead of the raw `JSONDecodeError` crashing the whole verdict run.
  CI now installs the `llm` extra so `structured_judge` has live coverage.

## [9.0.0]

### Changed (BREAKING)
- **Model leaves verdict identity.** The verdict table's unique key drops from
  `(dedup_key, role, prompt_version, model)` to `(dedup_key, role, prompt_version)`:
  one verdict per event per role per prompt version. `model` stays `NOT NULL` as
  pure provenance — recorded and reported, never filtered on — so a judge-backend
  flip no longer mass-re-judges the corpus (81 events double-judged in production)
  and `judged()`'s consumers stop double-counting per-model duplicates.
  `VerdictStoreMixin.unjudged` loses its `model` parameter.

### Added
- **`canonical_key`.** A nullable `canonical_key` column on the verdict table and a
  read-only `VerdictLike.canonical_key` (`str | None`) — the normalized durable-rule
  key a judge may assign, `None` when the verdict names no rule. `record_verdict`
  persists it and carries it across a summary-to-full upgrade.
- **Slug primitives.** `SLUG_PATTERN` (two to six hyphenated `[a-z0-9]` groups, by
  construction never a 64-character hex digest) and `canonical_slug`, both exported
  from `cc_transcript.judge`.
- **Canonical-key suggester.** `cc_transcript.judge.similar` — a sqlite-vec retrieval
  layer behind the new `[judge]` extra (`sqlite-vec`, `model2vec`, `numpy`).
  `record_verdict` embeds each canonical-key verdict's feedback with
  `potion-retrieval-32M` and upserts the vector into the store's own database, created
  on first use and never part of the base schema. `suggest_canonical_keys` ranks stored
  keys by evidence similarity to a new correction; `near_duplicate_keys` flags distinct
  keys whose evidence centroids nearly coincide, a split-detection signal that never
  auto-merges. The vector deps load lazily, so importing `cc_transcript.judge` needs
  none of them installed, and assigning a canonical key without the extra fails loud
  with an `ImportError` naming it.

### Fixed
- **The summary treadmill.** `unjudged(refresh_summary=True)` now orders truly-unjudged
  events ahead of summary-refresh rows, so a backlog of summary rows no longer starves
  fresh events under `limit`, and drops summary rows whose context window no longer
  hydrates — its transcript expired or a ref was compacted away — so a dead transcript
  stops re-judging every session.
- **Cross-model fidelity upgrade.** A summary-to-full re-record updates `model` and
  `canonical_key` alongside the content, so an upgrade judged by a different backend
  no longer keeps the summary pass's provenance on the full pass's content.
- **Option picks carrying notes.** An `ask_user_question` option pick with notes
  ("Option A, and never do X again") scores on its notes like a freeform answer
  instead of the flat `option_pick` weak floor, so the richest durable-rule shape
  survives the confidence gate.

## [8.1.0]

### Added
- **`ask_user_question` mining detector.** One signal per answered `AskUserQuestion`
  Q/A pair, scoring option picks apart from freeform answers and splitting the
  answer's notes and command preview into evidence.

### Changed
- The `[llm]` extra's `spawnllm` floor rises to `>=0.5.4`.

## [8.0.0]

### Changed (BREAKING)
- **One command-parsing layer.** capt-hook's tree-sitter-bash `Command`/`CommandLine`/
  `CommandLineQuery`/`Redirect` move into `cc_transcript/command.py` and replace every
  shlex/regex heuristic. `bash_prefixes` is now `command_prefixes` (grammar-exact:
  `timeout 30 git push` counts as `git push`, quoted operators and loop bodies parse
  correctly); `Command` gains `unwrapped`, `prefix`, and `runs(*argv)`;
  `CommandLine` gains `prefixes`. tree-sitter and tree-sitter-bash are now required
  dependencies. The Rust extension exposes a bulk `command_prefixes` fast path
  (4–5× the Python reference), parity-tested in `tests/test_command_parity.py`.
- **tools.py rewritten.** Per-class `from_raw` constructors and a `TOOL_TYPES`
  registry replace the monolithic dispatcher; dual camelCase/snake_case keys resolve
  presence-first (explicit `null` falls through); non-mapping input raises in strict
  mode and degrades to an empty-input `OtherCall` under `on_error="other"`;
  a `TaskUpdate` without a task id now raises. `BashCall.command_line` returns the
  parsed `CommandLine`. `typed_tool_call`, `segment_prefix`, `split_on_operators`,
  and the shell token tables are gone.
- **toolcalls.py is now facts.py.** `ToolFact` carries `tool_use_id` and
  `command_prefixes`; `bash_prefix_counts` is `command_prefix_counts`; the CLI's
  parallel `Outcome` projection is deleted in favor of the one `ToolFact` join.
- **`Session.has_command(*argv)`** matches parsed commands via `Command.runs`
  (wrapper-transparent, quote-exact) instead of regexing raw text;
  `Session.command_lines()` exposes the parsed lines and `commands()` stays raw.
- **One interrupt-marker semantics.** Head-anchored, case-insensitive
  (`^\s*\[Request interrupted by user`) everywhere: the parser's `interrupted` flag,
  filter groups, `JUNK_USER_MESSAGE_RE`, and mining agree, in both languages.
  Mid-text quotes of the marker no longer count.
- **Alias-aware mining.** `MiningSpec` gains `plan_tools`/`denial_excluded_tools`,
  and `edit_tools`/`subagent_tools` are alias-closed; `ExitSpecMode` denials and plan
  rejections now classify correctly, including `mcp__server__Tool` forms.
  New `tools.matches_names`; expanded sets travel in the spec JSON.
- **Sentiment rides the event spine.** `messages.py` is deleted;
  `ConversationBucket.events` holds `UserEvent | AssistantEvent`
  (`ConversationEvent`), built by `bucket_events`. New `models.tool_uses` and
  `models.thinking_chars` helpers replace app-side adapters.
- **Removed:** `STOP_HOOK_RE`, the `build_event` alias (use `parse_event`),
  `activity.meta_of` (use `filterspec.event_meta`), `Command.empty()`
  (`CommandLine.primary`/`head` are `Command | None`).

### Fixed
- One failed `git show` no longer discards corrections already collected from other
  commits; the sentiment lexicon fails fast instead of scoring with a silently
  degraded lexicon; the judge fan-out catches only `JudgeError`, so programming
  errors propagate; `Session.subagents` skips macOS `._*` resource forks.

## [7.1.0]

### Added
- `WorkflowCall`, a typed tool call for the Workflow (dynamic-orchestration) tool:
  `script`, `script_path`, `workflow_name`, `args`, and `resume_from_run_id`.
  Workflow dispatches previously degraded to `OtherCall`, leaving hook authors and
  miners raw-dict access only.

## [7.0.0]

### Changed (BREAKING)
- The structured-LLM path migrates to spawnllm 0.5's `run`/`call`/`extract` API and
  now requires `spawnllm>=0.5.0`. `cc_transcript.judge.run_structured` and
  `run_structured_on` are removed; the correction picker (`extract/correct.py`) and
  `structured_judge` call `spawnllm.extract(...)`, which returns the validated model
  directly. A failing CLI backend now raises `spawnllm.BackendCallError` carrying the
  backend's real stderr/exit code, instead of a misleading `Invalid JSON: EOF` parse
  error on empty output. `default_backend` and `resolved_model` are unchanged.

## [6.0.0]

### Changed (BREAKING)
- The mining layer is rebuilt on the declarative dual-backend standard (Rust fast
  path + Python reference), like `filterspec`/`scorespec`/`lexicon`. All mining
  policy is now a serializable `MiningSpec` (`cc_transcript/mining/spec.py`):
  confidence stages (`Base`/`BumpIfSubstantive`/`DemoteIfHedged`/`DemoteIfShort`/
  `BumpIfProximate`/`NoiseIfStructural`) folded by `run_confidence`,
  `ProvenanceSpec`, and a `ReviewSpec` over `RegexReviewFormat | CallableReviewFormat
  | StructuredFormat`. `mining_spec_to_json` / `mining_spec_is_portable` mirror the
  other stages; portability rejects lookaround/backref regexes and callable formats.
- The six `iter_*_signals` detectors collapse into `mine(events, spec)` (the
  Python-events reference) and `mine_signals(raw, spec)` (the dual-backend entry that
  takes raw transcript bytes and yields `MiningSignal`s — portable specs take the
  Rust parse+detect fast path via `_parser_rs.mine_signals`, others fall back to the
  Python reference). The old `ReviewFormat` dataclass, `extract_all`,
  `DEFAULT_DETECTORS`, the bare `iter_*_signals` exports, and the unparameterized
  `classify_provenance` are removed. `MiningSignal`/`CandidateSignal`/`ReviewComment`/
  `StructuredFormat`/`extract_structured` are unchanged.

### Added
- `rust/src/mining.rs`: a Rust executor at full parity with the Python reference
  across all six detectors and both review-format families, proven byte-identical
  over the corpus (`tests/test_mining_parity.py`). `CC_TRANSCRIPT_DISABLE_RUST=1`
  forces the Python reference.

### Internal
- `cc_transcript/mining/formats.py` parses structured payloads via `orjson`; the
  CC-protocol marker constants moved into core `filterspec.py`.

## [5.0.0]

### Added
- Review-comment mining can scan beyond human-typed user text.
  `iter_review_comment_signals` takes a `surfaces` set of `Provenance`
  (`typed` / `surfaced` / `claude`) selecting which event surfaces to scan —
  user text, tool-result content (workflow/Bash output), etc. — and stamps each
  signal's `evidence["provenance"]` accordingly. `classify_provenance` is exported.
- `StructuredFormat` + `extract_structured`: JSON finding-array extraction for
  structured review payloads, with a caller-supplied field-map (`file_keys`,
  `line_keys`, `comment_keys`, `fix_keys`, `finding_keys`); tolerant of int / `"96"`
  / `"24-51"` line forms and nested `result` / `confirmed*` arrays.

### Changed (BREAKING)
- `iter_review_comment_signals`: `surfaces` and `structured_formats` are now
  REQUIRED keyword-only arguments — the defaults and the `TYPED_SURFACE` constant
  are removed. Pass `surfaces=frozenset({"typed"})` for the previous behavior.
- `run_verdicts`: `prompt_for` must now be an async callable returning `str`; the
  synchronous `str` path is removed.
- `OtherCall.get(key, default)` is removed — use `OtherCall.raw.get(...)`.

## [4.2.0]

### Added
- On-disk assistant usage/cost retention: `AssistantEvent.usage` plus shared
  `Usage`, `CacheCreation`, and `ServerToolUse` models, parsed by both the Python
  reference and the Rust fast path.
- `parse_print_result`: a `claude -p --output-format json` parser (Python +
  Rust) yielding a `PrintResult` with `ModelUsage`, `InitInfo`/`McpServer`/
  `Plugin`, and `PrintMessage`.
- `cc_transcript.cost`: a token-to-dollar cost helper — `cost_of`,
  `cost_of_assistant`, `ModelPricing`, `CostBreakdown`, `resolve_pricing`, and
  the `PRICING` table.

## [4.1.0]

### Changed
- The decision ledger table is renamed `decisions_v1` → `decisions` (its indexes
  likewise); cc-review's vendored DDL is renamed in lockstep. No in-place
  migration — delete `~/.cc-transcript/decisions.db*` and let it rebuild.
- `extract_correction` returns None cleanly when an anchor resolves no turn,
  instead of fabricating a zero anchor-turn for the (unreachable) no-turn branch.

## [4.0.0]

The corrections ledger becomes the cc-family's single code-correction substrate:
one table, one access surface, LLM-grounded rows by default.

### Added
- `cc-transcript corrections` CLI group — `add` (structured insert, the write
  path for non-Python producers like cc-review's Go), `query`, and a raw-SQL
  `sql` escape hatch. The ledger location stays internal; callers never name it.
- `cc_transcript.extract` domain: `extract_correction` harvests the candidate
  edits around a feedback anchor and appends the one the feedback faults — an LLM
  pick via `spawnllm.select_backend` by default, the best-overlap candidate when
  no backend is ready, idempotent per anchor.
- `CorrectionLog.for_repo` and `.since` readers, and `judge.run_structured_on`
  (one structured completion on an explicit backend).
- The `correction_text` column and the `review` correction origin, so a human
  reviewer's natural-language correction is a first-class ledger row.

### Changed
- The structured-LLM path (`run_structured`, `structured_judge`, `resolved_model`)
  runs whichever backend `spawnllm.select_backend` resolves instead of pinning
  Claude; `resolved_model` reflects the active backend. Requires `spawnllm>=0.2.0`.
- `corrections` is keyed by `(session_id, anchor_uuid, incorrect_digest)`, and
  `incorrect_digest` is now nullable — human review rows join by anchor.

### Removed
- `EXTRACTOR_VERSION` and the `extractor_version` column/parameters; the ledger no
  longer versions its deterministic extraction.

### Migration
- The `corrections_v1` table is renamed to `corrections`. There is no in-place
  migration: delete `~/.cc-transcript/corrections.db*` and let it rebuild.

## [3.1.0]

### Added
- `grep` and `stats` now report when the default `--limit` hid transcripts
  (`searched N of M transcripts — use --all`), so an empty result is explained
  rather than mistaken for a broken filter.

### Fixed
- `grep --tool` matches through `tool_name_matches`, honoring pipe specs
  (`Read|Edit`), renamed aliases (Bash↔Execute), and MCP-suffixed names —
  consistent with the query API.

## [3.0.1]

### Changed
- Documentation and internal cleanups.

## [3.0.0]

The cruft purge: a leaner, more uniform public surface. Breaking throughout —
consumers move in lockstep.

### Removed
- `FilterConfig`, `apply_filters`, and the `cc_transcript.filters` module —
  compose a `FilterSpec` with `build_spec` and apply it with `apply_spec`.
  `JUNK_USER_MESSAGE_RE` now lives in `cc_transcript.filterspec`.
- The `[lexicon]` extra alias — install `cc-transcript[sentiment]`.
- `effective_confidence` and the optional `None` signal in **mining**:
  `FeedbackCandidate.signal`, `FeedbackCandidate.ref`, and `MiningSignal.signal`
  are required; `from_payload` requires a payload.
- `ContextWindow.origin` and the `Origin` type; `anchor` is required.
- The vendored parity tests for the junk regex and spec presets.

## [2.1.0]

The shared `corrections_v1` ledger: a durable, cross-language home for the
incorrect→correct edit pairs the evidence harvest derives, mirroring
`decisions_v1` so every consumer reads and writes one source. Never published
to PyPI on its own — it ships as part of 3.0.0.

### Added
- `cc_transcript.corrections` — an import-light `CorrectionLog` over the
  `corrections_v1` table (WAL, `INSERT OR IGNORE`, `~/.cc-transcript/corrections.db`),
  keyed by `(session_id, anchor_uuid, incorrect_digest, extractor_version)`.
  Reads via `for_session` / `for_anchor` / `by_digest`; the cross-consumer join
  is by `incorrect_digest`, the same `tool_digest` that `decisions_v1` carries.
- `evidence.record_harvest(log, activity, anchor, pairs)` — lowers
  `CandidatePair`/`GitFix` into `Correction` rows in the heavy evidence tier,
  resolving each incorrect edit's raw tool call to its cross-language digest.
- The `corrections_v1` DDL is frozen as a byte-compared cross-language contract
  (`tests/testdata/corrections_v1.sql`), exactly like `decisions_v1`.

## [2.0.0]

The greenfield platform release: cc-transcript becomes the cc-family's
session-activity platform. Breaking throughout — consumers move in lockstep,
no compatibility shims.

### Added
- `cc_transcript.ids` — identity and digests: `SessionId`/`EventUuid`/
  `ToolUseId`/`ToolDigest`, the universal `EventRef` handle, `canonical_json`
  (RFC 8785 JCS: UTF-16 key order, ECMAScript number layout), and
  `tool_digest` — the single cross-language content join key, computed over
  the raw input mapping only. Standard library only.
- `cc_transcript.tools` — the single typed `ToolCall` hierarchy shared by
  hook runtimes and the parser, with first-class `MultiEditCall` (fixing the
  silent first-span-only degradation), `hunks_of`/`file_path_of` lowering,
  alias + `mcp__` name matching, and `parse_tool_call(on_error=)` — strict by
  default, with a hook-runtime degrade to `OtherCall` whose digest stays
  correct via the raw substrate.
- `cc_transcript.activity` — the spine: `SessionActivity` of `Turn`s,
  `ToolUse`s, and first-class `Edit`s, with `edits_before`/`edits_after`
  anchored queries, an injectable `UserClassifier` turn-segmentation seam,
  and `hunk_overlap`.
- `cc_transcript.evidence` — incorrect-edit/correction harvest:
  `harvest_pairs`, `match_corrections`, read-only `git_corrections` pickaxe
  fallback, `GitFix`, `CandidatePair`, `EXTRACTOR_VERSION`.
- `cc_transcript.context` — refs-not-prose context: `ContextWindow` persists
  `EventRef`s plus labeled render-time previews (`cc-transcript.context/1`),
  hydrates asynchronously to a full-fidelity `HydratedWindow` or degrades to
  an explicitly labeled summary; never hydrate-or-fail.
- `cc_transcript.render` — the one renderer: `Budget`-bounded
  `render_tool_call`/`render_turn`/`render_session`; truncation happens only
  here, marked with omitted-char counts; MultiEdit renders every span.
- `cc_transcript.query` — the transcript query surface (`Session`,
  `ToolCallQuery`, `FileRef`, subagent recursion) built from the measured
  consumer call sites formerly served by captain-hook's `Transcript` API.
- `cc_transcript.decisions` — the unified decision ledger (`decisions_v1` at
  `~/.cc-transcript/decisions.db`): dual-writer (Python hooks, cc-review's Go
  daemon) via a vendored byte-compared DDL; `attribute_tool` joins decisions
  to tool calls by digest + nearest-preceding timestamp, never by message
  substring.
- `cc_transcript.disktruth` — typed reader for cc-review's
  `cc-review.activity/1` export (turns, version-dimensioned attributions).
- CLI: `slice` (per-tool-call JSONL for a session UUID + time window — the
  language-neutral bridge consumed by cc-review) and `digest` (cross-language
  fixture generator/checker for the digest contract).
- `cc_transcript.judge` — verdict persistence is fidelity-aware: verdicts
  record the context fidelity they were judged at, summary-fidelity verdicts
  are re-judgeable via `unjudged(refresh_summary=True)`, and a full-fidelity
  re-judge replaces a summary verdict.
- PEP 562 lazy package root with an import-weight CI guard: importing
  `cc_transcript.ids`/`cc_transcript.tools` pulls zero heavy dependencies.

### Changed
- `domains.mining` → `cc_transcript.mining` and `cc_transcript.judge`;
  `domains.sentiment` → `cc_transcript.sentiment` (replacing the pre-0.6
  shim). `FeedbackCandidate` now carries a `ContextWindow` and an `EventRef`.
- `EntryUuid` is now `EventUuid`, defined in `cc_transcript.ids` alongside
  `SessionId`/`ToolUseId`.
- `messages.ToolCall` renamed `MessageToolCall`; the root `ToolCall` export
  is the typed tool-call union.
- By-UUID transcript discovery (`find_transcript`/`find_transcript_sync`):
  symlink spellings collapse to one real path, newest mtime wins; paths are
  resolution hints, never keys.

### Removed
- `domains.mining.context` (`ContextSnapshot`, `summarize_tool_input`,
  `TOOL_INPUT_LIMIT` bake-time truncation), `nav`, `markers` — replaced by
  `context`/`activity`/`mining.signals`.
- The `domains` package and the legacy `cc_transcript.sentiment` shim.

## [0.9.0]

### Added
- The `cc-transcript` CLI (`uvx cc-transcript`): `list`, `show`, `grep`, and
  `stats` for token-efficient transcript investigation, runnable as
  `python -m cc_transcript` or via the `cc-transcript` entry point.
- Bundled Claude Code plugin: `.claude-plugin/` manifests and the
  `cc-transcript-investigate` skill, installed via
  `/plugin marketplace add yasyf/cc-transcript`.
- Docs: a mining-feedback guide, a transcript-CLI guide, a curated API
  reference covering `domains.mining` and `domains.sentiment`, and a generated
  Click CLI reference.

### Changed
- Project metadata (description, keywords, classifiers) now covers the CLI and
  the domain tiers.
- README quickstart fixed (`parse_events` was removed in 0.5.0).

## [0.8.0]

### Added
- **domains.mining**: per-instance confidence calibration — detectors now use the
  full band with named reasons (`trigger_proximate`, `short_followup`,
  `structural_only`, `substantive`, `hedged`, …); structural-only corrections
  score below `NOISE_FLOOR`.
- **domains.mining**: `CandidateFilterSpec` — candidate-level filtering
  (`ConfidenceAtLeast`/`SourceKindIn`/`HasReason`/`IsDurable` predicates,
  `CandidateClause`, `keep_candidate`/`apply_candidate_filter`, and the
  `at_least`/`only_kinds`/`build_candidate_filter` builders), mirroring the
  event-level `filterspec`. Ships mechanism only; consumers own thresholds.
- **domains.mining**: the verdicts mechanism, lifted from cc-pushback —
  `VerdictLike`, `VerdictStoreMixin` (parameterized physical names; generated DDL
  is byte-compatible with cc-pushback's existing `triage` table), the generic
  `run_verdicts` runner, deterministic stratified `sample_audit`, and the eval
  math (`Metrics`, `AuditEstimate`, `exact_upper_bound`, `GoldenRow`,
  `golden_result`, `flip_pairs`).
- **domains.mining**: `clip`/`render_turn`/`render_turns` — pure presentation of
  `ContextTurn`s (including `tool_inputs`) for judge prompts
  (`TURN_TEXT_LIMIT = 700`).
- **`[llm]` extra**: `domains.mining.llm` — `run_structured`, `resolved_model`,
  and the `structured_judge` factory over spawnllm's Claude CLI backend, imported
  lazily so the domain keeps importing without the extra.

### Changed
- **domains.mining**: detector signals persist calibrated confidences in
  `payload_json`; rows written before this release still decode (`None` reads as
  `MEDIUM`).

## [0.7.1]

### Added
- **domains.mining**: `ContextTurn.tool_inputs` — one bounded input summary per
  tool call (the Bash command, the Edit diff, the ExitPlanMode plan body, …),
  produced by the new `summarize_tool_input` helper (`TOOL_INPUT_LIMIT = 1500`)
  and carried on every assistant turn in a `ContextSnapshot`. Serialized
  `context_json` gains a `tool_inputs` key; snapshots persisted before this
  release read back with empty summaries.

## [0.7.0]

Internal re-architecture: the package now has an explicit **core** layer and a
**domains** tier (`cc_transcript.domains.{sentiment,mining}`) built on top of it.
Additive for core consumers; the sentiment import paths move behind deprecation
shims. An import-fence test enforces the layering: core never imports domains,
and the two domains never import each other.

### Added
- **core**: `cc_transcript.messages` — the distilled message projection
  (`UserMessage`/`AssistantMessage`/`ToolCall`/`TranscriptMessage`), promoted out
  of the sentiment tier and re-exported at the package root.
- **core**: `ToolResultBlock.is_async`, populated from the entry-level
  `toolUseResult.isAsync` in both the Python and Rust parsers, and a public
  `parse_event(data)` single-entry parser (re-exported at the package root) for
  consumers that parse a transcript line at a time while retaining the raw entry.
- **domains.mining**: new built-in domain. Neutral fact-detectors
  (`MiningSignal` + `iter_*_signals`) over Claude Code transcripts, the generic
  `FeedbackCandidate`/`dedup_key`, the context-window builder, nav/marker
  primitives, review-format infra (`ReviewComment`/`ReviewFormat`/`extract_all`),
  a `FeedbackStore` base over `FileStateStore`, and the `Confidence`/
  `CandidateSignal` de-noising layer. Apps map signals to their own candidate
  records with policy injected.

### Changed
- **domains.sentiment**: the sentiment scoring tier moved from
  `cc_transcript.sentiment` to `cc_transcript.domains.sentiment`. The `[lexicon]`
  extra is renamed `[sentiment]`, with `[lexicon]` kept as a back-compat alias.

### Deprecated
- The `cc_transcript.sentiment.*` import paths are now thin re-export shims for
  `cc_transcript.domains.sentiment` and the promoted core message types, to be
  removed in a future release. Import from `cc_transcript.domains.sentiment` and
  `cc_transcript` instead.

## [0.5.0]

Breaking. All I/O is now async-native (`anyio` + `aiosqlite`); the synchronous
`parse_events(path)` is removed.

### Changed
- `FileStateStore` is backed by `aiosqlite`: `open`, `close`, `transaction`,
  `record_file`, `upsert_file`, and `file_mtimes` are now coroutines, the store
  is an async context manager (`async with`), and `transaction()` is an
  `@asynccontextmanager`. The reentrancy guard now keys on the owning task.
- `TranscriptDiscovery.find_in`, `find_transcripts`, `stat_mtime`, and
  `transcript_mtime` are now coroutines, using native async `anyio.Path`
  filesystem traversal (no thread offload).
- Both parsing backends now uniformly **skip** transcripts that fail to parse:
  `PythonBackend.parse_batch` swallows per-file `OSError`/`ValueError`/`KeyError`
  to match `RustBackend`, so `stream_transcripts` no longer aborts a batch on one
  bad file.

### Added
- `parse_events_async(path)` — async single-file parse via `anyio.Path`.

### Removed
- `parse_events(path)`. Migrate to `parse_events_async(path)` for a single file,
  `TranscriptParser.stream_transcripts(...)` for a batch, or
  `parse_events_from_bytes(path.read_bytes())` when you already hold the bytes.

## [0.4.0]

Breaking. Consumer-named presets are removed: the filter **and** score pipelines
are now composed by each consumer from shared primitives, and both run in Rust
with a Python fallback at verified parity.

### Added
- Declarative, both-backend filtering via `FilterSpec`: an ordered list of
  typed predicate `Clause`s (`KindIs`, `MetaFlag`, `EntrypointIn`, `ModelIs`,
  `TextEmpty`, `TextMatchesAny` with named regex groups, `TextInSet`,
  `WordCountAtMost`) with `DROP`/`TAG` actions. `apply_spec`, `annotate_spec`,
  `keep`, and `labels_for` interpret it in Python.
- Composable event-filter builders (`build_spec`, `keep_only`, `drop_synthetic`,
  `drop_empty`, `drop_sidechain`, `drop_meta_flag`, `drop_compacted`,
  `drop_entrypoints`, `drop_junk`, `drop_phrases`, `drop_short`), the
  `JUNK_CATEGORIES` registry, and a generic `NOISE_SPEC`.
- Rust-side execution of portable specs: events are dropped during parsing,
  before any Python object is materialized, with automatic Python fallback for
  non-portable specs (`is_portable`, `spec_to_json`).
- Named, composable junk groups (`STRUCTURAL_GROUPS`, `AGENT_INJECTION_GROUPS`,
  `INTERRUPT_MARKER_GROUPS`, `STOP_HOOK_GROUPS`) and shared closed sets
  (`RESUME_PHRASE_SET`, `TRIVIAL_ACK_SET`, `FRUSTRATION_GROUPS`,
  `MILD_IMPATIENCE_GROUPS`).
- Declarative, Rust-executable `ScoreSpec`: composable score stages
  (`flag_frustration`, `clamp_positive`, `demote_mild_irritation`, `clamp_resume`,
  `build_score_spec`), run by a Rust executor with a Python interpreter at parity.
- `cc_transcript.sentiment` subpackage: the conversation-bucketing + sentiment
  tier, hosted here for cc-sentiment. Includes the message/bucket types
  (`UserMessage`/`AssistantMessage`/`ConversationBucket`/`SentimentScore`),
  `ConversationBucketer`, `FilteredEngine`, and the `InferenceEngine` protocol;
  it ships the `ScoreSpec` stages rather than a `ScoreFilter` ABC or `*Filter`
  classes.
- Rust sentiment lexicon: lemmatizes via `udpipe-rs` (English UD model, downloaded
  and cached at runtime) plus AFINN + domain-override scoring; used by default when
  available, with the unchanged spaCy + afinn path as the at-parity fallback
  (`Lexicon.has_hit`).

### Changed
- `FilteredEngine(inner, spec: ScoreSpec)` replaces the filter-tuple constructor.
- Renamed `SENTIMENT_STRUCTURAL_GROUPS` → `STRUCTURAL_GROUPS` and
  `PUSHBACK_STRUCTURAL_EXTRA_GROUPS` → `AGENT_INJECTION_GROUPS`.
- Trimmed the top-level `cc_transcript` export surface; predicate classes and raw
  group tuples remain importable from `cc_transcript.filterspec`.
- `FilterConfig` / `apply_filters` are now a thin shim that lowers to a
  `FilterSpec` (`FilterConfig.to_spec`); behavior is unchanged, proven
  byte-for-byte against the prior predicate over the real corpus.

### Removed
- Consumer presets `SENTIMENT_SPEC`, `PUSHBACK_SPEC`, `SENTIMENT_FILTER`,
  `DEFAULT_FILTERS`, and the `ScoreFilter` ABC + the `*Filter` classes. Consumers
  compose their own specs from the builders.

## [0.1.0]

### Added
- Typed superset event model for Claude Code transcripts: `UserEvent`,
  `AssistantEvent`, `SystemEvent`, `ModeEvent`, and `OtherEvent` with full
  content blocks (text, thinking, tool_use, tool_result) and envelope
  metadata — no semantic filtering in the parser.
- `TranscriptDiscovery` with mtime-cursored incremental discovery.
- Pure-Python reference parser and a Rust fast-path (`_parser_rs`, PyO3 +
  rayon) with event-for-event parity, prefetch-bounded memory, and a
  `CC_TRANSCRIPT_DISABLE_RUST` escape hatch.
- Opt-in consumer-side filtering (`FilterConfig`), including
  `SENTIMENT_FILTER` reproducing cc-sentiment's exact drops.
- `FileStateStore`: a generic WAL/locked SQLite mtime ledger with atomic
  consumer transactions, for idempotent incremental scans.

[10.5.0]: https://github.com/yasyf/cc-transcript/compare/v10.4.0...v10.5.0
[10.4.0]: https://github.com/yasyf/cc-transcript/compare/v10.3.0...v10.4.0
[10.3.0]: https://github.com/yasyf/cc-transcript/compare/v10.2.0...v10.3.0
[10.2.0]: https://github.com/yasyf/cc-transcript/compare/v10.1.0...v10.2.0
[10.1.0]: https://github.com/yasyf/cc-transcript/compare/v10.0.0...v10.1.0
[0.9.0]: https://github.com/yasyf/cc-transcript/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/yasyf/cc-transcript/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/yasyf/cc-transcript/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/yasyf/cc-transcript/compare/v0.5.0...v0.7.0
[0.5.0]: https://github.com/yasyf/cc-transcript/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/yasyf/cc-transcript/compare/v0.1.0...v0.4.0
[0.1.0]: https://github.com/yasyf/cc-transcript/releases/tag/v0.1.0
