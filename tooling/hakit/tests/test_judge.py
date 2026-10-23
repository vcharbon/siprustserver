"""Window attribution and budgets: nothing is accepted by default (ADR-0049 §3.2 to §3.5)."""

from __future__ import annotations

from kit import T0, record, rejected, timed_out

from hakit.classify import Impact, SetupLatency, classify
from hakit.judge import judge
from hakit.jsonval import Json
from hakit.model import Allowance, Budget, ExpectedEnding, SetupSlowAllowance, Shape
from hakit.windows import Phase, Window

ANSWER = Shape("basic", "c", None, [], [], [ExpectedEnding("answered", None, None)], True)
KILL = Budget(
    "kill",
    setup_failed=[Allowance("*", 0, "timeout", None, 2.0)],
    unexpected_ending=[],
    setup_slow=SetupSlowAllowance(2000, 5, None),
    raw={},
)
STRICT = Budget("strict", [], [], SetupSlowAllowance(None, None, None), raw={})


def window(event: str, opened: int, closed: int, budget: str = "kill") -> Window:
    return Window(event, "schedule", ["kill_worker"], budget, "ok", opened, closed, [Phase("inject", opened, "kill_worker")])


def run(records: list[Json], windows: list[Window], cps: float = 10) -> tuple[dict[str, list[Json]], list[Json], list[Json]]:
    impacts: list[Impact] = []
    latencies: list[SetupLatency] = []
    for r in records:
        c = classify(r, ANSWER)
        impacts += c.impacts
        if c.latency is not None:
            latencies.append(c.latency)
    j = judge(windows, impacts, latencies, {"kill": KILL, "strict": STRICT}, 1000, {"basic": cps, "other": cps})
    # The full Call-ID list rides apart from the verdict's rows (test_attribution).
    for row in [r for rows in j.per_window.values() for r in rows] + j.unattributed:
        row.pop("all_call_ids")
    return j.per_window, j.in_dialog, j.unattributed


def test_timeouts_inside_the_window_are_charged_against_its_seconds_of_offered_rate() -> None:
    w = window("001-kill_worker", T0 + 60_000, T0 + 160_000)
    recs = [timed_out(f"c{i}", T0 + 50_000 + i) for i in range(20)]
    per, _, unatt = run(recs, [w])
    (row,) = per["001-kill_worker"]
    assert row == {
        "class": "setup_failed",
        "stream": "basic",
        "count": 20,
        "allowed": 20.0,
        "finding": False,
        "call_ids": [f"c{i}" for i in range(20)],
        "seconds": 2.0,
        "code": 0,
        "reason": "timeout",
    }
    assert unatt == []
    per, _, _ = run([*recs, timed_out("c20", T0 + 50_100)], [w])
    assert per["001-kill_worker"][0]["finding"] is True


def test_a_code_the_budget_does_not_name_is_a_finding() -> None:
    w = window("001-kill_worker", T0, T0 + 100_000)
    per, _, _ = run([rejected("a", T0 + 1000, 480, "480")], [w])
    (row,) = per["001-kill_worker"]
    assert (row["class"], row["code"], row["reason"], row["count"], row["allowed"], row["finding"]) == (
        "setup_failed",
        480,
        "480",
        1,
        0,
        True,
    )


def test_a_star_stream_entry_allows_each_stream_separately() -> None:
    w = window("001-kill_worker", T0, T0 + 100_000)
    recs = [timed_out(f"a{i}", T0) for i in range(15)]
    other = [{**timed_out(f"b{i}", T0), "stream": "other"} for i in range(15)]
    per, _, _ = run(recs + other, [w], cps=10)
    assert [(r["stream"], r["count"], r["allowed"]) for r in per["001-kill_worker"]] == [("basic", 15, 20.0), ("other", 15, 20.0)]


def test_impact_outside_every_window_is_unattributed() -> None:
    w = window("001-kill_worker", T0 + 100_000, T0 + 200_000)
    per, _, unatt = run([rejected("a", T0, 503, "503"), timed_out("b", T0 + 300_000)], [w])
    assert per["001-kill_worker"] == []
    assert [(u["class"], u["code"], u["reason"], u["count"], u["finding"]) for u in unatt] == [
        ("setup_failed", 0, "timeout", 1, True),
        ("setup_failed", 503, "503", 1, True),
    ]


def test_setup_slow_uses_the_window_threshold_inside_and_the_profile_threshold_outside() -> None:
    w = window("001-kill_worker", T0, T0 + 100_000)
    inside = record("a", T0 + 1000, provisional=1500, setup=1600, answered=1600)
    outside = record("b", T0 + 200_000, provisional=1500, setup=1600, answered=1600)
    per, _, unatt = run([inside, outside], [w])
    assert per["001-kill_worker"] == []
    assert [(u["class"], u["count"]) for u in unatt] == [("setup_slow", 1)]
    slower = record("c", T0 + 1000, provisional=2500, setup=2600, answered=2600)
    per, _, _ = run([slower], [w])
    assert [(r["class"], r["count"], r["allowed"], r["finding"]) for r in per["001-kill_worker"]] == [("setup_slow", 1, 5.0, False)]


def test_an_in_dialog_impact_carries_every_window_injected_during_the_call() -> None:
    w1 = window("001-kill_worker", T0 + 5_000, T0 + 10_000)
    w2 = window("002-kill_worker", T0 + 20_000, T0 + 25_000)
    cut = record("a", T0, "cut", failures=[{"at_ms": T0 + 22_000, "kind": "dialog_request", "method": "BYE", "code": 481}])
    _, in_dialog, unatt = run([cut], [w1, w2])
    assert [(i["class"], i["candidate_windows"]) for i in in_dialog] == [
        ("call_cut", ["001-kill_worker", "002-kill_worker"]),
        ("dialog_request_failed", ["001-kill_worker", "002-kill_worker"]),
    ]
    assert unatt == []
    late = record("b", T0 + 30_000, "stranded")
    _, in_dialog, unatt = run([late], [w1, w2])
    assert in_dialog == [] and [(u["class"], u["count"], u["finding"]) for u in unatt] == [("call_stranded", 1, True)]


def test_a_window_without_an_allowance_allows_nothing() -> None:
    w = window("001-x", T0, T0 + 100_000, budget="strict")
    per, _, _ = run([timed_out("a", T0)], [w])
    assert [(r["count"], r["allowed"], r["finding"]) for r in per["001-x"]] == [(1, 0, True)]


def test_an_impact_is_charged_to_the_first_entry_that_matches_it() -> None:
    first_then_wide = Budget(
        "ordered",
        setup_failed=[Allowance("*", 0, "timeout", 1, None), Allowance("*", 0, "*", 100, None)],
        unexpected_ending=[],
        setup_slow=SetupSlowAllowance(None, None, None),
        raw={},
    )
    w = window("001-x", T0, T0 + 100_000, budget="ordered")
    impacts = [i for r in (timed_out("a", T0), timed_out("b", T0 + 1)) for i in classify(r, ANSWER).impacts]
    j = judge([w], impacts, [], {"ordered": first_then_wide}, 1000, {"basic": 10})
    (row,) = j.per_window["001-x"]
    assert (row["reason"], row["count"], row["allowed"], row["finding"]) == ("timeout", 2, 1.0, True)


def test_a_window_injected_after_the_failure_is_no_candidate() -> None:
    before = window("001-kill_worker", T0 + 5_000, T0 + 10_000)
    after = window("002-kill_worker", T0 + 25_000, T0 + 30_000)
    cut = record("a", T0, "cut", ended=40_000, failures=[{"at_ms": T0 + 20_000, "kind": "dialog_request", "method": "BYE", "code": 481}])
    _, in_dialog, _ = run([cut], [before, after])
    assert [i["candidate_windows"] for i in in_dialog] == [["001-kill_worker"], ["001-kill_worker"]]


def test_a_window_holding_the_failure_is_a_candidate_for_a_call_started_after_its_inject() -> None:
    w = window("001-kill_worker", T0 + 5_000, T0 + 60_000)
    late = record("a", T0 + 5_093, "cut", ended=30_000, failures=[{"at_ms": T0 + 13_000, "kind": "dialog_request", "method": "BYE", "code": 503}])
    _, in_dialog, unatt = run([late], [w])
    assert [(i["class"], i["candidate_windows"]) for i in in_dialog] == [
        ("call_cut", ["001-kill_worker"]),
        ("dialog_request_failed", ["001-kill_worker"]),
    ]
    assert unatt == []


def test_a_setup_timeout_ending_after_the_window_is_charged_to_the_window_it_was_pending_in() -> None:
    # The INVITE went out just after the kill and no hop ever answered: the
    # generator gives up 32 s later, after the window closed.
    w = window("001-kill_worker", T0 + 60_000, T0 + 85_000)
    per, _, unatt = run([timed_out("a", T0 + 60_040)], [w])
    assert [(r["class"], r["reason"], r["count"]) for r in per["001-kill_worker"]] == [("setup_failed", "timeout", 1)]
    assert unatt == []
    # A setup pending in no window stays unattributed.
    per, _, unatt = run([timed_out("b", T0 + 90_000)], [w])
    assert per["001-kill_worker"] == [] and [u["reason"] for u in unatt] == ["timeout"]


def test_a_late_final_to_a_setup_sent_into_the_window_is_charged_to_it() -> None:
    # The INVITE went out into the kill; the final (a 480 the takeover sent)
    # lands after the window closed.
    w = window("001-kill_worker", T0 + 60_000, T0 + 85_000)
    late = record("a", T0 + 60_040, "rejected", answered=None, setup=30_000, ended=30_100, code=480, reason="480")
    per, _, unatt = run([late], [w])
    assert [(r["class"], r["code"], r["count"]) for r in per["001-kill_worker"]] == [("setup_failed", 480, 1)]
    assert unatt == []


def test_a_reject_shaped_treatment_timing_out_after_the_window_is_charged_to_it() -> None:
    # A treatment whose only expected ending is a rejection: its pre-answer
    # timeout is an unexpected ending, pending in the window.
    reject = Shape("final", "c", None, [], [], [ExpectedEnding("rejected", 603, None)], True)
    w = window("001-kill_worker", T0 + 60_000, T0 + 85_000)
    impacts = classify({**timed_out("a", T0 + 60_200), "shape": "final"}, reject).impacts
    j = judge([w], impacts, [], {"kill": KILL, "strict": STRICT}, 1000, {"basic": 10.0})
    assert [(r["class"], r["reason"]) for r in j.per_window["001-kill_worker"]] == [("unexpected_ending", "timeout")]
    assert j.unattributed == []


def test_an_answered_calls_late_unexpected_ending_stays_with_the_window_that_holds_it() -> None:
    # Sent into the first window and answered in the second: the setup rule
    # for a call pending in a window covers no answered call.
    w1 = window("001-kill_worker", T0 + 60_000, T0 + 85_000)
    w2 = window("002-kill_worker", T0 + 88_000, T0 + 100_000)
    odd = record("a", T0 + 60_040, "transferred", setup=30_000, answered=30_000, ended=40_000)
    per, in_dialog, unatt = run([odd], [w1, w2])
    assert per["001-kill_worker"] == [] and per["002-kill_worker"] == []
    assert [(i["class"], i["candidate_windows"]) for i in in_dialog] == [("unexpected_ending", ["002-kill_worker"])]
    assert unatt == []


def test_a_setup_no_hop_answered_takes_no_setup_latency() -> None:
    # The generator's giving up is no latency of the system under test: the
    # call is a failed setup, never also a slow one.
    assert classify({**timed_out("a", T0), "first_provisional_at_ms": None}, ANSWER).latency is None
    late = {**timed_out("b", T0), "first_provisional_at_ms": T0 + 1500}
    lat = classify(late, ANSWER).latency
    assert lat is not None and lat.ms == 1500
