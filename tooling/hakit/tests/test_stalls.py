"""Generator health (ADR-0049 §3.12): a stall is read off the call starts, and what it
measured is an integrity finding, never an impact of the system under test."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from pathlib import Path

from kit import T0, paced, record, timed_out
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.stalls import Stall, detect
from hakit.verdict import compute


def rate(cps: float) -> Callable[[float, float], float]:
    return lambda _a, _b: cps


def test_a_gap_longer_than_the_rate_plus_the_threshold_is_a_stall_from_the_start_due() -> None:
    starts = [T0, T0 + 50, T0 + 600, T0 + 1150.001, T0 + 1200]
    # At 20 cps a start is due every 50 ms: 550 ms is the bound, 550.001 ms a
    # stall, from the start that was due (T0 + 650) to the one that came.
    assert detect("s", starts, rate(20), 500) == [Stall("s", T0 + 650, T0 + 1150.001)]
    assert detect("s", list(reversed(starts)), rate(20), 500) == [Stall("s", T0 + 650, T0 + 1150.001)]


def test_the_lowest_rate_over_the_gap_sets_the_bound_and_a_zero_rate_never_stalls() -> None:
    starts = [T0, T0 + 1400]
    assert detect("s", starts, rate(1), 500) == []
    assert detect("s", starts, rate(2), 500) == [Stall("s", T0 + 500, T0 + 1400)]
    assert detect("s", starts, rate(0), 500) == []
    # 20 cps at the gap's start, 1 cps from T0 + 500: the bound is 1.5 s.
    stepped: Callable[[float, float], float] = lambda a, b: 1 if b > T0 + 500 else 20  # noqa: E731
    assert detect("s", starts, stepped, 500) == []


def test_a_gap_while_the_generator_holds_its_call_limit_is_no_stall() -> None:
    starts = [T0, T0 + 1400]
    assert detect("s", starts, rate(2), 500, lambda a, b: a <= T0 + 700 <= b) == []
    assert detect("s", starts, rate(2), 500, lambda a, b: a <= T0 + 3000 <= b) == [Stall("s", T0 + 500, T0 + 1400)]


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
    assert (i["kind"], i["stream"], i["at_ms"], i["call_ids"]) == ("generator_stall", "basic", at + 50, ["late"])
    assert i["detail"] == "1 measurements taken across a 950 ms generator stall"
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == [{"at_ms": at + 50, "ms": 950.0, "spoiled": 1, "source": "starts"}]
    assert v["status"] == "invalid"


def test_a_stall_that_spoiled_nothing_leaves_the_status_alone(tmp_path: Path) -> None:
    at = T0 + 60_000
    v = verdict(tmp_path, paced_with_stall(at, 1000))
    assert v["status"] == "pass" and v["integrity"] == []
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == [{"at_ms": at + 50, "ms": 950.0, "spoiled": 0, "source": "starts"}]


def test_the_step_in_force_sets_the_rate_of_the_verdict_s_stall_bound(tmp_path: Path) -> None:
    basic = {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 20}
    stream = {**basic, "setup_deadline_s": 32, "steps": [{"at_s": 60, "cps": 1}]}
    recs = paced("a", T0, T0 + 60_000, 20) + paced("b", T0 + 60_000, T0 + 120_000, 1)
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={"streams": [stream]})
    (s,) = jv.objs(compute(d, SCHEMAS), "streams")
    assert s["stalls"] == []


def test_a_gap_across_a_rate_drop_is_judged_at_the_lower_rate(tmp_path: Path) -> None:
    basic = {"name": "basic", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 20}
    stream = {**basic, "setup_deadline_s": 32, "steps": [{"at_s": 60, "cps": 1}]}
    # The last start at 20 cps is at +59.95 s, the first at 1 cps at +61 s: 1.05 s
    # apart, a stall at 20 cps, none at 1 cps.
    recs = paced("a", T0, T0 + 60_000, 20) + paced("b", T0 + 61_000, T0 + 120_000, 1)
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
    # Its 32 s deadline falls 500 ms into the stall; the generator notices it
    # 5 ms after the stall, at its next tick: the deadline is what is tested.
    late = timed_out("deadline", at - 31_500)
    late["setup_ended_at_ms"] = late["ended_at_ms"] = at + 1_005
    recs.append(late)
    recs.append(timed_out("before", at - 32_100 + 50))  # its deadline 50 ms before the stall
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


def stat_csv(d: Path, rows: list[tuple[float, int, int]]) -> None:
    """The SIPp stat file of stream `basic`: (epoch ms, CurrentCall, WatchdogMinor) per row."""
    p = d / "streams" / "basic" / "stat.csv"
    p.parent.mkdir(parents=True, exist_ok=True)
    lines = ["StartTime;CurrentTime;CurrentCall;WatchdogMajor(C);WatchdogMinor(C);"]
    lines += [f"x;2026-01-01\t00:00:00.0\t{t / 1000:.6f};{calls};0;{minor};" for t, calls, minor in rows]
    p.write_text("\n".join(lines) + "\n")


def test_a_gap_while_the_stream_held_its_call_limit_is_not_a_stall(tmp_path: Path) -> None:
    at = T0 + 60_000
    recs = paced_with_stall(at, 1000)
    recs = [record("slow", at, provisional=1005, setup=5000, answered=5000) if r["started_at_ms"] == at else r for r in recs]
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={}, stream={"call_limit": 400})
    stat_csv(d, [(at - 1000, 300, 0), (at + 500, 400, 0), (at + 1500, 390, 0)])
    v = compute(d, SCHEMAS)
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == []
    assert [(u["class"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("setup_slow", ["slow"])]


def test_the_generator_s_watchdog_is_a_second_source_of_stalls(tmp_path: Path) -> None:
    # No gap in the starts, but SIPp's watchdog fired between two stat rows.
    recs = paced("p", T0, T0 + 120_000, 20)
    at = T0 + 60_000
    recs = [record("late", at, provisional=700, setup=5000, answered=5000) if r["started_at_ms"] == at else r for r in recs]
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={}, stream={"call_limit": 400})
    stat_csv(d, [(at - 1000, 10, 3), (at + 400, 10, 3), (at + 1400, 10, 4)])
    v = compute(d, SCHEMAS)
    (s,) = jv.objs(v, "streams")
    assert s["stalls"] == [{"at_ms": at + 400, "ms": 1000.0, "spoiled": 0, "source": "watchdog"}]
    assert v["unattributed"] == [] and v["status"] == "pass"  # 700 ms is under the 1000 ms threshold
    recs = [record("late", at, provisional=1200, setup=5000, answered=5000) if r["call_id"] == "late" else r for r in recs]
    (tmp_path / "run").rename(tmp_path / "first")
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={}, stream={"call_limit": 400})
    stat_csv(d, [(at - 1000, 10, 3), (at + 400, 10, 3), (at + 1400, 10, 4)])
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["call_ids"]) for i in jv.objs(v, "integrity")] == [("generator_stall", ["late"])]


def test_a_timeout_spoiled_by_a_stall_is_left_out_of_the_recovery_samples(tmp_path: Path) -> None:
    from hakit.verdict import recovery_samples

    at = T0 + 60_000
    recs = paced_with_stall(at, 1000)
    recs.append(timed_out("deadline", at - 31_500))
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={})
    samples = recovery_samples(d, SCHEMAS)
    assert [s for s in samples["basic"].samples if not s.ok] == []


def test_the_sipp_call_limit_covers_every_call_at_its_worst() -> None:
    from hakit.lab import lab_from_env
    from hakit.profile import Stream
    from hakit.sipp_driver import SippStream, worst_call_s

    from hakit.paths import BASE_CONTENT

    s = Stream("basic", "sipp", "run", "basic", "uac-basic-hold.xml", 20, 32, 5, 20, None, {}, [(60, 50)])
    scenario = BASE_CONTENT / "scenarios" / "uac-basic-hold.xml"
    stream = SippStream("r", s, 0, scenario, Path("."), lab_from_env({}), None)  # type: ignore[arg-type]
    assert worst_call_s(s) == 96
    assert stream.max_concurrent() == 50 * 96 + 10


def burst_stalls(tmp_path: Path, firings: Sequence[tuple[float, float]], recs: list[Json]) -> list[Stall]:
    """The stalls of a 2 cps `start: incident` stream `burst` fired at each
    `(inject, heal)` of `firings`, read back from a run directory."""
    from hakit.readers.run_view import read_run
    from hakit.windows import event_line, facts_line, phase_line

    burst = {"name": "burst", "driver": "sipp", "shape": "basic", "scenario": "uac-basic-hold.xml", "cps": 2}
    streams = [{**burst, "setup_deadline_s": 32, "start": "incident"}]
    events: list[Json] = []
    for seq, (at, heal) in enumerate(firings, 1):
        ev = f"00{seq}-spike"
        events += [
            event_line(ev, "schedule", ["spike"], "any", 20, 720),
            phase_line(ev, "spike", "inject", int(at)),
            facts_line(ev, "spike", "inject", {"stream": "burst", "firing": seq}),
            phase_line(ev, "spike", "heal", int(heal)),
            phase_line(ev, "spike", "recovered", int(heal) + 100),
        ]
    d = run_dir(tmp_path, recs, calls=None, events=events, cps=2, profile={"streams": streams})
    return read_run(d).stalls


def test_the_gap_between_two_firings_of_an_incident_stream_is_no_stall(tmp_path: Path) -> None:
    # Each firing starts its calls for 10 s, 80 s apart; the second stalls 2 s.
    second = [r for r in paced("b", T0 + 100_000, T0 + 110_000, 2, stream="burst") if not T0 + 105_000 < jv.number(r, "started_at_ms") < T0 + 107_000]
    recs = paced("a", T0 + 10_000, T0 + 20_000, 2, stream="burst") + second
    firings = [(T0 + 10_000, T0 + 20_000), (T0 + 100_000, T0 + 110_000)]
    assert burst_stalls(tmp_path, firings, recs) == [Stall("burst", T0 + 105_500, T0 + 107_000)]


def test_a_firing_that_stops_placing_calls_before_its_heal_stalls_until_the_heal(tmp_path: Path) -> None:
    recs = paced("a", T0, T0 + 5_000, 2, stream="burst") + paced("b", T0 + 100_000, T0 + 110_000, 2, stream="burst")
    firings = [(T0, T0 + 60_000), (T0 + 100_000, T0 + 110_000)]
    assert burst_stalls(tmp_path, firings, recs) == [Stall("burst", T0 + 5_000, T0 + 60_000)]
