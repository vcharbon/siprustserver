"""Calls the generator failed with no record (ADR-0049 §4.5): tolerated only
when the counters agree, the generator's statistics place every failure
outside every window, and they stay within the profile's
`record_shortfall_pct`; anything else keeps the run `invalid`."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.summary import render
from hakit.verdict import compute

PCT = {"record_shortfall_pct": 1}
HEADER = "StartTime;LastResetTime;CurrentTime;CurrentCall;TotalCallCreated;SuccessfulCall(C);FailedCall(C)\n"


def stat_rows(d: Path, rows: list[tuple[int, int]], firing: str = "") -> None:
    """A SIPp stat file for `basic` (of one firing when named): one row per
    (instant ms, cumulative failed calls)."""
    lines = [HEADER]
    for at_ms, failed in rows:
        epoch = f"x\ty\t{at_ms / 1000:.6f}"
        lines.append(f"{epoch};{epoch};{epoch};0;100;{100 - failed};{failed}\n")
    where = d / "streams" / "basic" / firing
    where.mkdir(parents=True, exist_ok=True)
    (where / "stat.csv").write_text("".join(lines))


def integrity(v: Json) -> list[tuple[object, object, object]]:
    return [(i["kind"], i["detail"], i.get("tolerated")) for i in jv.objs(v, "integrity")]


RECS = [record(f"c{i}", T0 + i * 10) for i in range(99)]


def test_a_failure_in_calm_traffic_within_the_tolerance_leaves_the_run_valid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, RECS, calls=100, events=kill(1, T0 + 80_000), profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 10_000, 0), (T0 + 11_000, 1), (T0 + 12_000, 1)])
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    assert integrity(v) == [
        (
            "generator_failure",
            "the generator failed 1 calls without a record "
            "(tolerated: 1 of 100 calls, all outside every window, within 1 %)",
            True,
        )
    ]
    assert v["status"] == "pass"


def test_a_failure_inside_a_window_names_it_and_makes_the_run_invalid(tmp_path: Path) -> None:
    """The window's budget allows nothing: a call the fault may have broken
    must not pass as a generator artefact."""
    d = run_dir(tmp_path, RECS, calls=100, events=kill(1, T0 + 80_000), profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 90_000, 0), (T0 + 91_000, 1)])
    v = compute(d, SCHEMAS)
    assert integrity(v) == [
        ("generator_failure", "the generator failed 1 calls without a record; 1 in window 001-kill_worker", None)
    ]
    assert v["status"] == "invalid"


def test_a_failure_within_a_call_life_of_a_window_belongs_to_it(tmp_path: Path) -> None:
    """The window of a kill at 80 s closes at 125 s; a `basic` call lives up to
    96 s. A call failing at 201 s may have started inside the window, so its
    failure counts there; one failing at 301 s cannot have, and is calm traffic.
    Spans straddling either edge of the window count inside it."""
    cases = {
        "after-close": ([(T0 + 200_000, 0), (T0 + 201_000, 1)], "invalid"),
        "straddles-close": ([(T0 + 124_000, 0), (T0 + 126_000, 1)], "invalid"),
        "straddles-open": ([(T0 + 79_000, 0), (T0 + 81_000, 1)], "invalid"),
        "beyond-reach": ([(T0 + 300_000, 0), (T0 + 301_000, 1)], "pass"),
    }
    for name, (rows, status) in cases.items():
        d = run_dir(tmp_path / name, RECS, calls=100, events=kill(1, T0 + 80_000), profile=PCT, stream={"generator_failures": 1})
        stat_rows(d, rows)
        assert compute(d, SCHEMAS)["status"] == status, name


def test_the_reach_of_a_failed_call_counts_its_scenario_defaults(tmp_path: Path) -> None:
    """An OPTIONS flood with no count of its own takes the base UAC's declared
    200 requests 200 ms apart: 40 s of flood instead of the 20 s hold, so a
    call failing at 227 s may have started inside the window that closed at
    125 s."""
    flood = {
        "name": "basic",
        "driver": "sipp",
        "shape": "basic",
        "scenario": "uac-basic-hold.xml",
        "cps": 20,
        "setup_deadline_s": 32,
        "ring_s": 5,
        "hold_s": 20,
        "params": {"abuse": "options_flood"},
    }
    profile = {**PCT, "streams": [flood]}
    d = run_dir(tmp_path, RECS, calls=100, events=kill(1, T0 + 80_000), profile=profile, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 226_000, 0), (T0 + 227_000, 1)])
    assert compute(d, SCHEMAS)["status"] == "invalid"


def test_a_failure_its_statistics_do_not_place_makes_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path / "none", RECS, calls=100, events=[], profile=PCT, stream={"generator_failures": 1})
    v = compute(d, SCHEMAS)
    assert integrity(v) == [("generator_failure", "the generator failed 1 calls without a record; 1 not placed in time", None)]
    assert v["status"] == "invalid"

    d = run_dir(tmp_path / "first", RECS, calls=100, events=[], profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 300_000, 1), (T0 + 301_000, 1)])
    assert compute(d, SCHEMAS)["status"] == "invalid"


def test_a_firing_is_placed_from_its_own_statistics(tmp_path: Path) -> None:
    d = run_dir(tmp_path, RECS, calls=100, events=kill(1, T0 + 80_000), profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 90_000, 0), (T0 + 91_000, 1)], firing="001-kill_worker")
    v = compute(d, SCHEMAS)
    assert integrity(v) == [
        ("generator_failure", "the generator failed 1 calls without a record; 1 in window 001-kill_worker", None)
    ]


def test_counters_that_disagree_are_never_tolerated(tmp_path: Path) -> None:
    recs = RECS[:90]
    d = run_dir(tmp_path, recs, calls=100, events=[], profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 10_000, 0), (T0 + 11_000, 1)])
    v = compute(d, SCHEMAS)
    assert [(k, t) for k, _, t in integrity(v)] == [("generator_failure", None), ("record_gap", None)]
    assert v["status"] == "invalid"


def test_a_missing_record_with_no_failure_counted_makes_the_run_invalid(tmp_path: Path) -> None:
    v = compute(run_dir(tmp_path, RECS, calls=100, events=[], profile=PCT), SCHEMAS)
    assert integrity(v) == [("record_gap", "99 records + 0 not placed != 100 calls the generator counted", None)]
    assert v["status"] == "invalid"


def test_an_incomplete_copy_or_an_unreconciled_stream_is_never_tolerated(tmp_path: Path) -> None:
    for name, extra in (("copy", {"records_complete": False}), ("abandoned", {"reconciled": False})):
        d = run_dir(tmp_path / name, RECS, calls=100, events=[], profile=PCT, stream={"generator_failures": 1, **extra})
        stat_rows(d, [(T0 + 10_000, 0), (T0 + 11_000, 1)])
        v = compute(d, SCHEMAS)
        assert not any(t for _, _, t in integrity(v)), name
        assert v["status"] == "invalid", name


def test_the_summary_gives_as_reasons_only_the_findings_not_tolerated(tmp_path: Path) -> None:
    died = {"kind": "stream_died", "stream": "basic", "at_ms": T0 + 1, "detail": "generator exited"}
    d = run_dir(tmp_path, RECS, calls=100, events=[], integrity=[died], profile=PCT, stream={"generator_failures": 1})
    stat_rows(d, [(T0 + 10_000, 0), (T0 + 11_000, 1)])
    v = compute(d, SCHEMAS)
    assert v["status"] == "invalid"
    why = render(v).split("## Why")[1].split("##")[0]
    assert "generator exited" in why
    assert "generator_failure" not in why
