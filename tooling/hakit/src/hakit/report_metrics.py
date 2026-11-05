"""The profile's `report_metrics`: series of the system under test read per window.

A reading for the run's report, never a judgement: read once from
VictoriaMetrics after the drain, kept in `report-metrics.json` of the run
directory, so a verdict recomputed later still shows it. Each is summed (a
counter's `increase`) or maxed (a gauge's `max`) per pod over the window, one
scrape past its close so a value written at the end is in. A read that fails
is kept as its error.
"""

from __future__ import annotations

import json
import math
from pathlib import Path

from . import jsonval as jv
from .jsonval import Json, as_obj
from .profile import Profile, ReportMetric
from .vm import QueryFailed, VictoriaMetrics
from .windows import Window

FILE = "report-metrics.json"
SCRAPE_PAD_S = 30


def query(m: ReportMetric, span_s: int) -> str:
    if m.read == "increase":
        return f"sum by (pod) (increase({m.metric}[{span_s}s]))"
    return f"max by (pod) (max_over_time({m.metric}[{span_s}s]))"


def _read(vm: VictoriaMetrics, m: ReportMetric, w: Window) -> Json:
    row: Json = {"metric": m.metric, "read": m.read}
    span_s = math.ceil((w.closed_at_ms - w.opened_at_ms) / 1000) + SCRAPE_PAD_S
    try:
        row["pods"] = vm.by_label(query(m, span_s), "pod", (w.closed_at_ms / 1000) + SCRAPE_PAD_S)
    except QueryFailed as e:
        row["error"] = str(e)[:200]
    return row


def collect(profile: Profile, windows: list[Window], vm: VictoriaMetrics, run_dir: Path) -> None:
    """Writes `report-metrics.json` when the profile names any series."""
    if not profile.report_metrics:
        return
    doc = {w.event: [_read(vm, m, w) for m in profile.report_metrics] for w in windows}
    (run_dir / FILE).write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")


def kept(run_dir: Path) -> dict[str, list[Json]]:
    """The rows `collect` kept, by window; empty when it kept none."""
    p = run_dir / FILE
    if not p.is_file():
        return {}
    doc = as_obj(json.loads(p.read_text()))
    return {event: jv.objs(doc, event) for event in doc}
