---
name: cc-transcript-investigate
description: Investigates the user's past Claude Code sessions via the cc-transcript CLI. Triggers on questions like "what did I ask Claude yesterday", "analyze my past sessions", "find the session where we fixed X", "every time I mentioned X", "how often do I interrupt", "which tools do I use most", and on debugging hooks, session resume, compaction, sidechains, or token blowups from transcript evidence. Rule, never read a raw .jsonl transcript directly — use list, stats, grep, then show; for a sweep across many sessions, corpus then grep --corpus.
allowed-tools: Bash(uvx cc-transcript:*), Bash(uvx cc-transcript@latest:*)
---

# cc-transcript (investigate)

Claude Code writes every session to a `.jsonl` transcript under
`~/.claude/projects/`. This skill answers questions about those sessions —
past prompts, tool usage, interrupts, hook firings, compaction, sidechains,
token blowups — using the `cc-transcript` CLI.

## The one rule

**Never `cat`, `Read`, `head`, or `tail` a raw `.jsonl` transcript.** A single
transcript routinely exceeds 1MB of dense JSON; one careless read blows the
context window. Every question is answerable through the subcommands, which
parse, filter, and truncate server-side — and `corpus` is how you obey the
rule across a whole projects tree, where a hand-rolled `rg` over the raw
files is the same mistake at gigabyte scale:

```bash
uvx cc-transcript --help
```

## Command cheat-sheet

Every command that takes `[PATHS]…` discovers transcripts under
`~/.claude/projects` when none are given (override with `--root`), filters
them with `--project <substr>` (project dir name) and `--contains <substr>`
(file name), and — except `corpus` — searches only the newest 50 unless you
pass `--limit N` or `--all`. Their trailer says so:
`searched 3 of 13274 transcripts — use --all`.

### Investigate one session

| Command | What it does | Key flags |
|---|---|---|
| `list` | List transcripts, newest first, with mtime + size. | `--project`, `--contains`, `--limit N` (default 50), `--all`, `--provider claude\|codex\|all` (`--codex-root` for Codex sessions), `--json` |
| `stats [PATHS]…` | Histogram of kinds/models/tools/attachments, char totals (text/thinking/tool io), sessions, time span, interrupts, tool errors, slowest tools, sidechain count. | `--per-file`, `--json` |
| `grep PATTERN [PATHS]…` | Regex search over events; per-file `==` headers; each hit prefixed with its raw index. **Exit 1 = no match** (not an error). **Stops at 20 matches by default** and warns on stderr when that cap bites. | `--kind user\|assistant\|system\|mode\|other\|attachment` (repeatable), `--tool <name>` (tool calls + their results), `--errors`, `-i`, `-C N` events of context, `--where text\|thinking\|tools`, `--max-matches N` (0 = no cap), `--with-result`, `--width N`, `--uuids`, `--json` |
| `show PATH` | One compact line per event: `index tag hh:mm:ss payload` (`*` marks sidechain). Defaults to the last 200 events. | `--head N` / `--tail N` / `--range A:B` (raw indexes, half-open; `A:` and `:B` work), `--all`, `--kind`, `--signal`, `--no-junk`, `--errors`, `--thinking`, `--width N` (default 100, 0 = no cut), `--uuids`, `--json` |

### Sweep many sessions

| Command | What it does | Key flags |
|---|---|---|
| `corpus PATTERN [PATHS]… -o FILE` | Sweep every matched transcript once and write a deduped extract: one line per hit, `--window` characters either side. **Searches all transcripts by default**, not the newest 50. stderr reports `<N> files scanned, <N> windows kept, <N> duplicates dropped, <N> bytes written`; stdout is empty. **Exit 1 = zero windows** (the file is still written, empty). | `-o/--out FILE` (required), `--window N` (default 200 characters), `--where text\|thinking\|tools`, `-i`, `--project`, `--contains`, `--limit N` (cap the sweep at the newest N; unlimited by default) |
| `grep --corpus FILE PATTERN` | Search an extract instead of transcripts: matching windows verbatim, then a `<N> matches in <path>` trailer. **Uncapped by default.** Exit 1 = no match. | `-i`, `--max-matches N`. Every flag that selects transcripts or needs event structure (`PATHS`, `--root`, `--project`, `--contains`, `--limit`, `--all`, `--kind`, `--tool`, `--errors`, `--where`, `-C`, `--width`, `--uuids`, `--with-result`, `--json`) conflicts with `--corpus` and exits 2 naming both arguments |
| `tools [PATHS]…` | Every tool call, one line each: time, session prefix, tool (`server/tool` for MCP), Bash command prefixes, and a `[denied]` or `[err]` marker. | `--tool <name>`, `--file <glob>` (full path or basename), `--since`/`--until` (RFC 3339, `YYYY-MM-DD`, or `2d`), `--json` |
| `commands [PATHS]…` | Tally of Bash command prefixes, most frequent first. | `--json` |
| `permissions [PATHS]…` | Tool uses the user denied, with the instruction they gave instead. | `--json` |
| `mcp [PATHS]…` | MCP server and tool usage summary. | `--json` |

### Attribute a working-tree file

| Command | What it does | Key flags |
|---|---|---|
| `blame PATH` | Sessions that wrote a working-tree file, newest first: last-write time, session, `main`/`worktree:<name>`, write count, tools, opening prompt. **Exit 1 = no sessions wrote it.** | `--all-projects`, `--since`/`--until`, `--limit N`, `--json` |
| `attribute PATH` | Classify a working-tree file: `claude:<session>` (an edit call wrote it), `generated:<sessions>` (mtime inside a session's window with Bash activity), or `external`. | `--all-projects`, `--json` |

### Live tail and integration surfaces

| Command | What it does | Key flags |
|---|---|---|
| `watch` | Tail transcripts live, one line per newly appended event, until interrupted. | `--root` (repeatable), `--poll <seconds>` (default 1), `--from-start` (replay existing content), `--json` (NDJSON) |
| `slice --session UUID --since T --until T` | One session window's tool calls, one `cc-transcript.slice/1` JSON line each (RFC 3339 bounds; `--since` inclusive, `--until` exclusive). | `--root` |
| `scratchpad --session UUID` | Print a session's scratchpad directory. `--session` reads `CLAUDE_CODE_SESSION_ID` when omitted. | |
| `digest` | Generate the tool-digest fixture corpus from stdin, or verify one with `--check FILE`. A test-fixture tool, not an investigation command. | `--check FILE` |
| `corrections add\|query\|sql` | The shared code-correction ledger that review tooling writes to: `add` appends one correction (`--session`, `--source`, `--anchor`, `--incorrect-file` required), `query` emits matching rows as JSON lines (`--session`, `--repo`, `--digest`, `--since`, `--source`), `sql` runs a raw statement. | see `corrections <cmd> --help` |

## Sweeping many sessions with `corpus`

`grep` over the projects tree parses every transcript per question, stops at
20 matches, and its `-C` counts events, so one event of context can be
megabytes. For any question that spans more than a handful of transcripts,
sweep once and query the extract:

```bash
uvx cc-transcript corpus 'slop-cop' --project monorepo -o /tmp/slop-cop.corpus
# 13274 files scanned, 32814 windows kept, 288310 duplicates dropped, 10707492 bytes written
uvx cc-transcript grep --corpus /tmp/slop-cop.corpus -i 'plainify' --max-matches 2
# …one window per line…
# 2 matches in /tmp/slop-cop.corpus
```

Measured on this machine: the sweep scanned about 13,270 transcripts and
kept about 32,800 windows (10.7 MB) in 2 to 3.5 seconds wall, depending on
load; each `grep --corpus` over the extract takes about 10 ms. The
hand-rolled alternative — `xargs rg -o '.{0,200}PATTERN.{0,200}' | sort -u`
over the raw files — took about two minutes per query over a narrower file
set, because it re-reads 2.7 GB every time. That is 35 to 70 times slower
per sweep, and the cost recurs on every question instead of once. File and
window counts drift upward between runs because live sessions keep
appending transcripts; the tool is deterministic over a fixed tree.

The extract is also better, not only faster. `corpus` windows the *rendered*
haystack — text, thinking, tool inputs, tool results — not raw JSONL bytes,
so no envelope metadata or JSON escaping inflates a window, and dedupe
collapses the CLAUDE.md boilerplate every session injects. A raw sweep's
windows carry surrounding uuids and timestamps that make near-duplicates
unique, and they survive `sort -u`. On the same literal, `grep --corpus
… -i 'false positive'` found 341 matches against 189 in the hand-built
artifact.

What a window is: `--window` *characters* either side of each hit (200 by
default), not events. Line breaks inside a window become the literal two
characters `\n`, so one window is always exactly one output line. Dedupe is
exact-string in first-seen order.

> **Delegation rule.** When a corpus artifact exists, hand subagents that
> path and nothing else. Never pass a raw-transcript recipe (`rg` over
> `~/.claude/projects`, or `grep` without `--corpus`) alongside it: given
> both, agents reach for the expensive one. Six lanes running eight
> whole-tree sweeps each cost roughly 130 GB of redundant I/O for answers
> the 10 MB file already held.

Getting from a window back to its transcript: the extract carries no path or
index, so lift a distinctive phrase from the window and run a structured
`grep` on it to find the file and raw index, then `show --range` around it.

## Token efficiency

- **Single-session funnel: `list` → `stats` → `grep` → `show --range`.**
  Locate the file, shape it before reading anything, find the exact events,
  then read only the relevant slice. Use it when the question names one
  session ("yesterday", "the session where we fixed X") or a handful.
- **Sweep funnel: `list` → `corpus` → `grep --corpus` → `show --range`.**
  Use it when the question is "every time", "how often", "across all my
  sessions", or anything that spans more than a handful of transcripts.
  `list --project <substr>` first, to confirm the filter matches the
  projects you mean, then one `corpus` sweep, then as many `grep --corpus`
  queries as the question needs.
- **`grep` stops at 20 matches unless told otherwise.** A whole-tree
  `grep` returning 20 rows is capped, not complete; the stderr warning
  `stopped at --max-matches 20` says so. Pass `--max-matches 0` to lift the
  cap, or switch to `corpus`, which never caps.
- **Every command but `corpus` searches the newest 50 transcripts.** Read
  the `searched N of M transcripts` trailer before treating a result as
  exhaustive; `--all` widens it.
- **Indexes are stable raw-file positions.** The index `grep` prints (and
  `show` prints) is the event's position in the unfiltered file — filtering
  with `--kind` or `--signal` never renumbers. A `grep` hit at index 142 is
  always valid input to `show <path> --range 130:155`.
- **Compact output is the right default fidelity.** Reach for `--json`
  (full-fidelity event objects) only on a slice already narrowed to a few
  events; never `--json` a whole transcript.
- **`--signal` strips the noise.** It keeps only substantive user/assistant
  turns — structural junk, synthetic events, sidechains, and empty turns
  drop out. Use it whenever the question is about the human conversation.
- **Widen `--width` before reaching for `--json`.** If a compact line is
  truncated mid-answer, `--width 300` (or `--width 0` on a tight `--range`)
  is far cheaper than full JSON.
- **`stats` first on anything unfamiliar.** A dozen compact summary lines
  (three histograms plus totals) tell you whether a transcript is worth
  grepping at all.

## Worked workflows

### "What did I ask Claude yesterday?"

```bash
uvx cc-transcript list --limit 10
# pick the relevant path(s) by project dir + mtime, then:
uvx cc-transcript show ~/.claude/projects/<project>/<session>.jsonl --kind user --signal --tail 100
```

`--kind user --signal` reduces a multi-megabyte transcript to the human's
actual prompts, one line each.

### "Find the session where we fixed the login bug"

```bash
uvx cc-transcript grep -i 'login' --kind user
# hit: == ~/.claude/projects/-Users-me-app/abc123.jsonl
#        142 user  09:14:03 the login redirect is broken after …
uvx cc-transcript show ~/.claude/projects/-Users-me-app/abc123.jsonl --range 130:180
```

The hit's index (142) is a raw position, so widening the range around it
shows the surrounding conversation and tool activity exactly as it happened.

### "Every time slop-cop came up across all my monorepo sessions"

```bash
uvx cc-transcript list --project monorepo --limit 5
# confirm the filter matches the projects you mean, then sweep once:
uvx cc-transcript corpus 'slop-cop' --project monorepo -o /tmp/slop-cop.corpus
# 13274 files scanned, 32814 windows kept, 288310 duplicates dropped, 10707492 bytes written
uvx cc-transcript grep --corpus /tmp/slop-cop.corpus -i 'false positive'
uvx cc-transcript grep --corpus /tmp/slop-cop.corpus -i 'plainify' --max-matches 5
```

Each follow-up query is a millisecond-scale read of the extract, so ask as
many as the question needs. To hand the mining to subagents, give them the
extract path and `grep --corpus`, and nothing about the raw tree.

### "Which tools do I use most, and what do I deny?"

```bash
uvx cc-transcript stats --all
uvx cc-transcript commands --all
uvx cc-transcript permissions --all
```

`stats` histograms tools and attachments across every transcript; `commands`
tallies Bash prefixes; `permissions` lists each denied tool use with the
instruction the user gave instead.

### "Debug a hook"

```bash
uvx cc-transcript grep 'stop_hook' --kind system -C 2
```

Hook firings, blocks, and error payloads land in `system` events; `-C 2`
shows what triggered each one and what happened next. The same pattern works
for compaction (`grep -i 'compact'`), session resume, and sidechain
investigation (`stats` reports the sidechain count; `*`-tagged lines in
`show` are sidechain events).

## Troubleshooting

- **`uvx cc-transcript` fails or lacks these commands** — the CLI requires
  `cc-transcript >= 0.9.0` on PyPI. Run `uvx cc-transcript@latest --help` to
  bust a stale uvx cache and pull the current release.
- **`0 transcripts under ~/.claude/projects` (exit 0)** — a clean empty
  result, not an error: this machine has no recorded sessions (or `--root`
  points somewhere empty). Report that; don't retry.
- **`grep` exits 1** — zero matches, not a failure. Loosen the pattern
  (`-i`, drop `--kind`) or pass `--all` to search older transcripts.
- **`grep` prints `warning: stopped at --max-matches 20`** — the result is
  truncated. Rerun with `--max-matches 0`, or build a `corpus` if more
  questions are coming.
- **`corpus` exits 1** — the sweep matched nothing; the output file exists
  and is empty. Check the pattern and the `--project` filter with `list`
  before widening.
- **`grep --corpus` exits 2 naming two arguments** — you passed a flag that
  needs event structure or selects transcripts. Only the pattern, `-i`, and
  `--max-matches` apply to an extract; run a structured `grep` without
  `--corpus` for anything else.
- **`error: unexpected argument '-U' found`** — a bare `-Users-…` path,
  which clap reads as a flag because project directory names start with a
  hyphen. Pass it as `./-Users-…`, put `--` before it, or use the absolute
  `~/.claude/projects/-Users-…` form `list` prints.
