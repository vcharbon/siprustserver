"""Freshness of the series a run reads live (ADR-0049 §6.6).

Every scrape interval during the load: each stream's record count in
VictoriaLogs must grow once its first calls could have ended, and the newest
sample of each `live_series` selector in VictoriaMetrics must be recent. A
series that stops for three scrape intervals (for records, at least five calls
of the stream's rate) is one `series_stale` finding, with the instant it was
last fresh.
"""

from __future__ import annotations

from dataclasses import dataclass

from .clock import Clock
from .jsonval import Json
from .vlogs import VictoriaLogs
from .vm import QueryFailed, VictoriaMetrics

SCRAPE_MS = 15_000
STALE_INTERVALS = 3
RECORD_CALLS = 5


@dataclass
class _Watched:
    name: str
    stale_after_ms: int
    fresh_at_ms: int
    last_count: int = -1
    reported: bool = False


class Freshness:
    def __init__(
        self,
        run: str,
        vm: VictoriaMetrics,
        vlogs: VictoriaLogs,
        clock: Clock,
        streams: list[tuple[str, float, float]],
        selectors: list[str],
        run_start_ms: int,
    ) -> None:
        """`streams`: name, cps, and the seconds before its first calls can end."""
        self.run = run
        self.vm = vm
        self.vlogs = vlogs
        self.clock = clock
        self.run_start_ms = run_start_ms
        now = clock.now_ms()
        self.next_ms = now + SCRAPE_MS
        stale = SCRAPE_MS * STALE_INTERVALS
        self.records = [
            _Watched(name, max(stale, int(RECORD_CALLS * 1000 / cps)), now + int(first_s * 1000))
            for name, cps, first_s in streams
        ]
        self.series = [_Watched(sel, stale, now) for sel in selectors]

    def _stale(self, w: _Watched, now: int, kind: str) -> Json | None:
        if w.reported or now - w.fresh_at_ms <= w.stale_after_ms:
            return None
        w.reported = True
        detail = f"{kind} {w.name} stopped updating"
        return {"kind": "series_stale", "at_ms": w.fresh_at_ms, "detail": detail}

    def tick(self) -> list[Json]:
        """Check every series once a scrape interval; the new findings."""
        now = self.clock.now_ms()
        if now < self.next_ms:
            return []
        self.next_ms = now + SCRAPE_MS
        found: list[Json] = []
        for w in self.records:
            n = self.vlogs.live_count(self.run, w.name, self.run_start_ms - 3_600_000, now + 60_000)
            if n is not None and n > w.last_count:
                if w.last_count >= 0:
                    w.fresh_at_ms = max(w.fresh_at_ms, now)
                w.last_count = n
            stale = self._stale(w, now, "the records of stream")
            if stale is not None:
                found.append({**stale, "stream": w.name})
        for w in self.series:
            try:
                newest_s = self.vm.scalar(f"max(timestamp({w.name}))")
                w.fresh_at_ms = max(w.fresh_at_ms, int(newest_s * 1000))
            except QueryFailed:
                pass
            stale = self._stale(w, now, "the series")
            if stale is not None:
                found.append(stale)
        return found
