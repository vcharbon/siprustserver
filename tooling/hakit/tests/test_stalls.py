"""Generator health (ADR-0049 §3.12): a stall is read off the call starts, and what it
measured is an integrity finding, never an impact of the system under test."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path

from kit import T0, paced, record, timed_out
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.stalls import Stall, detect
from hakit.verdict import compute


def rate(cps: float) -> Callable[[float], float]:
    return lambda _at: cps


def test_a_gap_longer_than_the_rate_plus_the_threshold_is_a_stall() -> None:
    starts = [T0, T0 + 50, T0 + 600, T0 + 1150.001, T0 + 1200]
    # At 20 cps a start is due every 50 ms: 550 ms is the bound, 550.001 ms a stall.
    assert detect("s", starts, rate(20), 500) == [Stall("s", T0 + 600, T0 + 1150.001)]
    assert detect("s", list(reversed(starts)), rate(20), 500) == [Stall("s", T0 + 600, T0 + 1150.001)]


def test_the_rate_in_force_sets_the_bound_and_a_zero_rate_never_stalls() -> None:
    starts = [T0, T0 + 1400]
    assert detect("s", starts, rate(1), 500) == []
    assert detect("s", starts, rate(2), 500) == [Stall("s", T0, T0 + 1400)]
    assert detect("s", starts, rate(0), 500) == []


def test_a_stall_spans_a_measurement_that_overlaps_it() -> None:
    st = Stall("s", 1000, 2000)
    assert st.spans(1000, 2005) and st.spans(990, 1001) and st.spans(1999, 2100)
    assert not st.spans(900, 1000) and not st.spans(2000, 2100)
    assert st.holds(1000) and st.holds(2000) and not st.holds(2000.5)


def paced_with_stall(stall_at: float, stall_ms: float) -> list[Json]:
    """20 cps from T0 to T0 + 120 s with no start for `stall_ms` from `stall_at`."""
    recs = [r for r in paced("p", T0, T0 + 120_000, 20) if not stall_at < r["started_at_ms"] < stall_at + stall_ms]  # type: ignore[operator]
    return recs


def verdict(tmp_path: Path, recs: list[Json], events: list[Json] | None = None) -> Json:
    d = run_dir(tmp_path, recs, calls=len(recs), events=events or [], profile={})
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    return v


def test_a_slow_setup_measured_across_a_stall_is_a_generator_stall_finding(tmp_path: Path) -> None:
    at = T0 + 60_000
    recs = paced_with_stall(at, 1000)
    # The call started as the generator stalled read its provisional 1005 ms late.
    recs = [record("late", at, provisional=1005, setup=5000, answered=5000) if r["started_at_ms"] == at else r for r in recs]
    v = verdict(tmp_path, recs)
    assert v["unattributed"] == []
    (i,) = jv.objs(v, "integrity")
    assert (i["kind"], i["stream"], i["at_ms"], i["call_ids"]) == ("generator_stall", "basic", at, ["late"])
    assert i["detail"] == "1 measurements taken across a 1000 ms generator stall"
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == [{"at_ms": at, "ms": 1000.0, "spoiled": 1}]
    assert v["status"] == "invalid"


def test_a_stall_that_spoiled_nothing_leaves_the_status_alone(tmp_path: Path) -> None:
    at = T0 + 60_000
    v = verdict(tmp_path, paced_with_stall(at, 1000))
    assert v["status"] == "pass" and v["integrity"] == []
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == [{"at_ms": at, "ms": 1000.0, "spoiled": 0}]


def test_the_step_in_force_sets_the_rate_of_the_verdict_s_stall_bound(tmp_path: Path) -> None:
    basic = {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 20}
    stream = {**basic, "setup_deadline_s": 32, "steps": [{"at_s": 60, "cps": 1}]}
    recs = paced("a", T0, T0 + 60_000, 20) + paced("b", T0 + 60_000, T0 + 120_000, 1)
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={"streams": [stream]})
    (s,) = jv.objs(compute(d, SCHEMAS), "streams")
    assert s["stalls"] == []


def test_a_slow_setup_outside_any_stall_stays_an_impact(tmp_path: Path) -> None:
    at = T0 + 60_000
    recs = paced_with_stall(at, 1000)
    recs.append(record("slow", T0 + 90_010, provisional=1500, setup=1600, answered=1600))
    v = verdict(tmp_path, recs)
    assert [(u["class"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("setup_slow", ["slow"])]
    assert v["status"] == "fail"


def test_a_setup_deadline_inside_a_stall_is_a_generator_stall_finding(tmp_path: Path) -> None:
    at = T0 + 60_000
    recs = paced_with_stall(at, 1000)
    recs.append(timed_out("deadline", at - 31_500))  # its 32 s deadline falls 500 ms into the stall
    recs.append(timed_out("before", at - 32_100))  # 100 ms before it
    v = verdict(tmp_path, recs)
    assert [(u["class"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("setup_failed", ["before"])]
    (i,) = jv.objs(v, "integrity")
    assert (i["kind"], i["call_ids"]) == ("generator_stall", ["deadline"])


def test_a_measurement_across_a_stall_is_left_out_of_the_recovery_baseline(tmp_path: Path) -> None:
    at = T0 + 50_000
    inject = at + 2_000
    recs = paced_with_stall(at, 1000)

    def shaped(r: Json) -> Json:
        start = jv.number(r, "started_at_ms")
        if start == at:
            return record("late", at, provisional=1005)
        if inject <= start < inject + 10_000:
            return record(jv.string(r, "call_id"), start, provisional=30)
        return r

    recs = [shaped(r) for r in recs]
    rule: Json = {"latency_slack_ms": 0, "baseline_s": 3}
    budget: Json = {"schema": "budget.v1", "name": "rec", "description": "d", "recovery": rule}
    d = run_dir(tmp_path, recs, calls=len(recs), events=kill(1, inject, budget="rec"), profile={}, budgets=[budget])
    v = compute(d, SCHEMAS)
    (w,) = jv.objs(v, "windows")
    # The baseline's setups take 20 ms; the 1005 ms one measured the stall. With
    # it the 30 ms setups after the kill would read as back at once.
    assert w["recovery"] == [{"stream": "basic", "recovery_s": 10.0}]
