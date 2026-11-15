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


def stat_rows(d: Path, rows: list[tuple[int, int]]) -> None:
    """A SIPp stat file for `basic`: one row per (instant ms, cumulative failed calls)."""
    lines = [HEADER]
    for at_ms, failed in rows:
        epoch = f"x\ty\t{at_ms / 1000:.6f}"
        lines.append(f"{epoch};{epoch};{epoch};0;100;{100 - failed};{failed}\n")
    (d / "streams" / "basic").mkdir(parents=True, exist_ok=True)
    (d / "streams" / "basic" / "stat.csv").write_text("".join(lines))


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


def test_a_failure_its_statistics_do_not_place_makes_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, RECS, calls=100, events=[], profile=PCT, stream={"generator_failures": 1})
    v = compute(d, SCHEMAS)
    assert integrity(v) == [("generator_failure", "the generator failed 1 calls without a record; 1 not placed in time", None)]
    assert v["status"] == "invalid"


def test_counters_that_disagree_are_never_tolerated(tmp_path: Path) -> None:
    d = run_dir(tmp_path, RECS, calls=100, events=[], profile=PCT, stream={"generator_failures": 40})
    stat_rows(d, [(T0 + 10_000, 0), (T0 + 11_000, 40)])
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
