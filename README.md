# cc-transcript

![cc-transcript banner](https://github.com/yasyf/cc-transcript/raw/main/docs/assets/readme-banner.webp)

[![PyPI](https://img.shields.io/pypi/v/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Python](https://img.shields.io/pypi/pyversions/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![Docs](https://img.shields.io/github/actions/workflow/status/yasyf/cc-transcript/docs.yml?branch=main&label=docs)](https://yasyf.github.io/cc-transcript/)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/License-PolyForm--Noncommercial--1.0.0-blue.svg)](https://github.com/yasyf/cc-transcript/blob/main/LICENSE)

A non-lossy parser for Claude Code's on-disk JSONL transcripts, plus a CLI to investigate sessions.

`cc-transcript` parses Claude Code's JSONL transcripts into a typed superset event model — every entry type preserved, nothing dropped. The parser never silently discards sidechains, synthetic turns, tool results, or unrecognized entries; filtering is opt-in and lives in your code, not buried in the parser. It ships as a Python library, a `uvx`-runnable CLI, and a Claude Code plugin.

## Install

```bash
uvx cc-transcript --help    # run the CLI
uv add cc-transcript        # use as a library
```

## Quickstart

Discover the transcripts on disk, parse one, and walk the events:

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

Filtering is opt-in. Compose a spec from small builders and apply it — `build_spec` assembles clauses, `apply_spec` yields the survivors. `NOISE_SPEC` is a ready-made spec for universal structural noise (system reminders, local-command output, skill banners):

```python
from cc_transcript import apply_spec, build_spec, keep_only, drop_junk, drop_short

spec = build_spec(keep_only("user", "assistant"), drop_junk("structural"), drop_short(2))
clean = list(apply_spec(events, spec))
```

## CLI

Every command runs as `uvx cc-transcript <command> ...`; see `--help` on any command for its flags.

| Command  | What it does |
| -------- | ------------ |
| `list`   | Find transcripts on disk, newest first |
| `stats`  | Summarize a session — counts, kinds, models, tools, token sizes, span |
| `show`   | Render one compact line per event (`--signal` keeps the conversational spine) |
| `grep`   | Search event content; hit indexes feed back into `show --range` |
| `slice`  | Emit per-tool-call JSONL for a session UUID and time window (the bridge cc-review consumes) |
| `digest` | Generate and check the cross-language fixture corpus for the tool-digest contract |

The output is compact by design — one line per event, hard truncation — so an agent triages a session in a few hundred tokens instead of paging through megabytes of JSONL:

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

## Claude Code plugin

Install the bundled plugin from inside Claude Code:

```
/plugin marketplace add yasyf/cc-transcript
/plugin install cc-transcript@cc-transcript
```

The plugin's skill teaches Claude to answer questions about its own history — "what did I ask yesterday", "find the session where we fixed the parser" — by funneling through the CLI's `list`, `stats`, `grep`, and `show` commands instead of reading raw JSONL.

## How it works

The event model is a superset: sidechains, `<synthetic>` turns, thinking blocks, and unrecognized entry types all survive parsing, so you decide what to drop via composable filter specs. Two engines sit behind one `Backend` protocol — `RustBackend` (PyO3 + rayon) is the default fast path, `PythonBackend` is the readable reference, parity-asserted against each other. On top of the spine, `SessionActivity` lifts events into turns and typed tool calls, `FileStateStore` tracks per-file mtimes in SQLite for incremental re-parses, and `cc_transcript.mining`/`judge`/`sentiment` extract feedback, run LLM verdict passes, and score conversational sentiment. The [docs site](https://yasyf.github.io/cc-transcript/) covers each of these in depth.

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
