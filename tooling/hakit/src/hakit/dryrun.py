"""`--dry-run`: the whole profile on a fake clock against a simulated lab.

The simulated lab answers what a healthy one would: every executable exits 0
with no facts, a generator runs until it is drained, places no call, and the
system under test holds no call. Each effect is recorded, and the timeline
printed from the run's own `events.jsonl`.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

from .clock import FakeClock
from .executor import Call, FakeExecutor, HttpResult, ProcResult, Rule
from .verdict import read_jsonl

STAT_HEADER = "StartTime;LastResetTime;CurrentTime;CurrentCall;TotalCallCreated\n"
STAT_ROW = "0;0;0;0;0\n"


class SimulatedLab:
    """Docker generators that run until drained, and quiet observability endpoints."""

    def __init__(self) -> None:
        self.drained: set[str] = set()

    def rule(self, call: Call) -> ProcResult | HttpResult | str | None:
        if call.kind == "run" and call.argv[:2] == ["docker", "kill"]:
            self.drained.add(call.argv[-1])
            return ProcResult(0, "", "")
        if call.kind == "run" and call.argv[:2] == ["docker", "inspect"]:
            name, fmt = call.argv[-1], call.argv[-2]
            if "Running" in fmt:
                return ProcResult(0, "false\n" if name in self.drained else "true\n", "")
            return ProcResult(0, "0\n", "")
        if call.kind == "read" and call.path is not None and call.path.name == "stat.csv":
            return STAT_HEADER + STAT_ROW
        if call.kind == "read" and call.path == Path("/proc/meminfo"):
            return "MemAvailable: 67108864 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n"
        if call.kind == "read" and call.path == Path("/proc/loadavg"):
            return "0.00 0.00 0.00 1/100 1\n"
        if call.kind == "http" and "/select/logsql/query" in call.url:
            return HttpResult(200, '{"n":"0"}\n' if "stats" in call.url else "")
        if call.kind == "http" and "/api/v1/query" in call.url:
            return HttpResult(200, '{"status":"success","data":{"resultType":"vector","result":[]}}')
        return None


def simulated(extra: list[Rule] | None = None) -> tuple[FakeClock, FakeExecutor]:
    lab = SimulatedLab()
    return FakeClock(), FakeExecutor([*(extra or []), lab.rule])


def timeline(run_dir_events: list[dict[str, object]], start_ms: int) -> list[str]:
    out: list[str] = []
    for line in run_dir_events:
        if line.get("kind") != "phase":
            continue
        at = line.get("at_ms")
        if not isinstance(at, (int, float)):
            continue
        rel = (at - start_ms) / 1000
        stamp = datetime.fromtimestamp(at / 1000, UTC).strftime("%H:%M:%S")
        out.append(f"+{rel:8.1f}s {stamp}  {line.get('event')}  {line.get('step')} {line.get('phase')}")
    return out


__all__ = ["SimulatedLab", "read_jsonl", "simulated", "timeline"]
