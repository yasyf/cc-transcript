# cc-transcript

![cc-transcript banner](https://github.com/yasyf/cc-transcript/raw/main/docs/assets/readme-banner.png)

[![PyPI](https://img.shields.io/pypi/v/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Python](https://img.shields.io/pypi/pyversions/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-transcript/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-transcript/)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/License-PolyForm--Noncommercial--1.0.0-blue.svg)](https://github.com/yasyf/cc-transcript/blob/main/LICENSE)

`cc-transcript` parses Claude Code's on-disk JSONL transcripts into a **typed superset event model** — every entry type preserved, nothing dropped — so you build on one faithful representation and apply your own semantic filtering on top.

The one property that makes it worth using: the parser is non-lossy. It never silently discards sidechains, synthetic turns, tool results, or unrecognized entry types; filtering is opt-in and lives in your code, not buried in the parser. It ships as a Python library, a `uvx`-runnable CLI, and a Claude Code plugin.

## Install

```bash
uv add cc-transcript        # or: pip install cc-transcript
uvx cc-transcript --help    # CLI, no install needed
```

## Quickstart

Discover the transcripts on disk, parse one, and look at the events:

```python
import anyio

from cc_transcript import AssistantEvent, TranscriptDiscovery, UserEvent, parse_events_from_bytes

path = anyio.run(TranscriptDiscovery.find_transcripts)[0]
events = parse_events_from_bytes(path.read_bytes())

for event in events:
    match event:
        case UserEvent(text=text):
            print("user:", text[:80])
        case AssistantEvent(model=model, text=text):
            print(f"assistant ({model}):", text[:80])
```

Compose a filter from small builders and apply it. The builders return clauses,
`build_spec` assembles them into a spec, and `apply_spec` yields the survivors:

```python
from cc_transcript import apply_spec, build_spec, keep_only, drop_junk, drop_short

spec = build_spec(keep_only("user", "assistant"), drop_junk("structural"), drop_short(2))
clean = list(apply_spec(events, spec))
```

`NOISE_SPEC` is a ready-made spec for the universal structural noise (system reminders,
local-command output, skill banners). For flag-style filtering, `FilterConfig` is also
available — every rule is off by default, so a bare `FilterConfig()` passes everything through.

## The CLI

Four commands — `list`, `show`, `grep`, `stats` — and every one runs as `uvx cc-transcript ...`, no install step. `list` finds transcripts, newest first:

```console
$ uvx cc-transcript list --limit 3
2026-06-11 19:27    1.0MB ~/.claude/projects/-Users-yasyf-Code-captain-hook/d2ca206a-2561-4c2c-9a4c-3ecaac9f8443/subagents/agent-a804d9aea43a110b5.jsonl
2026-06-11 19:27   70.6KB ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e/subagents/agent-affd5dbe069a3660d.jsonl
2026-06-11 19:27  740.8KB ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e.jsonl
3 of 6608 transcripts under ~/.claude/projects
```

`stats` summarizes a session before you read any of it:

```console
$ uvx cc-transcript stats ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e.jsonl
files        1
events       181
kinds        other 68 · assistant 53 · user 33 · mode 22 · system 5
models       claude-fable-5 53
tools        TaskCreate 10 · Agent 5 · Read 5 · TaskUpdate 5 · Bash 2 · ToolSearch 2 · AskUserQuestion 1 · ExitPlanMode 1
text         14.8KB
thinking     8.7KB
tool io      89.0KB
sessions     1
span         2026-06-12 01:07:55 → 2026-06-12 02:28:03
interrupts   0
tool errors  0
sidechain    0
```

`show` renders one compact line per event; `--signal` keeps the conversational spine, and the index column is the event's position in the raw file:

```console
$ uvx cc-transcript show ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e.jsonl --signal --tail 4
  189 asst  02:30:49 [claude-fable-5] Bash(rg -A3 'name = "great-docs"' /Users/yasyf/Code/cc-transcript/uv.lock | head -6; echo ---; rg -n "cl…)
  194 asst  02:31:31 [claude-fable-5] "`cli:` support confirmed in the pinned great-docs. Checking the exact config shape before writing:"
  195 asst  02:31:31 [claude-fable-5] TaskUpdate(8)
  196 asst  02:31:32 [claude-fable-5] Bash(sed -n '40,60p;1750,1790p' /Users/yasyf/.cache/uv/git-v0/checkouts/a9f52a54772f9b4e/d318527/great_d…)
```

`grep` searches event content; hit indexes feed straight back into `show --range`:

```console
$ uvx cc-transcript grep -i "filterspec" --kind user --max-matches 3 ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e.jsonl
== ~/.claude/projects/-Users-yasyf-Code-cc-transcript/4c77d556-8694-4613-8f50-253d905da68e.jsonl
   16 user  01:12:00 <-Agent (10161ch) ## Findings Report: cc-transcript Repository Based on a thorough exploration of `/Users/yasyf/Code/…
   29 user  01:16:29 <-? (1378ch) /Users/yasyf/Code/cc-transcript/cc_transcript/: total 8648 drwxr-xr-x@ 19 yasyf staff 608 Jun 11 17…
   69 user  01:36:17 <-Read (4247ch) 1 """Composable builder fragments for :class:`~cc_transcript.FilterSpec`. 2 3 Each fragment returns…
1 files, 3 matches
```

The output is compact by design — one line per event, hard truncation — so an agent triages a session in a few hundred tokens instead of paging through megabytes of JSONL.

## Claude Code plugin

Install the bundled plugin from inside Claude Code:

```
/plugin marketplace add yasyf/cc-transcript
/plugin install cc-transcript@cc-transcript
```

The plugin's skill teaches Claude to answer questions about its own history — "what did I ask yesterday", "find the session where we fixed the parser" — by funneling through the CLI's `list`, `stats`, `grep`, and `show` commands instead of reading raw JSONL.

## What problems does this solve?

- **One faithful parse.** Anything reading Claude Code transcripts re-implements the same JSONL quirks (str-or-list content, tool results nested two ways, envelope-less mode markers). This is that parser, written once and typed strictly.
- **Non-lossy by design.** The event model is a superset: sidechains, `<synthetic>` turns, thinking blocks, and unrecognized entry types all survive parsing. You decide what to drop, via composable filter specs (`build_spec`) or `FilterConfig`.
- **Incremental ingestion.** `FileStateStore` tracks per-file mtimes in SQLite (WAL, safe across concurrent tasks) so re-runs only reparse changed files, and you compose your own writes in the same transaction.
- **Two engines, one contract.** A single `Backend` protocol with two implementations: `RustBackend` (PyO3 + rayon) is the default fast path, and `PythonBackend` is the readable reference — parity-asserted against each other. Filter specs are portable, so a spec built in Python runs Rust-side without giving up the fast path.
- **Analysis domains.** `domains.sentiment` scores conversational sentiment per time-bucketed conversation window; `domains.mining` mines transcripts for user feedback — detectors, confidence calibration, candidate filtering, and verdicts.
- **Transcript investigation for agents.** The CLI answers "what happened in that session" in a few hundred tokens, which is what makes the Claude Code plugin viable.

## Docs

Each section of [the docs site](https://yasyf.github.io/cc-transcript/) is a focused guide:

- [Getting Started](https://yasyf.github.io/cc-transcript/docs/getting-started/index.html) — install, parse, filter, persist.
- [Filtering events](https://yasyf.github.io/cc-transcript/docs/guide/filtering-events.html) — clauses, specs, and `NOISE_SPEC`.
- [Scoring sentiment](https://yasyf.github.io/cc-transcript/docs/guide/scoring-sentiment.html) — the lexicon engine and score specs.
- [Rust/Python backends & parity](https://yasyf.github.io/cc-transcript/docs/guide/backends-and-parity.html) — the `Backend` protocol and parity testing.
- [Compose your own policy](https://yasyf.github.io/cc-transcript/docs/guide/compose-your-own-policy.html) — building a bespoke filtering policy.
- [Mining feedback](https://yasyf.github.io/cc-transcript/docs/guide/mining-feedback.html) — detectors, confidence, candidates, and verdicts.
- [The transcript CLI](https://yasyf.github.io/cc-transcript/docs/guide/transcript-cli.html) — `list`/`show`/`grep`/`stats` end to end.
- [API reference](https://yasyf.github.io/cc-transcript/reference/index.html) — the complete typed surface.

## License

[PolyForm Noncommercial 1.0.0](LICENSE).
