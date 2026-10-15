"""`verdict.json` (`verdict.v1`), recomputed from a run directory alone (ADR-0049 §3.11).

The run directory holds what the verdict reads: the resolved profile, the
budgets and catalogs it used, `events.jsonl`, the exported `records.jsonl`,
`streams.json` (each generator's own call counter) and `integrity.jsonl` (what
the runner saw go wrong while it ran).
"""

from __future__ import annotations

import json
from collections import Counter, defaultdict
from pathlib import Path

from . import jsonval as jv
from .classify import Impact, SetupLatency, classify
from .judge import judge
from .jsonval import Json, as_obj
from .model import Budget, Shape, parse_budget, parse_catalog
from .profile import parse_profile
from .schemas import SchemaSet
from .windows import build_windows


def read_jsonl(p: Path) -> list[Json]:
    if not p.is_file():
        return []
    return [as_obj(json.loads(line), p.name) for line in p.read_text().splitlines() if line.strip()]


def _integrity(kind: str, detail: str, **extra: object) -> Json:
    return {"kind": kind, "detail": detail, **extra}


def compute(run_dir: Path, schemas: SchemaSet) -> Json:
    meta = as_obj(json.loads((run_dir / "run-meta.json").read_text()))
    profile = parse_profile(as_obj(json.loads((run_dir / "profile.json").read_text())))
    budgets: dict[str, Budget] = {}
    for p in sorted((run_dir / "budgets").glob("*.json")):
        b = parse_budget(as_obj(json.loads(p.read_text())))
        budgets[b.name] = b
    shapes: dict[str, Shape] = {}
    for p in sorted((run_dir / "catalogs").glob("*.json")):
        for s in parse_catalog(as_obj(json.loads(p.read_text()))).shapes:
            shapes[s.shape] = s
    streams_doc = [as_obj(s) for s in jv.as_list(json.loads((run_dir / "streams.json").read_text()))]
    integrity: list[Json] = read_jsonl(run_dir / "integrity.jsonl")
    windows = build_windows(read_jsonl(run_dir / "events.jsonl"))

    impacts: list[Impact] = []
    latencies: list[SetupLatency] = []
    records_by_stream: Counter[str] = Counter()
    success_by_stream: Counter[str] = Counter()
    seen: Counter[str] = Counter()
    bad_records: dict[str, list[str]] = defaultdict(list)
    for r in read_jsonl(run_dir / "records.jsonl"):
        stream = str(r.get("stream", "?"))
        records_by_stream[stream] += 1
        errs = schemas.errors("call-outcome.v1", r)
        shape = shapes.get(str(r.get("shape")))
        if errs or shape is None:
            bad_records[stream].append(errs[0] if errs else f"shape {r.get('shape')!r} is in no catalog of the run")
            continue
        seen[jv.string(r, "call_id")] += 1
        c = classify(r, shape)
        if c.generator_failure is not None:
            integrity.append(
                _integrity("generator_failure", c.generator_failure, stream=stream, at_ms=jv.number(r, "ended_at_ms"))
            )
            continue
        impacts.extend(c.impacts)
        if c.latency is not None:
            latencies.append(c.latency)
        if c.success:
            success_by_stream[stream] += 1
    for stream, errs in sorted(bad_records.items()):
        integrity.append(
            _integrity("generator_failure", f"{len(errs)} records unreadable, first: {errs[0]}", stream=stream)
        )
    dups = sum(n - 1 for n in seen.values() if n > 1)
    if dups:
        integrity.append(_integrity("record_gap", f"{dups} records repeat a Call-ID already recorded"))

    streams_out: list[Json] = []
    cps: dict[str, float] = {}
    for s in streams_doc:
        name = jv.string(s, "stream")
        cps[name] = jv.number(s, "cps")
        calls = jv.opt_int(s, "calls")
        not_placed = jv.opt_int(s, "not_placed") or 0
        records = records_by_stream.get(name, 0)
        if calls is None and jv.boolean(s, "reconciled", True):
            integrity.append(
                _integrity("record_gap", f"the generator's call counter could not be read; {records} records", stream=name)
            )
        elif calls is not None and jv.boolean(s, "reconciled", True) and records + not_placed != calls:
            integrity.append(
                _integrity(
                    "record_gap",
                    f"{records} records + {not_placed} not placed != {calls} calls the generator counted",
                    stream=name,
                )
            )
        streams_out.append(
            {
                "stream": name,
                "shape": jv.string(s, "shape"),
                "start": jv.string(s, "start"),
                "cps": cps[name],
                "calls": calls,
                "not_placed": not_placed,
                "records": records,
                "success": success_by_stream.get(name, 0),
            }
        )

    prev_close: int | None = None
    for w in windows:
        if prev_close is not None and w.opened_at_ms < prev_close:
            integrity.append(_integrity("window_overlap", "a window opened while another was open", event=w.event))
        prev_close = max(prev_close or 0, w.closed_at_ms)
        if w.status == "incident_failed":
            failed = [p for p in w.phases if p.phase == "failed"]
            detail = f"step {failed[0].step} failed" if failed else "the event never recovered"
            integrity.append(_integrity("incident_failed", detail, event=w.event))

    judged = judge(windows, impacts, latencies, budgets, profile.setup_slow_ms, cps)
    windows_out: list[Json] = []
    findings = len(judged.in_dialog) + len(judged.unattributed)
    for w in windows:
        rows = judged.per_window.get(w.event, [])
        n = sum(1 for r in rows if r.get("finding") is True)
        findings += n
        windows_out.append(
            {
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
        )
    status = "invalid" if integrity else ("fail" if findings else "pass")
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
        "integrity": integrity,
    }
