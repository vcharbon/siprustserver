"""The planned events of a profile: when each fires, and how long its window may last."""

from __future__ import annotations

from dataclasses import dataclass

from .model import Incident
from .profile import EventSpec, Profile


@dataclass(frozen=True)
class PlannedEvent:
    seq: int
    at_s: float
    spec: EventSpec
    duration_s: float
    expected_s: float
    settle_s: float

    @property
    def event_id(self) -> str:
        return f"{self.seq:03d}-{self.spec.label}"

    @property
    def window_end_s(self) -> float:
        """The planned close used by the dry-run overlap check (ADR-0049 §3.7)."""
        return self.at_s + self.expected_s + self.settle_s


def plan_event(seq: int, at_s: float, spec: EventSpec, profile: Profile, incidents: dict[str, Incident]) -> PlannedEvent:
    incs = [incidents[n] for n in spec.incidents]
    duration = spec.duration_s if spec.duration_s is not None else max(i.duration_s for i in incs)
    if spec.expected_s is not None:
        expected = spec.expected_s
    elif spec.duration_s is not None:
        expected = duration + 60
    else:
        expected = max(i.expected_s for i in incs)
    settle = spec.settle_s if spec.settle_s is not None else profile.settle_s
    return PlannedEvent(seq=seq, at_s=at_s, spec=spec, duration_s=duration, expected_s=expected, settle_s=settle)


def plan(profile: Profile, incidents: dict[str, Incident]) -> list[PlannedEvent]:
    """Every scheduled event that fires before the load ends, in time order."""
    sched = profile.schedule
    if sched is None:
        return []
    if sched.at:
        ordered = sorted(sched.at, key=lambda e: e.at_s or 0.0)
        return [
            plan_event(i + 1, e.at_s or 0.0, e, profile, incidents)
            for i, e in enumerate(ordered)
            if (e.at_s or 0.0) < profile.duration_s
        ]
    out: list[PlannedEvent] = []
    t = sched.first_at_s or 0.0
    interval = sched.interval_s or profile.duration_s
    i = 0
    while t < profile.duration_s:
        spec = sched.cycle[i % len(sched.cycle)]
        out.append(plan_event(i + 1, t, spec, profile, incidents))
        i += 1
        t = (sched.first_at_s or 0.0) + i * interval
    return out


def overlaps(events: list[PlannedEvent]) -> list[str]:
    """Pairs of planned windows that overlap: the dry-run refuses them."""
    errs: list[str] = []
    for a, b in zip(events, events[1:]):
        if b.at_s < a.window_end_s:
            errs.append(
                f"event {b.event_id} at {b.at_s:g}s opens before {a.event_id}'s window closes "
                f"({a.at_s:g}s + expected {a.expected_s:g}s + settle {a.settle_s:g}s = {a.window_end_s:g}s)"
            )
    return errs
