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

MESSAGE = (
    "Semantic computation belongs in the Rust core, never in `cc_transcript/*.py`. "
    "Implement it in `rust/crates/core`, then regenerate `_native.pyi` with `cargo run -p cc-transcript-py --bin stub_gen`."
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
    message=MESSAGE,
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
        ): Block(pattern="Rust core"),
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
