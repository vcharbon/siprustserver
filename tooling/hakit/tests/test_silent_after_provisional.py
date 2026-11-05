"""A caller who heard a provisional and no final fails the run, whatever the
window's budget says; the same run without that call passes."""

from __future__ import annotations

from pathlib import Path

import pytest
from kit import T0, record, silent_after, timed_out, write_yaml
from rundir import CONTENT, SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.content import ContentError, discover
from hakit.jsonval import Json
from hakit.summary import render
from hakit.verdict import compute, recovery_samples

AT = T0 + 100_000
BUDGET_NAMES = sorted(CONTENT.budgets)
# Every key a budget can state for a setup with no final, wildcards included.
ALLOWS_EVERYTHING: Json = {
    "schema": "budget.v1",
    "name": "allows-everything",
    "description": "d",
    "setup_failed": [{"stream": "*", "code": 0, "reason": "*", "max_calls": 1000}],
    "unexpected_ending": [{"stream": "*", "code": 0, "reason": "*", "max_calls": 1000}],
    "setup_slow": {"max_calls": 1000},
    "in_dialog": [{"stream": "*", "class": "protocol_anomaly", "detail": "*", "max_calls": 1000}],
}


def rows_of(v: Json, cls: str) -> list[Json]:
    (w,) = jv.objs(v, "windows")
    return [r for r in jv.objs(w, "impacts") if r["class"] == cls]


def test_a_run_holding_such_a_call_fails_and_passes_without_it(tmp_path: Path) -> None:
    clean = [record("ok", AT + 1_000), timed_out("no-100", AT + 1_000)]
    passed = compute(run_dir(tmp_path / "a", clean, calls=2, events=kill(1, AT)), SCHEMAS)
    assert passed["status"] == "pass"
    assert rows_of(passed, "silent_after_provisional") == []

    failed = compute(run_dir(tmp_path / "b", [*clean, silent_after("heard-100", AT + 1_000)], calls=3, events=kill(1, AT)), SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", failed) == []
    assert failed["status"] == "fail"
    (row,) = rows_of(failed, "silent_after_provisional")
    assert (row["stream"], row["count"], row["allowed"], row["finding"]) == ("basic", 1, 0, True)
    assert row["call_ids"] == ["heard-100"]
    # The call with no 100 stays in its own class, under the budget's allowance.
    (other,) = rows_of(failed, "setup_failed")
    assert (other["count"], other["finding"]) == (1, False)


def test_no_budget_allows_it_even_with_every_key_a_wildcard(tmp_path: Path) -> None:
    rec = [silent_after("heard-100", AT + 1_000)]
    d = run_dir(tmp_path, rec, calls=1, events=kill(1, AT, budget="allows-everything"), budgets=[ALLOWS_EVERYTHING])
    v = compute(d, SCHEMAS)
    assert v["status"] == "fail"
    assert [(r["count"], r["allowed"]) for r in rows_of(v, "silent_after_provisional")] == [(1, 0)]


@pytest.mark.parametrize("name", BUDGET_NAMES)
def test_no_budget_of_the_base_allows_it(tmp_path: Path, name: str) -> None:
    rec = [silent_after("heard-100", AT + 1_000)]
    d = run_dir(tmp_path, rec, calls=1, events=kill(1, AT, budget=name), budgets=[CONTENT.budgets[name].raw])
    assert compute(d, SCHEMAS)["status"] == "fail", name


def test_a_budget_cannot_name_the_class(tmp_path: Path) -> None:
    write_yaml(
        tmp_path / "budgets" / "names-it.yaml",
        {"schema": "budget.v1", "name": "names-it", "description": "d", "silent_after_provisional": {"max_calls": 1}},
    )
    with pytest.raises(ContentError, match="silent_after_provisional"):
        discover([tmp_path], SCHEMAS)


def test_a_give_up_after_every_window_closed_is_charged_to_the_window_it_was_pending_in(tmp_path: Path) -> None:
    # Sent into the fault; the generator gave up 250 s later, well past the close.
    v = compute(run_dir(tmp_path, [silent_after("heard-100", AT + 1_000)], calls=1, events=kill(1, AT)), SCHEMAS)
    assert v["status"] == "fail"
    assert jv.objs(v, "unattributed") == []
    assert len(rows_of(v, "silent_after_provisional")) == 1


def test_outside_every_window_it_is_unattributed_and_a_finding(tmp_path: Path) -> None:
    v = compute(run_dir(tmp_path, [silent_after("heard-100", T0 + 1_000)], calls=1, events=[]), SCHEMAS)
    assert v["status"] == "fail"
    (u,) = jv.objs(v, "unattributed")
    assert (u["class"], u["count"], u["finding"]) == ("silent_after_provisional", 1, True)


def test_the_summary_and_the_unanswered_table_keep_it_apart_from_a_timeout(tmp_path: Path) -> None:
    recs = [silent_after("heard-100", AT + 1_000), timed_out("no-100", AT + 1_000)]
    v = compute(run_dir(tmp_path, recs, calls=2, events=kill(1, AT)), SCHEMAS)
    text = render(v)
    assert "- window 001-kill_worker: `silent_after_provisional` basic: 1" in text
    assert "a provisional, then no final" in text
    (s,) = jv.objs(jv.obj(v, "unanswered"), "streams")
    assert sorted((c["cause"], c["count"]) for c in jv.objs(s, "causes")) == [("setup_timeout", 1), ("silent_after_provisional", 1)]


def test_recovery_reads_it_as_a_lost_setup(tmp_path: Path) -> None:
    recs = [silent_after("heard-100", AT + 1_000)]
    samples = recovery_samples(run_dir(tmp_path, recs, calls=1, events=kill(1, AT)), SCHEMAS)
    (sample,) = samples["basic"].samples
    assert sample.ok is False
