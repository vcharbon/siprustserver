"""Recovery from the records, per stream (ADR-0049 §3.6).

The baseline of a stream is its successful-setup ratio and nearest-rank p99
setup time over the calls started in `baseline_s` before `inject`, leaving out
calls started inside an earlier window. A baseline needs MIN_SAMPLES calls, a
p99 MIN_SAMPLES setup times, on both sides. From `healed`, in steps of 1 s,
the stream is back at the start of the first span of `hold_s` holding at least
MIN_SAMPLES calls, a ratio of at least the baseline's × (1 − tolerance) and a
p99 of at most max(baseline × (1 + tolerance), baseline + `latency_slack_ms`).
Calls are placed in time by their `started_at_ms`, so a rate change is no
impact.
"""

from __future__ import annotations

import bisect
import math
from collections.abc import Sequence
from dataclasses import dataclass

from .model import RecoveryRule

STEP_MS = 1000
MIN_SAMPLES = 20


@dataclass(frozen=True)
class Sample:
    """One call's setup: whether it succeeded, and its setup time unless none was
    measured or a generator stall spoiled it."""

    started_at_ms: float
    ok: bool
    latency_ms: float | None


@dataclass(frozen=True)
class Baseline:
    ratio: float
    p99_ms: float | None


@dataclass(frozen=True)
class StreamRecovery:
    stream: str
    baseline: Baseline
    back_at_ms: float | None

    def recovery_s(self, healed_ms: float) -> float | None:
        return None if self.back_at_ms is None else round((self.back_at_ms - healed_ms) / 1000, 3)


@dataclass(frozen=True)
class NoBaseline:
    """A stream whose baseline is too small to judge a recovery: why."""

    stream: str
    reason: str


def p99(values: Sequence[float]) -> float | None:
    """The nearest-rank 99th percentile; None without a value."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(0.99 * len(ordered)) - 1)]


class StreamSamples:
    """The samples of one stream, ordered by start, with span reads."""

    def __init__(self, samples: Sequence[Sample]) -> None:
        self.samples = sorted(samples, key=lambda s: s.started_at_ms)
        self.starts = [s.started_at_ms for s in self.samples]

    def span(self, from_ms: float, to_ms: float) -> list[Sample]:
        """The samples started in [from_ms, to_ms)."""
        lo = bisect.bisect_left(self.starts, from_ms)
        hi = bisect.bisect_left(self.starts, to_ms)
        return self.samples[lo:hi]


def measure(samples: Sequence[Sample]) -> Baseline | None:
    """The ratio and p99 of a set of calls; None under MIN_SAMPLES calls, a p99 of
    None under MIN_SAMPLES setup times."""
    if len(samples) < MIN_SAMPLES:
        return None
    ratio = sum(1 for s in samples if s.ok) / len(samples)
    times = [s.latency_ms for s in samples if s.latency_ms is not None]
    return Baseline(ratio, p99(times) if len(times) >= MIN_SAMPLES else None)


def is_back(span: Baseline, base: Baseline, rule: RecoveryRule) -> bool:
    if span.ratio < base.ratio * (1 - rule.tolerance):
        return False
    if base.p99_ms is None:
        return True
    if span.p99_ms is None:
        return False
    return span.p99_ms <= max(base.p99_ms * (1 + rule.tolerance), base.p99_ms + rule.latency_slack_ms)


def recover(
    stream: str,
    samples: StreamSamples,
    inject_ms: float,
    healed_ms: float,
    limit_ms: float,
    rule: RecoveryRule,
    excluded: Sequence[tuple[float, float]] = (),
) -> StreamRecovery | NoBaseline:
    """The recovery of one stream after an event; `excluded` are the spans of
    earlier windows. `back_at_ms` is None when no span starting before `limit_ms`
    is back."""
    pool = [
        s
        for s in samples.span(inject_ms - rule.baseline_s * 1000, inject_ms)
        if not any(a <= s.started_at_ms <= b for a, b in excluded)
    ]
    base = measure(pool)
    if base is None:
        return NoBaseline(
            stream,
            f"{len(pool)} calls started in the {rule.baseline_s:g} s before inject outside earlier windows, under {MIN_SAMPLES}",
        )
    hold_ms = rule.hold_s * 1000
    t = healed_ms
    while t < limit_ms:
        got = measure(samples.span(t, t + hold_ms))
        if got is not None and is_back(got, base, rule):
            return StreamRecovery(stream, base, t)
        t += STEP_MS
    return StreamRecovery(stream, base, None)
