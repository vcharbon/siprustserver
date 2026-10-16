"""`--dry-run`: the whole profile on a fake clock against a simulated lab.

The simulated lab answers what a healthy one would: every executable exits 0
with no facts, a generator runs until it is drained, places no call, the
system under test holds no call, a loadgen image registers every shape of the
search path, the API server serves every resource definition asked for (Chaos
Mesh's included), and every Service a stream targets has a cluster IP. Each effect is recorded, and the timeline
printed from the run's own `events.jsonl`.
"""

from __future__ import annotations

import json
import urllib.parse
from collections.abc import Sequence
from datetime import UTC, datetime
from pathlib import Path

from .clock import FakeClock
from .executor import Call, FakeExecutor, HttpResult, ProcResult, Rule
from .jsonl import read_jsonl
from .jsonval import as_obj
from .loadgen import FINAL_METRICS

STAT_HEADER = "StartTime;LastResetTime;CurrentTime;CurrentCall;TotalCallCreated\n"
STAT_ROW = "0;0;0;0;0\n"


class SimulatedLab:
    """Docker generators that run until drained, and quiet observability endpoints
    whose live series stay fresh: a sample at every instant, a record a second."""

    def __init__(self, clock: FakeClock, shapes: Sequence[str] = ()) -> None:
        self.drained: set[str] = set()
        # Pod-placed generators by pod name: their stream.
        self.pods: dict[str, str] = {}
        self.clock = clock
        self.t0 = clock.now_ms()
        self.shapes = list(shapes)

    def pod_rule(self, call: Call) -> ProcResult | None:
        """Pod-placed generators: a created pod runs until drained or deleted;
        its `/stats` holds the counters of the stream it is labelled with."""
        a = call.argv
        if call.kind != "run" or a[:1] != ["kubectl"]:
            return None
        if "service" in a:
            return ProcResult(0, "10.96.0.10", "")
        if "create" in a and "-f" in a and "--raw" not in a:
            doc = as_obj(json.loads(Path(a[a.index("-f") + 1]).read_text()))
            meta = as_obj(doc["metadata"])
            name = str(meta["name"])
            self.pods[name] = str(as_obj(meta["labels"])["hakit-stream"])
            self.drained.discard(name)
            return ProcResult(0, "", "")
        if "--raw" in a and a[a.index("--raw") + 1].endswith("/proxy/drain"):
            self.drained.add(a[a.index("--raw") + 1].split("/pods/")[1].split(":")[0])
            return ProcResult(0, "", "")
        if "delete" in a and "pod" in a:
            self.drained.add(a[a.index("pod") + 1])
            return ProcResult(0, "", "")
        if "get" in a and "pod" in a and "json" in a:
            name = a[a.index("pod") + 1]
            state = '{"terminated":{"exitCode":0}}' if name in self.drained else '{"running":{}}'
            return ProcResult(0, f'{{"status":{{"containerStatuses":[{{"name":"gen","state":{state}}}]}}}}', "")
        if "exec" in a and a[-1] == f"/stats/{FINAL_METRICS}":
            stream = self.pods.get(a[a.index("exec") + 1], "")
            return ProcResult(0, f'loadgen_calls_offered_total{{stream="{stream}",shape="s"}} 0\n', "")
        if "exec" in a and a[-2:] == ["-1", "/stats"]:
            return ProcResult(0, f"{FINAL_METRICS}\nrecords.jsonl\n", "")
        return None

    def rule(self, call: Call) -> ProcResult | HttpResult | str | None:
        pod = self.pod_rule(call)
        if pod is not None:
            return pod
        if call.kind == "run" and call.argv[:2] == ["docker", "run"] and "--list-shapes" in call.argv:
            return ProcResult(0, "".join(f"{s}\n" for s in self.shapes), "")
        if call.kind == "read" and call.path is not None and call.path.name == FINAL_METRICS:
            parts = call.path.parts
            stream = parts[parts.index("streams") + 1] if "streams" in parts else ""
            return f'loadgen_calls_offered_total{{stream="{stream}",shape="s"}} 0\n'
        if call.kind == "run" and call.argv[:2] == ["docker", "run"] and "--name" in call.argv:
            self.drained.discard(call.argv[call.argv.index("--name") + 1])
            return None
        if call.kind == "run" and call.argv[:1] == ["kubectl"] and "crd" in call.argv:
            return ProcResult(0, f"customresourcedefinition.apiextensions.k8s.io/{call.argv[-3]}\n", "")
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
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(call.url).query).get("query", [""])[0]
            if query.endswith("count() live"):
                return HttpResult(200, f'{{"live":"{(self.clock.now_ms() - self.t0) // 1000}"}}\n')
            for field in ("n", "cdrs"):
                if query.endswith(f"count() {field}"):
                    return HttpResult(200, f'{{"{field}":"0"}}\n')
            return HttpResult(200, "")
        if call.kind == "http" and "/api/v1/query" in call.url:
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(call.url).query).get("query", [""])[0]
            value = self.clock.now_ms() / 1000 if "timestamp(" in query else 0
            body = f'{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{}},"value":[0,"{value}"]}}]}}}}'
            return HttpResult(200, body)
        return None


def simulated(extra: list[Rule] | None = None, shapes: Sequence[str] = ()) -> tuple[FakeClock, FakeExecutor]:
    """The simulated lab; `shapes` are the ones a loadgen image registers."""
    clock = FakeClock()
    lab = SimulatedLab(clock, shapes)
    return clock, FakeExecutor([*(extra or []), lab.rule])


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
