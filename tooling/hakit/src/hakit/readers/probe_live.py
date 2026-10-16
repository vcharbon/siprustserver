"""The run directory and the records of a run that may still be running, read
for `hakit probe`.

Windows come from `events.jsonl` as the runner wrote them so far, each with
its live status: `open` until every incident recovered, `settling` until
`settle_s` after that, `closed`, or `failed` when a step failed. The records
are those VictoriaLogs holds that ended in the recent span; they are classified
and judged against the windows as the verdict would, provisionally: the calls
still in flight are missing, a window is not yet extended to the instant
traffic came back, and no generator stall is left out.
"""

from __future__ import annotations

import json
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path

from .. import jsonval as jv
from ..classify import Impact, SetupLatency, classify
from ..judge import Judged, judge
from ..jsonl import read_jsonl
from ..jsonval import Json, as_obj
from ..model import Budget, Shape, parse_budget, parse_catalog
from ..profile import Profile, parse_profile
from ..windows import Window, build_windows

MAX_CALL_IDS = 20


def window_status(w: Window, now_ms: int) -> str:
    if any(p.phase == "failed" for p in w.phases):
        return "failed"
    if sum(1 for p in w.phases if p.phase == "recovered") < len(w.incidents):
        return "open"
    return "settling" if now_ms < w.closed_at_ms else "closed"


def event_rows(windows: list[Window], now_ms: int) -> list[Json]:
    out: list[Json] = []
    for w in windows:
        out.append(
            {
                "event": w.event,
                "source": w.source,
                "incidents": w.incidents,
                "budget": w.budget,
                "status": window_status(w, now_ms),
                "opened_at_ms": w.opened_at_ms,
                "phases": [{"phase": p.phase, "at_ms": p.at_ms, "step": p.step} for p in w.phases],
                "facts": dict(w.facts),
            }
        )
    return out


def planned_events(profile: Profile) -> int:
    """How many scheduled events fire before the load ends."""
    sched = profile.schedule
    if sched is None:
        return 0
    if sched.at:
        return sum(1 for e in sched.at if (e.at_s or 0.0) < profile.duration_s)
    first, interval = sched.first_at_s or 0.0, sched.interval_s or profile.duration_s
    n = 0
    while first + n * interval < profile.duration_s:
        n += 1
    return n


@dataclass(frozen=True)
class RunFiles:
    meta: Json
    profile: Profile
    budgets: dict[str, Budget]
    shapes: dict[str, Shape]
    windows: list[Window]
    integrity: list[Json]


def read_files(run_dir: Path) -> RunFiles:
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
    windows = build_windows(read_jsonl(run_dir / "events.jsonl"))
    return RunFiles(meta, profile, budgets, shapes, windows, read_jsonl(run_dir / "integrity.jsonl"))


def second_final_after_2xx(r: Json) -> bool:
    """A non-2xx final for the INVITE after its 2xx: a second final on an INVITE
    server transaction that already sent a 2xx (RFC 3261 §17.2.1)."""
    answered = jv.opt_number(r, "answered_at_ms")
    if answered is None:
        return False
    return any(
        jv.string(f, "kind") == "protocol"
        and jv.opt_string(f, "method") == "INVITE"
        and (jv.opt_int(f, "code") or 0) >= 300
        and jv.number(f, "at_ms") >= answered
        for f in jv.objs(r, "failures")
    )


def judge_recent(files: RunFiles, records: list[Json]) -> Json:
    """The recent records per stream, classified and judged against the windows."""
    impacts: list[Impact] = []
    latencies: list[SetupLatency] = []
    per_stream: Counter[str] = Counter()
    success: Counter[str] = Counter()
    unknown: Counter[str] = Counter()
    second_final: list[str] = []
    for r in records:
        stream = str(r.get("stream", "?"))
        per_stream[stream] += 1
        shape = files.shapes.get(str(r.get("shape")))
        if shape is None:
            unknown[stream] += 1
            continue
        c = classify(r, shape)
        impacts += c.impacts
        if c.latency is not None:
            latencies.append(c.latency)
        if c.success:
            success[stream] += 1
        if second_final_after_2xx(r):
            second_final.append(jv.string(r, "call_id"))
    cps = {s.name: s.cps for s in files.profile.streams}
    run_cps = sum(s.cps for s in files.profile.streams if s.start == "run")
    judged: Judged = judge(
        files.windows, impacts, latencies, files.budgets, files.profile.setup_slow_ms, cps, run_cps=run_cps
    )
    in_dialog: dict[tuple[str, str], list[str]] = defaultdict(list)
    for e in judged.in_dialog:
        in_dialog[(jv.string(e, "class"), jv.string(e, "stream"))].append(jv.string(e, "call_id"))
    return {
        "records": dict(sorted(per_stream.items())),
        "successes": dict(sorted(success.items())),
        "unknown_shape": dict(sorted(unknown.items())),
        "window_findings": {
            ev: found
            for ev, rows in judged.per_window.items()
            if (found := [r for r in rows if r.get("finding") is True])
        },
        "unattributed": judged.unattributed,
        "in_dialog": [
            {"class": cls, "stream": stream, "count": len(ids), "call_ids": ids[:MAX_CALL_IDS]}
            for (cls, stream), ids in sorted(in_dialog.items())
        ],
        "second_final_after_2xx": {"count": len(second_final), "call_ids": second_final[:MAX_CALL_IDS]},
    }
