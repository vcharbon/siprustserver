"""Windows close on recovery; recovery is budgeted per stream (ADR-0049 §3.5, §3.6)."""

from __future__ import annotations

from pathlib import Path

from kit import T0, paced
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.verdict import compute

INJECT = T0 + 100_000
HEALED = INJECT + 110


def budget(max_s: float | None) -> Json:
    rec: Json = {} if max_s is None else {"max_s": max_s}
    return {
        "schema": "budget.v1",
        "name": "kill-rec",
        "description": "d",
        "setup_failed": [{"stream": "*", "code": 503, "reason": "*", "max_calls": 1000}],
        "recovery": rec,
    }


def refused(prefix: str, from_ms: float, to_ms: float) -> list[Json]:
    return paced(prefix, from_ms, to_ms, 2, ending="rejected", answered=None, code=503, reason="503", ended=200)


def verdict(tmp_path: Path, records: list[Json], events: list[Json], max_s: float | None) -> Json:
    d = run_dir(tmp_path, records, calls=len(records), events=events, budgets=[budget(max_s)], cps=2, profile={})
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    return v


def recovery_rows(w: Json) -> list[tuple[object, ...]]:
    return [(r["stream"], r["count"], r["allowed"], r["finding"]) for r in jv.objs(w, "impacts") if r["class"] == "recovery"]


def test_a_window_closes_settle_after_the_stream_is_back_and_charges_its_recovery(tmp_path: Path) -> None:
    recs = paced("a", T0, INJECT, 2) + refused("r", INJECT, T0 + 130_000) + paced("b", T0 + 130_000, T0 + 300_000, 2)
    events = kill(1, INJECT, budget="kill-rec")
    v = verdict(tmp_path, recs, events, 20)
    (w,) = jv.objs(v, "windows")
    # Back at the 1 s step whose 10 s span holds one refusal of 20: healed + 29 s.
    assert w["recovery"] == [{"stream": "basic", "recovery_s": 29.0}]
    assert recovery_rows(w) == [("basic", 29.0, 20, True)]
    assert w["closed_at_ms"] == HEALED + 29_000 + 40_000
    assert v["status"] == "fail" and v["integrity"] == []
    (tmp_path / "run").rename(tmp_path / "strict")
    v = verdict(tmp_path, recs, events, 40)
    (w,) = jv.objs(v, "windows")
    assert recovery_rows(w) == [("basic", 29.0, 40, False)] and v["status"] == "pass"


def test_recovery_equal_to_its_allowance_passes_and_later_fails(tmp_path: Path) -> None:
    recs = paced("a", T0, INJECT, 2) + refused("r", INJECT, T0 + 130_000) + paced("b", T0 + 130_000, T0 + 300_000, 2)
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec"), 29)
    assert recovery_rows(jv.objs(v, "windows")[0]) == [("basic", 29.0, 29, False)]
    (tmp_path / "run").rename(tmp_path / "first")
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec"), 28.9)
    assert recovery_rows(jv.objs(v, "windows")[0]) == [("basic", 29.0, 28.9, True)]


def test_a_call_cut_after_its_setup_is_a_successful_setup_for_recovery(tmp_path: Path) -> None:
    bye = {"at_ms": 0, "kind": "dialog_request", "method": "BYE", "code": 481}
    cut = [{**r, "ending": "cut", "failures": [{**bye, "at_ms": r["ended_at_ms"]}]} for r in paced("c", INJECT, T0 + 200_000, 2)]
    v = verdict(tmp_path, paced("a", T0, INJECT, 2) + cut, kill(1, INJECT, budget="kill-rec"), None)
    (w,) = jv.objs(v, "windows")
    assert w["recovery"] == [{"stream": "basic", "recovery_s": 0.0}]


def test_a_stream_that_starts_with_an_incident_has_no_recovery(tmp_path: Path) -> None:
    burst = {"name": "burst", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 2}
    basic = {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 2}
    streams = [{**basic, "setup_deadline_s": 32}, {**burst, "setup_deadline_s": 32, "start": "incident"}]
    # The burst ran in an earlier window: its calls before inject are no baseline.
    recs = paced("a", T0, T0 + 300_000, 2) + paced("x", T0 + 50_000, INJECT, 2, stream="burst")
    recs += [{**r, "stream": "burst"} for r in refused("y", INJECT, T0 + 200_000)]
    d = run_dir(tmp_path, recs, calls=None, events=kill(1, INJECT, budget="kill-rec"), budgets=[budget(None)], cps=2, profile={"streams": streams})
    (w,) = jv.objs(compute(d, SCHEMAS), "windows")
    assert w["recovery"] == [{"stream": "basic", "recovery_s": 0.0}]


def test_the_probe_close_stands_when_traffic_was_back_before_it(tmp_path: Path) -> None:
    recs = paced("a", T0, T0 + 300_000, 2)
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec", recovered_after=30_000), None)
    (w,) = jv.objs(v, "windows")
    assert w["recovery"] == [{"stream": "basic", "recovery_s": 0.0}]
    assert w["closed_at_ms"] == INJECT + 30_000 + 40_000


def test_a_stream_never_back_closes_the_window_at_its_maximum_as_a_finding(tmp_path: Path) -> None:
    recs = paced("a", T0, INJECT, 2) + refused("r", INJECT, T0 + 300_000)
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec"), None)
    (w,) = jv.objs(v, "windows")
    limit = INJECT + 220_000 + 40_000
    assert w["recovery"] == [{"stream": "basic", "recovery_s": None}]
    observed = (limit - HEALED) / 1000
    assert recovery_rows(w) == [("basic", observed, observed, True)]
    assert w["closed_at_ms"] == limit and v["status"] == "fail"


def test_a_stream_never_back_before_the_next_window_closes_at_its_opening(tmp_path: Path) -> None:
    recs = paced("a", T0, INJECT, 2) + refused("r", INJECT, T0 + 300_000)
    second = T0 + 200_000
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec") + kill(2, second, budget="kill-rec"), None)
    w1, w2 = jv.objs(v, "windows")
    assert w1["closed_at_ms"] == second and w1["recovery"] == [{"stream": "basic", "recovery_s": None}]
    # Every call before the second inject started inside the first window: the
    # second window has no baseline, no recovery row, and says why.
    assert "recovery" not in w2 and recovery_rows(w2) == []
    assert w2["facts"] == {
        "hakit": {"recovery": {"basic": "no recovery: 0 calls started in the 60 s before inject outside earlier windows, under 20"}}
    }
    assert [i["kind"] for i in jv.objs(v, "integrity")] == []


def test_a_window_without_a_baseline_has_no_recovery(tmp_path: Path) -> None:
    recs = paced("a", INJECT + 1_000, T0 + 300_000, 2)
    v = verdict(tmp_path, recs, kill(1, INJECT, budget="kill-rec"), None)
    (w,) = jv.objs(v, "windows")
    assert "recovery" not in w and recovery_rows(w) == []
    assert w["closed_at_ms"] == INJECT + 5_000 + 40_000
