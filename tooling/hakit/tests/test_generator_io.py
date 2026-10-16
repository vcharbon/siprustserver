"""A generator's IO never waits on the host's disk or swap (ADR-0049 §6.4): a
docker generator writes its records, stat file and logs into a working
directory on tmpfs (`HAKIT_GENERATOR_SCRATCH`), copied into the run's stream
directory once its containers are removed; its containers run with no swap;
a pod generator's shared directory is memory-backed; and cleanup copies out
what a crashed run left there."""

from __future__ import annotations

import json
import os
from pathlib import Path

from kit import budget, write_yaml

from hakit.cleanup import cleanup
from hakit.clock import FakeClock
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.jsonval import Json
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()
SCRATCH = "/dev/shm/hakit-test"


def streams() -> list[Json]:
    return [
        {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5,
         "setup_deadline_s": 10, "ring_s": 5, "hold_s": 20},
        {"name": "lg", "driver": "loadgen", "shape": "basic", "cps": 5, "setup_deadline_s": 10, "ring_s": 1,
         "hold_s": 2, "params": {"image": "product:dev", "command": "/bin/product"}},
        {"name": "pod", "driver": "loadgen", "placement": "pod", "target": "cell-0:5060", "shape": "basic",
         "cps": 4, "setup_deadline_s": 10, "ring_s": 1, "hold_s": 2,
         "params": {"image": "product:dev", "command": "/bin/product"}},
    ]


def content(tmp_path: Path) -> Content:
    root = tmp_path / "entry"
    budget(root, "any")
    doc: Json = {"schema": "profile.v1", "name": "io", "description": "d", "duration_s": 300, "drain_s": 130,
                 "settle_s": 15, "setup_slow_ms": 1000, "catalogs": ["x-api-call"], "streams": streams()}
    write_yaml(root / "profiles" / "io.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def run(tmp_path: Path, env: dict[str, str]) -> tuple[Path, list[Call]]:
    clock, fake = simulated([], shapes=["basic"])
    r = Runner(content(tmp_path), SCHEMAS, lab_from_env(env), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("io"), fake.calls


def docker_runs(calls: list[Call]) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    for c in calls:
        if c.kind == "run" and c.argv[:2] == ["docker", "run"] and "--name" in c.argv:
            out[c.argv[c.argv.index("--name") + 1]] = c.argv
    return out


def flag(argv: list[str], name: str) -> str:
    return argv[argv.index(name) + 1]


def mounts(argv: list[str]) -> list[str]:
    return [argv[i + 1] for i, a in enumerate(argv) if a == "-v"]


def test_every_generator_container_runs_with_no_swap(tmp_path: Path) -> None:
    _, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    runs = docker_runs(calls)
    assert {"hakit-basic", "hakit-basic-exporter", "hakit-lg"} <= set(runs)
    for name in ("hakit-basic", "hakit-basic-exporter", "hakit-lg"):
        argv = runs[name]
        assert flag(argv, "--memory-swap") == flag(argv, "--memory"), name


def test_every_generator_container_runs_as_the_runner_so_its_files_can_be_collected(tmp_path: Path) -> None:
    """A file a container writes as root in the scratch could not be removed
    once copied out."""
    _, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    runs = docker_runs(calls)
    for name in ("hakit-basic", "hakit-basic-exporter", "hakit-lg"):
        assert flag(runs[name], "--user") == f"{os.getuid()}:{os.getgid()}", name
    _, calls = run(tmp_path / "other", {"HAKIT_GENERATOR_SCRATCH": SCRATCH, "HAKIT_GENERATOR_USER": "123:45"})
    assert flag(docker_runs(calls)["hakit-lg"], "--user") == "123:45"


def test_docker_generators_write_into_the_scratch_and_never_into_the_run_dir(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    runs = docker_runs(calls)
    work = {s: f"{SCRATCH}/{run_dir.name}/streams/{s}" for s in ("basic", "lg")}
    for name, stream in (("hakit-basic", "basic"), ("hakit-basic-exporter", "basic"), ("hakit-lg", "lg")):
        stats = [m for m in mounts(runs[name]) if m.endswith(":/stats")]
        assert stats == [f"{work[stream]}:/stats"], name
    prepared = [c.argv for c in calls if c.kind == "run" and c.argv[:2] == ["mkdir", "-p"]]
    assert ["mkdir", "-p", work["basic"]] in prepared and ["mkdir", "-p", work["lg"]] in prepared
    inject = [c.argv for c in calls if c.kind == "run" and c.argv[:1] == ["cp"] and c.argv[-1] == work["basic"]]
    assert inject == [["cp", str(run_dir / "streams" / "basic" / "inject.csv"), work["basic"]]]


def test_a_work_dir_is_copied_into_its_stream_dir_after_its_containers_go(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    work = f"{SCRATCH}/{run_dir.name}/streams/basic"
    argvs = [c.argv for c in calls if c.kind == "run"]
    copy = ["cp", "-a", f"{work}/.", str(run_dir / "streams" / "basic")]
    drop = ["rm", "-r", "--", work]
    assert argvs.count(copy) == 1 and argvs.count(drop) == 1
    last_rm = max(i for i, a in enumerate(argvs) if a[:3] == ["docker", "rm", "-f"] and "hakit-basic" in a)
    assert last_rm < argvs.index(copy) < argvs.index(drop)
    # Once every work dir is collected, the run's own scratch directory goes.
    assert argvs.index(drop) < argvs.index(["rm", "-r", "--", f"{SCRATCH}/{run_dir.name}"])
    # A pod generator copies its files out itself: no work dir of its own.
    assert not any(a[:1] in (["cp"], ["rm"]) and any("/streams/pod" in x for x in a) for a in argvs)
    assert json.loads((run_dir / "verdict.json").read_text())["status"] in ("pass", "fail", "invalid")


def test_without_a_scratch_generators_write_into_the_stream_dir(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": ""})
    stats = [m for m in mounts(docker_runs(calls)["hakit-basic"]) if m.endswith(":/stats")]
    assert stats == [f"{run_dir / 'streams' / 'basic'}:/stats"]
    assert not any(c.kind == "run" and c.argv[:1] in (["cp"], ["rm"]) for c in calls)


def test_a_pod_generators_shared_directory_is_memory_backed(tmp_path: Path) -> None:
    run_dir, _ = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    pod = json.loads((run_dir / "streams" / "pod" / "pod.json").read_text())
    assert pod["spec"]["volumes"] == [{"name": "stats", "emptyDir": {"medium": "Memory", "sizeLimit": "512Mi"}}]


def test_cleanup_copies_out_what_a_crashed_run_left_in_the_scratch(tmp_path: Path) -> None:
    root = tmp_path / "results"
    (root / "dead-run").mkdir(parents=True)

    def rule(c: Call) -> ProcResult | None:
        if c.argv[:3] == ["docker", "ps", "-a"] and "--format" in c.argv:
            return ProcResult(0, "dead-run\n", "")
        if c.argv[:3] == ["docker", "ps", "-aq"]:
            return ProcResult(0, "c1\n", "")
        return None

    fake = FakeExecutor([rule])
    lab = lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    assert cleanup(discover([BASE_CONTENT], SCHEMAS), lab, fake, FakeClock(), root, [], dry_run=False) == 0
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    copy = ["cp", "-a", f"{SCRATCH}/dead-run/.", str(root / "dead-run")]
    assert copy in argvs and ["rm", "-r", "--", f"{SCRATCH}/dead-run"] in argvs
    assert argvs.index(copy) > max(i for i, a in enumerate(argvs) if a[:2] == ["docker", "rm"])
