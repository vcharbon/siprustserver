"""Events off the schedule (ADR-0049 §3.9): `hakit fire` requests and detected events,
run on a fake clock against the simulated lab."""

from __future__ import annotations

import json
from pathlib import Path

from kit import budget, fault_incident, write_yaml

from hakit import jsonval as jv
from hakit.cli import manual_events
from hakit.clock import FakeClock
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, ProcResult, Rule
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.offschedule import active_runs, mark_active, request_fire
from hakit.paths import BASE_CONTENT
from hakit.preflight import open_events
from hakit.runner import Runner
from hakit.schemas import SchemaSet
from hakit.verdict import read_jsonl

SCHEMAS = SchemaSet.load()


def content_with_defaults(tmp_path: Path, schedule: Json | None) -> Content:
    root = tmp_path / "entry"
    fault_incident(root, "flaky", duration_s=5)
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "fire-smoke",
        "description": "d",
        "duration_s": 120,
        "drain_s": 120,
        "settle_s": 40,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": [
            {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 5, "setup_deadline_s": 32}
        ],
        "defaults": {"flaky": {"budget": "any", "params": {}}},
    }
    if schedule is not None:
        doc["schedule"] = schedule
    write_yaml(root / "profiles" / "fire-smoke.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def requests_on_first_wait(tmp_path: Path, names: list[str]) -> Rule:
    """A rule leaving `fire/<n>-<incident>.request` in the run directory at the
    generator's first liveness check, as `hakit fire` would."""
    done = {"n": False}

    def rule(c: Call) -> ProcResult | None:
        if not done["n"] and c.kind == "run" and c.argv[:2] == ["docker", "inspect"]:
            (run,) = (tmp_path / "results").iterdir()
            (run / "fire").mkdir()
            for i, name in enumerate(names):
                (run / "fire" / f"{i}-{name}.request").write_text(json.dumps({"incident": name}))
            done["n"] = True
        return None

    return rule


def answers(run_dir: Path) -> list[Json]:
    return [as_obj(json.loads(p.read_text())) for p in sorted((run_dir / "fire").glob("*.answer"))]


def run(content: Content, tmp_path: Path, rule: Rule) -> Path:
    clock, fake = simulated([rule])
    r = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("fire-smoke")


def test_a_fire_request_runs_a_manual_event_and_one_during_its_window_is_refused(tmp_path: Path) -> None:
    content = content_with_defaults(tmp_path, None)
    run_dir = run(content, tmp_path, requests_on_first_wait(tmp_path, ["flaky", "flaky", "kill_worker"]))
    assert answers(run_dir) == [
        {"accepted": True, "reason": "fired", "event": "m001-flaky"},
        {"accepted": False, "reason": "a window is open (m001-flaky)", "event": ""},
        {"accepted": False, "reason": "profile 'fire-smoke' has no defaults entry for 'kill_worker'", "event": ""},
    ]
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    (w,) = jv.objs(v, "windows")
    assert (w["event"], w["source"], w["budget"], w["status"]) == ("m001-flaky", "manual", "any", "ok")
    assert [p["phase"] for p in jv.objs(w, "phases")] == ["inject", "injected", "heal", "healed", "recovered"]
    assert not (run_dir / "active").exists()


def test_a_manual_window_overlapping_a_scheduled_one_makes_the_run_invalid(tmp_path: Path) -> None:
    content = content_with_defaults(tmp_path, {"at": [{"at_s": 30, "incident": "flaky", "budget": "any"}]})
    run_dir = run(content, tmp_path, requests_on_first_wait(tmp_path, ["flaky"]))
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert [w["event"] for w in jv.objs(v, "windows")] == ["m001-flaky", "001-flaky"]
    assert [(i["kind"], i.get("event")) for i in jv.objs(v, "integrity")] == [("window_overlap", "001-flaky")]
    assert v["status"] == "invalid"


def test_a_died_stream_opens_a_detected_window_that_allows_nothing(tmp_path: Path) -> None:
    content = content_with_defaults(tmp_path, None)
    seen = {"n": 0}

    def dies(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[:2] == ["docker", "inspect"] and "Running" in c.argv[3]:
            seen["n"] += 1
            return ProcResult(0, "false\n" if seen["n"] > 3 else "true\n", "")
        return None

    clock, fake = simulated([dies])
    r = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    run_dir = r.run("fire-smoke")
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    (w,) = jv.objs(v, "windows")
    pushed = [ln for c in fake.calls if c.kind == "http" and c.body for ln in c.body.decode().splitlines()]
    opened = int(jv.number(w, "opened_at_ms"))
    assert f'sip_chaos_window{{incident="stream_died",run="{run_dir.name}"}} 0 {opened + 40_000}' in pushed
    assert (w["event"], w["source"], w["budget"], w["incidents"]) == ("d001-stream_died", "detected", "", ["stream_died"])
    assert jv.number(w, "closed_at_ms") - jv.number(w, "opened_at_ms") == 40_000
    assert open_events(run_dir) == []
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["stream_died"]


def test_fire_waits_for_the_runner_s_answer_and_withdraws_its_request_without_one(tmp_path: Path) -> None:
    run_dir = tmp_path / "r"
    run_dir.mkdir()
    clock = FakeClock()
    got = request_fire(run_dir, "flaky", clock, 5, "n1")
    assert got == {"accepted": False, "reason": "the runner did not answer within 5 s", "event": ""}
    assert list((run_dir / "fire").iterdir()) == []
    (run_dir / "fire" / "n2-flaky.answer").write_text('{"accepted": true, "reason": "fired", "event": "m001-flaky"}')
    assert request_fire(run_dir, "flaky", clock, 5, "n2")["event"] == "m001-flaky"


def test_only_runs_whose_runner_lives_are_active(tmp_path: Path) -> None:
    live, dead = tmp_path / "a", tmp_path / "b"
    live.mkdir()
    dead.mkdir()
    mark_active(live)
    (dead / "active").write_text("999999999\n")
    assert active_runs(tmp_path) == [live]


def test_cleanup_plans_a_manual_event_from_the_profile_defaults(tmp_path: Path) -> None:
    content = content_with_defaults(tmp_path, None)
    profile = content.profiles["fire-smoke"]
    (ev,) = manual_events(profile, content, ["m002-flaky", "001-flaky", "m003-nope"])
    assert (ev.event_id, ev.spec.budget, ev.duration_s) == ("m002-flaky", "any", 5)


def test_events_jsonl_records_the_manual_source(tmp_path: Path) -> None:
    content = content_with_defaults(tmp_path, None)
    run_dir = run(content, tmp_path, requests_on_first_wait(tmp_path, ["flaky"]))
    (begin,) = [e for e in read_jsonl(run_dir / "events.jsonl") if e["kind"] == "event"]
    assert (begin["event"], begin["source"]) == ("m001-flaky", "manual")
