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
from hakit.dryrun import read_jsonl, simulated
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
    assert not any(c.kind == "run" and (c.argv[:1] == ["cp"] or c.argv[:2] == ["rm", "-r"]) for c in calls)


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
        if c.argv == ["ls", "-1", SCRATCH]:
            return ProcResult(0, "dead-run\n", "")
        return None

    fake = FakeExecutor([rule])
    lab = lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    assert cleanup(discover([BASE_CONTENT], SCHEMAS), lab, fake, FakeClock(), root, [], dry_run=False) == 0
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    copy = ["cp", "-a", f"{SCRATCH}/dead-run/.", str(root / "dead-run")]
    assert copy in argvs and ["rm", "-r", "--", f"{SCRATCH}/dead-run"] in argvs
    assert argvs.index(copy) > max(i for i, a in enumerate(argvs) if a[:2] == ["docker", "rm"])


def test_a_work_dir_left_uncollected_is_an_integrity_finding(tmp_path: Path) -> None:
    """Its stat file never reaches the run directory, so its stalls cannot be read."""
    clock, fake = simulated([lambda c: ProcResult(1, "", "No space left") if c.argv[:2] == ["cp", "-a"] else None],
                            shapes=["basic"])
    r = Runner(content(tmp_path), SCHEMAS, lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH}), fake, clock,
               tmp_path / "results", say=lambda _m: None)
    run_dir = r.run("io")
    found = [i for i in read_jsonl(run_dir / "integrity.jsonl") if i.get("kind") == "generator_failure"]
    assert sorted(str(i["stream"]) for i in found) == ["basic", "lg"]
    assert all("stayed in the generator scratch" in str(i["detail"]) for i in found)
    # The scratch is kept for `hakit cleanup`.
    assert ["rm", "-r", "--", f"{SCRATCH}/{run_dir.name}"] not in [c.argv for c in fake.calls]


def test_cleanup_collects_a_scratch_whose_run_left_no_generator(tmp_path: Path) -> None:
    root = tmp_path / "results"
    (root / "gone-run").mkdir(parents=True)

    def rule(c: Call) -> ProcResult | None:
        if c.argv[:2] == ["ls", "-1"] and c.argv[2] == SCRATCH:
            return ProcResult(0, "gone-run\n", "")
        return None

    fake = FakeExecutor([rule])
    lab = lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    assert cleanup(discover([BASE_CONTENT], SCHEMAS), lab, fake, FakeClock(), root, [], dry_run=False) == 0
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    assert ["cp", "-a", f"{SCRATCH}/gone-run/.", str(root / "gone-run")] in argvs


def mib(v: str) -> int:
    return int(v.removesuffix("m").removesuffix("Mi")) << 20


# What the exporter twin's python and a SIPp generator's process may hold besides the files.
EXPORTER_RSS = 64 << 20
GENERATOR_RSS = 1024 << 20


def env_bytes(argv: list[str], name: str) -> int:
    return next(int(e.split("=", 1)[1]) for e in argv if e.startswith(f"{name}="))


def test_a_twins_kept_error_file_and_record_buffer_leave_its_exporter_room_in_its_memory(tmp_path: Path) -> None:
    """errors.kept.log is written by the twin, so its tmpfs pages are charged to
    the twin's memory, beside the records it holds on their way to the disk."""
    _, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    twin = docker_runs(calls)["hakit-basic-exporter"]
    held = env_bytes(twin, "SIPP_ERROR_KEPT_BYTES") + env_bytes(twin, "SIPP_RECORD_BUFFER_BYTES")
    assert held + EXPORTER_RSS <= mib(flag(twin, "--memory"))


def test_a_sipp_uacs_records_cross_a_fifo_to_the_stream_dir_and_never_fill_its_tmpfs(tmp_path: Path) -> None:
    """The memory charged to the UAC does not grow with its calls: it writes its
    records into a FIFO made in its working directory before either container
    starts, and the twin carries them into the stream directory on disk; the
    FIFO goes with the containers, before the working directory is collected."""
    run_dir, calls = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    runs = docker_runs(calls)
    uac, twin = runs["hakit-basic"], runs["hakit-basic-exporter"]
    work = f"{SCRATCH}/{run_dir.name}/streams/basic"
    keep = run_dir / "streams" / "basic"
    assert flag(uac, "-log_file") == "/stats/records.fifo"
    assert f"{keep}:/records" in mounts(twin)
    assert {"SIPP_RECORD_FIFO=/stats/records.fifo", "SIPP_RECORD_FILE=/records/records.jsonl"} <= set(twin)
    argvs = [c.argv for c in calls if c.kind == "run"]
    fifo = f"{work}/records.fifo"
    assert argvs.index(["mkfifo", fifo]) < argvs.index(twin) < argvs.index(uac)
    assert argvs.index(["rm", "-f", "--", fifo], argvs.index(uac)) < argvs.index(["cp", "-a", f"{work}/.", str(keep)])


def test_records_the_twins_buffer_dropped_are_a_record_gap_naming_their_stream(tmp_path: Path) -> None:
    """A disk slower than SIPp for longer than the twin's buffer holds loses
    records; the twin counts them and the run says so."""

    def full(c: Call) -> str | None:
        if c.kind == "read" and c.path is not None and c.path.name == "records.copy.json" and "basic" in str(c.path):
            return '{"complete": true, "lines": 0, "lost": 7}\n'
        return None

    clock, fake = simulated([full], shapes=["basic"])
    r = Runner(content(tmp_path), SCHEMAS, lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH}), fake, clock,
               tmp_path / "results", say=lambda _m: None)
    verdict = json.loads((r.run("io") / "verdict.json").read_text())
    lost = [i for i in verdict["integrity"] if "record buffer" in str(i["detail"])]
    assert [(i["kind"], i["stream"]) for i in lost] == [("record_gap", "basic")]
    assert str(lost[0]["detail"]).startswith("7 records")


def test_a_record_copy_not_complete_by_the_settle_deadline_is_a_record_gap_naming_its_stream(tmp_path: Path) -> None:
    """A twin that died or a disk that stalls past the run leaves records off the disk, uncounted."""

    def stuck(c: Call) -> str | None:
        if c.kind == "read" and c.path is not None and c.path.name == "records.copy.json" and "basic" in str(c.path):
            return '{"complete": false, "lines": 0, "lost": 0}\n'
        return None

    clock, fake = simulated([stuck], shapes=["basic"])
    r = Runner(content(tmp_path), SCHEMAS, lab_from_env({"HAKIT_GENERATOR_SCRATCH": SCRATCH}), fake, clock,
               tmp_path / "results", say=lambda _m: None)
    verdict = json.loads((r.run("io") / "verdict.json").read_text())
    stuck_ = [i for i in verdict["integrity"] if "did not complete" in str(i["detail"])]
    assert [(i["kind"], i["stream"]) for i in stuck_] == [("record_gap", "basic")]


def test_a_pod_generators_files_leave_its_process_room_in_its_memory(tmp_path: Path) -> None:
    """A memory-backed emptyDir's pages are charged to the container that writes them."""
    run_dir, _ = run(tmp_path, {"HAKIT_GENERATOR_SCRATCH": SCRATCH})
    pod = json.loads((run_dir / "streams" / "pod" / "pod.json").read_text())
    gen = next(c for c in pod["spec"]["containers"] if c["name"] == "gen")
    size = pod["spec"]["volumes"][0]["emptyDir"]["sizeLimit"]
    assert mib(size) + GENERATOR_RSS <= mib(gen["resources"]["limits"]["memory"])
