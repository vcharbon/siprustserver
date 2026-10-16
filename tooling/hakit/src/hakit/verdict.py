"""`verdict.json` (`verdict.v1`), recomputed from a run directory alone (ADR-0049 §3.11).

The run directory holds what the verdict reads: the resolved profile, the
budgets and catalogs it used, `events.jsonl`, the exported `records.jsonl`,
`cdrs.jsonl` when the profile joins CDRs, the kept answers of the product
classes (`classes/`), `streams.json` (each generator's own counters),
`integrity.jsonl` (what the runner saw go wrong while it ran) and `host.jsonl`
(the host's pressure, host_stress.py).
"""

from __future__ import annotations

import json
from collections import Counter, defaultdict
from pathlib import Path

from . import jsonval as jv
from .cdr_check import cdr_impacts_of
from .cdrjoin import DIFFERS
from .classes import read_counts
from .jsonl import read_jsonl
from .classify import SETUP_FAILED, Classified, Impact, SetupLatency, classify
from .judge import ALL_CALL_IDS, judge
from .jsonval import Json, as_obj
from .model import Budget, Shape, parse_budget, parse_catalog
from .profile import Profile, parse_profile
from .recovery import Sample, StreamSamples
from .reconcile import integrity, reconcile
from .schemas import SchemaSet
from .generator_health import run_stalls
from .host_stress import stress_findings, thresholds
from .stalls import Stall, stall_holding, stall_of
from .window_recovery import recover_windows
from .windows import Window, build_windows

MAX_CALL_IDS = 50


def _load_json(p: Path) -> Json:
    return as_obj(json.loads(p.read_text()), p.name)


class _Calls:
    """The schema-valid records of a run, classified, with what the classifier refused."""

    def __init__(self, records: list[Json], shapes: dict[str, Shape], schemas: SchemaSet) -> None:
        self.valid: list[tuple[Json, Classified]] = []
        self.per_stream: Counter[str] = Counter()
        self.bad: dict[str, list[str]] = defaultdict(list)
        self.dups = 0
        self.by_id: dict[str, Json] = {}
        seen: set[str] = set()
        for r in records:
            stream = str(r.get("stream", "?"))
            self.per_stream[stream] += 1
            errs = schemas.errors("call-outcome.v1", r)
            shape = shapes.get(str(r.get("shape")))
            if errs or shape is None:
                self.bad[stream].append(errs[0] if errs else f"shape {r.get('shape')!r} is in no catalog of the run")
                continue
            call_id = jv.string(r, "call_id")
            if call_id in seen:
                self.dups += 1
            seen.add(call_id)
            self.by_id.setdefault(call_id, r)
            self.valid.append((r, classify(r, shape)))

    def starts(self) -> dict[str, list[float]]:
        out: dict[str, list[float]] = defaultdict(list)
        for r, _ in self.valid:
            out[jv.string(r, "stream")].append(jv.number(r, "started_at_ms"))
        return out


def _spoiled_timeout(imp: Impact, stalls: list[Stall], profile: Profile) -> Stall | None:
    """The stall a timeout's setup deadline (its start + the stream's
    `setup_deadline_s`) falls in: the generator checked it while it stalled."""
    s = profile.stream(imp.stream)
    if imp.cls != SETUP_FAILED or imp.reason != "timeout" or s is None:
        return None
    return stall_holding(stalls, imp.stream, imp.started_at_ms + s.setup_deadline_s * 1000)


def _spoiled_cdr(imp: Impact, by_id: dict[str, Json], stalls: list[Stall]) -> Stall | None:
    """The stall a CDR that differs from its record was compared across: the
    caller's answer or end instant fell in a stall of its stream, so the
    generator, not the system under test, measured the call."""
    r = by_id.get(imp.call_id)
    if imp.reason != DIFFERS or r is None:
        return None
    for key in ("answered_at_ms", "ended_at_ms"):
        at = jv.opt_number(r, key)
        st = None if at is None else stall_holding(stalls, imp.stream, at)
        if st is not None:
            return st
    return None


def _samples(calls: _Calls, stalls: list[Stall], profile: Profile) -> dict[str, StreamSamples]:
    """Per stream that runs for the whole load, the setups recovery reads; a call
    whose timeout a stall spoiled is left out."""
    eligible = {s.name for s in profile.streams if s.start == "run"}
    by_stream: dict[str, list[Sample]] = defaultdict(list)
    for r, c in calls.valid:
        stream = jv.string(r, "stream")
        if stream not in eligible or c.generator_failure is not None:
            continue
        if any(_spoiled_timeout(i, stalls, profile) is not None for i in c.impacts):
            continue
        lat = c.latency
        ms = None
        if lat is not None and stall_of(stalls, stream, lat.at_ms - lat.ms, lat.at_ms) is None:
            ms = lat.ms
        ok = not any(i.budgetable for i in c.impacts)
        by_stream[stream].append(Sample(jv.number(r, "started_at_ms"), ok, ms))
    return {k: StreamSamples(v) for k, v in by_stream.items()}


def _stall_findings(stalled: dict[Stall, list[str]]) -> list[Json]:
    return [
        integrity(
            "generator_stall",
            f"{len(ids)} measurements taken across a {st.ms:g} ms generator stall",
            stream=st.stream,
            at_ms=st.at_ms,
            call_ids=ids[:MAX_CALL_IDS],
        )
        for st, ids in sorted(stalled.items(), key=lambda kv: kv[0].at_ms)
    ]


def _window_integrity(windows: list[Window]) -> list[Json]:
    found: list[Json] = []
    prev_close: int | None = None
    for w in windows:
        if prev_close is not None and w.opened_at_ms < prev_close:
            found.append(integrity("window_overlap", "a window opened while another was open", event=w.event))
        prev_close = max(prev_close or 0, w.closed_at_ms)
        if w.status == "incident_failed":
            failed = [p for p in w.phases if p.phase == "failed"]
            detail = f"step {failed[0].step} failed" if failed else "the event never recovered"
            found.append(integrity("incident_failed", detail, event=w.event))
    return found


class _Run:
    """The inputs of a run directory the verdict reads, its calls classified and
    its generator stalls found."""

    def __init__(self, run_dir: Path, schemas: SchemaSet) -> None:
        self.meta = _load_json(run_dir / "run-meta.json")
        self.profile = parse_profile(_load_json(run_dir / "profile.json"))
        self.budgets: dict[str, Budget] = {}
        for p in sorted((run_dir / "budgets").glob("*.json")):
            b = parse_budget(_load_json(p))
            self.budgets[b.name] = b
        shapes: dict[str, Shape] = {}
        for p in sorted((run_dir / "catalogs").glob("*.json")):
            for s in parse_catalog(_load_json(p)).shapes:
                shapes[s.shape] = s
        self.calls = _Calls(read_jsonl(run_dir / "records.jsonl"), shapes, schemas)
        load_start = jv.opt_number(self.meta, "load_started_at_ms") or jv.number(self.meta, "started_at_ms")
        self.load_start_ms = load_start
        self.stalls = run_stalls(run_dir, self.calls.starts(), self.profile, load_start)


def recovery_samples(run_dir: Path, schemas: SchemaSet) -> dict[str, StreamSamples]:
    """The setups recovery reads, per stream."""
    run = _Run(run_dir, schemas)
    return _samples(run.calls, run.stalls, run.profile)


def compute(run_dir: Path, schemas: SchemaSet) -> Json:
    return compute_with_calls(run_dir, schemas)[0]


def _take_calls(where: str | None, rows: list[Json], out: list[Json]) -> None:
    """Moves each row's full Call-ID list out of it, into `out` with its key."""
    for row in rows:
        ids = row.pop(ALL_CALL_IDS, [])
        if ids:
            key = {k: row[k] for k in ("class", "stream", "code", "reason", "retry_after") if k in row}
            out.append({"window": where, **key, "call_ids": ids})


def compute_with_calls(run_dir: Path, schemas: SchemaSet) -> tuple[Json, list[Json]]:
    """The verdict, and every impact row's full Call-ID list (the verdict keeps 50)."""
    v = _compute(run_dir, schemas)
    calls: list[Json] = []
    for w in jv.objs(v, "windows"):
        _take_calls(str(w["event"]), jv.objs(w, "impacts"), calls)
    _take_calls(None, jv.objs(v, "unattributed"), calls)
    return v, calls


def _compute(run_dir: Path, schemas: SchemaSet) -> Json:
    run = _Run(run_dir, schemas)
    meta, profile, budgets, calls, stalls = run.meta, run.profile, run.budgets, run.calls, run.stalls
    streams_doc = [as_obj(s) for s in jv.as_list(json.loads((run_dir / "streams.json").read_text()))]
    found: list[Json] = read_jsonl(run_dir / "integrity.jsonl")
    windows = build_windows(read_jsonl(run_dir / "events.jsonl"))
    stalled: dict[Stall, list[str]] = defaultdict(list)
    impacts: list[Impact] = []
    latencies: list[SetupLatency] = []
    success: Counter[str] = Counter()
    for r, c in calls.valid:
        stream = jv.string(r, "stream")
        if c.generator_failure is not None:
            found.append(integrity("generator_failure", c.generator_failure, stream=stream, at_ms=jv.number(r, "ended_at_ms")))
            continue
        for imp in c.impacts:
            st = _spoiled_timeout(imp, stalls, profile)
            if st is None:
                impacts.append(imp)
            else:
                stalled[st].append(imp.call_id)
        if c.latency is not None:
            latencies.append(c.latency)
        if c.success:
            success[stream] += 1
    for stream, errs in sorted(calls.bad.items()):
        found.append(integrity("generator_failure", f"{len(errs)} records unreadable, first: {errs[0]}", stream=stream))
    if calls.dups:
        found.append(integrity("record_gap", f"{calls.dups} records repeat a Call-ID already recorded"))

    recovered = recover_windows(windows, _samples(calls, stalls, profile), budgets)
    cdr_all, cdr_found = cdr_impacts_of(run_dir, profile, [r for r, _ in calls.valid], schemas)
    cdr_impacts: list[Impact] = []
    for imp in cdr_all:
        st = _spoiled_cdr(imp, calls.by_id, stalls)
        if st is None:
            cdr_impacts.append(imp)
        else:
            stalled[st].append(imp.call_id)
    class_impacts, class_found = read_counts(run_dir, jv.strings(meta, "classes"))
    found += cdr_found + class_found

    cps = {jv.string(s, "stream"): jv.number(s, "cps") for s in streams_doc}
    judged = judge(
        windows,
        impacts,
        latencies,
        budgets,
        profile.setup_slow_ms,
        cps,
        counted=cdr_impacts + class_impacts,
        stall_of=lambda lat: stall_of(stalls, lat.stream, lat.at_ms - lat.ms, lat.at_ms),
        run_cps=sum(jv.number(s, "cps") for s in streams_doc if s.get("start", "run") == "run"),
        cdr_in_flight_ms=profile.cdr_in_flight_s * 1000,
    )
    for lat, st in judged.stalled:
        stalled[st].append(lat.call_id)
    stall_rows: dict[str, list[Json]] = defaultdict(list)
    for st in stalls:
        stall_rows[st.stream].append(
            {"at_ms": st.at_ms, "ms": round(st.ms, 3), "spoiled": len(stalled.get(st, [])), "source": st.source}
        )
    streams_out, reconciled = reconcile(streams_doc, calls.per_stream, success, stall_rows)
    found += reconciled + _stall_findings(stalled) + _window_integrity(windows)
    spoiled = {st: len(ids) for st, ids in stalled.items()}
    found = stress_findings(run_dir, thresholds(profile.preflight), stalls, spoiled, found, run.load_start_ms) + found

    windows_out: list[Json] = []
    findings = len(judged.in_dialog) + len(judged.unattributed)
    for w in windows:
        recs, rec_rows = recovered.get(w.event, ([], []))
        rows = judged.per_window.get(w.event, []) + rec_rows
        healed = w.last("healed")
        n = sum(1 for r in rows if r.get("finding") is True)
        findings += n
        out: Json = {
            "event": w.event,
            "source": w.source,
            "incidents": w.incidents,
            "budget": w.budget,
            "status": w.status,
            "opened_at_ms": w.opened_at_ms,
            "closed_at_ms": w.closed_at_ms,
            "phases": [{"phase": p.phase, "at_ms": p.at_ms, "step": p.step} for p in w.phases],
            "facts": dict(w.facts),
            "impacts": rows,
            "findings": n,
        }
        if healed is not None and recs:
            out["recovery"] = [{"stream": r.stream, "recovery_s": r.recovery_s(healed)} for r in recs]
        windows_out.append(out)
    status = "invalid" if found else ("fail" if findings else "pass")
    return {
        "schema": "verdict.v1",
        "run": jv.string(meta, "run"),
        "profile": profile.name,
        "started_at_ms": jv.number(meta, "started_at_ms"),
        "ended_at_ms": jv.number(meta, "ended_at_ms"),
        "status": status,
        "streams": streams_out,
        "windows": windows_out,
        "in_dialog": judged.in_dialog,
        "unattributed": judged.unattributed,
        "integrity": found,
    }
