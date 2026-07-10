# Changelog

All notable changes to this project are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[10.2.0]: https://github.com/yasyf/cc-transcript/compare/v10.1.0...v10.2.0
[10.1.0]: https://github.com/yasyf/cc-transcript/compare/v10.0.0...v10.1.0
[0.9.0]: https://github.com/yasyf/cc-transcript/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/yasyf/cc-transcript/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/yasyf/cc-transcript/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/yasyf/cc-transcript/compare/v0.5.0...v0.7.0
[0.5.0]: https://github.com/yasyf/cc-transcript/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/yasyf/cc-transcript/compare/v0.1.0...v0.4.0
[0.1.0]: https://github.com/yasyf/cc-transcript/releases/tag/v0.1.0
