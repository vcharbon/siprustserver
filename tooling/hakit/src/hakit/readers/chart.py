"""`hakit chart`: a VictoriaMetrics range query as one standalone inline SVG.

With `--run <run-dir>` the range defaults to the run's load and drain, the
verdict's windows are shaded, and each event's inject and healed are marked;
`--marker <epoch>:<label>` adds rules of its own.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

from .run_view import RunView
from .svg import Chart, render
from .vmrange import Vm


@dataclass(frozen=True)
class ChartRequest:
    title: str
    exprs: list[str]
    legends: list[str]
    start_s: float | None = None
    end_s: float | None = None
    step_s: float = 30
    unit: str = ""
    markers: tuple[str, ...] = ()


def _markers(raw: Sequence[str]) -> list[tuple[float, str]]:
    out: list[tuple[float, str]] = []
    for m in raw:
        ts, _, label = m.partition(":")
        try:
            out.append((float(ts), label))
        except ValueError:
            continue
    return out


def chart(vm: Vm, req: ChartRequest, view: RunView | None = None) -> str:
    """The SVG; without a range and a run it raises ValueError."""
    start = req.start_s if req.start_s is not None else (view.load_start_s - 60 if view else None)
    end = req.end_s if req.end_s is not None else (view.ended_s if view else None)
    if start is None or end is None:
        raise ValueError("a range needs --start and --end, or --run")
    c = Chart(req.title, start, end, unit=req.unit, markers=_markers(req.markers))
    if view is not None:
        c.regions = [(w.opened_s, w.closed_s, "window") for w in view.windows]
        c.markers += view.markers
    for i, expr in enumerate(req.exprs):
        base = req.legends[i] if i < len(req.legends) else expr[:28]
        res = vm.range(expr, start, end, req.step_s)
        for s in res:
            label = base if len(res) == 1 else f"{base}:{'/'.join(v for _, v in sorted(s.labels.items()))}"
            c.series.append((label, s.points))
    return render(c)
