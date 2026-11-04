"""The calls established when a window's fault began (budget `established`):
each must end whole and the window must hold at least `min_calls` of them."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.verdict import compute

INJECT = T0 + 80_000
SPANNING: Json = {"schema": "budget.v1", "name": "spanning", "description": "d", "established": {"min_calls": 2}}


def established(tmp_path: Path, records: list[Json]) -> Json:
    d = run_dir(tmp_path, records, calls=len(records), events=kill(1, INJECT, budget="spanning"), budgets=[SPANNING])
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    (row,) = [r for r in jv.objs(w, "impacts") if r["class"] == "established"]
    return row


def test_calls_up_at_the_inject_that_end_whole_pass(tmp_path: Path) -> None:
    up = [record(f"up{i}", INJECT - 10_000 + i) for i in range(3)]
    later = [record("later", INJECT + 1_000), record("before", T0)]
    row = established(tmp_path, up + later)
    assert (row["calls"], row["count"], row["finding"]) == (3, 0, False)


def test_an_established_call_whose_bye_failed_is_a_finding(tmp_path: Path) -> None:
    bye = [{"at_ms": INJECT + 15_000, "kind": "dialog_request", "method": "BYE", "code": 503}]
    up = [record("up0", INJECT - 10_000), record("up1", INJECT - 9_000, failures=bye), record("up2", INJECT - 8_000)]
    row = established(tmp_path, up)
    assert (row["calls"], row["count"], row["call_ids"], row["finding"]) == (3, 1, ["up1"], True)


def test_a_window_no_call_spanned_is_a_finding(tmp_path: Path) -> None:
    row = established(tmp_path, [record("up0", INJECT - 10_000), record("after", INJECT + 500)])
    assert (row["calls"], row["min_calls"], row["count"], row["finding"]) == (1, 2, 0, True)


def test_a_budget_without_it_adds_no_row(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("up0", INJECT - 10_000)], calls=1, events=kill(1, INJECT))
    (w,) = jv.objs(compute(d, SCHEMAS), "windows")
    assert all(r["class"] != "established" for r in jv.objs(w, "impacts"))


def test_a_run_that_answered_no_call_before_the_window_has_nothing_to_judge(tmp_path: Path) -> None:
    row = established(tmp_path, [record("after", INJECT + 500)])
    assert (row["calls"], row["finding"]) == (0, False)


def test_an_impact_after_the_next_window_opened_is_not_this_one(tmp_path: Path) -> None:
    second = T0 + 200_000
    bye = [{"at_ms": second + 1_000, "kind": "dialog_request", "method": "BYE", "code": 503}]
    up = [record(f"up{i}", INJECT - 10_000 + i, ended=200_000) for i in range(2)]
    up.append(record("late", INJECT - 9_000, ended=200_000, failures=bye))
    events = kill(1, INJECT, budget="spanning") + kill(2, second, budget="spanning")
    d = run_dir(tmp_path, up, calls=len(up), events=events, budgets=[SPANNING])
    w1, w2 = jv.objs(compute(d, SCHEMAS), "windows")
    rows = [[r for r in jv.objs(w, "impacts") if r["class"] == "established"][0] for w in (w1, w2)]
    assert [(r["calls"], r["count"]) for r in rows] == [(3, 0), (3, 1)]
