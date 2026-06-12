---
name: cc-transcript-investigate
description: Investigates the user's past Claude Code sessions via the cc-transcript CLI. Triggers on questions like "what did I ask Claude yesterday", "analyze my past sessions", "find the session where we fixed X", "how often do I interrupt", "which tools do I use most", and on debugging hooks, session resume, compaction, sidechains, or token blowups from transcript evidence. Rule, never read a raw .jsonl transcript directly — use list, stats, grep, then show.
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
context window. Every question is answerable through the four subcommands,
which parse, filter, and truncate server-side:

```bash
uvx cc-transcript --help
```

## Command cheat-sheet

| Command | What it does | Key flags |
|---|---|---|
| `list` | List transcripts, newest first, with mtime + size. | `--project <substr>` (project dir filter), `--contains <substr>` (filename filter), `--limit N` (default 50), `--all`, `--json` |
| `stats [PATHS]…` | Histogram of kinds/models/tools, char totals (text/thinking/tool io), sessions, time span, interrupts, tool errors, sidechain count. | `--project`, `--contains`, `--limit`, `--per-file`, `--json` |
| `grep PATTERN [PATHS]…` | Regex search over events; per-file `==` headers; each hit prefixed with its raw index. **Exit 1 = no match** (not an error). | `--kind user\|assistant\|system\|mode\|other` (repeatable), `--tool <name>` (tool calls + their results), `-i`, `-C N` context, `--where text\|thinking\|tools`, `--max-matches` (default 20) |
| `show PATH` | One compact line per event: `index tag hh:mm:ss payload` (`*` marks sidechain). Defaults to the last 200 events. | `--head N` / `--tail N` / `--range A:B` (raw indexes, half-open; `A:` and `:B` work), `--kind`, `--signal`, `--thinking`, `--width N` (default 100, 0 = no cut), `--json` |

All discovery commands default to `~/.claude/projects`; override with `--root`.

## Token efficiency

- **Funnel: `list` → `stats` → `grep` → `show --range`.** Locate the file,
  shape it before reading anything, find the exact events, then read only
  the relevant slice.
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
  (`-i`, drop `--kind`) or raise `--limit` to search older transcripts.
