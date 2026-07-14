# cc-transcript Development Guide

Typed events for Claude Code transcripts: discovery, a superset JSONL parser — a Rust fast path and a Python reference behind one `Backend` protocol — session-activity queries, durable context windows, mining/judging/sentiment domain tiers, and a transcript-investigation CLI. Published to PyPI as `cc-transcript`; the CLI is `cc-transcript`, run as `uvx cc-transcript`.

## Repository Structure

```
cc-transcript/
├── cc_transcript/      # The package
│   ├── __init__.py     # The platform façade: the one-spine public API re-exports
│   ├── models.py       # Typed superset event model (the central contract)
│   ├── ids.py          # Identity primitives shared by every layer of the platform
│   ├── discovery.py    # Locating transcript files under ~/.claude/projects
│   ├── backend.py      # Backend protocol + ParsedTranscript
│   ├── parser.py       # PythonBackend reference parser + TranscriptParser facade
│   ├── rust.py         # RustBackend — the fast path over cc_transcript._native
│   ├── filterspec.py   # Declarative FilterSpec: typed predicate clauses + interpreters, plus the CC-protocol text layer (denial/interrupt markers)
│   ├── builders.py     # Composable spec builders (keep_only, drop_junk, NOISE_SPEC, …)
│   ├── tools.py        # The single typed tool-call hierarchy (stdlib-only by contract)
│   ├── command.py      # Parsed bash command lines: Command/CommandLine/CommandLineQuery, tree-sitter-backed
│   ├── facts.py        # Tool-call analytics substrate lifted from session activity
│   ├── activity.py     # Session activity lifted from parsed transcript events
│   ├── activity_probe.py # Dual-backend is_waiting oracle over one transcript (captain-hook's probe)
│   ├── notifications.py # Harness notification-delivery queue, replayed from a session's events
│   ├── query.py        # Session-level queries over lifted activity
│   ├── context.py      # Durable context windows: refs plus previews that re-hydrate
│   ├── evidence.py     # Evidence harvest around a feedback anchor
│   ├── ledger.py       # SyncLedger — the append-only SQLite base under both family ledgers
│   ├── decisions.py    # The unified decision ledger shared by hook and gate writers
│   ├── corrections.py  # The shared code-correction ledger every consumer reads and writes
│   ├── corrections_cli.py # The corrections CLI: the ledger for non-Python consumers
│   ├── disktruth.py    # What actually hit disk, per cc-review's turn ledger
│   ├── cost.py         # Token-usage → USD cost model (cost_of, PRICING)
│   ├── cli.py          # The cc-transcript CLI (list/show/grep/stats)
│   ├── __main__.py     # python -m cc_transcript → the CLI
│   ├── render.py       # The one renderer — every cut happens here, under a Budget
│   ├── store.py        # FileStateStore — SQLite ingestion-state tracking
│   ├── mining/         # Feedback-mining domain: detectors, confidence, feedback store
│   ├── judge/          # LLM verdict passes over mined feedback
│   ├── extract/        # LLM-grounded correction extractor: evidence + judge, one pick
│   └── sentiment/      # Sentiment domain: event buckets + composable score spec
├── rust/               # Rust extension (cc_transcript._native)
├── rust-swift/         # swift-bridge crate (cc_transcript_swift) over the Rust core
├── swift/              # Generated Swift package: bridge sources + the committed macos-arm64 xcframework
├── tests/              # Pytest suite
├── docs/               # Guide and getting-started sources for the docs site
├── great-docs/         # The Great Docs Quarto project: site config + curated API reference
├── scripts/            # Maintenance scripts: lexicon data, Rust literals, the Swift package build
├── .claude-plugin/     # Claude Code plugin + marketplace manifests
├── skills/             # cc-transcript-investigate skill (CLI-driven transcript investigation)
├── .github/            # CI, docs, and PyPI release workflows
├── Package.swift       # Root SPM manifest: CCTranscript over the committed xcframework
├── AGENTS.md           # This file — shared conventions
└── README.md           # Project overview
```

## Ask Before Assuming

When the user's request has ambiguity — unclear scope, multiple plausible interpretations, undefined edge cases, or unspecified tradeoffs — stop and ask. Propose 2-4 concrete options and let the user pick, or list the assumptions you'd otherwise make and ask which ones hold. There is no such thing as too many questions; one wrong implementation costs more than ten clarifying exchanges. Default to interrogating the user when in doubt — multiple short questions early beat a wrong direction later.

## Code Review Response (Plan Re-Entry)

When the user reviews code you wrote and re-enters plan mode — whether by leaving inline diff comments, pasting a numbered list of issues, or otherwise sending review-shaped feedback after a recent edit cycle — you MUST:

0. **Delegate context-gathering to a subagent.** Spawn one `Explore` subagent with every cite (file:line + the user's verbatim comment text). Instruct it to, per cite, `Grep` the file with ~5 lines of context either side of the cited line (`-B 5 -A 5`), and only escalate to a full `Read` when the ±5-line window is insufficient (e.g. the comment refers to a function defined further up). Have it also surface sibling call sites with the same issue (Grep across the module). Use the subagent's digest as your source of truth when drafting the plan. Do NOT bulk-`Read` the cited files yourself in the main turn — it bloats the main context window before you've even started writing the plan.
1. **Draft a new plan**, not a code change. Plan-mode re-entry is the user asking "let's align on what you'll do next," not "go fix it."
2. **Inline every comment verbatim** in the plan. Each comment gets a short anchor (`#N`, the file:line if provided, or a quoted excerpt) plus the user's exact wording in a blockquote or `*"…"*` italics. Do not paraphrase. The user must be able to scan the plan and see every comment they wrote reproduced exactly.
3. **Cluster when many.** If there are more than ~5 comments, group them into themes (e.g. "T1 — Guards against impossible states") and list every verbatim trigger per theme. Address every cited line *and* extrapolate the rule to other call sites that have the same problem.
4. **Map every comment.** Maintain a "verbatim feedback table" near the end of the plan with one row per comment: `# | file:line | verbatim | cluster`. No comment may be silently dropped.
5. **Do NOT start implementing** before the plan is approved via `ExitPlanMode`. Delegating reads via #0 is fine; editing source is not.

The canonical shape is the `Overarching themes` table + per-cluster `**#N (verbatim):** *"…"*` anchors + final mapping table. When a comment is ambiguous, ask via `AskUserQuestion` rather than guessing.

### Plan follow-up questions

After you write a plan, the user may respond with questions ("why this approach?", "what about X?", "did you consider Y?") rather than approval. In that case you MUST NOT edit the plan to bake in answers. Instead:

1. **Answer the question conversationally** in your text response — explain the reasoning, the tradeoffs, and what you'd recommend.
2. **Propose options via `AskUserQuestion`** — one question per ambiguity, each with 2–4 concrete options the user can pick from. Batch related questions into one `AskUserQuestion` call.
3. **Wait for the user's choice** before editing the plan. The plan edit then reflects the user's pick, not your assumption.

Editing the plan first robs the user of the choice and forces them to diff the plan to find what you decided. Surface the decision point first.

## Parallelize Independent Work

Sequential is the exception, not the default. Two steps that don't consume each other's output run at the same time; when unsure whether they're independent, assume they are and fan out. The orchestrator routes and synthesizes — it never executes work a subagent could. Pick the surface by scale:

- **Batch tool calls in one message** — the cheapest parallelism and the most missed. Independent reads, greps, globs, and read-only Bash go in a *single* message, never one per turn.
- **Parallel subagent calls in one message** — ad-hoc independent investigations: "explore X while I check Y", multi-file reviews, independent edits. One message, N `Agent` tool uses, results gathered in parallel.
- **Dynamic workflow** — default for substantive multi-step work; the script holds the loop, branching, and intermediate results. See CLAUDE.md `## Plan Execution & Orchestration`.
- **Named team** — long-running peers needing agent-to-agent handoffs mid-run, via `TeamCreate`.

Single-step exception: one task, no parallel sibling, no follow-on → one subagent call is fine.

## Writing Plans

When you write a plan — in plan mode, or any "here's what I'll do" before you start editing — use this shape so it's fast to scan and complete enough to execute:

- **Context** — why this change: the problem or need, what prompted it, the intended outcome.
- **Approach** — the recommended approach only (not every alternative you weighed), as ordered steps. Name the critical files to touch; for a pattern repeated across many files, describe it once with a few representative paths instead of listing them all. Cite existing utilities/patterns you'll reuse, with their paths.
- **Potential Pitfalls** — the sharp edges specific to this work: ordering constraints, code that looks safe to change but isn't, prior art that must not be "fixed", state that diverges from how it's described. One bullet each — front-load the gotchas you'd otherwise hit mid-implementation.
- **Workflow Plan** — required in every plan; a plan without it is incomplete. One line on what the main agent alone does (track state, dispatch, decide, report), then a `Phase | Shape | Agents | Verification` table covering every fan-out the plan anticipates: Shape is `pipeline` / `parallel` / `loop`; Agents names each phase's model and effort per the Models table (e.g. `opus xhigh ×4`, `sonnet low → codex`); Verification names the check that gates each phase's output. When nothing fans out, one line saying everything stays at the main-agent level replaces the table.
- **Verification** — how to prove it works end to end: the exact commands to run, tests to add, and behavior to observe.
