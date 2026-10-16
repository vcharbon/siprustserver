"""A refusal that asks for a retry later is told apart from a bare one of the
same code, and a budget may count beyond its window's outage (ADR-0049 §3.3)."""

from __future__ import annotations

from kit import T0, rejected

from hakit.classify import Impact, classify
from hakit.judge import judge
from hakit.jsonval import Json
from hakit.model import Allowance, Budget, ExpectedEnding, SetupSlowAllowance, Shape, parse_budget
from hakit.windows import Phase, Window

ANSWER = Shape("basic", "c", None, [], [], [ExpectedEnding("answered", None, None)], True)


def bare_only(over_outage: bool = False, max_seconds: float = 1.0) -> Budget:
    entry = Allowance("*", 503, "503", None, max_seconds, retry_after=False, over_outage=over_outage)
    return Budget("bare", [entry], [], SetupSlowAllowance(None, None, None), raw={})


def refused(call_id: str, at: int, retry_after: int | None) -> Json:
    r = rejected(call_id, at, 503, "503")
    r["retry_after_s"] = retry_after
    return r


def window(phases: list[Phase]) -> Window:
    return Window("001-fault", "schedule", ["fault"], "bare", "ok", T0, T0 + 600_000, phases)


def judged(records: list[Json], budget: Budget, w: Window) -> list[Json]:
    impacts: list[Impact] = [i for r in records for i in classify(r, ANSWER).impacts]
    return judge([w], impacts, [], {"bare": budget}, 1000, {"basic": 10.0}).per_window["001-fault"]


def test_the_record_s_retry_after_reaches_its_impact() -> None:
    (with_ra,) = classify(refused("a", T0 + 1000, 5), ANSWER).impacts
    (bare,) = classify(refused("b", T0 + 1000, None), ANSWER).impacts
    assert (with_ra.retry_after, bare.retry_after) == (True, False)


def test_a_bare_only_entry_never_charges_a_refusal_with_retry_after() -> None:
    w = window([Phase("inject", T0, "fault")])
    rows = judged([refused("a", T0 + 1000, None), refused("b", T0 + 1000, 7)], bare_only(), w)
    by_ra = {r.get("retry_after", False): r for r in rows}
    assert (by_ra[False]["count"], by_ra[False]["finding"]) == (1, False)
    assert (by_ra[True]["count"], by_ra[True]["allowed"], by_ra[True]["finding"]) == (1, 0, True)


def test_an_over_outage_entry_counts_beyond_the_window_s_inject_to_recovered() -> None:
    w = window([Phase("inject", T0, "fault"), Phase("healed", T0 + 20_000, "fault"), Phase("recovered", T0 + 21_000, "fault")])
    recs = [refused(f"c{i}", T0 + 1000 + i, None) for i in range(250)]
    (row,) = judged(recs, bare_only(over_outage=True, max_seconds=6.0), w)
    assert (row["allowed"], row["finding"]) == ((6.0 + 21.0) * 10, False)
    (row,) = judged(recs + [refused(f"d{i}", T0 + 2000, None) for i in range(21)], bare_only(True, 6.0), w)
    assert row["finding"] is True


def test_a_budget_file_states_both_fields() -> None:
    b = parse_budget(
        {
            "schema": "budget.v1",
            "name": "x",
            "description": "d",
            "setup_failed": [
                {"stream": "*", "code": 503, "reason": "503", "max_seconds": 6, "retry_after": False, "over_outage": True}
            ],
        }
    )
    (e,) = b.setup_failed
    assert (e.retry_after, e.over_outage, e.allowed(10.0, 21.0)) == (False, True, 270.0)
