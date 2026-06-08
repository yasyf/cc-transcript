# cc-transcript

[![PyPI](https://img.shields.io/pypi/v/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Python](https://img.shields.io/pypi/pyversions/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-transcript/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-transcript/)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/License-PolyForm--Noncommercial--1.0.0-blue.svg)](https://github.com/yasyf/cc-transcript/blob/main/LICENSE)

`cc-transcript` parses Claude Code's on-disk JSONL transcripts into a **typed superset event model** — every entry type preserved, nothing dropped — so you build on one faithful representation and apply your own semantic filtering on top.

The one property that makes it worth using: the parser is non-lossy. It never silently discards sidechains, synthetic turns, tool results, or unrecognized entry types; filtering is opt-in and lives in your code, not buried in the parser.

## Install

```bash
uv add cc-transcript        # or: pip install cc-transcript
```

## Quickstart

Discover the transcripts on disk, parse one, and look at the events:

```python
from cc_transcript import TranscriptDiscovery, parse_events, AssistantEvent, UserEvent

events = parse_events(TranscriptDiscovery.find_transcripts()[0])

for event in events:
    match event:
        case UserEvent(text=text):
            print("user:", text[:80])
        case AssistantEvent(model=model, text=text):
            print(f"assistant ({model}):", text[:80])
```

Compose a filter from small building blocks and apply it. The builders return clauses,
`build_spec` assembles them into a spec, and `apply_spec` yields the survivors:

```python
from cc_transcript import apply_spec, build_spec, keep_only, drop_junk, drop_short

spec = build_spec(keep_only("user", "assistant"), drop_junk("structural"), drop_short(2))
clean = list(apply_spec(events, spec))
```

`NOISE_SPEC` is a ready-made spec for the universal structural noise (system reminders,
local-command output, skill banners). For flag-style filtering, `FilterConfig` is also
available — every rule is off by default, so a bare `FilterConfig()` passes everything through.

## What problems does this solve?

- **One faithful parse.** Anything reading Claude Code transcripts re-implements the same JSONL quirks (str-or-list content, tool results nested two ways, envelope-less mode markers). This is that parser, written once and typed strictly.
- **Non-lossy by design.** The event model is a superset: sidechains, `<synthetic>` turns, thinking blocks, and unrecognized entry types all survive parsing. You decide what to drop, via composable filter specs (`build_spec`) or `FilterConfig`.
- **Incremental ingestion.** `FileStateStore` tracks per-file mtimes in SQLite (WAL, thread-safe) so re-runs only reparse changed files, and you compose your own writes in the same transaction.
- **Pluggable backends.** A Rust backend (PyO3 + rayon) is the default fast path, with a pure-Python reference parser behind the same `Backend` protocol as the fallback — both at full event parity.

## Docs

[Read the docs](https://yasyf.github.io/cc-transcript/) for the full guide and API reference.
