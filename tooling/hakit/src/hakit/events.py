"""The run's event journal: `events.jsonl` and the series pushed beside it.

Each phase is written to `events.jsonl` and pushed as `sip_chaos_event` at the
same instant. An open window is pushed as `sip_chaos_window` 1 at its open,
again every 10 s while it stays open, and 0 at its close. Pushes are for
display: one that fails is counted, and the count of an event's failed pushes
lands in its facts (`hakit` / `vm_push`) when its window closes.
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
    opened_ms: int = 0


class EventJournal:
    def __init__(self, run: str, run_dir: Path, clock: Clock, vm: VictoriaMetrics) -> None:
        self.run = run
        self.path = run_dir / "events.jsonl"
        self.clock = clock
        self.vm = vm
        self.open: dict[str, OpenWindow] = {}
        self.sources: dict[str, str] = {}
        self.push_failed: dict[str, int] = {}
        # (event, opened, closed) of every window closed since the last `take_closed`.
        self.closed: list[tuple[str, int, int]] = []

    def _push(self, event: str, lines: list[str]) -> None:
        if lines and not self.vm.push(lines):
            self.push_failed[event] = self.push_failed.get(event, 0) + 1

    def _append(self, line: Json) -> None:
        with self.path.open("a") as fh:
            fh.write(json.dumps(line) + "\n")

    def begin(self, event: str, source: str, incidents: list[str], budget: str, settle_s: float, max_s: float) -> None:
        self.sources[event] = source
        self._append(event_line(event, source, incidents, budget, settle_s, max_s))

    def phase(self, event: str, step: str, phase: str, at_ms: int | None = None) -> int:
        """Journal and push a phase, at `at_ms` when the step measured its own
        instant (a load stream's exit), else now."""
        at = self.clock.now_ms() if at_ms is None else at_ms
        self._append(phase_line(event, step, phase, at))
        lines = [chaos_event(step, phase, self.sources.get(event, "schedule"), self.run, at)]
        if phase == "inject" and event not in self.open:
            w = OpenWindow(event, step.split("+"), last_push_ms=at, opened_ms=at)
            self.open[event] = w
            lines += [chaos_window(i, self.run, 1, at) for i in w.incidents]
        self._push(event, lines)
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
        for ev, w in list(self.open.items()):
            if w.closes_at_ms is not None and now >= w.closes_at_ms:
                self._push(ev, [chaos_window(i, self.run, 0, w.closes_at_ms) for i in w.incidents])
                del self.open[ev]
                self.closed.append((ev, w.opened_ms, w.closes_at_ms))
                failed = self.push_failed.pop(ev, 0)
                if failed:
                    self.facts(ev, "hakit", "vm_push", {"failed": failed})
            elif now - w.last_push_ms >= KEEPALIVE_MS:
                self._push(ev, [chaos_window(i, self.run, 1, now) for i in w.incidents])
                w.last_push_ms = now

    def take_closed(self) -> list[tuple[str, int, int]]:
        """The windows closed since the last call: (event, opened, closed)."""
        out, self.closed = self.closed, []
        return out

    def next_close_ms(self) -> int | None:
        closes = [w.closes_at_ms for w in self.open.values() if w.closes_at_ms is not None]
        return min(closes) if closes else None
