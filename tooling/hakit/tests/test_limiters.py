"""The limited streams' self-repair (ADR-0049 §3.13): per minute, the median
of a limiter's answered calls stays in [cap - 2, cap] outside every window's
zone, and within the window's own allowance per direction inside it."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record
from rundir import SCHEMAS, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.limiters import judge_limiters, limiter_errors
from hakit.model import parse_budget
from hakit.profile import Profile, parse_profile
from hakit.verdict import compute
from hakit.windows import Window

CAP = 4
LOAD_S = 1200
REPAIR_S = 30


def profile(streams: list[Json] | None = None, limiter_streams: list[str] | None = None) -> Profile:
    return parse_profile(
        {
            "name": "p",
            "duration_s": LOAD_S,
            "setup_slow_ms": 1000,
            "catalogs": [],
            "streams": streams
            if streams is not None
            else [{"name": "lim_0", "driver": "loadgen", "shape": "s", "cps": 2, "setup_deadline_s": 10, "ring_s": 0.4, "hold_s": 8}],
            "limiters": [{"id": "lim", "cap": CAP, "streams": limiter_streams or ["lim_0"], "repair_s": REPAIR_S}],
        }
    )


def level(segments: list[tuple[int, int, int]], stream: str = "lim_0") -> list[Json]:
    """`n` calls answered at `from_s` and ended at `to_s` of the load, per segment."""
    out: list[Json] = []
    for i, (a, b, n) in enumerate(segments):
        for k in range(n):
            r: Json = {"stream": stream, "answered_at_ms": T0 + a * 1000, "ended_at_ms": T0 + b * 1000, "call_id": f"c{i}-{k}"}
            out.append(r)
    return out


def window(opened_s: int, closed_s: int, budget: str) -> Window:
    return Window("001-x", "schedule", ["x"], budget, "ok", T0 + opened_s * 1000, T0 + closed_s * 1000)


BUDGETS = {
    "kill": parse_budget({"schema": "budget.v1", "name": "kill", "description": "", "limiter": {"overshoot": 2}}),
    "fail-open": parse_budget({"schema": "budget.v1", "name": "fail-open", "description": "", "limiter": {"overshoot": "offered"}}),
    "starve": parse_budget({"schema": "budget.v1", "name": "starve", "description": "", "limiter": {"undershoot": True}}),
    "none": parse_budget({"schema": "budget.v1", "name": "none", "description": ""}),
}


def judge(records: list[Json], windows: list[Window] | None = None) -> list[Json]:
    return judge_limiters(profile(), records, windows or [], BUDGETS, T0, T0 + LOAD_S * 1000)


def kinds(found: list[Json]) -> list[tuple[str, str, str | None]]:
    return [(jv.string(f, "kind"), jv.string(f, "severity"), jv.opt_string(f, "window")) for f in found]


def test_a_limiter_at_its_cap_or_two_under_it_is_steady() -> None:
    assert judge(level([(0, LOAD_S, CAP)])) == []
    assert judge(level([(0, LOAD_S, CAP - 2)])) == []


def test_a_limiter_under_its_band_outside_every_window_is_a_major_undershoot() -> None:
    found = judge(level([(0, 600, CAP), (600, 900, 1), (900, LOAD_S, CAP)]))
    assert kinds(found) == [("limiter_undershoot", "major", None)]
    f = found[0]
    assert (f["from_ms"], f["to_ms"]) == (T0 + 630_000, T0 + 870_000)
    assert jv.lst(f, "bound") == [CAP - 2, CAP]


def test_a_limiter_over_its_cap_outside_every_window_is_a_minor_overshoot() -> None:
    found = judge(level([(0, LOAD_S, CAP), (600, 900, 2)]))
    assert kinds(found) == [("limiter_overshoot", "minor", None)]


def test_a_window_allows_its_overshoot_until_its_close_plus_the_repair_time() -> None:
    w = window(600, 660, "kill")
    assert judge(level([(0, LOAD_S, CAP), (600, 660 + REPAIR_S, CAP)]), [w]) == []
    late = judge(level([(0, LOAD_S, CAP), (600, 900, CAP)]), [w])
    assert kinds(late) == [("limiter_overshoot", "minor", None)]
    assert late[0]["from_ms"] == T0 + 690_000


def test_a_window_bounds_its_overshoot_by_its_factor() -> None:
    found = judge(level([(0, LOAD_S, CAP), (600, 690, 3 * CAP)]), [window(600, 660, "kill")])
    assert kinds(found) == [("limiter_overshoot", "minor", "001-x")]


def test_a_window_allows_an_undershoot_only_when_its_budget_says_so() -> None:
    gap = level([(0, 600, CAP), (690, LOAD_S, CAP)])
    assert judge(gap, [window(600, 660, "starve")]) == []
    assert kinds(judge(gap, [window(600, 660, "kill")])) == [("limiter_undershoot", "major", "001-x")]
    assert kinds(judge(gap, [window(600, 660, "none")])) == [("limiter_undershoot", "major", "001-x")]


def test_a_limiter_names_the_profile_s_streams_and_is_saturated_by_them() -> None:
    assert limiter_errors("p", profile(limiter_streams=["ghost"])) == [
        "p: limiter 'lim': names streams the profile does not run: ['ghost']"
    ]
    thin: list[Json] = [{"name": "lim_0", "driver": "loadgen", "shape": "s", "cps": 0.5, "setup_deadline_s": 10, "ring_s": 0, "hold_s": 8}]
    assert limiter_errors("p", profile(streams=thin)) == [
        "p: limiter 'lim': its streams offer 4 concurrent calls, under cap 4 + 2"
    ]


def test_the_verdict_fails_on_a_limiter_out_of_band(tmp_path: Path) -> None:
    lim = {"limiters": [{"id": "lim", "cap": CAP, "streams": ["basic"], "repair_s": 0}], "generator_stall_ms": 3_600_000}
    under = [record(f"c{i}", T0, answered=0, ended=600_000) for i in range(1)]
    v = compute(run_dir(tmp_path, under, calls=1, events=[], profile=lim), SCHEMAS)
    assert v["status"] == "fail"
    assert [jv.string(f, "kind") for f in jv.objs(v, "limiters")] == ["limiter_undershoot"]


def test_a_window_failing_open_allows_every_offered_call() -> None:
    # lim_0 offers 2 cps x (0.4 + 8) s = 16.8 calls at once.
    assert judge(level([(0, LOAD_S, CAP), (600, 690, 12)]), [window(600, 660, "fail-open")]) == []
    found = judge(level([(0, LOAD_S, CAP), (600, 690, 16)]), [window(600, 660, "fail-open")])
    assert kinds(found) == [("limiter_overshoot", "minor", "001-x")]


def test_streams_with_no_record_leave_their_limiter_unjudged() -> None:
    # A run without records (a dry run) measures no admission; the streams'
    # own reconciliation reports the missing records.
    assert judge([]) == []
