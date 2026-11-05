"""Every call that did not end answered falls in one cause and one place, per
stream and for priority calls against the others; the verdict carries it and
the summary renders it without changing the status."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record, rejected, timed_out
from rundir import CONTENT, SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.model import Budget, ExpectedEnding, Shape, parse_budget
from hakit.profile import Profile, parse_profile
from hakit.summary import priority_verdict, render
from hakit.unanswered import Places, cause_of, is_priority, unanswered
from hakit.verdict import compute
from hakit.windows import Window

SHAPES = {s.shape: s for s in CONTENT.catalogs["x-api-call"].shapes}
BASIC = SHAPES["basic"]
ANNOUNCE = Shape("announce", "c", None, [], [], [ExpectedEnding("rejected", 603, True)], True)


def budget(name: str, retry_after: bool | None) -> Json:
    entry: Json = {"stream": "*", "code": 503, "reason": "*", "max_calls": 100}
    if retry_after is not None:
        entry["retry_after"] = retry_after
    return {"schema": "budget.v1", "name": name, "description": "d", "setup_failed": [entry]}


BUDGETS: dict[str, Budget] = {
    "bare": parse_budget(budget("bare", False)),
    "any": parse_budget(budget("any", None)),
    "ra": parse_budget(budget("ra", True)),
}
FAULT = Window("001-fault", "schedule", ["fault"], "bare", "ok", T0 + 10_000, T0 + 20_000)
SPIKE = Window("002-spike", "schedule", ["spike"], "any", "ok", T0 + 40_000, T0 + 50_000)
STARVE = Window("003-starve", "schedule", ["starve"], "ra", "ok", T0 + 60_000, T0 + 70_000)
KILL = Window("004-kill", "schedule", ["kill"], "", "ok", T0 + 80_000, T0 + 90_000)
PLACES = Places([SPIKE, FAULT, STARVE, KILL], BUDGETS)


def refused(call_id: str, started: float, retry_after: int | None, setup: float = 100, stream: str = "basic") -> Json:
    r = rejected(call_id, started, 503, "503", stream=stream)
    r["setup_ended_at_ms"] = started + setup
    r["retry_after_s"] = retry_after
    return r


def profile(*streams: tuple[str, str, str]) -> Profile:
    raw = CONTENT.profiles["kill-worker-smoke"].raw
    base = jv.objs(raw, "streams")[0]
    return parse_profile({**raw, "streams": [{**base, "name": n, "shape": sh, "start": st} for n, sh, st in streams]})


def cause(r: Json, shape: Shape = BASIC) -> tuple[str, str, str] | None:
    c = cause_of(r, shape, PLACES)
    return None if c is None else (c.cause, c.detail, c.where)


def test_a_bare_503_whose_setup_ends_inside_a_window_is_a_dependency_fault_of_its_kind() -> None:
    assert cause(refused("a", T0 + 9_000, None, setup=1_500)) == ("dependency_fault", "rejected 503", "fault")
    assert cause(refused("b", T0 + 8_000, None, setup=1_500)) == ("other", "rejected 503", "outside")


def test_retry_after_tells_an_overload_refusal_from_a_dependency_fault() -> None:
    assert cause(refused("a", T0 + 12_000, 5)) == ("overload", "rejected 503", "fault")
    assert cause(refused("b", T0 + 30_000, 0)) == ("overload", "rejected 503", "outside")
    assert cause(refused("c", T0 + 41_000, None)) == ("dependency_fault", "rejected 503", "spike")
    absent = refused("d", T0 + 12_000, None)
    del absent["retry_after_s"]
    assert cause(absent) == ("dependency_fault", "rejected 503", "fault")


def test_a_bare_503_is_a_dependency_fault_only_where_the_window_s_budget_allows_one() -> None:
    assert cause(refused("a", T0 + 61_000, None)) == ("other", "503 without Retry-After in starve window", "starve")
    assert cause(refused("b", T0 + 81_000, None)) == ("other", "503 without Retry-After in kill window", "kill")
    assert cause(refused("c", T0 + 61_000, 2)) == ("overload", "rejected 503", "starve")


def test_every_other_ending_falls_in_one_cause() -> None:
    assert cause(record("ok", T0)) is None
    assert cause(rejected("lim", T0 + 11_000, 486, "486"), SHAPES["limited"]) == ("expected", "rejected 486", "fault")
    assert cause(timed_out("t", T0 + 45_000)) == ("setup_timeout", "timeout", "spike")
    assert cause(rejected("r", T0 + 30_000, 480, "480")) == ("other", "rejected 480", "outside")
    assert cause(record("u", T0 + 30_000), ANNOUNCE) == ("other", "answered 200", "outside")
    no_setup = timed_out("n", T0 + 9_000)
    del no_setup["setup_ended_at_ms"]
    no_setup["ended_at_ms"] = T0 + 11_000
    assert cause(no_setup) == ("setup_timeout", "timeout", "fault")
    generator = [{"at_ms": T0 + 30_100, "kind": "generator", "detail": "d"}]
    assert cause(record("g", T0 + 30_000, failures=generator)) == ("other", "generator", "outside")


def test_a_cut_is_placed_at_its_cut_instant() -> None:
    bye = [{"at_ms": T0 + 15_000, "kind": "dialog_request", "method": "BYE", "code": 481}]
    assert cause(record("c", T0, "cut", ended=16_000, failures=bye)) == ("cut", "cut 200", "fault")
    stranded = record("s", T0 + 30_000, "stranded", ended=12_000, failures=[{"at_ms": T0 + 31_000, "kind": "protocol"}])
    assert cause(stranded) == ("cut", "stranded 200", "spike")
    failed = record("f", T0 + 30_000, "failed", ended=2_000, failures=[{"at_ms": T0 + 31_000, "kind": "protocol"}])
    assert cause(failed) == ("cut", "failed 200", "outside")
    early = record("e", T0 + 30_000, "failed", answered=None, code=None, ended=200)
    assert cause(early) == ("other", "failed", "outside")


def test_a_final_its_shape_expects_with_other_early_media_says_so() -> None:
    plain = rejected("a", T0 + 30_000, 603, "603")
    assert cause(plain, ANNOUNCE) == ("other", "rejected 603 without early media", "outside")
    assert cause({**plain, "early_media": True}, ANNOUNCE) == ("expected", "rejected 603", "outside")


def test_priority_streams_against_the_other_run_streams_incident_streams_in_neither() -> None:
    assert is_priority(SHAPES["basic_em"]) and not is_priority(BASIC)
    prof = profile(("plain", "basic", "run"), ("prio", "basic_em", "run"), ("burst", "basic_em", "incident"))
    recs = [
        (record("p1", T0, stream="plain"), BASIC),
        (refused("p2", T0 + 12_000, None, stream="plain"), BASIC),
        (refused("p3", T0 + 30_000, 3, stream="plain"), BASIC),
        (record("e1", T0, stream="prio"), SHAPES["basic_em"]),
        (refused("e2", T0 + 12_000, None, stream="prio"), SHAPES["basic_em"]),
        (refused("b1", T0 + 42_000, 3, stream="burst"), SHAPES["basic_em"]),
    ]
    u = unanswered(recs, [FAULT, SPIKE], BUDGETS, prof, SHAPES)
    assert [(s["stream"], s["start"], s["priority"], s["calls"]) for s in jv.objs(u, "streams")] == [
        ("plain", "run", False, 3),
        ("prio", "run", True, 2),
        ("burst", "incident", True, 1),
    ]
    prio, others = jv.objs(u, "groups")
    assert (prio["group"], prio["streams"], prio["calls"], prio["answered"]) == ("priority", ["prio"], 2, 1)
    assert prio["causes"] == [{"cause": "dependency_fault", "detail": "rejected 503", "count": 1, "where": {"fault": 1}}]
    assert prio["answered_share"] == 1.0
    assert (others["group"], others["streams"], others["calls"]) == ("others", ["plain"], 3)
    assert [c["cause"] for c in jv.objs(others, "causes")] == ["overload", "dependency_fault"]
    assert others["answered_share"] == 0.5


def test_the_verdict_carries_it_and_the_summary_answers_the_priority_question(tmp_path: Path) -> None:
    recs = [record(f"ok{i}", T0 + i * 50) for i in range(20)]
    recs += [refused(f"d{i}", T0 + 80_200 + i, None) for i in range(4)] + [refused("o", T0 + 400_000, 2)]
    recs += [refused("k", T0 + 230_200, None)]
    events = kill(1, T0 + 80_000, budget="bare") + kill(2, T0 + 230_000)
    d = run_dir(tmp_path, recs, calls=26, events=events, budgets=[budget("bare", False)])
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (s,) = jv.objs(jv.obj(v, "unanswered"), "streams")
    assert [(c["cause"], c["count"], c["where"]) for c in jv.objs(s, "causes")] == [
        ("overload", 1, {"outside": 1}),
        ("dependency_fault", 4, {"kill_worker": 4}),
        ("other", 1, {"kill_worker": 1}),
    ]
    assert s["answered_share"] == round(20 / 22, 7)
    text = render(v)
    assert "| basic | 26 | 20 | 0 | 4 | 1 | 0 | 0 | 0 | 1 | 90.9091% |" in text
    assert "- basic: overload (503 + Retry-After) rejected 503: 1 (outside 1); dependency fault (503) rejected 503: 4 (kill_worker 4)" in text
    assert "No `run` stream places priority calls." in text


def test_the_priority_sentence_names_each_loss() -> None:
    group: Json = {"streams": ["prio"], "causes": [{"cause": "dependency_fault", "detail": "rejected 503", "count": 3, "where": {}}]}
    assert priority_verdict(group) == (
        "No priority call was refused for overload, left with a provisional and no final, timed out, cut or ended otherwise."
    )
    group["causes"] = [
        {"cause": "overload", "detail": "rejected 503", "count": 2, "where": {"spike": 2}},
        {"cause": "cut", "detail": "cut 200", "count": 1, "where": {"outside": 1}},
        {"cause": "cut", "detail": "stranded 200", "count": 3, "where": {"fault": 2, "spike": 1}},
    ]
    assert priority_verdict(group) == (
        "Priority calls: 2 refused for overload (spike 2), none left with a provisional and no final, none timed out, "
        "4 cut or never released (outside 1; fault 2, spike 1)."
    )
    assert priority_verdict({"streams": []}) == "No `run` stream places priority calls."


def test_the_priority_sentence_never_says_none_lost_while_a_priority_call_ended_otherwise() -> None:
    other: Json = {"cause": "other", "detail": "503 without Retry-After in kill window", "count": 2, "where": {"kill": 2}}
    group: Json = {"streams": ["prio"], "causes": [other]}
    assert priority_verdict(group) == (
        "Priority calls: none refused for overload, none left with a provisional and no final, none timed out, none cut or never released, "
        "and 2 ended otherwise (503 without Retry-After in kill window: kill 2)."
    )


def test_a_long_window_still_holds_a_call_after_a_shorter_later_one_closed() -> None:
    long = Window("001-long", "schedule", ["long"], "b", "incident_failed", T0, T0 + 100_000)
    short = Window("002-short", "schedule", ["short"], "b", "ok", T0 + 20_000, T0 + 30_000)
    places = Places([short, long], {})
    assert (places.where(T0 + 50_000, T0 + 50_100), places.where(T0 + 25_000, T0 + 25_000)) == ("long", "long")
    assert places.where(T0 + 100_001, T0 + 100_002) == "outside"
