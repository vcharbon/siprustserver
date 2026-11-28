"""VictoriaMetrics: the incident series the runner pushes, instant queries, and
raw sample exports.

`sip_chaos_event{incident,phase,source,run} 1` at each phase instant, and
`sip_chaos_window{incident,run}` at 1 while a window is open, 0 when it closes
(ADR-0049 §2.9). Both are for display; the verdict reads `events.jsonl`.
"""

from __future__ import annotations

import json
import math
import urllib.parse
from dataclasses import dataclass

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


class QueryFailed(Exception):
    pass


@dataclass(frozen=True)
class RawSeries:
    """One series' raw samples, `(epoch ms, value)` ascending, staleness markers left out."""

    labels: dict[str, str]
    points: list[tuple[int, float]]

    def label(self, name: str) -> str:
        return self.labels.get(name, "")


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

    def scalar(self, query: str, latency_offset: str | None = None) -> float:
        """The summed value of an instant query; `QueryFailed` when VictoriaMetrics
        does not answer or the query selects no series. An instant query at now
        sees no sample newer than VictoriaMetrics' latency offset (30 s by
        default); `latency_offset` sets it for this query."""
        params = {"query": query} | ({"latency_offset": latency_offset} if latency_offset else {})
        url = f"{self.base_url}/api/v1/query?{urllib.parse.urlencode(params)}"
        res = self.executor.http("GET", url)
        if res.status != 200:
            raise QueryFailed(f"VictoriaMetrics did not answer {query!r}: HTTP {res.status} {res.body[:120]}")
        try:
            data = as_obj(as_obj(json.loads(res.body)).get("data"))
            result = as_list(data.get("result"))
        except (ValueError, TypeError) as e:
            raise QueryFailed(f"VictoriaMetrics answered {query!r} with no vector: {e}") from e
        if not result:
            raise QueryFailed(f"VictoriaMetrics holds no series for {query!r}")
        total = 0.0
        for r in result:
            value = as_list(as_obj(r).get("value"))
            total += float(str(value[1]))
        return total

    def by_label(self, query: str, label: str, at_s: float) -> dict[str, float]:
        """An instant query at `at_s`, each series' value by its `label`; empty
        when it selects no series; `QueryFailed` when VictoriaMetrics does not answer."""
        url = f"{self.base_url}/api/v1/query?{urllib.parse.urlencode({'query': query, 'time': f'{at_s:.3f}'})}"
        res = self.executor.http("GET", url)
        if res.status != 200:
            raise QueryFailed(f"VictoriaMetrics did not answer {query!r}: HTTP {res.status} {res.body[:120]}")
        try:
            data = as_obj(as_obj(json.loads(res.body)).get("data"))
            out: dict[str, float] = {}
            for r in as_list(data.get("result")):
                row = as_obj(r)
                out[str(as_obj(row.get("metric")).get(label, ""))] = float(str(as_list(row.get("value"))[1]))
            return out
        except (ValueError, TypeError) as e:
            raise QueryFailed(f"VictoriaMetrics answered {query!r} with no vector: {e}") from e

    def export(self, selector: str, start_ms: int, end_ms: int) -> list[RawSeries]:
        """The raw samples of every series `selector` matches in `[start_ms, end_ms]`,
        each series once however many lines it comes in; `QueryFailed` when
        VictoriaMetrics does not answer or answers lines that do not parse."""
        params = {"match[]": selector, "start": f"{start_ms / 1000:.3f}", "end": f"{end_ms / 1000:.3f}"}
        res = self.executor.http("GET", f"{self.base_url}/api/v1/export?{urllib.parse.urlencode(params)}", timeout_s=60)
        if res.status != 200:
            raise QueryFailed(f"VictoriaMetrics did not export {selector!r}: HTTP {res.status} {res.body[:120]}")
        merged: dict[str, RawSeries] = {}
        try:
            for line in res.body.splitlines():
                if not line.strip():
                    continue
                row = as_obj(json.loads(line))
                labels = {k: str(v) for k, v in as_obj(row.get("metric")).items()}
                pairs = zip(as_list(row.get("timestamps")), as_list(row.get("values")), strict=True)
                points = [(int(str(t)), float(str(v))) for t, v in pairs if v is not None]
                key = json.dumps(labels, sort_keys=True)
                prior = merged[key].points if key in merged else []
                merged[key] = RawSeries(labels, prior + [p for p in points if math.isfinite(p[1])])
        except (ValueError, TypeError) as e:
            raise QueryFailed(f"VictoriaMetrics exported {selector!r} in lines that do not parse: {e}") from e
        return [RawSeries(s.labels, sorted(s.points)) for s in merged.values()]
