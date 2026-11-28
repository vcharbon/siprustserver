"""What a counter counted over a span, read from its raw samples.

A counter series lives as long as one scrape target: a rollout or a pod
replacement ends a series and starts another, and a container restarted in
place resets its own. An `increase()` over the span guesses whether a series
first seen inside it started at 0, and its lookbehind margin adds what the
counter counted before the span. These reads count only what the scrapes saw
inside the span. What a target counted after its last scrape is lost to every
reader; `lossy` says when a span holds such a loss.
"""

from __future__ import annotations

from collections.abc import Callable

from .vm import RawSeries

# How far before a span the export reaches for each series' last sample before it.
BASELINE_LOOKBACK_MS = 600_000


def _before(s: RawSeries, start_ms: int) -> float | None:
    return next((v for t, v in reversed(s.points) if t <= start_ms), None)


def increase(series: list[RawSeries], start_ms: int) -> float:
    """The sum of what each series counted after `start_ms`. A series counts
    from its last sample at or before `start_ms`, or from 0 when its first
    sample comes after it (a target born inside the span). A drop is a reset:
    the counter counted its new value from 0."""
    total = 0.0
    for s in series:
        prev = _before(s, start_ms)
        for _, v in (p for p in s.points if p[0] > start_ms):
            total += v - prev if prev is not None and v >= prev else v
            prev = v
    return total


def lossy(series: list[RawSeries], start_ms: int) -> bool:
    """Whether a series ended or reset after `start_ms`: its target went away
    between two scrapes, and what it counted after its last scrape is in no
    series, so `increase` is a lower bound."""
    for s in series:
        if s.ended_ms is not None and s.ended_ms > start_ms:
            return True
        prev = _before(s, start_ms)
        for _, v in (p for p in s.points if p[0] > start_ms):
            if prev is not None and v < prev:
                return True
            prev = v
    return False


def grown[K](series: list[RawSeries], start_ms: int, target: Callable[[RawSeries], K]) -> dict[K, float]:
    """For a counter that never resets for one `target` (several exporters may
    each export it): by target, its highest sample after `start_ms` minus its
    highest at or before it, or minus 0 for a target born after `start_ms`.
    Targets that did not grow are left out."""
    before: dict[K, float] = {}
    after: dict[K, float] = {}
    for s in series:
        key = target(s)
        for t, v in s.points:
            side = before if t <= start_ms else after
            side[key] = max(side.get(key, v), v)
    grew = {k: v - before.get(k, 0.0) for k, v in after.items()}
    return {k: v for k, v in grew.items() if v > 0}
