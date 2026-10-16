"""Generator health: the stalls of a paced generator (ADR-0049 §3.12).

Two sources. From the records: a gap between two consecutive call starts of a
stream longer than 1/cps + `stall_ms`, at the lowest rate in force over the
gap, unless the stream held its call limit then (the system under test holding
calls, not the generator stalling); the stall runs from the start that was due
to the start that came. From the generator's own watchdog: the interval between
two statistics rows across which its watchdog counter grew. A measurement the
generator took across a stall measures the generator: a setup time whose span
overlaps it, and a setup deadline inside it.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass

from .generator_stats import StatRow

STARTS = "starts"
WATCHDOG = "watchdog"

type RateBetween = Callable[[float, float], float]
type AtLimit = Callable[[float, float], bool]


@dataclass(frozen=True)
class Stall:
    stream: str
    at_ms: float
    until_ms: float
    source: str = STARTS

    @property
    def ms(self) -> float:
        return self.until_ms - self.at_ms

    def spans(self, started_ms: float, measured_ms: float) -> bool:
        """A measurement from `started_ms` to `measured_ms` was taken across this stall."""
        return started_ms < self.until_ms and measured_ms > self.at_ms

    def holds(self, at_ms: float) -> bool:
        return self.at_ms <= at_ms <= self.until_ms


def _never(_a: float, _b: float) -> bool:
    return False


def detect(
    stream: str, starts: Sequence[float], rate_between: RateBetween, stall_ms: float, at_limit: AtLimit = _never
) -> list[Stall]:
    """The stalls of one stream from its call starts; `rate_between(a, b)` is the
    lowest offered rate in force over [a, b] (0 while the stream offers nothing),
    `at_limit(a, b)` whether the stream held its call limit over it."""
    out: list[Stall] = []
    ordered = sorted(starts)
    for a, b in zip(ordered, ordered[1:]):
        rate = rate_between(a, b)
        if rate <= 0:
            continue
        due = 1000 / rate
        if b - a > due + stall_ms and not at_limit(a, b):
            out.append(Stall(stream, a + due, b))
    return out


def watchdog_stalls(stream: str, rows: Sequence[StatRow]) -> list[Stall]:
    """One stall per pair of consecutive rows across which the watchdog counter grew."""
    return [
        Stall(stream, p.at_ms, r.at_ms, WATCHDOG)
        for p, r in zip(rows, rows[1:])
        if p.watchdog is not None and r.watchdog is not None and r.watchdog > p.watchdog
    ]


def stall_of(stalls: Sequence[Stall], stream: str, started_ms: float, measured_ms: float) -> Stall | None:
    """The first stall of `stream` a measurement from `started_ms` to `measured_ms` spans."""
    return next((s for s in stalls if s.stream == stream and s.spans(started_ms, measured_ms)), None)


def stall_holding(stalls: Sequence[Stall], stream: str, at_ms: float) -> Stall | None:
    """The stall of `stream` an instant falls in."""
    return next((s for s in stalls if s.stream == stream and s.holds(at_ms)), None)
