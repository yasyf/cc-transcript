# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.9.0]: https://github.com/yasyf/cc-transcript/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/yasyf/cc-transcript/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/yasyf/cc-transcript/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/yasyf/cc-transcript/compare/v0.5.0...v0.7.0
[0.5.0]: https://github.com/yasyf/cc-transcript/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/yasyf/cc-transcript/compare/v0.1.0...v0.4.0
[0.1.0]: https://github.com/yasyf/cc-transcript/releases/tag/v0.1.0
