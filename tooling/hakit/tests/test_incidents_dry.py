"""Every base incident fired once by the runner on a fake clock and the
simulated lab: its phases in order, its executables called with the contract's
environment, and a load incident's stream started at inject and stopped at
heal the way its `stop` says."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from kit import budget, write_yaml

from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor
from hakit.incident_exec import fmt_param
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT, FAULT_LIB
from hakit.runner import Runner
from hakit.schemas import SchemaSet
from hakit.verdict import read_jsonl

SCHEMAS = SchemaSet.load()
BASE = discover([BASE_CONTENT], SCHEMAS)
PHASES = ["inject", "injected", "heal", "healed", "recovered"]
AT_S = 30


def stream(name: str, start: str = "run", hold_s: float = 20) -> Json:
    return {
        "name": name,
        "driver": "sipp",
        "start": start,
        "shape": "basic",
        "scenario": "uac-basic-hold.xml",
        "cps": 5,
        "setup_deadline_s": 10,
        "ring_s": 5,
        "hold_s": hold_s,
    }


def one_event(tmp_path: Path, name: str) -> Content:
    root = tmp_path / "entry"
    budget(root, "any")
    event: Json = {"at_s": AT_S, "incident": name, "budget": "any"}
    streams = [stream("basic")]
    if BASE.incidents[name].kind == "load":
        streams.append(stream("extra", "incident", hold_s=2))
        event["stream"] = "extra"
    doc: Json = {
        "schema": "profile.v1",
        "name": "one",
        "description": "d",
        "duration_s": 60,
        "drain_s": 130,
        "settle_s": 15,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": streams,
        "schedule": {"at": [event]},
    }
    write_yaml(root / "profiles" / "one.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def fire(tmp_path: Path, name: str) -> tuple[Path, FakeExecutor]:
    clock, fake = simulated()
    r = Runner(one_event(tmp_path, name), SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("one"), fake


def phases(run_dir: Path) -> list[tuple[str, int]]:
    return [(str(e["phase"]), int(str(e["at_ms"]))) for e in read_jsonl(run_dir / "events.jsonl") if e.get("kind") == "phase"]


def runs_of(fake: FakeExecutor, name: str, step: str) -> list[Call]:
    return [c for c in fake.calls if c.kind == "run" and c.argv[0] == str(BASE.incidents[name].directory / step)]


@pytest.mark.parametrize("name", sorted(n for n, i in BASE.incidents.items() if i.kind == "fault"))
def test_a_fault_fires_through_its_executables(name: str, tmp_path: Path) -> None:
    inc = BASE.incidents[name]
    run_dir, fake = fire(tmp_path, name)
    got = phases(run_dir)
    assert [p for p, _ in got] == PHASES
    at = dict(got)
    assert at["heal"] - at["injected"] == int(inc.duration_s * 1000)
    (inject,) = runs_of(fake, name, "inject")
    (heal,) = runs_of(fake, name, "heal")
    assert len(runs_of(fake, name, "probe")) == (1 if inc.executable("probe") else 0)
    assert fake.calls.index(inject) < fake.calls.index(heal)
    assert inc.target is not None
    for c in (inject, heal):
        assert c.env["HAKIT_TARGET_SELECTOR"] == inc.target.selector
        assert c.env["HAKIT_TARGET_PICK"] == inc.target.pick
        assert c.env["HAKIT_LIB"] == str(FAULT_LIB) and (FAULT_LIB / "pod-faults.sh").is_file()
        assert c.env["HAKIT_EVENT"] == f"001-{name}" and c.env["HAKIT_INCIDENT"] == name
        assert c.env["HAKIT_DURATION_S"] == fmt_param(inc.duration_s)
        for p, decl in inc.params.items():
            assert decl.default is not None, f"{name}.{p} has no default: the base profiles set every parameter"
            assert c.env[f"HAKIT_PARAM_{p.upper()}"] == fmt_param(decl.default)
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "pass"


@pytest.mark.parametrize("name", sorted(n for n, i in BASE.incidents.items() if i.kind == "load"))
def test_a_load_incident_starts_its_stream_at_inject_and_stops_it_at_heal(name: str, tmp_path: Path) -> None:
    inc = BASE.incidents[name]
    run_dir, fake = fire(tmp_path, name)
    assert [p for p, _ in phases(run_dir)] == PHASES
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    started = next(i for i, a in enumerate(argvs) if a[:2] == ["docker", "run"] and "hakit-extra" in a)
    soft = ["docker", "kill", "--signal", "USR1", "hakit-extra"]
    hard = ["docker", "kill", "hakit-extra"]
    assert (soft in argvs, hard in argvs) == ((True, False) if inc.stop == "drain" else (False, True))
    stopped = argvs.index(soft if inc.stop == "drain" else hard)
    removed = max(i for i, a in enumerate(argvs) if a[:3] == ["docker", "rm", "-f"] and "hakit-extra" in a)
    assert started < stopped < removed
    assert f"%u-%p-{run_dir.name}-001-{name}@%s" in argvs[started]
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert docs["extra"]["reconciled"] is (inc.stop == "drain") and docs["extra"]["firings"] == 1
    facts = [e for e in read_jsonl(run_dir / "events.jsonl") if e.get("kind") == "facts" and e.get("executable") == "heal"]
    assert as_obj(facts[0]["facts"])["stop"] == inc.stop
