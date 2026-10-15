"""The verdict recomputed from a run directory alone: windows, integrity, status."""

from __future__ import annotations

from pathlib import Path

from kit import T0, record, rejected, timed_out
from rundir import SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.verdict import compute


def test_per_window_numbers_match_the_records(tmp_path: Path) -> None:
    recs = [record(f"ok{i}", T0 + i * 50) for i in range(100)]
    recs += [timed_out(f"t{i}", T0 + 80_200 + i) for i in range(30)]
    recs += [rejected("r1", T0 + 400_000, 480, "480")]
    d = run_dir(tmp_path, recs, calls=131, events=kill(1, T0 + 80_000) + kill(2, T0 + 230_000))
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    assert v["streams"] == [
        {
            "stream": "basic",
            "shape": "basic",
            "start": "run",
            "cps": 20.0,
            "calls": 131,
            "not_placed": 0,
            "records": 131,
            "success": 100,
            "stalls": [],
        }
    ]
    w1, w2 = jv.objs(v, "windows")
    assert (w1["opened_at_ms"], w1["closed_at_ms"], w1["status"]) == (T0 + 80_000, T0 + 125_000, "ok")
    (row,) = jv.objs(w1, "impacts")
    assert (row["class"], row["count"], row["seconds"], row["allowed"], row["finding"]) == ("setup_failed", 30, 1.5, 40.0, False)
    assert w2["impacts"] == [] and w2["findings"] == 0
    (u,) = jv.objs(v, "unattributed")
    assert (u["class"], u["code"], u["count"]) == ("setup_failed", 480, 1)
    assert v["status"] == "fail" and v["integrity"] == []


def test_a_clean_run_passes(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("a", T0)], calls=1, events=kill(1, T0 + 80_000))
    v = compute(d, SCHEMAS)
    assert v["status"] == "pass"


def test_records_that_miss_the_counter_make_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("a", T0), record("a", T0 + 5)], calls=3, events=[])
    v = compute(d, SCHEMAS)
    kinds = [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")]
    assert kinds == [
        ("record_gap", "1 records repeat a Call-ID already recorded"),
        ("record_gap", "2 records + 0 not placed != 3 calls the generator counted"),
    ]
    assert v["status"] == "invalid"


def test_a_failed_incident_closes_its_window_at_its_maximum_and_invalidates_the_run(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("a", T0)], calls=1, events=kill(1, T0 + 80_000, failed=True))
    v = compute(d, SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert (w["status"], w["closed_at_ms"]) == ("incident_failed", T0 + 80_000 + 220_000 + 40_000)
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["incident_failed"]
    assert v["status"] == "invalid"


def test_overlapping_windows_and_runner_findings_are_integrity(tmp_path: Path) -> None:
    died = {"kind": "stream_died", "stream": "basic", "at_ms": T0 + 1, "detail": "generator exited"}
    d = run_dir(tmp_path, [record("a", T0)], calls=1, events=kill(1, T0 + 80_000) + kill(2, T0 + 90_000), integrity=[died])
    v = compute(d, SCHEMAS)
    assert [i["kind"] for i in jv.objs(v, "integrity")] == ["stream_died", "window_overlap"]


def test_a_record_outside_the_schema_is_a_generator_failure(tmp_path: Path) -> None:
    bad = {**record("a", T0), "reason": "Busy Here", "ending": "rejected", "answered_at_ms": None, "final_code": 486}
    d = run_dir(tmp_path, [bad], calls=1, events=[])
    v = compute(d, SCHEMAS)
    (i,) = jv.objs(v, "integrity")
    assert i["kind"] == "generator_failure" and str(i["detail"]).startswith("1 records unreadable")


def test_a_generator_counter_that_cannot_be_read_is_a_record_gap(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [record("a", T0)], calls=None, events=[])
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [
        ("record_gap", "the generator's call counter could not be read; 1 records")
    ]


def test_a_window_whose_incident_never_recovered_closes_at_its_maximum(tmp_path: Path) -> None:
    events = kill(1, T0 + 80_000)[:-1]  # inject .. healed, no recovered
    d = run_dir(tmp_path, [record("a", T0)], calls=1, events=events)
    v = compute(d, SCHEMAS)
    (w,) = jv.objs(v, "windows")
    assert (w["status"], w["closed_at_ms"]) == ("incident_failed", T0 + 80_000 + 220_000 + 40_000)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [("incident_failed", "the event never recovered")]

