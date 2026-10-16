"""A failed heal is retried once; a heal that fails twice stops the schedule
and drains the load there, and the run is invalid: no later window is judged
on a system under test that may still be faulted."""

from __future__ import annotations

import json
from pathlib import Path

from kit import budget, fault_incident, write_yaml

from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, ProcResult
from hakit.jsonl import read_jsonl
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def two_events(tmp_path: Path) -> Content:
    root = tmp_path / "entry"
    fault_incident(root, "sticky", duration_s=5)
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1", "name": "sticky-smoke", "description": "d", "duration_s": 600, "drain_s": 130,
        "settle_s": 15, "setup_slow_ms": 1000, "catalogs": ["x-api-call"],
        "streams": [{"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5,
                     "setup_deadline_s": 10, "ring_s": 5, "hold_s": 20}],
        "schedule": {"at": [{"at_s": 30, "incident": "sticky", "budget": "any"},
                            {"at_s": 300, "incident": "sticky", "budget": "any"}]},
    }
    write_yaml(root / "profiles" / "sticky-smoke.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def run(tmp_path: Path, heal_failures: int) -> tuple[Path, list[Call]]:
    left = {"n": heal_failures}

    def heal(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[0].endswith("sticky/heal") and left["n"] > 0:
            left["n"] -= 1
            return ProcResult(1, "", "still faulted")
        return None

    clock, fake = simulated([heal])
    r = Runner(two_events(tmp_path), SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("sticky-smoke"), fake.calls


def phases(run_dir: Path, event: str) -> list[str]:
    return [str(e["phase"]) for e in read_jsonl(run_dir / "events.jsonl") if e.get("kind") == "phase" and e.get("event") == event]


def test_a_heal_that_succeeds_on_its_retry_leaves_the_run_whole(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, 1)
    assert phases(run_dir, "001-sticky") == ["inject", "injected", "heal", "healed", "recovered"]
    assert len([c for c in calls if c.kind == "run" and c.argv[0].endswith("sticky/heal")]) == 3  # 2 for 001, 1 for 002
    assert phases(run_dir, "002-sticky")[-1] == "recovered"
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "pass"
    retry = as_obj(as_obj(as_obj(as_obj(v["windows"][0])["facts"])["sticky"])["hakit"])  # type: ignore[index]
    assert "heal_retry" in retry


def test_a_heal_that_fails_twice_stops_the_schedule_and_drains_the_load(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, 2)
    assert phases(run_dir, "001-sticky") == ["inject", "injected", "heal", "failed"]
    assert phases(run_dir, "002-sticky") == []
    meta = as_obj(json.loads((run_dir / "run-meta.json").read_text()))
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "invalid"
    kinds = {(str(as_obj(i)["kind"]), str(as_obj(i).get("detail", ""))) for i in v["integrity"]}  # type: ignore[union-attr]
    assert ("incident_failed", "its heal failed twice: the schedule stopped and the load drained there") in kinds
    usr1 = next(i for i, c in enumerate(calls) if c.kind == "run" and c.argv[:4] == ["docker", "kill", "--signal", "USR1"])
    assert usr1 > 0 and int(str(meta["ended_at_ms"])) - int(str(meta["load_started_at_ms"])) < 600_000  # before duration_s
