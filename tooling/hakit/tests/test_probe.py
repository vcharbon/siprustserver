"""`hakit probe`: the live snapshot of a run from its directory, its recent
records in VictoriaLogs and a scripted VictoriaMetrics."""

from __future__ import annotations

import json
import os
import urllib.parse
from pathlib import Path

from kit import T0, record, timed_out
from rundir import kill, run_dir

from hakit import jsonval as jv
from hakit.executor import Call, FakeExecutor, HttpResult, ProcResult
from hakit.jsonval import Json
from hakit.readers.probe import probe, run_state
from hakit.readers.probe_live import second_final_after_2xx
from hakit.readers.probe_spec import Bound, ProbeSpec

NOW = T0 + 200_000


def shipped(records: list[Json]) -> str:
    return "".join(json.dumps({"_msg": json.dumps(r)}) + "\n" for r in records)


def lab(records: list[Json], vm: dict[str, float] | None = None, pods: str = '{"items": []}') -> FakeExecutor:
    def rule(c: Call) -> HttpResult | ProcResult | None:
        if c.kind == "http" and "/select/logsql/query" in c.url:
            return HttpResult(200, shipped(records))
        if c.kind == "http" and "/api/v1/query" in c.url:
            q = urllib.parse.parse_qs(urllib.parse.urlsplit(c.url).query)["query"][0]
            hits = [v for k, v in (vm or {}).items() if k in q]
            result: list[Json] = [{"metric": {}, "value": [0, str(hits[0])]}] if hits else []
            return HttpResult(200, json.dumps({"status": "success", "data": {"result": result}}))
        if c.kind == "run" and c.argv[:1] == ["kubectl"] and "pods" in c.argv:
            return ProcResult(0, pods, "")
        return None

    return FakeExecutor([rule])


def snap(d: Path, records: list[Json], spec: ProbeSpec = ProbeSpec(), **kw: object) -> Json:
    return probe(d, spec, lab(records, **kw), "vm", "vl", "ctx", NOW)  # type: ignore[arg-type]


def test_an_ended_run_reports_its_windows_and_judges_the_recent_records(tmp_path: Path) -> None:
    # Three timeouts at the kill: the base worker-kill budget allows 2 s at 20 cps (40).
    late = [timed_out(f"t{i}", T0 + 100_000 + i) for i in range(3)]
    d = run_dir(tmp_path, late, calls=3, events=kill(1, T0 + 100_000))
    (d / "verdict.json").write_text("{}")
    s = snap(d, late)
    assert s["state"] == "ended" and jv.obj(s, "load")["phase"] == "done"
    (ev,) = jv.objs(s, "events")
    assert (ev["event"], ev["status"]) == ("001-kill_worker", "closed")
    assert s["events_fired"] == 1 and s["open_windows"] == []
    assert jv.obj(jv.obj(s, "recent"), "window_findings") == {}
    assert s["healthy"] is True


def test_impacts_outside_every_window_and_in_dialog_failures_are_anomalies(tmp_path: Path) -> None:
    stray = timed_out("out", T0 - 40_000)
    bye = [{"at_ms": T0 + 20_000, "kind": "dialog_request", "method": "BYE", "code": 481}]
    cut = record("cut", T0 + 1_000, "cut", ended=20_000, failures=bye)
    second = record("two", T0 + 2_000, failures=[{"at_ms": T0 + 3_000, "kind": "protocol", "method": "INVITE", "code": 487}])
    # A kill at +15 s: the cut call was up when it was injected; the stray
    # timeout and the second final came before any window.
    d = run_dir(tmp_path, [], calls=0, events=kill(1, T0 + 15_000))
    s = snap(d, [stray, cut, second])
    an = [str(a) for a in jv.lst(s, "anomalies")]
    assert "outside every window: setup_failed/basic/0/timeout x1 (provisional)" in an
    assert "outside every window: protocol_anomaly/basic x1 (provisional)" in an
    assert "in-dialog call_cut on basic x1" in an
    assert "a second final after the INVITE's 2xx on 1 calls" in an
    assert s["healthy"] is False


def test_the_second_final_needs_a_non_2xx_for_the_invite_after_the_answer() -> None:
    fail = {"at_ms": T0 + 3_000, "kind": "protocol", "method": "INVITE", "code": 487}
    assert second_final_after_2xx(record("a", T0, failures=[fail]))
    assert not second_final_after_2xx(record("b", T0, failures=[{**fail, "method": "BYE"}]))
    assert not second_final_after_2xx(record("c", T0, answered=None, failures=[fail]))


def test_a_running_run_checks_liveness_bounds_and_pods(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [], calls=0, events=[])
    meta = json.loads((d / "run-meta.json").read_text())
    (d / "run-meta.json").write_text(json.dumps({**meta, "load_started_at_ms": NOW - 60_000}))
    (d / "active").write_text(f"{os.getpid()}\n")
    assert run_state(d) == "running"
    pods = json.dumps({"items": [{"metadata": {"name": "w-0"}, "status": {"phase": "Pending"}}]})
    spec = ProbeSpec(bounds=[Bound("limiter admission", "max(limiter_admission_max)", 4)])
    s = snap(d, [], spec, vm={"limiter_admission_max": 5}, pods=pods)
    an = [str(a) for a in jv.lst(s, "anomalies")]
    assert jv.obj(s, "load")["phase"] == "loading"
    assert "stream basic wrote no record in the last 900 s" in an
    assert "bound limiter admission: 5 > 4" in an
    assert "pod not ready outside every window: w-0 Pending" in an


def test_integrity_findings_appended_while_the_run_runs_are_anomalies(tmp_path: Path) -> None:
    found = [{"kind": "stream_died", "stream": "basic", "at_ms": T0 + 5_000, "detail": "generator exited"}]
    d = run_dir(tmp_path, [], calls=0, events=[], integrity=found)
    s = snap(d, [])
    assert "integrity stream_died: generator exited basic" in jv.lst(s, "anomalies")
