# cc-transcript

[![PyPI](https://img.shields.io/pypi/v/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Python](https://img.shields.io/pypi/pyversions/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-transcript/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-transcript/)
[![License: PolyForm-Noncommercial-1.0.0](https://img.shields.io/badge/License-PolyForm-Noncommercial-1.0.0-blue.svg)](https://github.com/yasyf/cc-transcript/blob/main/LICENSE)

The shared transcript-parsing core extracted from [cc-sentiment](https://github.com/yasyf/cc-sentiment), now powering cc-pushback, cc-sentiment, and captain-hook. It parses Claude Code's on-disk JSONL transcripts into a **typed superset event model** — every entry type preserved, nothing dropped — so each consumer applies its own semantic filtering on top of one faithful representation.

The one property that makes it worth using: the parser is non-lossy. It never silently discards sidechains, synthetic turns, tool results, or unrecognized entry types; filtering is opt-in and lives in the consumer, not buried in the parser.

## Install

```bash
uv add cc-transcript
```

## Quickstart

Discover the transcripts on disk, parse one, and look at the events:

```python
from cc_transcript import TranscriptDiscovery, parse_events, AssistantEvent, UserEvent

path, _mtime = TranscriptDiscovery.find_in(TranscriptDiscovery.find_transcripts()[0].parent)[0]
events = parse_events(path)

for event in events:
    match event:
        case UserEvent(text=text):
            print("user:", text[:80])
        case AssistantEvent(model=model, text=text):
            print(f"assistant ({model}):", text[:80])
```

Apply cc-sentiment's filtering rules to drop sidechains, synthetic turns, and junk:

```python
from cc_transcript import apply_filters, SENTIMENT_FILTER

clean = list(apply_filters(events, SENTIMENT_FILTER))
```

## What problems does this solve?

- **One faithful parse, many consumers.** Every project that reads Claude Code transcripts re-implements the same JSONL quirks (str-or-list content, tool results nested two ways, envelope-less mode markers). This is that parser, written once and typed strictly.
- **Non-lossy by design.** The event model is a superset: sidechains, `<synthetic>` turns, thinking blocks, and unrecognized entry types all survive parsing. You decide what to drop, via `FilterConfig`.
- **Incremental ingestion.** `FileStateStore` tracks per-file mtimes in SQLite (WAL, thread-safe) so re-runs only reparse changed files, and consumers compose their own writes in the same transaction.
- **Pluggable backends.** A pure-Python reference parser ships today; a Rust backend behind the same `Backend` protocol reaches parity in a later release.

## Docs

[Read the docs](https://yasyf.github.io/cc-transcript/) for the full guide and API reference.
