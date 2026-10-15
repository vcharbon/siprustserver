"""Incident windows rebuilt from a run's `events.jsonl` (ADR-0049 §3.5).

A window opens at its event's first `inject` and closes `settle_s` after its
last `recovered`. An event with a `failed` phase, or one that never recovered,
closes at its maximum (every timeout and the hold, plus `settle_s`) with status
`incident_failed`.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from . import jsonval as jv
from .jsonval import Json


@dataclass(frozen=True)
class Phase:
    phase: str
    at_ms: int
    step: str


@dataclass
class Window:
    event: str
    source: str
    incidents: list[str]
    budget: str
    status: str
    opened_at_ms: int
    closed_at_ms: int
    phases: list[Phase] = field(default_factory=lambda: [])
    facts: dict[str, Json] = field(default_factory=lambda: {})

    def holds(self, at_ms: float) -> bool:
        return self.opened_at_ms <= at_ms <= self.closed_at_ms

    def first(self, phase: str) -> int | None:
        return next((p.at_ms for p in self.phases if p.phase == phase), None)


def event_line(
    event: str, source: str, incidents: list[str], budget: str, settle_s: float, max_s: float
) -> Json:
    return {
        "kind": "event",
        "event": event,
        "source": source,
        "incidents": incidents,
        "budget": budget,
        "settle_s": settle_s,
        "max_s": max_s,
    }


def phase_line(event: str, step: str, phase: str, at_ms: int) -> Json:
    return {"kind": "phase", "event": event, "step": step, "phase": phase, "at_ms": at_ms}


def facts_line(event: str, step: str, executable: str, facts: Json) -> Json:
    return {"kind": "facts", "event": event, "step": step, "executable": executable, "facts": facts}


def build_windows(lines: list[Json]) -> list[Window]:
    metas: dict[str, Json] = {}
    phases: dict[str, list[Phase]] = {}
    facts: dict[str, dict[str, Json]] = {}
    for line in lines:
        kind = jv.string(line, "kind")
        ev = jv.string(line, "event")
        if kind == "event":
            metas[ev] = line
        elif kind == "phase":
            phases.setdefault(ev, []).append(
                Phase(jv.string(line, "phase"), int(jv.number(line, "at_ms")), jv.string(line, "step"))
            )
        elif kind == "facts":
            per_step = facts.setdefault(ev, {}).setdefault(jv.string(line, "step"), {})
            per_step[jv.string(line, "executable")] = jv.obj(line, "facts")
    out: list[Window] = []
    for ev, meta in metas.items():
        ps = phases.get(ev, [])
        injects = [p.at_ms for p in ps if p.phase == "inject"]
        if not injects:
            continue
        opened = min(injects)
        settle_ms = int(jv.number(meta, "settle_s") * 1000)
        recovered = [p.at_ms for p in ps if p.phase == "recovered"]
        failed = any(p.phase == "failed" for p in ps)
        incidents = jv.strings(meta, "incidents")
        if failed or len(recovered) < len(incidents):
            status = "incident_failed"
            closed = opened + int(jv.number(meta, "max_s") * 1000) + settle_ms
        else:
            status = "ok"
            closed = max(recovered) + settle_ms
        out.append(
            Window(
                event=ev,
                source=jv.string(meta, "source"),
                incidents=incidents,
                budget=jv.string(meta, "budget"),
                status=status,
                opened_at_ms=opened,
                closed_at_ms=closed,
                phases=ps,
                facts=facts.get(ev, {}),
            )
        )
    out.sort(key=lambda w: w.opened_at_ms)
    return out
