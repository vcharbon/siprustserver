"""Pods faulted one at a time (`one_at_a_time`): every event aimed at the
selector picks one pod, two such events are spaced past the first one's
recovery bound by `gap_s`, and before the load the selector has `min_ready`
ready pods."""

from __future__ import annotations

import json
from pathlib import Path

from kit import budget, fault_incident, write_yaml

from hakit.check import profile_errors
from hakit.content import Content, discover
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.jsonval import Json
from hakit.lab import lab_from_env
from hakit.one_at_a_time import normalised, ready_errors, ready_pods
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()
STREAM: Json = {
    "name": "basic",
    "driver": "sipp",
    "shape": "basic",
    "scenario": "uac-basic-hold.xml",
    "cps": 10,
    "setup_deadline_s": 32,
    "ring_s": 5,
    "hold_s": 20,
}
SPARE: Json = {"selector": "app=backend", "gap_s": 60}


def content(tmp_path: Path, schedule: list[Json], **profile: object) -> Content:
    fault_incident(tmp_path, "stop_backend", target={"selector": "app=backend", "pick": "all"}, expected_s=30)
    fault_incident(tmp_path, "stop_other", target={"selector": "app=other"}, expected_s=30)
    budget(tmp_path, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "p",
        "description": "d",
        "duration_s": 900,
        "drain_s": 120,
        "settle_s": 40,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": [STREAM],
        "schedule": {"at": schedule},
        **profile,
    }
    write_yaml(tmp_path / "profiles" / "p.yaml", doc)
    return discover([BASE_CONTENT, tmp_path], SCHEMAS)


def errors(c: Content) -> list[str]:
    return profile_errors(c.profiles["p"], c)


def one(at_s: int, **extra: object) -> Json:
    return {"at_s": at_s, "incident": "stop_backend", "budget": "any", "target": {"pick": "one"}, **extra}


def test_events_picking_one_pod_spaced_past_the_gap_pass(tmp_path: Path) -> None:
    other: Json = {"at_s": 170, "incident": "stop_other", "budget": "any"}
    c = content(tmp_path, [one(60), other, one(270)], one_at_a_time=[SPARE])
    assert errors(c) == []


def test_an_event_faulting_every_pod_of_the_selector_is_refused(tmp_path: Path) -> None:
    every: Json = {"at_s": 60, "incident": "stop_backend", "budget": "any"}
    c = content(tmp_path, [every], one_at_a_time=[SPARE])
    assert errors(c) == ["profile 'p': one_at_a_time 'app=backend': event 001-stop_backend fires stop_backend with pick 'all', not 'one'"]


def test_the_same_event_without_the_entry_is_not_judged(tmp_path: Path) -> None:
    every: Json = {"at_s": 60, "incident": "stop_backend", "budget": "any"}
    assert errors(content(tmp_path, [every])) == []


def test_a_second_event_inside_the_gap_is_refused(tmp_path: Path) -> None:
    c = content(tmp_path, [one(60), one(140)], one_at_a_time=[SPARE])
    assert errors(c) == [
        "profile 'p': one_at_a_time 'app=backend': event 002-stop_backend at 140s fires before 001-stop_backend's "
        "pod is back in use (60s + expected 30s + gap 60s = 150s)"
    ]


def test_an_event_retargeted_onto_the_selector_is_judged(tmp_path: Path) -> None:
    moved: Json = {"at_s": 100, "incident": "stop_other", "budget": "any", "target": {"selector": "app=backend", "pick": "all"}}
    c = content(tmp_path, [moved], one_at_a_time=[SPARE])
    assert errors(c) == ["profile 'p': one_at_a_time 'app=backend': event 001-stop_other fires stop_other with pick 'all', not 'one'"]


def test_a_single_ready_pod_leaves_none_to_serve(tmp_path: Path) -> None:
    c = content(tmp_path, [one(60)], one_at_a_time=[{**SPARE, "min_ready": 1}])
    assert errors(c) == ["profile 'p': one_at_a_time 'app=backend': min_ready 1 leaves no pod to serve while one is faulted"]


def pods(*ready: bool, deleting: int = -1) -> str:
    items: list[Json] = []
    for i, r in enumerate(ready):
        meta: Json = {"name": f"p{i}"}
        if i == deleting:
            meta["deletionTimestamp"] = "2026-01-01T00:00:00Z"
        items.append({"metadata": meta, "status": {"conditions": [{"type": "Ready", "status": "True" if r else "False"}]}})
    return json.dumps({"items": items})


def test_ready_pods_count_the_ready_ones_not_being_deleted() -> None:
    assert ready_pods(pods(True, True)) == 2
    assert ready_pods(pods(True, False)) == 1
    assert ready_pods(pods(True, True, deleting=0)) == 1
    assert ready_pods("not json") is None
    assert ready_pods('{"kind": "Pod"}') is None


def test_the_run_is_refused_under_min_ready(tmp_path: Path) -> None:
    c = content(tmp_path, [one(60)], one_at_a_time=[SPARE])
    answer = pods(True, False)

    def rule(call: Call) -> ProcResult | None:
        return ProcResult(0, answer, "") if call.argv[-5:-2] == ["pods", "-l", "app=backend"] else None

    fake = FakeExecutor([rule])
    got = ready_errors(c.profiles["p"], lab_from_env({}), fake)
    assert got == ["one_at_a_time 'app=backend': 1 pods ready, under min_ready 2"]
    answer = pods(True, True)
    assert ready_errors(c.profiles["p"], lab_from_env({}), fake) == []


def test_two_incidents_of_one_event_on_the_selector_are_refused(tmp_path: Path) -> None:
    both: Json = {
        "at_s": 60,
        "incidents": ["stop_backend", "stop_other"],
        "budget": "any",
        "target": {"stop_backend": {"pick": "one"}, "stop_other": {"selector": "app=backend", "pick": "one"}},
    }
    c = content(tmp_path, [both], one_at_a_time=[SPARE])
    assert errors(c) == [
        "profile 'p': one_at_a_time 'app=backend': event 001-stop_backend+stop_other fires stop_backend, stop_other together, each on its own pod"
    ]


def test_a_selector_is_compared_as_its_requirements(tmp_path: Path) -> None:
    every: Json = {"at_s": 60, "incident": "stop_other", "budget": "any", "target": {"selector": "tier=b, app=backend"}}
    c = content(tmp_path, [every], one_at_a_time=[{"selector": "app=backend,tier=b", "gap_s": 60}])
    assert errors(c) == []
    assert normalised("tier=b, app=backend") == normalised("app=backend,tier=b")
