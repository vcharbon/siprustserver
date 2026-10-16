"""Profile checks the schema cannot express: references, parameters, timing bounds, overlap."""

from __future__ import annotations

from pathlib import Path

from kit import budget, fault_incident, write_yaml

from hakit.check import profile_errors
from hakit.content import Content, discover
from hakit.jsonval import Json
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()
STREAM: Json = {
    "name": "basic",
    "driver": "sipp",
    "shape": "basic",
    "scenario": "uac-call.xml",
    "cps": 10,
    "setup_deadline_s": 32,
    "ring_s": 5,
    "hold_s": 20,
}


def entry(tmp_path: Path, **profile: object) -> Content:
    fault_incident(
        tmp_path,
        "freeze",
        params={"mode": {"type": "string", "description": "d"}, "grace_s": {"type": "integer", "default": 0, "description": "d"}},
    )
    budget(tmp_path, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "p",
        "description": "d",
        "duration_s": 600,
        "drain_s": 120,
        "settle_s": 40,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": [STREAM],
        **profile,
    }
    write_yaml(tmp_path / "profiles" / "p.yaml", doc)
    return discover([BASE_CONTENT, tmp_path], SCHEMAS)


def errors(content: Content) -> list[str]:
    return profile_errors(content.profiles["p"], content)


def test_references_resolve(tmp_path: Path) -> None:
    c = entry(tmp_path, schedule={"at": [{"at_s": 10, "incident": "nope", "budget": "absent"}]}, catalogs=["other"])
    got = errors(c)
    assert "profile 'p': catalog 'other' is not on the search path" in got
    assert "profile 'p' event 1 (nope): incident 'nope' is not on the search path" in got
    assert "profile 'p' event 1 (nope): budget 'absent' is not on the search path" in got
    assert "profile 'p': stream 'basic': shape 'basic' is in none of the profile's catalogs" in got


def test_parameters_are_set_and_typed(tmp_path: Path) -> None:
    c = entry(tmp_path, schedule={"at": [{"at_s": 10, "incident": "freeze", "budget": "any", "params": {"grace_s": "1", "x": 1}}]})
    assert sorted(errors(c)) == [
        "profile 'p' event 1 (freeze): incident 'freeze' has no parameter 'x'",
        "profile 'p' event 1 (freeze): parameter 'grace_s' of 'freeze' must be of type integer",
        "profile 'p' event 1 (freeze): parameter 'mode' of 'freeze' has no default and is not set",
    ]


def test_settle_and_drain_cover_the_streams(tmp_path: Path) -> None:
    c = entry(tmp_path, settle_s=10, drain_s=20, schedule={"at": [{"at_s": 10, "incident": "freeze", "budget": "any", "params": {"mode": "m"}}]})
    assert errors(c) == [
        "profile 'p': drain_s 20 is below the longest stream teardown 96",
        "profile 'p' event 1 (freeze): settle_s 10 is below the longest setup_deadline_s 32 plus the 1 s deadline tick",
        "profile 'p': settle_s 10 is below the longest setup_deadline_s 32 plus the 1 s deadline tick",
    ]


def test_settle_covers_the_setup_deadline_plus_the_deadline_tick(tmp_path: Path) -> None:
    sched = {"at": [{"at_s": 10, "incident": "freeze", "budget": "any", "params": {"mode": "m"}}]}
    assert errors(entry(tmp_path / "a", settle_s=32, schedule=sched)) == [
        "profile 'p' event 1 (freeze): settle_s 32 is below the longest setup_deadline_s 32 plus the 1 s deadline tick",
        "profile 'p': settle_s 32 is below the longest setup_deadline_s 32 plus the 1 s deadline tick",
    ]
    assert errors(entry(tmp_path / "b", settle_s=33, schedule=sched)) == []


def test_a_recovery_span_must_hold_enough_calls_of_each_stream(tmp_path: Path) -> None:
    slow = {**STREAM, "cps": 1.9}
    sched = {"at": [{"at_s": 10, "incident": "freeze", "budget": "any", "params": {"mode": "m"}}]}
    assert errors(entry(tmp_path, streams=[slow], schedule=sched)) == [
        "profile 'p' event 1 (freeze): budget 'any' recovery hold_s 10 holds 19 calls of stream 'basic', under 20",
    ]


def test_overlapping_windows_are_refused(tmp_path: Path) -> None:
    cycle = [{"incident": "freeze", "budget": "any", "params": {"mode": "m"}}]
    c = entry(tmp_path, schedule={"first_at_s": 60, "interval_s": 120, "cycle": cycle})
    assert errors(c) == []
    c = entry(tmp_path, schedule={"first_at_s": 60, "interval_s": 90, "cycle": cycle})
    assert errors(c) == [
        "profile 'p': event 002-freeze at 150s opens before 001-freeze's window closes "
        "(60s + expected 60s + settle 40s = 160s)",
        "profile 'p': event 003-freeze at 240s opens before 002-freeze's window closes "
        "(150s + expected 60s + settle 40s = 250s)",
        "profile 'p': event 004-freeze at 330s opens before 003-freeze's window closes "
        "(240s + expected 60s + settle 40s = 340s)",
        "profile 'p': event 005-freeze at 420s opens before 004-freeze's window closes "
        "(330s + expected 60s + settle 40s = 430s)",
        "profile 'p': event 006-freeze at 510s opens before 005-freeze's window closes "
        "(420s + expected 60s + settle 40s = 520s)",
    ]


def test_a_key_the_scenario_reads_and_no_param_sets_is_refused(tmp_path: Path) -> None:
    (tmp_path / "scenarios").mkdir()
    (tmp_path / "scenarios" / "uac-k.xml").write_text(
        '<scenario name="k"><send><![CDATA[INVITE sip:[field0]@[remote_ip] SIP/2.0\nX-A: [ring_ms] [hold_ms]\n]]></send></scenario>'
    )
    c = entry(tmp_path, streams=[{**STREAM, "scenario": "uac-k.xml"}])
    assert errors(c) == ["profile 'p': stream 'basic': scenario 'uac-k.xml' reads keys no param sets: ['ring_ms']"]
    c = entry(tmp_path, streams=[{**STREAM, "scenario": "uac-k.xml", "params": {"ring_ms": 1000}}])
    assert errors(c) == []


def test_the_drain_covers_each_re_invite_and_a_flood(tmp_path: Path) -> None:
    """Each re-INVITE of the base UAC adds its 64*T1 bound, a flood its count
    times its gap: the longest teardown is 32 + (20 + 2*32) + 32 s, 32 + 100 + 32 s."""
    c = entry(tmp_path, streams=[{**STREAM, "params": {"reinvites": 2}}])
    assert errors(c) == ["profile 'p': drain_s 120 is below the longest stream teardown 148"]
    c = entry(tmp_path, streams=[{**STREAM, "params": {"abuse": "options_flood", "flood_count": 500}}])
    assert errors(c) == ["profile 'p': drain_s 120 is below the longest stream teardown 164"]
