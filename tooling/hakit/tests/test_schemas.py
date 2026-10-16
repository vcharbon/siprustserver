"""The shipped schemas: valid 2020-12 schemas, and the contract cases they accept or refuse."""

from __future__ import annotations

import pytest

from hakit.jsonval import Json
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()

BASE: Json = {
    "schema": "call-outcome.v1",
    "run": "r1",
    "source": "sipp",
    "stream": "short",
    "shape": "basic",
    "call_id": "c1",
    "started_at_ms": 1.0,
    "ended_at_ms": 9.0,
}
STREAM: Json = {"name": "short", "driver": "sipp", "shape": "basic", "scenario": "uac.xml", "cps": 50, "setup_deadline_s": 40}
PROFILE: Json = {
    "schema": "profile.v1",
    "name": "p",
    "description": "d",
    "duration_s": 3600,
    "setup_slow_ms": 500,
    "streams": [STREAM],
    "schedule": {"first_at_s": 180, "interval_s": 180, "cycle": [{"incident": "kill_worker", "budget": "worker-crash"}]},
}
CATALOG: Json = {
    "schema": "treatment-catalog.v1",
    "name": "x-api-call",
    "placeholders": {"callee_host": "h", "callee_port": "p"},
    "shapes": [
        {
            "shape": "basic",
            "description": "d",
            "load_safe": True,
            "request": {
                "headers": [
                    {"name": "X-Api-Call", "value": '{"destination":{"host":"${callee_host}","port":${callee_port}}}'}
                ]
            },
            "expected": [{"ending": "answered"}],
        }
    ],
}

CASES: list[tuple[str, Json, bool, str]] = [
    (
        "call-outcome.v1",
        {
            **BASE,
            "phase_reached": "released",
            "ending": "answered",
            "answered_at_ms": 3.0,
            "first_provisional_at_ms": 2.0,
            "setup_ended_at_ms": 3.0,
            "final_code": 200,
            "reason": None,
        },
        True,
        "answered",
    ),
    (
        "call-outcome.v1",
        {**BASE, "phase_reached": "provisional", "ending": "rejected", "setup_ended_at_ms": 3.0, "final_code": 486, "reason": "486"},
        True,
        "rejected 486 no Reason",
    ),
    (
        "call-outcome.v1",
        {
            **BASE,
            "phase_reached": "provisional",
            "ending": "rejected",
            "setup_ended_at_ms": 3.0,
            "final_code": 503,
            "reason": "q.850;cause=34",
        },
        True,
        "rejected 503 q.850",
    ),
    (
        "call-outcome.v1",
        {
            **BASE,
            "phase_reached": "provisional",
            "ending": "rejected",
            "setup_ended_at_ms": 3.0,
            "final_code": 503,
            "reason": "Q.850;cause=34",
        },
        False,
        "a reason protocol not lowercased",
    ),
    (
        "call-outcome.v1",
        {**BASE, "phase_reached": "provisional", "ending": "timeout", "setup_ended_at_ms": 3.0, "final_code": None, "reason": "timeout"},
        True,
        "timeout",
    ),
    (
        "call-outcome.v1",
        {**BASE, "phase_reached": "provisional", "ending": "timeout", "setup_ended_at_ms": 3.0, "final_code": 408, "reason": "timeout"},
        False,
        "timeout with a final",
    ),
    (
        "call-outcome.v1",
        {**BASE, "phase_reached": "provisional", "ending": "rejected", "setup_ended_at_ms": 3.0, "final_code": 486, "reason": "Busy Here"},
        False,
        "reason phrase",
    ),
    (
        "call-outcome.v1",
        {
            **BASE,
            "phase_reached": "invited",
            "ending": "failed",
            "final_code": None,
            "reason": "transport",
            "failures": [{"at_ms": 2.0, "kind": "transport"}],
        },
        True,
        "failed pre-answer",
    ),
    ("budget.v1", {"schema": "budget.v1", "name": "b", "description": "d", "call_cut": {"max_calls": 1}}, False, "call_cut budget"),
    (
        "budget.v1",
        {"schema": "budget.v1", "name": "b", "description": "d", "classes": {"call_cut": {"max": 1}}},
        False,
        "call_cut via classes",
    ),
    (
        "budget.v1",
        {
            "schema": "budget.v1",
            "name": "b",
            "description": "d",
            "unexpected_ending": [{"stream": "*", "code": 200, "reason": "*", "max_calls": 1}],
        },
        False,
        "unexpected 200",
    ),
    (
        "budget.v1",
        {
            "schema": "budget.v1",
            "name": "b",
            "description": "d",
            "setup_slow": {"threshold_ms": 900},
            "recovery": {"tolerance": 0.2},
            "setup_failed": [{"stream": "*", "code": 0, "reason": "timeout", "max_seconds": 2}],
        },
        True,
        "kill budget",
    ),
    ("incident.v1", {"schema": "incident.v1", "name": "spike", "description": "d", "kind": "load"}, True, "load"),
    (
        "incident.v1",
        {"schema": "incident.v1", "name": "spike", "description": "d", "kind": "load", "target": {"selector": "a=b"}},
        False,
        "load with target",
    ),
    (
        "incident.v1",
        {
            "schema": "incident.v1",
            "name": "k",
            "description": "d",
            "kind": "fault",
            "target": {"selector": "a=b"},
            "params": {"grace_s": {"type": "integer", "default": "x", "description": "d"}},
        },
        False,
        "param default type",
    ),
    (
        "cdr-observation.v1",
        {
            "schema": "cdr-observation.v1",
            "call_id": "c",
            "start_ms": 1,
            "answer_ms": None,
            "end_ms": 2,
            "duration_ms": 0,
            "duration_unit_ms": 1000,
            "disposition": "not_answered",
        },
        True,
        "cdr",
    ),
    ("profile.v1", PROFILE, True, "3-min kill cycle"),
    (
        "profile.v1",
        {**PROFILE, "streams": [{k: v for k, v in STREAM.items() if k != "scenario"}]},
        False,
        "sipp without scenario",
    ),
    (
        "profile.v1",
        {**PROFILE, "schedule": {"first_at_s": 1, "interval_s": 2, "cycle": [{"incident": "k", "budget": "b", "at_s": 5}]}},
        False,
        "cycle with at_s",
    ),
    ("treatment-catalog.v1", CATALOG, True, "x-api-call"),
]


def test_every_schema_is_a_valid_2020_12_schema() -> None:
    assert sorted(SCHEMAS.by_name) == [
        "budget.v1",
        "call-outcome.v1",
        "cdr-observation.v1",
        "impact-class.v1",
        "incident.v1",
        "profile.v1",
        "treatment-catalog.v1",
        "verdict.v1",
    ]
    assert SCHEMAS.check_metaschema() == []


@pytest.mark.parametrize(("schema", "instance", "valid", "label"), CASES, ids=[c[3] for c in CASES])
def test_the_contract_cases(schema: str, instance: Json, valid: bool, label: str) -> None:
    errors = SCHEMAS.errors(schema, instance)
    assert (errors == []) is valid, f"{label}: {errors}"


def test_a_major_the_runner_does_not_ship_is_refused() -> None:
    doc = {**PROFILE, "schema": "profile.v2"}
    assert SCHEMAS.errors_for_document("profile", doc) == ["schema: profile.v2 is not a major this runner ships"]
    assert SCHEMAS.errors_for_document("budget", PROFILE) == ["schema: expected budget.v<major>, got 'profile.v1'"]
