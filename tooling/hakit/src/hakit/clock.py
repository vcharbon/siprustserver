"""The clock every wait of the runner goes through (ADR-0049 §6.3)."""

from __future__ import annotations

import time
from typing import Protocol


class Clock(Protocol):
    def now_ms(self) -> int: ...

    def sleep(self, seconds: float) -> None: ...


class RealClock:
    def now_ms(self) -> int:
        return time.time_ns() // 1_000_000

    def sleep(self, seconds: float) -> None:
        if seconds > 0:
            time.sleep(seconds)


class FakeClock:
    """A clock that only moves when the runner sleeps or an effect takes time."""

    def __init__(self, start_ms: int = 1_800_000_000_000) -> None:
        self._now = start_ms

    def now_ms(self) -> int:
        return self._now

    def sleep(self, seconds: float) -> None:
        if seconds > 0:
            self._now += int(round(seconds * 1000))

    def advance_ms(self, ms: int) -> None:
        self._now += ms
