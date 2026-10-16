"""Typed view of a schema-valid run profile (`profile.v1`)."""

from __future__ import annotations

from dataclasses import dataclass, field

from . import jsonval as jv
from .jsonval import Json
from .model import Scalar


@dataclass(frozen=True)
class Stream:
    name: str
    driver: str
    start: str
    shape: str
    scenario: str | None
    cps: float
    setup_deadline_s: float
    ring_s: float
    hold_s: float
    max_concurrent: int | None
    params: dict[str, Scalar]
    steps: list[tuple[float, float]] = field(default_factory=lambda: [])

    def cps_at(self, load_s: float) -> float:
        """The offered rate `load_s` seconds after the load started."""
        rate = self.cps
        for at_s, cps in self.steps:
            if at_s <= load_s:
                rate = cps
        return rate

    def rate_between(self, from_s: float, to_s: float) -> float:
        """The lowest offered rate in force over [from_s, to_s] of the load."""
        return min([self.cps_at(from_s), *(cps for at_s, cps in self.steps if from_s < at_s <= to_s)])

    @property
    def peak_cps(self) -> float:
        return max([self.cps, *(cps for _, cps in self.steps)])


@dataclass(frozen=True)
class EventSpec:
    """One schedule entry: an incident (or several) and the budget of its window."""

    incidents: list[str]
    budget: str
    at_s: float | None
    stream: str | None
    params: Json
    target: Json
    duration_s: float | None
    expected_s: float | None
    settle_s: float | None

    @property
    def label(self) -> str:
        return "+".join(self.incidents)


@dataclass(frozen=True)
class Schedule:
    first_at_s: float | None
    interval_s: float | None
    cycle: list[EventSpec]
    at: list[EventSpec]


@dataclass(frozen=True)
class Preflight:
    min_free_memory_gib: float | None
    max_swap_used_gib: float | None
    min_free_disk_gib: float | None
    max_load_per_cpu: float | None


@dataclass(frozen=True)
class Capture:
    file_mb: int
    files: int
    filter: str
    nodes: str | None


@dataclass(frozen=True)
class CdrRequired:
    """A call never answered that must still have a CDR: its ending, and
    optionally its final's code and whether that final carried a Retry-After."""

    ending: str
    code: int | None = None
    retry_after: bool | None = None

    def matches(self, r: Json) -> bool:
        if jv.string(r, "ending") != self.ending or jv.opt_number(r, "answered_at_ms") is not None:
            return False
        if self.code is not None and jv.opt_int(r, "final_code") != self.code:
            return False
        return self.retry_after is None or (jv.opt_int(r, "retry_after_s") is not None) == self.retry_after


@dataclass(frozen=True)
class Profile:
    name: str
    duration_s: float
    drain_s: float
    settle_s: float
    setup_slow_ms: float
    generator_stall_ms: float
    cdr_join: bool
    capture: Capture | None
    catalogs: list[str]
    preflight: Preflight
    streams: list[Stream]
    schedule: Schedule | None
    defaults: dict[str, Json]
    raw: Json = field(compare=False)
    cdr_required: list[CdrRequired] = field(default_factory=lambda: [])

    def stream(self, name: str) -> Stream | None:
        return next((s for s in self.streams if s.name == name), None)


def _capture(d: Json | None) -> Capture | None:
    if d is None:
        return None
    return Capture(
        file_mb=jv.opt_int(d, "file_mb") or 100,
        files=jv.opt_int(d, "files") or 20,
        filter=jv.string(d, "filter", "udp and portrange 5060-5100"),
        nodes=jv.opt_string(d, "nodes"),
    )


def _event(d: Json) -> EventSpec:
    one = jv.opt_string(d, "incident")
    return EventSpec(
        incidents=[one] if one is not None else jv.strings(d, "incidents"),
        budget=jv.string(d, "budget"),
        at_s=jv.opt_number(d, "at_s"),
        stream=jv.opt_string(d, "stream"),
        params=jv.obj(d, "params"),
        target=jv.obj(d, "target"),
        duration_s=jv.opt_number(d, "duration_s"),
        expected_s=jv.opt_number(d, "expected_s"),
        settle_s=jv.opt_number(d, "settle_s"),
    )


def _stream(d: Json) -> Stream:
    mc = jv.opt_int(d, "max_concurrent")
    return Stream(
        name=jv.string(d, "name"),
        driver=jv.string(d, "driver"),
        start=jv.string(d, "start", "run"),
        shape=jv.string(d, "shape"),
        scenario=jv.opt_string(d, "scenario"),
        cps=jv.number(d, "cps"),
        setup_deadline_s=jv.number(d, "setup_deadline_s"),
        ring_s=jv.number(d, "ring_s", 0),
        hold_s=jv.number(d, "hold_s", 0),
        max_concurrent=mc,
        params=jv.scalars(d, "params"),
        steps=sorted((jv.number(s, "at_s"), jv.number(s, "cps")) for s in jv.objs(d, "steps")),
    )


def parse_profile(d: Json) -> Profile:
    sched_d = jv.opt_obj(d, "schedule")
    schedule = (
        None
        if sched_d is None
        else Schedule(
            first_at_s=jv.opt_number(sched_d, "first_at_s"),
            interval_s=jv.opt_number(sched_d, "interval_s"),
            cycle=[_event(e) for e in jv.objs(sched_d, "cycle")],
            at=[_event(e) for e in jv.objs(sched_d, "at")],
        )
    )
    pf = jv.obj(d, "preflight")
    return Profile(
        name=jv.string(d, "name"),
        duration_s=jv.number(d, "duration_s"),
        drain_s=jv.number(d, "drain_s", 120),
        settle_s=jv.number(d, "settle_s", 60),
        setup_slow_ms=jv.number(d, "setup_slow_ms"),
        generator_stall_ms=jv.number(d, "generator_stall_ms", 500),
        cdr_join=jv.boolean(d, "cdr_join"),
        cdr_required=[
            CdrRequired(jv.string(m, "ending"), jv.opt_int(m, "code"), jv.opt_bool(m, "retry_after"))
            for m in jv.objs(d, "cdr_required")
        ],
        capture=_capture(jv.opt_obj(d, "capture")),
        catalogs=jv.strings(d, "catalogs"),
        preflight=Preflight(
            min_free_memory_gib=jv.opt_number(pf, "min_free_memory_gib"),
            max_swap_used_gib=jv.opt_number(pf, "max_swap_used_gib"),
            min_free_disk_gib=jv.opt_number(pf, "min_free_disk_gib"),
            max_load_per_cpu=jv.opt_number(pf, "max_load_per_cpu"),
        ),
        streams=[_stream(s) for s in jv.objs(d, "streams")],
        schedule=schedule,
        defaults={k: jv.as_obj(v, k) for k, v in jv.obj(d, "defaults").items()},
        raw=d,
    )
