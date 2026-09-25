from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript import evidence
from cc_transcript.activity import SessionActivity
from cc_transcript.ids import EventRef, EventUuid
from tests import testkit
from tests.support import BASE, SESSION, assistant, user

COMMIT = "1" * 40
RESPONSES = {
    "rev-parse": "true\n",
    "log": f"{COMMIT} 1767312000\n",
    "show": "@@ -1 +1 @@\n-bad\n+fixed\n",
}


@pytest.fixture
def activity() -> tuple[SessionActivity, EventRef]:
    return (
        SessionActivity.from_events(
            SESSION,
            (
                user("u", "edit"),
                assistant(
                    "a",
                    "",
                    blocks=(testkit.tool_use("t", "Edit", {"file_path": "a.py", "old_string": "old", "new_string": "bad"}),),
                ),
                user("anchor", "correct this"),
            ),
        ),
        EventRef(SESSION, EventUuid("anchor")),
    )


def unexpected_git(repo: Path, *args: str) -> str | None:
    raise AssertionError(f"unexpected default Git call: {repo}, {args}")


def test_harvest_forwards_runner_to_every_git_command(
    activity: tuple[SessionActivity, EventRef], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(evidence, "run_git", unexpected_git)
    calls: list[tuple[Path, tuple[str, ...]]] = []

    def runner(repo: Path, *args: str) -> str | None:
        calls.append((repo, args))
        return RESPONSES[args[0]]

    session, anchor = activity
    pairs = evidence.harvest_pairs(session, anchor, repo=Path("/repo"), git_runner=runner)
    assert calls == [
        (Path("/repo"), ("rev-parse", "--is-inside-work-tree")),
        (Path("/repo"), ("log", f"--since={BASE.isoformat()}", "--max-count=5", "-Sbad", "--format=%H %ct", "--", "a.py")),
        (Path("/repo"), ("show", "--format=", "--unified=0", COMMIT, "--", "a.py")),
    ]
    assert len(pairs) == 1
    assert isinstance(pairs[0].correction, evidence.GitFix)
    assert pairs[0].correction.commit == COMMIT
    assert pairs[0].overlap == 1.0


@pytest.mark.parametrize("failing_command", ["rev-parse", "log", "show"])
def test_incomplete_runner_error_propagates_without_default_fallback(
    activity: tuple[SessionActivity, EventRef], monkeypatch: pytest.MonkeyPatch, failing_command: str
) -> None:
    monkeypatch.setattr(evidence, "run_git", unexpected_git)
    incomplete = RuntimeError(f"incomplete Git evidence: {failing_command}")

    def runner(repo: Path, *args: str) -> str | None:
        if args[0] == failing_command:
            raise incomplete
        return RESPONSES[args[0]]

    session, anchor = activity
    with pytest.raises(RuntimeError, match="incomplete Git evidence") as raised:
        evidence.harvest_pairs(session, anchor, repo=Path("/repo"), git_runner=runner)
    assert raised.value is incomplete


def test_default_runner_remains_the_default(activity: tuple[SessionActivity, EventRef], monkeypatch: pytest.MonkeyPatch) -> None:
    commands: list[str] = []

    def runner(repo: Path, *args: str) -> str | None:
        commands.append(args[0])
        return RESPONSES[args[0]]

    monkeypatch.setattr(evidence, "run_git", runner)
    session, anchor = activity
    pairs = evidence.harvest_pairs(session, anchor, repo=Path("/repo"))
    assert commands == ["rev-parse", "log", "show"]
    assert isinstance(pairs[0].correction, evidence.GitFix)


def test_supplied_runner_none_does_not_fall_back(activity: tuple[SessionActivity, EventRef], monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(evidence, "run_git", unexpected_git)
    commands: list[str] = []

    def runner(repo: Path, *args: str) -> str | None:
        commands.append(args[0])
        return None

    session, anchor = activity
    pairs = evidence.harvest_pairs(session, anchor, repo=Path("/repo"), git_runner=runner)
    assert commands == ["rev-parse"]
    assert len(pairs) == 1
    assert pairs[0].correction is None
