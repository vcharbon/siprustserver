"""Typed views of schema-valid incident, budget, profile and catalog documents.

Each `parse_*` reads a document `SchemaSet` already accepted and fills the
schema defaults; it never re-checks what the schema checks.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from . import jsonval as jv
from .jsonval import Json

type Scalar = str | int | float | bool


@dataclass(frozen=True)
class Target:
    selector: str
    namespace: str | None
    pick: str


@dataclass(frozen=True)
class Param:
    type: str
    default: Scalar | None
    description: str


@dataclass(frozen=True)
class Timeouts:
    inject_s: float = 60
    heal_s: float = 120
    probe_s: float = 300
    probe_interval_s: float = 2


@dataclass(frozen=True)
class Incident:
    name: str
    kind: str
    directory: Path
    target: Target | None
    stop: str
    params: dict[str, Param]
    duration_s: float
    expected_s: float
    timeouts: Timeouts
    requires: list[str]

    def executable(self, step: str) -> Path | None:
        p = self.directory / step
        return p if p.is_file() else None

    def max_s(self, duration_s: float) -> float:
        """Every timeout and the hold: how long an event can take before it is declared failed."""
        t = self.timeouts
        return t.inject_s + duration_s + t.heal_s + t.probe_s


def parse_incident(d: Json, directory: Path) -> Incident:
    target_d = jv.opt_obj(d, "target")
    target = (
        None
        if target_d is None
        else Target(
            selector=jv.string(target_d, "selector"),
            namespace=jv.opt_string(target_d, "namespace"),
            pick=jv.string(target_d, "pick", "one"),
        )
    )
    params: dict[str, Param] = {}
    for name, raw in jv.obj(d, "params").items():
        p = jv.as_obj(raw, f"params.{name}")
        default = p.get("default")
        params[name] = Param(
            type=jv.string(p, "type"),
            default=default if isinstance(default, (str, int, float, bool)) else None,
            description=jv.string(p, "description"),
        )
    t = jv.obj(d, "timeouts")
    duration = jv.number(d, "duration_s", 0)
    return Incident(
        name=jv.string(d, "name"),
        kind=jv.string(d, "kind"),
        directory=directory,
        target=target,
        stop=jv.string(d, "stop", "drain"),
        params=params,
        duration_s=duration,
        expected_s=jv.number(d, "expected_s", duration + 60),
        timeouts=Timeouts(
            inject_s=jv.number(t, "inject_s", 60),
            heal_s=jv.number(t, "heal_s", 120),
            probe_s=jv.number(t, "probe_s", 300),
            probe_interval_s=jv.number(t, "probe_interval_s", 2),
        ),
        requires=jv.strings(d, "requires"),
    )


@dataclass(frozen=True)
class Allowance:
    """One `setup_failed` / `unexpected_ending` entry: a key and its allowance."""

    stream: str
    code: int
    reason: str
    max_calls: int | None
    max_seconds: float | None
    # Whether the final carried a `Retry-After` (None: either).
    retry_after: bool | None = None
    # Whether `max_seconds` counts beyond the window's outage, from its first
    # `inject` to its last `recovered`.
    over_outage: bool = False

    def matches(self, stream: str, code: int, reason: str, retry_after: bool = False) -> bool:
        return (
            (self.stream == "*" or self.stream == stream)
            and self.code == code
            and (self.reason == "*" or self.reason == reason)
            and (self.retry_after is None or self.retry_after == retry_after)
        )

    def allowed(self, cps: float, outage_s: float = 0.0) -> float:
        if self.max_calls is not None:
            return float(self.max_calls)
        extra = outage_s if self.over_outage else 0.0
        return ((self.max_seconds or 0.0) + extra) * cps


@dataclass(frozen=True)
class SetupSlowAllowance:
    threshold_ms: float | None
    max_calls: int | None
    max_seconds: float | None
    # Whether `max_seconds` counts beyond the window's outage.
    over_outage: bool = False

    def allowed(self, cps: float, outage_s: float = 0.0) -> float:
        if self.max_calls is not None:
            return float(self.max_calls)
        extra = outage_s if self.over_outage else 0.0
        return ((self.max_seconds or 0.0) + extra) * cps


@dataclass(frozen=True)
class CountAllowance:
    """One kind of a counted class's allowance (a `cdr_mismatch` kind):
    `max_calls`, or `max_seconds` of an offered rate, optionally beyond the
    window's outage; `excess_ms` also allows every impact whose excess past
    the join's tolerance is at most that much."""

    max_calls: int | None = 0
    max_seconds: float | None = None
    over_outage: bool = False
    excess_ms: float | None = None

    def allowed(self, cps: float, outage_s: float = 0.0) -> float:
        if self.max_calls is not None:
            return float(self.max_calls)
        extra = outage_s if self.over_outage else 0.0
        return ((self.max_seconds or 0.0) + extra) * cps


@dataclass(frozen=True)
class RecoveryRule:
    """How a window's traffic counts as back (ADR-0049 §3.6); `max_s` None means
    back before the incident's maximum."""

    max_s: float | None = None
    tolerance: float = 0.1
    baseline_s: float = 60
    hold_s: float = 10
    latency_slack_ms: float = 50


@dataclass(frozen=True)
class Budget:
    name: str
    setup_failed: list[Allowance]
    unexpected_ending: list[Allowance]
    setup_slow: SetupSlowAllowance
    raw: Json = field(compare=False)
    cdr_mismatch: dict[str, CountAllowance] = field(default_factory=lambda: {})
    recovery: RecoveryRule = RecoveryRule()
    classes: dict[str, float] = field(default_factory=lambda: {})


NO_BUDGET = Budget("", [], [], SetupSlowAllowance(None, None, None), raw={})


def _allowances(d: Json, key: str) -> list[Allowance]:
    out: list[Allowance] = []
    for e in jv.objs(d, key):
        code = jv.opt_int(e, "code")
        out.append(
            Allowance(
                stream=jv.string(e, "stream"),
                code=code if code is not None else 0,
                reason=jv.string(e, "reason"),
                max_calls=jv.opt_int(e, "max_calls"),
                max_seconds=jv.opt_number(e, "max_seconds"),
                retry_after=jv.opt_bool(e, "retry_after"),
                over_outage=jv.boolean(e, "over_outage"),
            )
        )
    return out


def _count_allowance(d: Json) -> CountAllowance:
    seconds = jv.opt_number(d, "max_seconds")
    calls = jv.opt_int(d, "max_calls")
    return CountAllowance(
        max_calls=calls if calls is not None or seconds is not None else 0,
        max_seconds=seconds,
        over_outage=jv.boolean(d, "over_outage"),
        excess_ms=jv.opt_number(d, "excess_ms"),
    )


def parse_budget(d: Json) -> Budget:
    slow = jv.obj(d, "setup_slow")
    rec = jv.obj(d, "recovery")
    return Budget(
        name=jv.string(d, "name"),
        setup_failed=_allowances(d, "setup_failed"),
        unexpected_ending=_allowances(d, "unexpected_ending"),
        setup_slow=SetupSlowAllowance(
            threshold_ms=jv.opt_number(slow, "threshold_ms"),
            max_calls=jv.opt_int(slow, "max_calls"),
            max_seconds=jv.opt_number(slow, "max_seconds"),
            over_outage=jv.boolean(slow, "over_outage"),
        ),
        raw=d,
        cdr_mismatch={k: _count_allowance(jv.as_obj(v, k)) for k, v in jv.obj(d, "cdr_mismatch").items()},
        recovery=RecoveryRule(
            max_s=jv.opt_number(rec, "max_s"),
            tolerance=jv.number(rec, "tolerance", 0.1),
            baseline_s=jv.number(rec, "baseline_s", 60),
            hold_s=jv.number(rec, "hold_s", 10),
            latency_slack_ms=jv.number(rec, "latency_slack_ms", 50),
        ),
        classes={k: jv.number(jv.as_obj(v, k), "max") for k, v in jv.obj(d, "classes").items()},
    )


@dataclass(frozen=True)
class ExpectedEnding:
    ending: str
    code: int | None
    early_media: bool | None


@dataclass(frozen=True)
class Leg:
    role: str
    ruri_prefixes: list[str]
    expected: list[ExpectedEnding]
    # The caller endings under which the leg opens; None: every expected one.
    when: list[ExpectedEnding] | None = None


@dataclass(frozen=True)
class Shape:
    shape: str
    catalog: str
    ruri_user: str | None
    headers: list[tuple[str, str]]
    legs: list[Leg]
    expected: list[ExpectedEnding]
    load_safe: bool

    @property
    def expects_answer(self) -> bool:
        return any(e.ending == "answered" for e in self.expected)


@dataclass(frozen=True)
class Catalog:
    name: str
    placeholders: dict[str, str]
    shapes: list[Shape]
    raw: Json = field(compare=False)


def _endings(items: list[Json]) -> list[ExpectedEnding]:
    out: list[ExpectedEnding] = []
    for e in items:
        em = e.get("early_media")
        out.append(
            ExpectedEnding(
                ending=jv.string(e, "ending"),
                code=jv.opt_int(e, "code"),
                early_media=em if isinstance(em, bool) else None,
            )
        )
    return out


def parse_catalog(d: Json) -> Catalog:
    name = jv.string(d, "name")
    shapes: list[Shape] = []
    for s in jv.objs(d, "shapes"):
        req = jv.obj(s, "request")
        shapes.append(
            Shape(
                shape=jv.string(s, "shape"),
                catalog=name,
                ruri_user=jv.opt_string(req, "ruri_user"),
                headers=[(jv.string(h, "name"), jv.string(h, "value")) for h in jv.objs(req, "headers")],
                legs=[
                    Leg(
                        role=jv.string(leg, "role"),
                        ruri_prefixes=jv.strings(leg, "ruri_prefixes"),
                        expected=_endings(jv.objs(leg, "expected")),
                        when=_endings(jv.objs(leg, "when")) if "when" in leg else None,
                    )
                    for leg in jv.objs(s, "legs")
                ],
                expected=_endings(jv.objs(s, "expected")),
                load_safe=jv.boolean(s, "load_safe"),
            )
        )
    placeholders = {k: str(v) for k, v in jv.obj(d, "placeholders").items()}
    return Catalog(name=name, placeholders=placeholders, shapes=shapes, raw=d)
