"""Attribution edges, generator counters and the human summary of a verdict."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record, rejected, timed_out
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.jsonval import Json
from hakit.summary import render
from hakit.verdict import compute
from hakit.windows import event_line, phase_line


def detected(seq: int, at: int, budget: str = "") -> list[Json]:
    ev = f"d{seq:03d}-stream_died"
    return [
        event_line(ev, "detected", ["stream_died"], budget, 40, 0),
        phase_line(ev, "stream_died", "inject", at),
        phase_line(ev, "stream_died", "recovered", at),
    ]


def test_an_impact_on_the_close_instant_is_in_the_window_and_one_after_is_not(tmp_path: Path) -> None:
    close = T0 + 100_000 + 5_000 + 40_000
    # A timeout's instant is its deadline: started 32 s before. One sent after
    # the close was never pending in the window.
    recs = [timed_out("edge", close - 32_000), timed_out("after", close + 1)]
    v = compute(run_dir(tmp_path, recs, calls=2, events=kill(1, T0 + 100_000)), SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert w["closed_at_ms"] == close
    assert [(r["call_ids"]) for r in jv.objs(w, "impacts")] == [["edge"]]
    assert [u["call_ids"] for u in jv.objs(v, "unattributed")] == [["after"]]


def test_a_setup_that_never_ended_is_charged_at_the_record_s_end(tmp_path: Path) -> None:
    failure = {"at_ms": T0 + 500, "kind": "transport", "detail": "send failed"}
    r = record("f", T0 + 100_500, "failed", provisional=None, setup=None, answered=None, code=None, reason="transport", ended=600, failures=[failure])
    v = compute(run_dir(tmp_path, [r], calls=1, events=kill(1, T0 + 100_000)), SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert [(x["class"], x["code"], x["reason"], x["call_ids"]) for x in jv.objs(w, "impacts")] == [
        ("setup_failed", 0, "transport", ["f"])
    ]
    # Its protocol failure came 100 s before the window: no candidate window.
    assert [(u["class"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("protocol_anomaly", ["f"])]


def test_a_detected_window_without_a_defaults_entry_allows_nothing(tmp_path: Path) -> None:
    recs = [rejected("r", T0 + 10_000, 503, "503")]
    v = compute(run_dir(tmp_path, recs, calls=1, events=detected(1, T0 + 10_000)), SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    assert (w["source"], w["budget"], w["closed_at_ms"]) == ("detected", "", T0 + 50_000)
    assert [(r["class"], r["count"], r["allowed"], r["finding"]) for r in jv.objs(w, "impacts")] == [
        ("setup_failed", 1, 0, True)
    ]
    assert v["status"] == "fail"


def test_a_detected_window_inside_a_scheduled_one_is_a_window_overlap(tmp_path: Path) -> None:
    events = kill(1, T0 + 100_000) + detected(1, T0 + 110_000)
    v = compute(run_dir(tmp_path, [record("a", T0)], calls=1, events=events), SCHEMAS)
    assert [(i["kind"], i.get("event")) for i in jv.objs(v, "integrity")] == [("window_overlap", "d001-stream_died")]


def test_calls_the_generator_failed_reconcile_and_make_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("a", T0)], calls=3, events=[], stream={"generator_failures": 2})
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("generator_failure", "the generator failed 2 calls without a record")
    ]
    (s,) = jv.objs(v, "streams")
    assert s["generator_failures"] == 2
    (tmp_path / "run").rename(tmp_path / "first")
    d = run_dir(tmp_path, [record("a", T0)], calls=4, events=[], stream={"generator_failures": 2})
    assert [i["detail"] for i in jv.objs(compute(d, SCHEMAS), "integrity")][1] == (
        "1 records + 0 not placed + 2 failed by the generator != 4 calls the generator counted"
    )


def test_the_summary_says_why_then_each_window_class_and_stream(tmp_path: Path) -> None:
    recs = [record("ok", T0), timed_out("t", T0 + 100_100), rejected("r", T0 + 400_000, 480, "480")]
    v = compute(run_dir(tmp_path, recs, calls=3, events=kill(1, T0 + 100_000)), SCHEMAS)
    text = render(v)
    why = text.split("## Why\n\n")[1].split("\n\n")[0]
    assert why == "- unattributed: `setup_failed` basic 480 480: 1 (0.05 s of offered rate), allowed 0 — FINDING"
    assert "| basic | basic | 20.0 | 3 | 3 | 1 | 0 (0) |" in text
    assert "### 001-kill_worker (schedule, ok, budget `worker-kill`)" in text
    assert "- `setup_failed` basic 0 timeout: 1 (0.05 s of offered rate), allowed 40.0 — ok" in text
    clean = compute(run_dir(tmp_path / "b", [record("ok", T0)], calls=1, events=[]), SCHEMAS)
    assert "- every impact within its allowance, measurement intact" in render(clean)


def test_a_window_with_in_dialog_candidates_says_so_in_why_and_in_its_section(tmp_path: Path) -> None:
    # A call up across the kill is cut after it: no budget row of the window
    # finds anything, yet its in-dialog impacts fail the run.
    bye = [{"at_ms": T0 + 102_000, "kind": "dialog_request", "method": "BYE", "code": 481}]
    cut = record("cut", T0 + 90_000, "cut", ended=12_000, failures=bye)
    v = compute(run_dir(tmp_path, [cut], calls=1, events=kill(1, T0 + 100_000)), SCHEMAS)
    text = render(v)
    why = text.split("## Why\n\n")[1].split("\n\n")[0]
    assert "- window 001-kill_worker is a candidate of 2 in-dialog impacts: `call_cut` basic x1, `dialog_request_failed` basic x1 — FINDING" in why
    assert "0 findings, candidate of 2 in-dialog impacts" in text
    assert "- in-dialog `call_cut` basic: 1 (this window a candidate) — FINDING" in text


def test_the_verdict_keeps_50_call_ids_a_row_and_the_full_list_apart(tmp_path: Path) -> None:
    from hakit.verdict import compute_with_calls

    recs = [rejected(f"r{i}", T0 + 400_000 + i, 480, "480") for i in range(60)]
    v, calls = compute_with_calls(run_dir(tmp_path, recs, calls=60, events=[]), SCHEMAS)
    (u,) = jv.objs(v, "unattributed")
    assert len(jv.strings(u, "call_ids")) == 50 and "all_call_ids" not in u
    (kept,) = calls
    assert (kept["window"], kept["class"], len(jv.strings(kept, "call_ids"))) == (None, "setup_failed", 60)
