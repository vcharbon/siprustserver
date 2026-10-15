"""The classifier: a record and its shape to impacts, per the class table of ADR-0049 §3.1."""

from __future__ import annotations

from kit import T0, record, rejected, timed_out

from hakit.classify import classify
from hakit.model import ExpectedEnding, Leg, Shape

ANSWER = Shape("basic", "c", None, [], [], [ExpectedEnding("answered", None, None)], True)
REJECT_486 = Shape("busy", "c", None, [], [], [ExpectedEnding("rejected", 486, None)], True)
FAILOVER = Shape(
    "failover",
    "c",
    None,
    [],
    [
        Leg("primary", ["1"], [ExpectedEnding("rejected", 486, None)]),
        Leg("backup", ["2"], [ExpectedEnding("answered", None, None)]),
    ],
    [ExpectedEnding("answered", None, None)],
    True,
)


def classes(c: object) -> list[str]:
    from hakit.classify import Classified

    assert isinstance(c, Classified)
    return [i.cls for i in c.impacts]


def test_an_answered_call_is_a_success_with_its_setup_latency() -> None:
    c = classify(record("a", T0), ANSWER)
    assert c.success and c.impacts == []
    assert c.latency is not None and (c.latency.at_ms, c.latency.ms) == (T0 + 20, 20)


def test_a_reject_where_an_answer_is_expected_is_setup_failed_keyed_by_code_and_reason() -> None:
    c = classify(rejected("a", T0, 503, "Q.850;cause=34"), ANSWER)
    assert not c.success
    (imp,) = c.impacts
    assert (imp.cls, imp.code, imp.reason, imp.at_ms, imp.budgetable) == ("setup_failed", 503, "Q.850;cause=34", T0 + 100, True)


def test_a_timeout_is_setup_failed_with_code_0_at_its_deadline() -> None:
    (imp,) = classify(timed_out("a", T0), ANSWER).impacts
    assert (imp.cls, imp.code, imp.reason, imp.at_ms) == ("setup_failed", 0, "timeout", T0 + 32_000)


def test_the_latency_without_a_provisional_runs_to_the_setup_end() -> None:
    c = classify(record("a", T0, provisional=None, setup=4000, answered=4000), ANSWER)
    assert c.latency is not None and c.latency.ms == 4000


def test_an_expected_reject_is_a_success_and_another_code_an_unexpected_ending() -> None:
    assert classify(rejected("a", T0, 486, "486"), REJECT_486).success
    (imp,) = classify(rejected("b", T0, 480, "480"), REJECT_486).impacts
    assert (imp.cls, imp.code, imp.budgetable) == ("unexpected_ending", 480, True)


def test_an_answer_where_only_a_reject_is_expected_takes_no_budget() -> None:
    (imp,) = classify(record("a", T0), REJECT_486).impacts
    assert (imp.cls, imp.answered, imp.budgetable) == ("unexpected_ending", True, False)


def test_a_cut_is_charged_at_the_failure_that_cut_it() -> None:
    failure = {"at_ms": T0 + 9000, "kind": "dialog_request", "method": "BYE", "code": 481}
    c = classify(record("a", T0, "cut", failures=[failure]), ANSWER)
    assert classes(c) == ["call_cut", "dialog_request_failed"]
    assert [i.at_ms for i in c.impacts] == [T0 + 9000, T0 + 9000]
    assert not any(i.budgetable for i in c.impacts)


def test_a_stranded_call_and_a_protocol_failure() -> None:
    assert classes(classify(record("a", T0, "stranded"), ANSWER)) == ["call_stranded"]
    failure = {"at_ms": T0 + 50, "kind": "protocol", "detail": "unexpected INFO"}
    c = classify(record("b", T0, "failed", failures=[failure]), ANSWER)
    assert classes(c) == ["unexpected_ending", "protocol_anomaly"]


def test_a_failure_on_an_answered_call_is_counted_and_the_call_still_succeeds() -> None:
    failure = {"at_ms": T0 + 10_000, "kind": "dialog_request", "method": "OPTIONS", "code": 500}
    c = classify(record("a", T0, failures=[failure]), ANSWER)
    assert c.success and classes(c) == ["dialog_request_failed"]


def test_a_generator_failure_is_no_impact_of_the_system_under_test() -> None:
    failure = {"at_ms": T0 + 1, "kind": "generator", "detail": "panic"}
    c = classify(record("a", T0, "failed", answered=None, code=None, reason="protocol", failures=[failure]), ANSWER)
    assert c.generator_failure == "panic" and c.impacts == []


def test_a_leg_that_never_opened_or_ended_otherwise_is_an_unexpected_ending() -> None:
    r = record("a", T0)
    r["legs"] = [{"role": "primary", "ending": "rejected", "final_code": 486}]
    c = classify(r, FAILOVER)
    (imp,) = c.impacts
    assert (imp.cls, imp.reason, imp.code) == ("unexpected_ending", "leg:backup", 0)
    assert not c.success
    r["legs"] = [{"role": "primary", "ending": "rejected", "final_code": 503}, {"role": "backup", "ending": "answered", "final_code": 200}]
    (imp,) = classify(r, FAILOVER).impacts
    assert (imp.reason, imp.code) == ("leg:primary", 503)
    r["legs"] = [{"role": "primary", "ending": "rejected", "final_code": 486}, {"role": "backup", "ending": "answered", "final_code": 200}]
    assert classify(r, FAILOVER).success
