"""The CDR join (ADR-0049 §3.8): every record against the CDRs of its call."""

from __future__ import annotations

from pathlib import Path

from kit import T0, paced, record, rejected
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
    got = details([ANSWERED], [cdr("a", T0, 100, 30_000), cdr("a", T0, 100, 30_001), cdr("a", T0, 100, 30_002)])
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
    assert details([cut], [cdr("c", T0, 100, 9_500)]) == []
    assert details([cut], []) == [("c", T0 + 9_000, "answered call without a CDR")]


def test_the_verdict_charges_mismatches_by_kind_to_their_window_and_outside_as_unattributed(tmp_path: Path) -> None:
    inside = record("in", T0 + 100_000)  # ends at +130 s, inside the window [+100 s, +145 s]
    outside = record("out", T0 + 300_000)
    budget: Json = {"schema": "budget.v1", "name": "cdr", "description": "d", "cdr_mismatch": {"missing": {"max_calls": 1}}}
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
    rows = [(r["reason"], r["count"], r["allowed"], r["finding"], r["call_ids"]) for r in jv.objs(w, "impacts")]
    # The missing CDR is within its kind's allowance; an extra one allows none.
    assert rows == [("extra", 1, 0.0, True, ["ghost"]), ("missing", 1, 1.0, False, ["in"])]
    (u,) = jv.objs(v, "unattributed")
    assert (u["class"], u["reason"], u["count"], u["call_ids"], "stream" in u) == ("cdr_mismatch", "missing", 1, ["out"], False)


def test_a_mismatch_allowance_in_seconds_reads_the_run_rate_beyond_the_outage(tmp_path: Path) -> None:
    # The window's outage is its inject to its recovered: 5 s. At 0.2 cps,
    # (5 s + 5 s) allows 2 missing CDRs; a third is a finding.
    budget: Json = {
        "schema": "budget.v1",
        "name": "sink",
        "description": "d",
        "cdr_mismatch": {"missing": {"max_seconds": 5, "over_outage": True}},
    }
    billed = record("billed", T0)
    lost = [record(f"l{i}", T0 + 100_000 + i * 1_000, ended=1_000) for i in range(3)]

    def rows(sub: str, n: int) -> list[tuple[object, object, object]]:
        d = run_dir(
            tmp_path / sub,
            [billed, *lost[:n]],
            calls=n + 1,
            events=kill(1, T0 + 100_000, budget="sink"),
            budgets=[budget],
            profile={"cdr_join": True, "generator_stall_ms": 3_600_000},
            cdrs=[cdr("billed", T0, 100, 30_000)],
            cps=0.2,
        )
        (w,) = jv.objs(compute(d, SCHEMAS), "windows")
        return [(r["count"], r["allowed"], r["finding"]) for r in jv.objs(w, "impacts")]

    assert rows("two", 2) == [(2, 2.0, False)]
    assert rows("three", 3) == [(3, 2.0, True)]


def test_a_cdr_lost_in_the_sink_just_before_a_window_belongs_to_that_window(tmp_path: Path) -> None:
    # The call ended 0.5 s before the inject: its CDR was still in the sink
    # (2 s in flight) when the fault hit. 3 s before, it was not.
    budget: Json = {"schema": "budget.v1", "name": "sink", "description": "d", "cdr_mismatch": {"missing": {"max_calls": 5}}}
    at = T0 + 100_000
    lost = [record("late", at - 30_500), record("early", at - 33_000)]  # each ends 30 s after its start
    d = run_dir(
        tmp_path,
        [record("billed", T0), *lost],
        calls=3,
        events=kill(1, at, budget="sink"),
        budgets=[budget],
        profile={"cdr_join": True, "generator_stall_ms": 3_600_000, "cdr_in_flight_s": 2},
        cdrs=[cdr("billed", T0, 100, 30_000)],
    )
    v = compute(d, SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert [(r["reason"], r["call_ids"]) for r in jv.objs(w, "impacts")] == [("missing", ["late"])]
    assert [(u["reason"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("missing", ["early"])]


def test_a_mismatch_on_a_call_that_spanned_a_window_and_ended_after_it_carries_every_window_it_overlapped(
    tmp_path: Path,
) -> None:
    """Its CDR may have been changed while the window was open, though the
    call ended after: like an in-dialog impact, it lists the windows of its
    lifetime and takes no budget. One after every window stays unattributed."""
    at = T0 + 100_000  # the window is [+100 s, +145 s]
    spanned = record("spanned", at - 10_000, ended=110_000)  # ends at +200 s
    after = record("after", at + 200_000)
    d = run_dir(
        tmp_path,
        [spanned, after],
        calls=2,
        events=kill(1, at, budget="worker-kill"),
        profile={"cdr_join": True, "generator_stall_ms": 3_600_000},
        cdrs=[cdr("spanned", at - 10_000, 100, 115_000), cdr("after", at + 200_000, 100, 35_000)],
    )
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    assert [(i["class"], i["reason"], i["call_id"], i["candidate_windows"]) for i in jv.objs(v, "in_dialog")] == [
        ("cdr_mismatch", "differs", "spanned", [w["event"]])
    ]
    assert not [r for r in jv.objs(w, "impacts") if r["class"] == "cdr_mismatch"]
    assert [(u["reason"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("differs", ["after"])]


def test_a_duration_within_the_kinds_excess_is_allowed(tmp_path: Path) -> None:
    # A BYE retransmitted once on a lossy plane ends the CDR 1.5 s after the
    # caller's end; excess_ms 1500 past the join's 1 s + unit allows it.
    budget: Json = {"schema": "budget.v1", "name": "lossy", "description": "d", "cdr_mismatch": {"differs": {"excess_ms": 1500}}}
    at = T0 + 100_000
    calls = [record("near", at + 1_000, ended=10_000), record("far", at + 2_000, ended=10_000)]
    d = run_dir(
        tmp_path,
        calls,
        calls=2,
        events=kill(1, at, budget="lossy"),
        budgets=[budget],
        profile={"cdr_join": True, "generator_stall_ms": 3_600_000},
        cdrs=[cdr("near", at + 1_000, 100, 12_400), cdr("far", at + 2_000, 100, 12_600)],
    )
    (w,) = jv.objs(compute(d, SCHEMAS), "windows")
    assert [(r["reason"], r["count"], r["allowed"], r["finding"]) for r in jv.objs(w, "impacts")] == [("differs", 2, 1.0, True)]


def test_the_cdrs_of_an_abandoned_firing_join_its_calls(tmp_path: Path) -> None:
    # An `orphan` firing placed 3 calls, killed with the generator: no record.
    # Each owes one CDR: one has two, one has none, and a CDR of no firing
    # stays a CDR without a record.
    at = T0 + 100_000
    heal: Json = {
        "kind": "facts",
        "event": "001-kill_worker",
        "step": "orphan",
        "executable": "heal",
        "facts": {"stream": "orphaned", "stop": "abandon", "calls": 3, "exited_at_ms": at + 105},
    }
    budget: Json = {"schema": "budget.v1", "name": "loose", "description": "d"}
    d = run_dir(
        tmp_path,
        [ANSWERED],
        calls=1,
        events=[*kill(1, at, budget="loose"), heal],
        budgets=[budget],
        profile={"cdr_join": True, "generator_stall_ms": 3_600_000},
        cdrs=[
            cdr("a", T0, 100, 30_000),
            cdr("7-1-r-001-kill_worker@h", at, 100, 5_000),
            cdr("7-1-r-001-kill_worker@h", at, 100, 6_000),
            cdr("8-1-r-001-kill_worker@h", at, 100, 5_000),
            cdr("ghost", T0, None, 500),
        ],
    )
    v = compute(d, SCHEMAS)
    (w,) = jv.objs(v, "windows")
    rows = {r["reason"]: (r["count"], r["call_ids"]) for r in jv.objs(w, "impacts")}
    assert rows == {"duplicate": (1, ["7-1-r-001-kill_worker@h"]), "missing": (1, [])}
    (u,) = jv.objs(v, "unattributed")
    assert (u["reason"], u["call_ids"]) == ("extra", ["ghost"])


def test_a_cdr_that_differs_from_a_call_measured_across_a_stall_is_spoiled(tmp_path: Path) -> None:
    # 20 cps with no start for 1 s from +60 s: the generator stalled. A call it
    # read the end of in that stall is compared to its CDR by the generator's
    # clock: no impact, a generator_stall finding.
    stall_at = T0 + 60_000
    recs = [r for r in paced("p", T0, T0 + 120_000, 20) if not stall_at < jv.number(r, "started_at_ms") < stall_at + 1000]
    late = record("late", stall_at - 30_100, ended=30_600)  # its end is read 500 ms into the stall
    recs.append(late)
    cdrs = [cdr(jv.string(r, "call_id"), jv.number(r, "started_at_ms"), 100, 30_000) for r in recs if r is not late]
    cdrs.append(cdr("late", stall_at - 30_100, 100, 28_000))
    d = run_dir(tmp_path, recs, calls=len(recs), events=[], profile={"cdr_join": True}, cdrs=cdrs)
    v = compute(d, SCHEMAS)
    assert jv.objs(v, "unattributed") == []
    assert [(i["kind"], i["call_ids"]) for i in jv.objs(v, "integrity")] == [("generator_stall", ["late"])]


def test_cdrs_off_the_schema_or_missing_make_the_run_invalid(tmp_path: Path) -> None:
    on: Json = {"cdr_join": True, "generator_stall_ms": 3_600_000}
    d = run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=on, cdrs=[{**cdr("a", T0, 100, 30_000), "disposition": "?"}])
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("class_failed", "cdr_mismatch: 1 observations break cdr-observation.v1"),
        ("class_failed", "cdr_mismatch: no CDR observation for 1 answered calls; is the consumer shipping?"),
    ]
    (tmp_path / "run").rename(tmp_path / "first")
    v = compute(run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=on), SCHEMAS)
    assert [i["detail"] for i in jv.objs(v, "integrity")] == ["cdr_mismatch: the run directory holds no CDR export"]


def test_without_cdr_join_no_cdr_is_read(tmp_path: Path) -> None:
    v = compute(run_dir(tmp_path, [ANSWERED], calls=1, events=[], cdrs=[]), SCHEMAS)
    assert v["status"] == "pass"


def test_a_byte_identical_observation_counts_once_and_a_distinct_second_cdr_is_a_mismatch() -> None:
    first = cdr("a", T0, 100, 30_000)
    assert details([ANSWERED], [first, dict(first)]) == []
    assert details([ANSWERED], [first, {**first, "end_ms": T0 + 30_001}]) == [("a", T0 + 30_000, "CDR 2 of the call")]


def test_a_cut_call_s_cdr_may_not_end_after_the_caller_s_end() -> None:
    cut = record("c", T0, "cut", ended=9_000, failures=[{"at_ms": T0 + 9_000, "kind": "dialog_request", "method": "BYE", "code": 481}])
    assert details([cut], [cdr("c", T0, 100, 10_001)]) == []  # 1 s + 1 ms unit after
    assert details([cut], [cdr("c", T0, 100, 1_800_000)]) == [
        ("c", T0 + 9_000, "CDR ends 1791000 ms after the caller's end (allowed 1001 ms)")
    ]
    stranded = record("s", T0, "stranded", ended=60_000)
    assert details([stranded], [cdr("s", T0, 100, 59_000)]) == []


ON: Json = {"cdr_join": True, "generator_stall_ms": 3_600_000}


def collected(complete: bool, written: float, observed: float, unread: float = 0) -> Json:
    return {"complete": complete, "written": written, "observed": observed, "unread": unread}


def test_no_observation_at_all_for_answered_calls_is_a_measurement_failure(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=ON, cdrs=[], cdr_collect=collected(True, 0, 0))
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("class_failed", "cdr_mismatch: no CDR observation for 1 answered calls; is the consumer shipping?")
    ]
    assert v["unattributed"] == []


def test_cdrs_still_owed_at_the_cut_are_an_incomplete_export_not_impacts(tmp_path: Path) -> None:
    second = record("b", T0 + 1_000)
    d = run_dir(tmp_path, [ANSWERED, second], calls=2, events=[], profile=ON, cdrs=[cdr("a", T0, 100, 30_000)], cdr_collect=collected(False, 2, 1))
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("class_failed", "cdr_mismatch: the export was cut with CDRs still owed: 2 written, 1 observed")
    ]
    assert v["unattributed"] == []


def test_payloads_the_consumer_could_not_read_make_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [ANSWERED], calls=1, events=[], profile=ON, cdrs=[cdr("a", T0, 100, 30_000)], cdr_collect=collected(True, 1, 1, unread=3))
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("class_failed", "cdr_mismatch: the consumer could not read 3 CDR payloads")
    ]


def test_a_refused_call_the_profile_requires_a_cdr_for_is_charged_without_one(tmp_path: Path) -> None:
    bare = rejected("bare", T0 + 101_000, 503, "503")
    retry = {**rejected("retry", T0 + 101_000, 503, "503"), "retry_after_s": 5}
    busy = rejected("busy", T0 + 101_000, 486, "486")
    budget: Json = {"schema": "budget.v1", "name": "cdr", "description": "d",
                    "setup_failed": [{"stream": "*", "code": 503, "reason": "503", "max_calls": 9},
                                     {"stream": "*", "code": 486, "reason": "486", "max_calls": 9}]}
    profile: Json = {
        "cdr_join": True,
        "generator_stall_ms": 3_600_000,
        "cdr_required": [{"ending": "rejected", "code": 503, "retry_after": False}],
    }
    d = run_dir(
        tmp_path,
        [ANSWERED, bare, retry, busy],
        calls=4,
        events=kill(1, T0 + 100_000, budget="cdr"),
        budgets=[budget],
        profile=profile,
        cdrs=[cdr("a", T0, 100, 30_000)],
    )
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    mismatch = [r for r in jv.objs(w, "impacts") if r["class"] == "cdr_mismatch"]
    assert [(r["count"], r["call_ids"], r["finding"]) for r in mismatch] == [(1, ["bare"], True)]
