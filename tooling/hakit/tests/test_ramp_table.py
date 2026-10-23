"""`hakit ramp-table`: the steps of a run's stepped streams, measured from its
records and the worker's series, then judged."""

from __future__ import annotations

import json
import urllib.parse
from pathlib import Path

from kit import T0, paced, rejected, write_jsonl
from rundir import run_dir

from hakit import jsonval as jv
from hakit.executor import Call, FakeExecutor, HttpResult
from hakit.jsonval import Json
from hakit.readers.ramp_table import RampSpec, build, spans, table
from hakit.readers.probe_live import read_files
from hakit.readers.vmrange import Vm

STEPS: Json = {"steps": [{"at_s": 120, "cps": 20}]}


def worker(accepted: dict[int, float], rejected_cps: dict[int, float]) -> FakeExecutor:
    """The worker's counters per step end (seconds after T0)."""

    def rule(c: Call) -> HttpResult | None:
        if c.kind != "http":
            return None
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query)
        expr, at = q["query"][0], int(float(q["time"][0]) * 1000 - T0) // 1000
        if 'outcome="accepted"' in expr:
            v = accepted[at] * 60
        elif 'outcome="rejected"' in expr:
            v = rejected_cps[at] * 60
        elif "container_cpu_usage" in expr:
            v = accepted[at] / 100
        else:
            return HttpResult(200, json.dumps({"status": "success", "data": {"result": []}}))
        return HttpResult(200, json.dumps({"status": "success", "data": {"result": [{"metric": {}, "value": [0, str(v)]}]}}))

    return FakeExecutor([rule])


def ramp_dir(tmp_path: Path) -> Path:
    # 10 cps for 120 s, then 20 cps whose last 2 cps are refused 503.
    records = paced("a", T0, T0 + 120_000, 10, provisional=20) + paced("b", T0 + 120_000, T0 + 240_000, 18, provisional=20)
    records += [rejected(f"r{i}", T0 + 120_000 + i * 500, 503, "503") for i in range(240)]
    d = run_dir(tmp_path, records, calls=len(records), events=[], cps=10)
    prof = json.loads((d / "profile.json").read_text())
    prof["duration_s"] = 240
    prof["streams"][0] |= STEPS
    (d / "profile.json").write_text(json.dumps(prof))
    write_jsonl(d / "records.jsonl", records)
    return d


def test_the_steps_are_the_stepped_streams_rate_changes_after_their_settle(tmp_path: Path) -> None:
    files = read_files(ramp_dir(tmp_path))
    got = [(s.label, s.start_ms - T0, s.end_ms - T0) for s in spans(files.profile, T0, 60)]
    assert got == [("+0s basic 10", 60_000, 120_000), ("+120s basic 20", 180_000, 240_000)]


def test_each_step_is_measured_from_the_records_and_the_worker_and_judged(tmp_path: Path) -> None:
    d = ramp_dir(tmp_path)
    rows, doc = build(d, RampSpec(settle_s=60), Vm(worker({120: 10, 240: 18}, {120: 0, 240: 2}), "vm"))
    first, second = rows
    assert jv.obj(jv.obj(first, "stepped"), "cps") == {"completed": 10.0, "shed_503": 0.0, "other_fail": 0.0}
    assert jv.obj(jv.obj(second, "stepped"), "cps") == {"completed": 18.0, "shed_503": 2.0, "other_fail": 0.0}
    assert first["setup_p99_ms"] == 20
    lim = jv.obj(doc, "limits")
    assert lim["accepted_call_capacity"] == {"cps": 18.0, "step": second["label"]}
    assert lim["reject_throughput"] == {"cps": 2.0, "step": second["label"]}
    assert (d / "ramp.json").is_file() and (d / "ramp-steps.json").is_file()
    assert "accepted-call capacity" in table(rows, doc)


def test_a_worker_count_the_callers_do_not_see_makes_the_step_unclean(tmp_path: Path) -> None:
    d = ramp_dir(tmp_path)
    rows, doc = build(d, RampSpec(settle_s=60), Vm(worker({120: 15, 240: 18}, {120: 0, 240: 2}), "vm"))
    steps = jv.obj(jv.obj(doc, "limits"), "steps")
    assert jv.obj(steps, str(rows[0]["label"]))["clean"] is False
