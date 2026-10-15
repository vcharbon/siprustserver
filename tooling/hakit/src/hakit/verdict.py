"""`verdict.json` (`verdict.v1`), recomputed from a run directory alone (ADR-0049 §3.11).

The run directory holds what the verdict reads: the resolved profile, the
budgets and catalogs it used, `events.jsonl`, the exported `records.jsonl`,
`cdrs.jsonl` when the profile joins CDRs, the kept answers of the product
classes (`classes/`), `streams.json` (each generator's own counters) and
`integrity.jsonl` (what the runner saw go wrong while it ran).
"""

from __future__ import annotations

import json
from collections import Counter, defaultdict
from pathlib import Path

from . import jsonval as jv
from .cdrjoin import join
from .classes import read_counts
from .classify import SETUP_FAILED, Classified, Impact, SetupLatency, classify
from .judge import judge
from .jsonval import Json, as_obj
from .model import Budget, Shape, parse_budget, parse_catalog
from .profile import Profile, parse_profile
from .recovery import Sample, StreamSamples
from .reconcile import integrity, reconcile
from .schemas import SchemaSet
from .stalls import Stall, detect, stall_holding, stall_of
from .window_recovery import recover_windows
from .windows import Window, build_windows

MAX_CALL_IDS = 50


def read_jsonl(p: Path) -> list[Json]:
    if not p.is_file():
        return []
    return [as_obj(json.loads(line), p.name) for line in p.read_text().splitlines() if line.strip()]


def _load_json(p: Path) -> Json:
    return as_obj(json.loads(p.read_text()), p.name)


class _Calls:
    """The schema-valid records of a run, classified, with what the classifier refused."""

    def __init__(self, records: list[Json], shapes: dict[str, Shape], schemas: SchemaSet) -> None:
        self.valid: list[tuple[Json, Classified]] = []
        self.per_stream: Counter[str] = Counter()
        self.bad: dict[str, list[str]] = defaultdict(list)
        self.dups = 0
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
            self.valid.append((r, classify(r, shape)))

    def starts(self) -> dict[str, list[float]]:
        out: dict[str, list[float]] = defaultdict(list)
        for r, _ in self.valid:
            out[jv.string(r, "stream")].append(jv.number(r, "started_at_ms"))
        return out


def _stalls(calls: _Calls, profile: Profile, load_start_ms: float) -> list[Stall]:
    found: list[Stall] = []
    for name, starts in sorted(calls.starts().items()):
        s = profile.stream(name)
        if s is None:
            continue
        found += detect(name, starts, lambda at, s=s: s.cps_at((at - load_start_ms) / 1000), profile.generator_stall_ms)
    return found


def _spoiled_timeout(imp: Impact, stalls: list[Stall]) -> Stall | None:
    if imp.cls != SETUP_FAILED or imp.reason != "timeout":
        return None
    return stall_holding(stalls, imp.stream, imp.at_ms)


def _samples(calls: _Calls, stalls: list[Stall], profile: Profile) -> dict[str, StreamSamples]:
    """Per stream that runs for the whole load, the setups recovery reads."""
    eligible = {s.name for s in profile.streams if s.start == "run"}
    by_stream: dict[str, list[Sample]] = defaultdict(list)
    for r, c in calls.valid:
        stream = jv.string(r, "stream")
        if stream not in eligible or c.generator_failure is not None:
            continue
        lat = c.latency
        ms = None
        if lat is not None and stall_of(stalls, stream, lat.at_ms - lat.ms, lat.at_ms) is None:
            ms = lat.ms
        ok = not any(i.budgetable for i in c.impacts)
        by_stream[stream].append(Sample(jv.number(r, "started_at_ms"), ok, ms))
    return {k: StreamSamples(v) for k, v in by_stream.items()}


def _cdr_impacts(run_dir: Path, profile: Profile, calls: _Calls, schemas: SchemaSet) -> tuple[list[Impact], list[Json]]:
    if not profile.cdr_join:
        return [], []
    p = run_dir / "cdrs.jsonl"
    if not p.is_file():
        return [], [integrity("class_failed", "cdr_mismatch: the run directory holds no cdrs.jsonl")]
    good: list[Json] = []
    bad = 0
    for line in read_jsonl(p):
        if schemas.errors("cdr-observation.v1", line):
            bad += 1
        else:
            good.append(line)
    found = [integrity("class_failed", f"cdr_mismatch: {bad} observations break cdr-observation.v1")] if bad else []
    return join([r for r, _ in calls.valid], good), found


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


def compute(run_dir: Path, schemas: SchemaSet) -> Json:
    meta = _load_json(run_dir / "run-meta.json")
    profile = parse_profile(_load_json(run_dir / "profile.json"))
    budgets: dict[str, Budget] = {}
    for p in sorted((run_dir / "budgets").glob("*.json")):
        b = parse_budget(_load_json(p))
        budgets[b.name] = b
    shapes: dict[str, Shape] = {}
    for p in sorted((run_dir / "catalogs").glob("*.json")):
        for s in parse_catalog(_load_json(p)).shapes:
            shapes[s.shape] = s
    streams_doc = [as_obj(s) for s in jv.as_list(json.loads((run_dir / "streams.json").read_text()))]
    found: list[Json] = read_jsonl(run_dir / "integrity.jsonl")
    windows = build_windows(read_jsonl(run_dir / "events.jsonl"))

    calls = _Calls(read_jsonl(run_dir / "records.jsonl"), shapes, schemas)
    load_start = jv.opt_number(meta, "load_started_at_ms") or jv.number(meta, "started_at_ms")
    stalls = _stalls(calls, profile, load_start)
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
            st = _spoiled_timeout(imp, stalls)
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
    cdr_impacts, cdr_found = _cdr_impacts(run_dir, profile, calls, schemas)
    class_impacts, class_found = read_counts(run_dir)
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
    )
    for lat, st in judged.stalled:
        stalled[st].append(lat.call_id)
    stall_rows: dict[str, list[Json]] = defaultdict(list)
    for st in stalls:
        stall_rows[st.stream].append({"at_ms": st.at_ms, "ms": round(st.ms, 3), "spoiled": len(stalled.get(st, []))})
    streams_out, reconciled = reconcile(streams_doc, calls.per_stream, success, stall_rows)
    found += reconciled + _stall_findings(stalled) + _window_integrity(windows)

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
