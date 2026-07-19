from __future__ import annotations

from captain_hook import (
    Allow,
    Block,
    Event,
    FilePath,
    Input,
    Introduced,
    TestFile,
    Tool,
    llm_gate,
)

RULE = (
    "AGENTS.md § Ownership & Layering: 'The Rust core is the implementation — new domains "
    "included' / 'Python keeps exactly four shapes.' Semantic computation (parsing, detection, "
    "scoring, selection, sampling, transformation) never lands in cc_transcript/*.py — the "
    "command splice layer (19126a8) and the CLI scratchpad (c85d850) were both implemented in "
    "Python first and had to be re-ported to Rust days later. Land the semantics in the Rust "
    "workspace (rust/crates/core, or the crate that owns the domain) with re-pinned goldens and "
    "the regenerated _native.pyi stub in the same commit — or, when the core already computes "
    "this, expose the missing binding instead of recomputing it in Python."
)

llm_gate(
    "You are judging a pending edit to cc_transcript/ — the Python facade tier of a library "
    "whose Rust core owns all semantics. The rule: semantic computation (parsing, detection, "
    "scoring, selection, sampling, transformation — anything that decides, ranks, matches, or "
    "derives) must be implemented in the Rust workspace, never in Python. Python legitimately "
    "keeps exactly four shapes: (1) LLM orchestration via spawnllm; (2) policy declared as "
    "frozen spec dataclasses with a JSON contract; (3) FFI marshalling and rehydration around "
    "_native calls; (4) I/O composition — filesystem, subprocess, and ML-library inference "
    "calls (model2vec, UDPipe): the call itself, never the math over its results. "
    "The <introduced> block holds the function definitions this edit newly introduces. Block "
    "ONLY if at least one introduced function body performs semantic computation in pure "
    "Python — loops or comprehensions that select, rank, or aggregate domain data; arithmetic "
    "or statistics; seeded random draws; regex- or split-based parsing of structured content — "
    "without delegating that computation to _native or store.engine. Do NOT block: bodies that "
    "call or await _native / store.engine and merely reshape the result; dataclass, NamedTuple, "
    "or Protocol declarations and their trivial accessors; spawnllm or prompt-building "
    "orchestration; subprocess and filesystem glue around external tools; type coercion at the "
    "FFI edge. If unsure, allow.",
    message=lambda r: f"{RULE} Judge: {r.reasoning}",
    label="rust-owns-semantics",
    events=Event.PreToolUse,
    only_if=[Tool("Edit", "Write"), FilePath("cc_transcript/*.py", "cc_transcript/**/*.py")],
    skip_if=[TestFile()],
    contexts=[Introduced(kind="function_definition")],
    tests={
        Input(
            tool="Edit",
            file="cc_transcript/mining/sampling.py",
            old="RADIUS = 2\n",
            content=(
                "RADIUS = 2\n\n"
                "def pick_windows(turns: list[int], seed: int) -> list[int]:\n"
                "    rng = random.Random(f'{seed}')\n"
                "    keep = [t for t in turns if t % RADIUS == 0]\n"
                "    return sorted(rng.sample(keep, min(3, len(keep))))\n"
            ),
        ): Block(pattern="Ownership & Layering"),
        Input(
            tool="Edit",
            file="cc_transcript/mining/sampling.py",
            old="RADIUS = 2\n",
            content=(
                "RADIUS = 2\n\n"
                "def pick_windows(turns: list[int], seed: int) -> list[int]:\n"
                "    rng = random.Random(f'{seed}')\n"
                "    keep = [t for t in turns if t % RADIUS == 0]\n"
                "    return sorted(rng.sample(keep, min(3, len(keep))))\n"
            ),
            llm={"block": False},
        ): Allow(),
        Input(
            tool="Edit",
            file="cc_transcript/cost.py",
            old="PRICING = {'opus': 15.0}\n",
            content="PRICING = {'opus': 15.0, 'fable': 25.0}\n",
        ): Allow(),
        Input(
            tool="Edit",
            file="tests/test_sampling.py",
            old="",
            content="def test_pick_windows():\n    assert pick_windows([2, 4], 7)\n",
        ): Allow(),
    },
)
