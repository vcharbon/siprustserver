"""`hakit probe <run-dir>`: one JSON snapshot of a run, for a supervisor that
watches it while it runs and after it ended.

It reads the run directory (the profile, `events.jsonl` with each window's
live status, the integrity findings the runner appended so far), the records
that ended in the recent span from VictoriaLogs, classified and judged
provisionally against the windows (`probe_live`), and a few generic series
from VictoriaMetrics: the system under test's live calls, the generators'
offered rate by stream, the container restarts, and the product's bounds
(`--spec`). `anomalies` lists what deserves a look; `healthy` is true when it
is empty. While the run is not loading, the liveness checks are skipped: a
finished run's empty rates say nothing.
"""

from __future__ import annotations

import json
from pathlib import Path

from .. import jsonval as jv
from ..executor import Executor
from ..jsonval import Json, as_list, as_obj
from ..offschedule import ACTIVE, pid_alive
from ..vlogs import VictoriaLogs
from .probe_live import RunFiles, event_rows, judge_recent, planned_events, read_files, window_status
from .probe_spec import ProbeSpec
from .vmrange import Vm

# Shipping and ingestion of a record, past the end of its call.
RECORD_GRACE_S = 30


def run_state(run_dir: Path) -> str:
    """`running` while its runner lives, `ended` once its verdict is written,
    else `stopped` (interrupted: `hakit cleanup` heals what it left open)."""
    marker = run_dir / ACTIVE
    if marker.is_file():
        try:
            if pid_alive(int(marker.read_text().strip())):
                return "running"
        except ValueError:
            pass
    return "ended" if (run_dir / "verdict.json").is_file() else "stopped"


def _load(files: RunFiles, now_ms: int, state: str) -> Json:
    start = jv.opt_number(files.meta, "load_started_at_ms")
    duration = files.profile.duration_s
    if start is None:
        return {"phase": "starting" if state == "running" else "never_loaded", "duration_s": duration}
    elapsed = (now_ms - start) / 1000
    phase = "loading" if elapsed < duration else "draining"
    if state != "running":
        phase = "done"
    return {"phase": phase, "started_at_ms": start, "elapsed_s": round(elapsed, 1), "duration_s": duration}


def _not_ready(executor: Executor, context: str, namespace: str) -> list[str]:
    res = executor.run(["kubectl", "--context", context, "-n", namespace, "get", "pods", "-o", "json"], timeout_s=30)
    if res.rc != 0:
        return [f"pods unread: {res.stderr.strip()[:120]}"]
    out: list[str] = []
    for item in jv.objs(as_obj(json.loads(res.stdout)), "items"):
        name = jv.string(jv.obj(item, "metadata"), "name")
        status = jv.obj(item, "status")
        phase = jv.string(status, "phase", "?")
        if phase == "Succeeded":
            continue
        ready = all(jv.boolean(as_obj(c), "ready") for c in as_list(status.get("containerStatuses", [])))
        if phase != "Running" or not ready:
            out.append(f"{name} {phase}{'' if ready else ' not ready'}")
    return out


def _series(vm: Vm, spec: ProbeSpec, files: RunFiles, now_s: float) -> Json:
    ns = spec.namespace
    start = jv.opt_number(files.meta, "load_started_at_ms")
    since = max(60, int(now_s - start / 1000)) if start is not None else 3600
    restart_sel = f'exported_namespace="{ns}"' + (f',exported_pod=~"{spec.restart_pods}"' if spec.restart_pods else "")
    offered = (
        "sum by (stream)(rate(sipp_calls_created_total[1m])) "
        "or sum by (stream)(rate(loadgen_calls_offered_total[1m]))"
    )
    return {
        "active_calls": sum(v for _, v in vm.at(f'sum({spec.active_calls}{{namespace="{ns}"}})', now_s)),
        "active_calls_by_pod": vm.by(f'sum by (pod)({spec.active_calls}{{namespace="{ns}"}})', now_s, "pod"),
        "offered_cps_by_stream": {k: round(v, 2) for k, v in vm.by(offered, now_s, "stream").items()},
        "restarts_since_load": {
            k: round(v)
            for k, v in vm.by(
                f"sum by (exported_pod)(increase(kube_pod_container_status_restarts_total{{{restart_sel}}}[{since}s])) > 0.5",
                now_s,
                "exported_pod",
            ).items()
        },
        "bounds": [
            {"name": b.name, "max": b.max, "value": max((v for _, v in vm.at(b.expr, now_s)), default=None)}
            for b in spec.bounds
        ],
    }


def _anomalies(snap: Json, files: RunFiles, spec: ProbeSpec, loading: bool, not_ready: list[str]) -> list[str]:
    an: list[str] = []
    for i in files.integrity:
        an.append(f"integrity {i.get('kind')}: {i.get('detail', '')} {i.get('stream', i.get('event', ''))}".rstrip())
    for e in jv.objs(snap, "events"):
        if e["status"] == "failed":
            an.append(f"event {e['event']} failed a step: its window is `incident_failed`, the run will be invalid")
    recent = jv.obj(snap, "recent")
    for ev, rows in jv.obj(recent, "window_findings").items():
        for r in jv.as_list(rows):
            row = as_obj(r)
            key = "/".join(str(row[k]) for k in ("class", "stream", "code", "reason") if k in row)
            an.append(f"window {ev}: {key} {row['count']} > allowed {row['allowed']} (provisional)")
    for row in jv.objs(recent, "unattributed"):
        key = "/".join(str(row[k]) for k in ("class", "stream", "code", "reason") if k in row)
        an.append(f"outside every window: {key} x{row['count']} (provisional)")
    for row in jv.objs(recent, "in_dialog"):
        an.append(f"in-dialog {row['class']} on {row['stream']} x{row['count']}")
    second = jv.obj(recent, "second_final_after_2xx")
    if jv.number(second, "count") > 0:
        an.append(f"a second final after the INVITE's 2xx on {second['count']} calls")
    series = jv.obj(snap, "series")
    for b in jv.objs(series, "bounds"):
        v = jv.opt_number(b, "value")
        if v is not None and v > jv.number(b, "max"):
            an.append(f"bound {b['name']}: {v:g} > {b['max']:g}")
    if loading:
        records = jv.obj(recent, "records")
        elapsed = jv.number(jv.obj(snap, "load"), "elapsed_s", 0)
        for s in files.profile.streams:
            # A stream's first records come once its first calls could have ended.
            due = max(s.setup_deadline_s, s.ring_s + s.hold_s) + RECORD_GRACE_S
            if s.start == "run" and s.name not in spec.ballast and elapsed >= due and not records.get(s.name):
                an.append(f"stream {s.name} wrote no record in the last {spec.recent_s:g} s")
        if not any(e["status"] in ("open", "settling") for e in jv.objs(snap, "events")):
            an.extend(f"pod not ready outside every window: {p}" for p in not_ready)
    return an


def probe(run_dir: Path, spec: ProbeSpec, executor: Executor, vm_url: str, vlogs_url: str, context: str, now_ms: int) -> Json:
    files = read_files(run_dir)
    state = run_state(run_dir)
    run = jv.string(files.meta, "run")
    load = _load(files, now_ms, state)
    loading = state == "running" and load["phase"] == "loading"
    since_ms = now_ms - int(spec.recent_s * 1000)
    bad: list[str] = []
    records = list(VictoriaLogs(executor, vlogs_url).export(run, since_ms, now_ms, bad))
    events = event_rows(files.windows, now_ms)
    snap: Json = {
        "run": run,
        "profile": files.profile.name,
        "state": state,
        "now_ms": now_ms,
        "load": load,
        "events": events,
        "events_fired": len(events),
        "events_planned": planned_events(files.profile),
        "open_windows": [w.event for w in files.windows if window_status(w, now_ms) in ("open", "settling")],
        "integrity": files.integrity,
        "recent": {"since_ms": since_ms, "unreadable_lines": len(bad), **judge_recent(files, records)},
        "series": _series(Vm(executor, vm_url), spec, files, now_ms / 1000),
    }
    not_ready = _not_ready(executor, context, spec.namespace) if loading else []
    snap["pods_not_ready"] = not_ready
    anomalies = _anomalies(snap, files, spec, loading, not_ready)
    if bad:
        anomalies.append(f"{len(bad)} record lines unreadable from VictoriaLogs")
    snap["anomalies"] = anomalies
    snap["healthy"] = not anomalies
    return snap
