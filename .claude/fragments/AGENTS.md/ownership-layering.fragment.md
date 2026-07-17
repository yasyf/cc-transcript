## Ownership & Layering

- **Parse once, at the parsing layer.** Interpreting a tool input after the fact goes
  through the typed layer: `ToolUseBlock.call` / `tools.parse_tool_call` for tool calls,
  `command.CommandLine` for bash. Never regex or split a raw command string or `input`
  dict at a call site — if the typed layer can't answer the question, extend it.
- **One owner module per concept.** Before writing a detector, predicate, or projection,
  find the existing implementation (`semble`, LSP) and extend it. Two implementations of
  one semantic are a bug even while both are correct.
- **Rust parity is atomic.** A semantic change to a Rust-mirrored domain (filterspec,
  mining, score, lexicon, command) lands with its Rust port and the parity-suite re-pin
  in the same commit. Shared literals and tables (protocol markers, mining
  ids/separators/floors, command tables) are generated: change the Python constant, run
  `uv run python scripts/build_rust_literals.py`, and commit the regenerated
  `rust/crates/core/src/generated/*.rs` with the change — never hand-edit them.
  `tests/test_literals_parity.py` is the drift gate. Hand-written `// Parity:` comments
  now cover only algorithm-level ports; they cite symbol names
  (`scorespec.apply_post_process`), never line numbers.
- **Build on the lifted layers.** New analysis features consume `activity`/`query`/
  `facts` or a domain package — never re-parse raw events — unless the feature *is*
  the raw layer. The fences in `tests/test_fence.py` and `tests/test_import_weight.py`
  are release contracts; a change that trips one is wrong until proven otherwise.

## Python Style

Target Python 3.13+. Run `uv sync --extra dev`, `uv run pytest`, and `uv build`.

**Docstrings on the public API only.** User-facing surfaces carry Google-style docstrings; they render into the docs site via Great Docs. Internal helpers get none. No comments except TODOs, non-obvious workarounds, or disabled code.

@STYLEGUIDE.md

## General Rules

**Minimal changes.** Stay within scope; fix the issue, then stop.

**Match surrounding code.** Follow the conventions of the file you're in, then the module.

**No defensive coding.** No fallbacks, shims, or backwards-compat layers; no guards against impossible states. If unused, delete it. Crash on the unexpected.

**Search before writing.** Before creating a helper, query the codebase via `ccx code search` (intent) or `ccx code symbol` (a named symbol). Sibling modules and base classes win over re-implementation.

**Code stewardship.** When you touch a file, fix nearby bugs, style violations, and broken tests; don't wave them off as pre-existing or out of scope. Trivial type-checker noise is the exception (see § Python Style).

**Observe, don't infer.** Inspect actual data — read fixtures, dump objects, run the code — before reasoning from assumption.

**Don't use external failures as an excuse to stop.** API quota, rate-limit, and outage errors rarely block the whole task; trace the catch sites and confirm a failure actually stops you before claiming it does.

**Verify before asserting.** Don't report something as working, fixed, blocked, or impossible until you've checked — run it, read the output, reproduce the failure. "It should work" is not "it works."

**Reproduce before fixing.** When something breaks, isolate the smallest failing case before editing or re-running. Re-running the whole command while changing code between runs hides the root cause; narrow to the one failing call, payload, or test first.

**Research after repeated failure.** After ~2 failed approaches, stop guessing and gather evidence — search the web, read the docs and source — before a third attempt.

**Get a second opinion on a plateau.** On a debugging plateau (2 failed attempts before a 3rd), a non-trivial architectural decision, or algorithmic/security-sensitive code, get an outside check (e.g. `/codex`) before committing to the approach.

**Don't contort code to satisfy a checker.** The type checker and linter serve the code, not the other way around. Don't reshape a data model, widen a type, or bolt on a `cast(...)` / narrowing-only `assert isinstance(...)` / blanket ignore just to silence a diagnostic. If a clean fix isn't obvious, leave the diagnostic — a visible diagnostic is preferable to scar tissue. (Most checker noise isn't worth acting on at all; act only when it flags a real bug.)

**Mechanical linting.** CI and hooks handle formatting and import order. Leave `ruff` to them and fix only what needs human judgment. When reviewing code, don't flag mechanical lint violations (line length, whitespace, import order, trailing commas).

**Testing.** The suite lives in `tests/`; run it with `uv run pytest`. Use strict assertions and mock external dependencies while leaving the code under test real.

**Docs.** Any public API change must keep the docs build green: `uv sync --group docs`, then `uv run --with "git+https://github.com/yasyf/cc-skills@main#subdirectory=tools/gd-build" gd-build build` — the exact command docs CI runs. Never bare `great-docs build`: it misses the pre_render titles script gd-build materializes into the gitignored `docs/scripts/.gd-build/`, and a large API reference can hang the render for an hour without it (pandoc #11687).

**Git.** Commits should be atomic and scoped. One logical change per commit.

**Releases.** Tagging `v*` triggers `.github/workflows/release-pypi.yml`, which builds, publishes to PyPI via trusted publishing, and cuts a GitHub release. The version comes from the tag.
