"""Every profile of the search path replays in seconds on a fake clock and a simulated lab."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from kit import budget, executable, fault_incident, write_yaml

from hakit import jsonval as jv
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor, HttpResult, ProcResult, Rule
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner, RunRefused
from hakit.schemas import SchemaSet
from hakit.verdict import read_jsonl

SCHEMAS = SchemaSet.load()
LAB = lab_from_env({})
SMOKE = "kill-worker-smoke"


def runner(content: Content, tmp_path: Path, rules: list[Rule] | None = None) -> tuple[Runner, FakeExecutor]:
    clock, fake = simulated(rules)
    return Runner(content, SCHEMAS, LAB, fake, clock, tmp_path / "results", say=lambda _m: None), fake


def phases(run_dir: Path) -> list[tuple[str, str, int]]:
    return [
        (str(e["event"]), str(e["phase"]), int(str(e["at_ms"])))
        for e in read_jsonl(run_dir / "events.jsonl")
        if e.get("kind") == "phase"
    ]


def vm_lines(fake: FakeExecutor) -> list[str]:
    return [
        ln for c in fake.calls if c.kind == "http" and "import/prometheus" in c.url and c.body for ln in c.body.decode().splitlines()
    ]


@pytest.mark.parametrize("profile", sorted(discover([BASE_CONTENT], SCHEMAS).profiles))
def test_every_profile_on_the_search_path_runs_dry(profile: str, tmp_path: Path) -> None:
    content = discover([BASE_CONTENT], SCHEMAS)
    r, _ = runner(content, tmp_path)
    run_dir = r.run(profile)
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert SCHEMAS.errors("verdict.v1", v) == []
    assert v["status"] == "pass"


def test_the_smoke_profile_fires_its_kills_on_schedule_with_their_series(tmp_path: Path) -> None:
    content = discover([BASE_CONTENT], SCHEMAS)
    r, fake = runner(content, tmp_path)
    run_dir = r.run(SMOKE)
    run = run_dir.name
    assert run == "kill-worker-smoke-20270115T080000Z"
    got = phases(run_dir)
    assert [p[1] for p in got[:5]] == ["inject", "injected", "heal", "healed", "recovered"]
    starts = [at for _ev, ph, at in got if ph == "inject"]
    load_start = starts[0] - 60_000
    assert [s - load_start for s in starts] == [60_000, 210_000, 360_000, 510_000]
    lines = vm_lines(fake)
    for _ev, ph, at in got:
        assert f'sip_chaos_event{{incident="kill_worker",phase="{ph}",source="schedule",run="{run}"}} 1 {at}' in lines
    opens = [ln for ln in lines if ln.startswith("sip_chaos_window") and " 1 " in ln]
    closes = [ln for ln in lines if ln.startswith("sip_chaos_window") and " 0 " in ln]
    assert len(closes) == 4 and len(opens) >= 4 * 4  # an open and a keepalive every 10 s of a 40 s window
    docker = [c.argv for c in fake.calls if c.kind == "run" and c.argv[:1] == ["docker"]]
    twin = next(a for a in docker if a[:2] == ["docker", "run"] and "hakit-basic-exporter" in a)
    uac = next(a for a in docker if a[:2] == ["docker", "run"] and "hakit-basic" in a and "sipp" in a)
    assert docker.index(twin) < docker.index(uac)
    assert "container:hakit-basic-exporter" in uac and f"%u-%p-{run}@%s" in uac
    assert ["docker", "kill", "--signal", "USR1", "hakit-basic"] in docker
    inject_env = next(c.env for c in fake.calls if c.kind == "run" and c.argv[0].endswith("kill_worker/inject"))
    assert inject_env["HAKIT_TARGET_SELECTOR"] == "app=b2bua-worker"
    assert inject_env["HAKIT_EVENT"] == "001-kill_worker" and inject_env["HAKIT_RUN"] == run


def entry_with(tmp_path: Path, inject_body: str) -> Content:
    root = tmp_path / "entry"
    d = fault_incident(root, "flaky", duration_s=5)
    executable(d / "inject", inject_body)
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "flaky-smoke",
        "description": "d",
        "duration_s": 120,
        "drain_s": 60,
        "settle_s": 40,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": [
            {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5, "setup_deadline_s": 32}
        ],
        "schedule": {"at": [{"at_s": 30, "incident": "flaky", "budget": "any"}]},
    }
    write_yaml(root / "profiles" / "flaky-smoke.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def test_a_failed_inject_is_healed_and_invalidates_the_run(tmp_path: Path) -> None:
    content = entry_with(tmp_path, "echo boom >&2; exit 3")

    def fail_inject(c: Call) -> ProcResult | None:
        return ProcResult(3, "", "boom") if c.kind == "run" and c.argv[0].endswith("flaky/inject") else None

    r, fake = runner(content, tmp_path, [fail_inject])
    run_dir = r.run("flaky-smoke")
    assert [p[1] for p in phases(run_dir)] == ["inject", "failed", "heal", "healed"]
    assert any(c.kind == "run" and c.argv[0].endswith("flaky/heal") for c in fake.calls)
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "invalid"
    assert (run_dir / "events" / "001-flaky" / "flaky" / "inject.stderr").read_text() == "boom"


def test_facts_on_stdout_reach_the_verdict(tmp_path: Path) -> None:
    content = entry_with(tmp_path, "exit 0")

    def facts(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[0].endswith("flaky/inject"):
            return ProcResult(0, '{"pod":"w-0","killed_at_ms":1}\n', "")
        return None

    r, _ = runner(content, tmp_path, [facts])
    v = as_obj(json.loads((r.run("flaky-smoke") / "verdict.json").read_text()))
    (w,) = jv.objs(v, "windows")
    assert w["facts"] == {"flaky": {"inject": {"pod": "w-0", "killed_at_ms": 1}}}


def test_a_generator_that_exits_early_is_a_stream_died_finding(tmp_path: Path) -> None:
    content = entry_with(tmp_path, "exit 0")
    seen = {"n": 0}

    def dies(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[:2] == ["docker", "inspect"] and "Running" in c.argv[3]:
            seen["n"] += 1
            return ProcResult(0, "false\n" if seen["n"] > 3 else "true\n", "")
        return None

    r, fake = runner(content, tmp_path, [dies])
    run_dir = r.run("flaky-smoke")
    integrity = read_jsonl(run_dir / "integrity.jsonl")
    assert [i["kind"] for i in integrity] == ["stream_died"]
    assert any('incident="stream_died",phase="inject",source="detected"' in ln for ln in vm_lines(fake))


def test_an_interrupted_run_heals_its_open_event_and_removes_its_generators(tmp_path: Path) -> None:
    content = entry_with(tmp_path, "exit 0")

    class Stop(Exception):
        pass

    def interrupt(c: Call) -> ProcResult | HttpResult | None:
        if c.kind == "run" and c.argv[0].endswith("flaky/inject"):
            raise Stop()
        return None

    r, fake = runner(content, tmp_path, [interrupt])
    with pytest.raises(Stop):
        r.run("flaky-smoke")
    runs = [c.argv for c in fake.calls if c.kind == "run"]
    assert any(a[0].endswith("flaky/heal") for a in runs)
    assert runs[-1][:3] == ["docker", "rm", "-f"]


def test_a_system_under_test_holding_calls_refuses_the_run(tmp_path: Path) -> None:
    content = discover([BASE_CONTENT], SCHEMAS)

    def holding(c: Call) -> HttpResult | None:
        if c.kind == "http" and "/api/v1/query?" in c.url:
            body = '{"status":"success","data":{"resultType":"vector","result":[{"metric":{},"value":[1,"7"]}]}}'
            return HttpResult(200, body)
        return None

    r, _ = runner(content, tmp_path, [holding])
    with pytest.raises(RunRefused, match="leftover_calls: the system under test holds 7 calls"):
        r.run(SMOKE)
    assert not (tmp_path / "results").exists()
