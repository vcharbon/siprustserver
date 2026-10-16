"""`hakit cleanup`: the generators of a run whose runner is gone are removed,
docker containers and pods alike, whether or not an event of it was open; a
running run's are left alone."""

from __future__ import annotations

import os
from pathlib import Path

from hakit.cleanup import cleanup
from hakit.clock import FakeClock
from hakit.content import discover
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

CONTENT = discover([BASE_CONTENT], SchemaSet.load())


def lab_with(runs: list[str]) -> FakeExecutor:
    def rule(c: Call) -> ProcResult | None:
        if c.argv[:3] == ["docker", "ps", "-a"] and "--format" in c.argv:
            return ProcResult(0, "".join(f"{r}\n" for r in runs), "")
        if c.argv[:1] == ["kubectl"] and "get" in c.argv and "pods" in c.argv:
            return ProcResult(0, "".join(f"{r}\n" for r in runs), "")
        if c.argv[:3] == ["docker", "ps", "-aq"]:
            return ProcResult(0, "c1\n", "")
        return None

    return FakeExecutor([rule])


def test_a_dead_runs_generators_go_and_a_live_runs_stay(tmp_path: Path) -> None:
    root = tmp_path / "results"
    (root / "dead-run").mkdir(parents=True)
    (root / "live-run").mkdir()
    (root / "live-run" / "active").write_text(f"{os.getpid()}\n")
    fake = lab_with(["dead-run", "live-run"])
    assert cleanup(CONTENT, lab_from_env({}), fake, FakeClock(), root, [], dry_run=False) == 0
    removed = [c.argv for c in fake.calls if c.argv[:3] == ["docker", "rm", "-f"]]
    deleted = [c.argv for c in fake.calls if c.argv[:1] == ["kubectl"] and "delete" in c.argv]
    assert removed == [["docker", "rm", "-f", "c1"]]
    assert len(deleted) == 1 and "hakit-run=dead-run" in deleted[0]
    assert not any("live-run" in " ".join(a) for a in removed + deleted)


def test_naming_a_run_dir_restricts_the_removal_to_it(tmp_path: Path) -> None:
    root = tmp_path / "results"
    for name in ("a-run", "b-run"):
        (root / name).mkdir(parents=True)
    fake = lab_with(["a-run", "b-run"])
    cleanup(CONTENT, lab_from_env({}), fake, FakeClock(), root, [root / "a-run"], dry_run=False)
    deleted = [" ".join(c.argv) for c in fake.calls if c.argv[:1] == ["kubectl"] and "delete" in c.argv]
    assert len(deleted) == 1 and "hakit-run=a-run" in deleted[0]
