# ![cc-transcript](https://github.com/yasyf/cc-transcript/raw/main/docs/assets/readme-banner.webp)

**Grep every Claude Code session you've ever run.** The lossless Rust parser reads Claude Code's on-disk JSONL at 169 MB/s; the compiled CLI greps it and renders one line per event, 25 ms cold.

[![CI](https://github.com/yasyf/cc-transcript/actions/workflows/ci.yml/badge.svg)](https://github.com/yasyf/cc-transcript/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/cc-transcript.svg)](https://pypi.org/project/cc-transcript/)
[![License PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm--Noncommercial--1.0.0-blue)](https://github.com/yasyf/cc-transcript/blob/main/LICENSE)

## Get started

```bash
uvx cc-transcript list
```

Every session is already on disk under `~/.claude/projects`. `list` finds them newest first, and `stats` collapses one into a screenful:

<img src="https://github.com/yasyf/cc-transcript/raw/main/docs/assets/demo.png" alt="Terminal running 'uvx cc-transcript stats' — one session summarized as counts, kinds, models, and tool names" width="700">

Driving with an agent? Paste this:

```text
/plugin marketplace add yasyf/cc-transcript
/plugin install cc-transcript@cc-transcript
```

The plugin's skill teaches Claude to answer "what did I ask you yesterday" from its own history, funneling through the CLI instead of reading raw JSONL.

<details>
<summary>No Claude Code? Point any agent at the CLI</summary>

```text
Use `uvx cc-transcript` to investigate my Claude Code sessions: `list` finds
transcripts on disk, `stats` summarizes one, `grep` searches content, and `show`
renders one compact line per event. Start by listing my newest sessions and
summarizing the most recent one. Docs: https://yasyf.github.io/cc-transcript/
```

</details>

---

## Use cases

### Ask Claude what you asked Claude yesterday

Yesterday's session is a megabyte of JSONL, and pasting it into a chat blows the context window. Render the spine instead:

```bash
uvx cc-transcript show --signal --tail 20 ~/.claude/projects/<project>/<session>.jsonl
```

`--signal` keeps only the substantive user and assistant turns. One line per event, a few hundred tokens for the whole exchange. With the plugin installed, skip the command and ask Claude directly; its skill runs this funnel for you.

### Find the session where you fixed that bug

The fix happened weeks ago, in one of a few hundred transcripts. Search them all:

```bash
uvx cc-transcript grep "TranscriptExpiredError" --project cc-transcript
```

Hits print under their transcript's path, each line carrying its raw event index; feed an index back into `show --range` to re-read the conversation around the fix. `grep` searches your newest 50 transcripts by default; `--all` takes it to every session on disk, and a no-match run exits 1 so it scripts cleanly.

### Debug a token blowup from transcript evidence

A session burned through the context window and you want the culprit, not a guess. Rank the evidence:

```bash
uvx cc-transcript stats --per-file --project myapp
```

Each block reports text, thinking, and tool-io bytes plus per-tool call counts; the blowup stands out as an outsized `tool io` line. Then `grep --tool <name> --with-result` pins it to the exact calls.

### Script it from Python

The CLI's funnel is four commands; your own tooling wants the events themselves. `parse` turns a path into a `Transcript` of typed lazy views, with the noise already dropped inside the engine:

```python
from cc_transcript import NOISE_SPEC, discover, parse

transcript = parse(discover()[0], drop=NOISE_SPEC)
print(len(transcript.events), "events after the noise drop")
```

`discover()` lists every transcript on disk newest first, `stream` fans a whole corpus across the parse pool, and `resolve` finds one session by UUID. The [getting-started guide](https://yasyf.github.io/cc-transcript/docs/getting-started/index.html) builds this out to a composed filter and a sentiment score.

## More in the docs

- [The Python library](https://yasyf.github.io/cc-transcript/docs/getting-started/index.html) turns a transcript into typed lazy views, a composed filter, and a score in a dozen lines.
- [Filtering events](https://yasyf.github.io/cc-transcript/docs/guide/filtering-events.html) covers composable clauses, specs, and the ready-made `NOISE_SPEC`.
- [Sentiment scoring](https://yasyf.github.io/cc-transcript/docs/guide/scoring-sentiment.html) buckets conversations and scores them around any inference engine.
- [Feedback mining](https://yasyf.github.io/cc-transcript/docs/guide/mining-feedback.html) runs detectors, confidence calibration, and LLM verdict passes over your corpus.
- [The Rust engine](https://yasyf.github.io/cc-transcript/docs/guide/rust-engine.html) is the implementation — parsing, filtering, scoring, and mining — its correctness pinned by hand-owned literals and golden fixtures.
- [API reference](https://yasyf.github.io/cc-transcript/reference/index.html) documents the complete typed surface, from `TranscriptEvent` to `SessionActivity`.

Read the [docs](https://yasyf.github.io/cc-transcript/) for the full guide. Licensed under [PolyForm Noncommercial 1.0.0](https://github.com/yasyf/cc-transcript/blob/main/LICENSE).
