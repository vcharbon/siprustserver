"""The CDR join (ADR-0049 §3.8): every record against the CDRs of its call."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record, rejected
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.cdrjoin import join
from hakit.jsonval import Json
from hakit.verdict import compute


def cdr(call_id: str, start: float, answer: float | None, end: float, unit: float = 1, **over: object) -> Json:
    """The CDR of a call, its instants offsets in ms from `start`."""
    duration = 0 if answer is None else end - answer
    return {
        "schema": "cdr-observation.v1",
        "call_id": call_id,
        "start_ms": start,
        "answer_ms": None if answer is None else start + answer,
        "end_ms": start + end,
        "duration_ms": duration,
        "duration_unit_ms": unit,
        "disposition": "not_answered" if answer is None else "answered",
        **over,
    }


def details(records: list[Json], cdrs: list[Json]) -> list[tuple[str, float, str]]:
    return [(i.call_id, i.at_ms, i.detail) for i in join(records, cdrs)]


ANSWERED = record("a", T0)  # answered at +100 ms, ended at +30 s: 29.9 s


def test_a_matching_cdr_is_no_mismatch_within_1_s_plus_its_unit() -> None:
    assert details([ANSWERED], [cdr("a", T0, 100, 30_000)]) == []
    assert details([ANSWERED], [cdr("a", T0, 100, 31_000)]) == []  # 1 s apart, unit 1 ms
    assert details([ANSWERED], [cdr("a", T0, 100, 31_002)]) == [
        ("a", T0 + 30_000, "CDR duration 30902 ms, the caller's 29900 ms (allowed 1001 ms apart)")
    ]
    # Whole-second CDRs: 1 s + 1000 ms of rounding.
    assert details([ANSWERED], [cdr("a", T0, 100, 31_900, unit=1000, duration_ms=31_000)]) == []
    assert details([ANSWERED], [cdr("a", T0, 100, 31_900, unit=1000, duration_ms=31_901)]) != []


def test_an_answered_call_without_a_cdr_is_charged_at_its_end_and_an_unanswered_one_is_not() -> None:
    assert details([ANSWERED], []) == [("a", T0 + 30_000, "answered call without a CDR")]
    assert details([rejected("r", T0, 486, "486")], []) == []


def test_a_cdr_without_a_record_is_charged_at_its_own_end() -> None:
    assert details([], [cdr("x", T0, None, 700)]) == [("x", T0 + 700, "CDR without a record")]


def test_every_cdr_after_the_first_of_a_call_is_a_mismatch() -> None:
    got = details([ANSWERED], [cdr("a", T0, 100, 30_000), cdr("a", T0, 100, 30_000), cdr("a", T0, 100, 30_000)])
    assert got == [("a", T0 + 30_000, "CDR 2 of the call"), ("a", T0 + 30_000, "CDR 3 of the call")]


def test_a_disposition_that_differs_is_a_mismatch() -> None:
    assert details([ANSWERED], [cdr("a", T0, None, 30_000)]) == [
        ("a", T0 + 30_000, "CDR not_answered, the caller saw the call answered")
    ]
    assert details([rejected("r", T0, 486, "486")], [cdr("r", T0, 50, 200)]) == [
        ("r", T0 + 200, "CDR answered, the caller saw the call not answered")
    ]


def test_a_cut_call_compares_presence_and_disposition_only() -> None:
    cut = record("c", T0, "cut", ended=9_000, failures=[{"at_ms": T0 + 9_000, "kind": "dialog_request", "method": "BYE", "code": 481}])
    assert details([cut], [cdr("c", T0, 100, 40_000)]) == []
    assert details([cut], []) == [("c", T0 + 9_000, "answered call without a CDR")]


def test_the_verdict_charges_mismatches_to_their_window_and_outside_as_unattributed(tmp_path: Path) -> None:
    inside = record("in", T0 + 100_000)  # ends at +130 s, inside the window [+100 s, +145 s]
    outside = record("out", T0 + 300_000)
    budget: Json = {"schema": "budget.v1", "name": "cdr", "description": "d", "cdr_mismatch": {"max_calls": 1}}
    d = run_dir(
        tmp_path,
        [inside, outside],
        calls=2,
        events=kill(1, T0 + 100_000, budget="cdr"),
        budgets=[budget],
        profile={"cdr_join": True, "generator_stall_ms": 3_600_000},
        cdrs=[cdr("ghost", T0 + 101_000, None, 0)],
    )
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    rows = [(r["class"], r["count"], r["allowed"], r["finding"], r["call_ids"]) for r in jv.objs(w, "impacts")]
    assert rows == [("cdr_mismatch", 2, 1.0, True, ["in", "ghost"])]
    (u,) = jv.objs(v, "unattributed")
    assert (u["class"], u["count"], u["call_ids"], "stream" in u) == ("cdr_mismatch", 1, ["out"], False)


def test_cdrs_off_the_schema_or_missing_make_the_run_invalid(tmp_path: Path) -> None:
    on: Json = {"cdr_join": True, "generator_stall_ms": 3_600_000}
    d = run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=on, cdrs=[{**cdr("a", T0, 100, 30_000), "disposition": "?"}])
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("class_failed", "cdr_mismatch: 1 observations break cdr-observation.v1")
    ]
    (tmp_path / "run").rename(tmp_path / "first")
    v = compute(run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=on), SCHEMAS)
    assert [i["detail"] for i in jv.objs(v, "integrity")] == ["cdr_mismatch: the run directory holds no cdrs.jsonl"]


def test_without_cdr_join_no_cdr_is_read(tmp_path: Path) -> None:
    v = compute(run_dir(tmp_path, [ANSWERED], calls=1, events=[], cdrs=[]), SCHEMAS)
    assert v["status"] == "pass"
