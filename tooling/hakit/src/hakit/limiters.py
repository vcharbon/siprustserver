"""The limited streams' self-repair (ADR-0049 §3.13): a coarse check that a call
limiter's admissions come back to its cap after whatever lost its messages.

The truth is the generator's: a limited call holds a slot from its answer to
its end, so the calls of a limiter's streams answered and not yet ended are
the calls it admitted, sampled each second and judged per span of `SPAN_S`.
Outside every window's zone a saturated limiter is full at least once a
minute (the span's max reaches the cap) and holds no more than its cap (the
span's median). A window's zone runs from its opening to its close plus the
limiter's `repair_s`; inside it the median may reach every call the streams
offer, cps × (ring + hold), when the budget states `limiter.overshoot:
offered` (callers failing open admit them all), and the max may stay under
the cap when the budget allows `limiter.undershoot`. A max under the cap is
`limiter_undershoot` (callers refused calls they are entitled to, a stuck
slot included), a median over the bound `limiter_overshoot` (minor).
"""

from __future__ import annotations

import statistics

from . import jsonval as jv
from .jsonval import Json
from .model import NO_BUDGET, Budget
from .profile import LimiterSpec, Profile
from .windows import Window

# One coarse sample of the admitted count: a minute of seconds.
SPAN_S = 60
# The streams offer at least this many calls above the cap, so the limiter
# runs full and its max reaches the cap every minute.
SLACK = 2


def offered_concurrency(profile: Profile, spec: LimiterSpec) -> float:
    """The calls the limiter's streams would hold at once uncapped: cps × (ring + hold)."""
    return sum(s.cps * (s.ring_s + s.hold_s) for n in spec.streams if (s := profile.stream(n)) is not None)


def limiter_errors(where: str, profile: Profile) -> list[str]:
    """A limiter names streams of the profile, and they saturate it: the band
    holds only for a limiter its calls keep full."""
    errs: list[str] = []
    for spec in profile.limiters:
        w = f"{where}: limiter {spec.id!r}"
        unknown = [n for n in spec.streams if profile.stream(n) is None]
        if unknown:
            errs.append(f"{w}: names streams the profile does not run: {unknown}")
            continue
        offered = offered_concurrency(profile, spec)
        if offered < spec.cap + SLACK:
            errs.append(f"{w}: its streams offer {offered:g} concurrent calls, under cap {spec.cap} + {SLACK}")
    return errs


def _active(records: list[Json], streams: set[str], from_s: int, to_s: int) -> list[int]:
    """The answered, not yet ended calls of `streams` at each second of [from_s, to_s)."""
    delta = [0] * (to_s - from_s + 1)
    for r in records:
        answered = jv.opt_number(r, "answered_at_ms")
        if answered is None or jv.string(r, "stream") not in streams:
            continue
        a = min(max(int(answered) // 1000, from_s), to_s) - from_s
        e = min(max(int(jv.number(r, "ended_at_ms")) // 1000, from_s), to_s) - from_s
        delta[a] += 1
        delta[e] -= 1
    out: list[int] = []
    level = 0
    for d in delta[:-1]:
        level += d
        out.append(level)
    return out


def _allowance(
    zones: list[tuple[int, int, Budget, str]], a_s: int, b_s: int, cap: int, offered: float
) -> tuple[float, bool, str | None]:
    """The highest median allowed over [a_s, b_s), whether a median under the
    band is, and the window granting them; outside every zone, the band."""
    high, under, event = float(cap), False, None
    for z_from, z_to, budget, ev in zones:
        if z_from < b_s and a_s < z_to:
            lim = jv.obj(budget.raw, "limiter")
            if lim.get("overshoot") == "offered":
                high = max(high, offered)
            under = under or jv.boolean(lim, "undershoot")
            event = ev
    return high, under, event


def judge_limiters(
    profile: Profile,
    records: list[Json],
    windows: list[Window],
    budgets: dict[str, Budget],
    load_start_ms: float,
    load_end_ms: float,
) -> list[Json]:
    """One finding per run of consecutive spans out of their bound, per limiter."""
    out: list[Json] = []
    for spec in profile.limiters:
        start = int(load_start_ms // 1000 + spec.repair_s)
        end = int(load_end_ms // 1000)
        # No record of its streams, no admission measured: the streams' own
        # reconciliation reports the missing records.
        if end - start < SPAN_S or not any(jv.string(r, "stream") in spec.streams for r in records):
            continue
        active = _active(records, set(spec.streams), start, end)
        zones = [
            (w.opened_at_ms // 1000, int(w.closed_at_ms // 1000 + spec.repair_s), budgets.get(w.budget, NO_BUDGET), w.event)
            for w in windows
        ]
        offered = offered_concurrency(profile, spec)
        current: Json | None = None
        for a in range(start, end - SPAN_S + 1, SPAN_S):
            b = a + SPAN_S
            span = active[a - start : b - start]
            median = statistics.median(span)
            high, under_ok, event = _allowance(zones, a, b, spec.cap, offered)
            under = max(span) < spec.cap and not under_ok
            kind = "limiter_overshoot" if median > high else "limiter_undershoot" if under else None
            if kind is None:
                current = None
                continue
            if current is not None and current["kind"] == kind and current["to_ms"] == a * 1000:
                current["to_ms"] = b * 1000
                current["medians"] = [*jv.as_list(current["medians"]), median]
                current["maxima"] = [*jv.as_list(current["maxima"]), max(span)]
                continue
            current = {
                "kind": kind,
                "severity": "major" if kind == "limiter_undershoot" else "minor",
                "limiter": spec.id,
                "streams": spec.streams,
                "from_ms": a * 1000,
                "to_ms": b * 1000,
                "medians": [median],
                "maxima": [max(span)],
                "bound": [spec.cap, high],
                "window": event,
            }
            out.append(current)
    return out
