"""Generator health: the stalls of a paced generator, read off its records (ADR-0049 §3.12).

A stall is a gap between two consecutive call starts of a stream longer than
1/cps + `stall_ms`, at the rate in force when the gap began. A measurement the
generator took across a stall measures the generator: a setup time whose span
overlaps it, and a setup deadline inside it.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass


@dataclass(frozen=True)
class Stall:
    stream: str
    at_ms: float
    until_ms: float

    @property
    def ms(self) -> float:
        return self.until_ms - self.at_ms

    def spans(self, started_ms: float, measured_ms: float) -> bool:
        """A measurement from `started_ms` to `measured_ms` was taken across this stall."""
        return started_ms < self.until_ms and measured_ms > self.at_ms

    def holds(self, at_ms: float) -> bool:
        return self.at_ms <= at_ms <= self.until_ms


def detect(stream: str, starts: Sequence[float], rate_at: Callable[[float], float], stall_ms: float) -> list[Stall]:
    """The stalls of one stream from its call starts; `rate_at(at_ms)` is the offered
    rate in force at an instant (0 while the stream offers nothing)."""
    out: list[Stall] = []
    ordered = sorted(starts)
    for a, b in zip(ordered, ordered[1:]):
        rate = rate_at(a)
        if rate <= 0:
            continue
        if b - a > 1000 / rate + stall_ms:
            out.append(Stall(stream, a, b))
    return out


def stall_of(stalls: Sequence[Stall], stream: str, started_ms: float, measured_ms: float) -> Stall | None:
    """The first stall of `stream` a measurement from `started_ms` to `measured_ms` spans."""
    return next((s for s in stalls if s.stream == stream and s.spans(started_ms, measured_ms)), None)


def stall_holding(stalls: Sequence[Stall], stream: str, at_ms: float) -> Stall | None:
    """The stall of `stream` an instant falls in."""
    return next((s for s in stalls if s.stream == stream and s.holds(at_ms)), None)
