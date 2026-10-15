"""VictoriaMetrics: the incident series the runner pushes, and instant queries.

`sip_chaos_event{incident,phase,source,run} 1` at each phase instant, and
`sip_chaos_window{incident,run}` at 1 while a window is open, 0 when it closes
(ADR-0049 §2.9). Both are for display; the verdict reads `events.jsonl`.
"""

from __future__ import annotations

import json
import urllib.parse

from .executor import Executor
from .jsonval import as_list, as_obj


def _esc(v: str) -> str:
    return v.replace("\\", "\\\\").replace('"', '\\"')


def sample(name: str, labels: dict[str, str], value: float, at_ms: int) -> str:
    body = ",".join(f'{k}="{_esc(v)}"' for k, v in labels.items())
    return f"{name}{{{body}}} {value:g} {at_ms}"


def chaos_event(incident: str, phase: str, source: str, run: str, at_ms: int) -> str:
    return sample("sip_chaos_event", {"incident": incident, "phase": phase, "source": source, "run": run}, 1, at_ms)


def chaos_window(incident: str, run: str, value: int, at_ms: int) -> str:
    return sample("sip_chaos_window", {"incident": incident, "run": run}, value, at_ms)


class VictoriaMetrics:
    def __init__(self, executor: Executor, base_url: str) -> None:
        self.executor = executor
        self.base_url = base_url
        self.failures = 0

    def push(self, lines: list[str]) -> bool:
        if not lines:
            return True
        body = ("\n".join(lines) + "\n").encode()
        res = self.executor.http("POST", f"{self.base_url}/api/v1/import/prometheus", body)
        ok = 200 <= res.status < 300
        if not ok:
            self.failures += 1
        return ok

    def scalar(self, query: str) -> float | None:
        """The summed value of an instant query, or None when it returns nothing or fails."""
        url = f"{self.base_url}/api/v1/query?{urllib.parse.urlencode({'query': query})}"
        res = self.executor.http("GET", url)
        if res.status != 200:
            return None
        try:
            data = as_obj(as_obj(json.loads(res.body)).get("data"))
            result = as_list(data.get("result"))
        except (ValueError, TypeError):
            return None
        if not result:
            return None
        total = 0.0
        for r in result:
            value = as_list(as_obj(r).get("value"))
            total += float(str(value[1]))
        return total
