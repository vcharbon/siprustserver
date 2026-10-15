"""The run's event journal: `events.jsonl` and the series pushed beside it.

Each phase is written to `events.jsonl` and pushed as `sip_chaos_event` at the
same instant. An open window is pushed as `sip_chaos_window` 1 at its open,
again every 10 s while it stays open, and 0 at its close.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from .clock import Clock
from .jsonval import Json
from .vm import VictoriaMetrics, chaos_event, chaos_window
from .windows import event_line, facts_line, phase_line

KEEPALIVE_MS = 10_000


@dataclass
class OpenWindow:
    event: str
    incidents: list[str]
    closes_at_ms: int | None = None
    last_push_ms: int = 0


class EventJournal:
    def __init__(self, run: str, run_dir: Path, clock: Clock, vm: VictoriaMetrics) -> None:
        self.run = run
        self.path = run_dir / "events.jsonl"
        self.clock = clock
        self.vm = vm
        self.open: dict[str, OpenWindow] = {}
        self.sources: dict[str, str] = {}

    def _append(self, line: Json) -> None:
        with self.path.open("a") as fh:
            fh.write(json.dumps(line) + "\n")

    def begin(self, event: str, source: str, incidents: list[str], budget: str, settle_s: float, max_s: float) -> None:
        self.sources[event] = source
        self._append(event_line(event, source, incidents, budget, settle_s, max_s))

    def phase(self, event: str, step: str, phase: str) -> int:
        at = self.clock.now_ms()
        self._append(phase_line(event, step, phase, at))
        lines = [chaos_event(step, phase, self.sources.get(event, "schedule"), self.run, at)]
        if phase == "inject" and event not in self.open:
            w = OpenWindow(event, step.split("+"), last_push_ms=at)
            self.open[event] = w
            lines += [chaos_window(i, self.run, 1, at) for i in w.incidents]
        self.vm.push(lines)
        return at

    def facts(self, event: str, step: str, executable: str, facts: Json) -> None:
        self._append(facts_line(event, step, executable, facts))

    def close_at(self, event: str, at_ms: int) -> None:
        """The instant the window of `event` closes; `tick` pushes the close when it passes."""
        w = self.open.get(event)
        if w is not None:
            w.closes_at_ms = at_ms

    def tick(self) -> None:
        """Push the keepalive of every open window and the close of every window that ended."""
        now = self.clock.now_ms()
        lines: list[str] = []
        for ev, w in list(self.open.items()):
            if w.closes_at_ms is not None and now >= w.closes_at_ms:
                lines += [chaos_window(i, self.run, 0, w.closes_at_ms) for i in w.incidents]
                del self.open[ev]
            elif now - w.last_push_ms >= KEEPALIVE_MS:
                lines += [chaos_window(i, self.run, 1, now) for i in w.incidents]
                w.last_push_ms = now
        self.vm.push(lines)

    def next_close_ms(self) -> int | None:
        closes = [w.closes_at_ms for w in self.open.values() if w.closes_at_ms is not None]
        return min(closes) if closes else None
