"""VictoriaMetrics range and instant queries for the readers, through the executor.

A query that fails or answers no vector reads as no series; the reader that
needed it reports the signal `na` rather than failing.
"""

from __future__ import annotations

import json
import math
import urllib.parse
from dataclasses import dataclass

from ..executor import Executor
from ..jsonval import as_list, as_obj

type Point = tuple[float, float]


@dataclass(frozen=True)
class Series:
    labels: dict[str, str]
    points: list[Point]

    def label(self, name: str) -> str:
        return self.labels.get(name, "?")


def _labels(raw: object) -> dict[str, str]:
    return {k: str(v) for k, v in as_obj(raw).items() if k != "__name__"}


def _value(raw: object) -> float | None:
    try:
        v = float(str(raw))
    except ValueError:
        return None
    return v if math.isfinite(v) else None


class Vm:
    def __init__(self, executor: Executor, base_url: str) -> None:
        self.executor = executor
        self.base_url = base_url
        self.errors: list[str] = []

    def _get(self, path: str, params: dict[str, str]) -> list[object]:
        url = f"{self.base_url}{path}?{urllib.parse.urlencode(params)}"
        res = self.executor.http("GET", url, timeout_s=60)
        try:
            doc = as_obj(json.loads(res.body)) if res.status == 200 else {}
            if doc.get("status") != "success":
                raise ValueError(f"HTTP {res.status}: {res.body[:120]}")
            return as_list(as_obj(doc.get("data")).get("result"))
        except ValueError as e:
            self.errors.append(f"{params.get('query')}: {e}")
            return []

    def range(self, expr: str, start_s: float, end_s: float, step_s: float) -> list[Series]:
        """Each series of a range query, non-finite samples dropped; an empty one left out."""
        params = {"query": expr, "start": f"{start_s:.3f}", "end": f"{end_s:.3f}", "step": f"{step_s:g}"}
        out: list[Series] = []
        for item in self._get("/api/v1/query_range", params):
            d = as_obj(item)
            pts: list[Point] = []
            for pair in as_list(d.get("values", [])):
                t, v = as_list(pair)
                val = _value(v)
                if val is not None:
                    pts.append((float(str(t)), val))
            if pts:
                out.append(Series(_labels(d.get("metric", {})), pts))
        return out

    def at(self, expr: str, t_s: float) -> list[tuple[dict[str, str], float]]:
        """Each series of an instant query at `t_s`."""
        out: list[tuple[dict[str, str], float]] = []
        for item in self._get("/api/v1/query", {"query": expr, "time": f"{t_s:.3f}"}):
            d = as_obj(item)
            val = _value(as_list(d.get("value"))[1])
            if val is not None:
                out.append((_labels(d.get("metric", {})), val))
        return out

    def by(self, expr: str, t_s: float, label: str) -> dict[str, float]:
        return {labels.get(label, "?"): v for labels, v in self.at(expr, t_s)}
