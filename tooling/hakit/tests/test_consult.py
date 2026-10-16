"""A leg opened only after an in-dialog consult (ADR-0049 §3.2): its consult's
contract refusal on an otherwise clean call takes a budget; nothing else in the
call does, and nothing outside a window that allows it."""

from __future__ import annotations

from kit import T0, record

from hakit import jsonval as jv
from hakit.classify import Impact, classify
from hakit.judge import Judged, judge
from hakit.jsonval import Json
from hakit.model import (
    AfterConsult,
    Allowance,
    Budget,
    ExpectedEnding,
    Leg,
    Refusal,
    SetupSlowAllowance,
    Shape,
)
from hakit.windows import Phase, Window

ANSWERED = [ExpectedEnding("answered", None, None)]
SIPFRAG_503 = "5349502f322e3020353033"  # "SIP/2.0 503"
CONSULT = AfterConsult("asker", [Refusal("NOTIFY", "terminated", SIPFRAG_503)])


def shape(flagged: bool = True) -> Shape:
    legs = [
        Leg("asker", ["1"], ANSWERED),
        Leg("target", ["2"], ANSWERED, after_consult=CONSULT if flagged else None),
    ]
    return Shape("xfer", "c", None, [], legs, ANSWERED, True)


DECISION = Budget(
    "decision",
    setup_failed=[],
    unexpected_ending=[Allowance("xfer", 0, "leg:target", 5, None)],
    setup_slow=SetupSlowAllowance(None, None, None),
    raw={},
)

WINDOW = Window(
    "001-decision_fault", "schedule", ["decision_fault"], "decision", "ok", T0, T0 + 60_000,
    [Phase("inject", T0, "decision_fault"), Phase("recovered", T0 + 30_000, "decision_fault")],
)


def call(
    cid: str,
    *,
    state: str = "terminated",
    frag: str = SIPFRAG_503,
    started: int = T0 + 1_000,
    failures: list[Json] | None = None,
) -> Json:
    r = record(cid, started, stream="xfer", shape="xfer", ended=20_000, failures=failures)
    r["legs"] = [
        {
            "role": "asker",
            "ending": "answered",
            "final_code": 200,
            "received": [{"at_ms": started + 6_000, "method": "NOTIFY", "subscription_state": state, "body_hex": frag + "20"}],
        }
    ]
    return r


def run(records: list[Json], flagged: bool = True, windows: list[Window] | None = None) -> Judged:
    impacts: list[Impact] = []
    for r in records:
        impacts += classify(r, shape(flagged)).impacts
    return judge([WINDOW] if windows is None else windows, impacts, [], {"decision": DECISION}, 1000, {"xfer": 1.0})


def rows(j: Judged) -> list[tuple[str, int, float, bool]]:
    return [
        (jv.string(r, "reason"), int(jv.number(r, "count")), jv.number(r, "allowed"), r["finding"] is True)
        for r in j.per_window["001-decision_fault"]
    ]


def test_the_contract_refusal_on_a_clean_call_takes_the_window_s_budget() -> None:
    j = run([call("a")])
    assert rows(j) == [("leg:target", 1, 5.0, False)]
    assert j.in_dialog == [] and j.unattributed == []
    assert j.per_window["001-decision_fault"][0]["call_ids"] == ["a"]


def test_a_leg_without_the_flag_is_never_absorbed() -> None:
    j = run([call("a")], flagged=False)
    assert rows(j) == []
    assert [(i["class"], i["detail"]) for i in j.in_dialog] == [("unexpected_ending", "leg never opened")]


def test_an_answer_other_than_the_contract_s_is_a_finding() -> None:
    other = call("a", frag="5349502f322e3020353030")  # "SIP/2.0 500"
    live = call("b", state="active")
    j = run([other, live])
    assert rows(j) == []
    assert [i["detail"] for i in j.in_dialog] == ["leg never opened; its consult drew no contract refusal"] * 2


def test_a_failed_re_invite_or_bye_afterwards_keeps_the_call_a_finding() -> None:
    reinvite = call("a", failures=[{"at_ms": T0 + 9_000, "kind": "dialog_request", "method": "INVITE", "code": 503}])
    bye = call("b", failures=[{"at_ms": T0 + 21_000, "kind": "dialog_request", "method": "BYE", "code": 503}])
    j = run([reinvite, bye])
    assert rows(j) == []
    got = sorted((i["call_id"], i["class"]) for i in j.in_dialog)
    assert got == [
        ("a", "dialog_request_failed"),
        ("a", "unexpected_ending"),
        ("b", "dialog_request_failed"),
        ("b", "unexpected_ending"),
    ]


def test_outside_a_window_that_allows_it_the_refusal_is_a_finding() -> None:
    late = call("a", started=T0 + 120_000)
    j = run([late])
    assert [(u["class"], u["reason"], u["count"], u["finding"]) for u in j.unattributed] == [
        ("unexpected_ending", "leg:target", 1, True)
    ]
    other = Window("001-decision_fault", "schedule", ["kill_worker"], "strict", "ok", T0, T0 + 60_000, [Phase("inject", T0, "kill_worker")])
    strict = Budget("strict", [], [], SetupSlowAllowance(None, None, None), raw={})
    impacts = classify(call("b"), shape()).impacts
    k = judge([other], impacts, [], {"strict": strict}, 1000, {"xfer": 1.0})
    assert [(r["reason"], r["allowed"], r["finding"]) for r in k.per_window["001-decision_fault"]] == [("leg:target", 0, True)]


def test_the_asking_leg_s_own_failed_request_is_named() -> None:
    # The transferor's REFER answered 503 by a hop: no consult ran, the call is
    # a finding, and the failure says whose request failed.
    refer: Json = {"at_ms": T0 + 7_000, "kind": "dialog_request", "method": "REFER", "code": 503, "leg": "asker"}
    r = call("a", failures=[refer])
    jv.objs(r, "legs")[0].pop("received")
    j = run([r])
    assert rows(j) == []
    assert sorted((i["class"], i.get("detail")) for i in j.in_dialog) == [
        ("dialog_request_failed", "asker REFER 503"),
        ("unexpected_ending", "leg never opened"),
    ]
