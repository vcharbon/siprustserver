"""One standalone inline SVG chart of time series: lines or sample dots, optional
fitted lines, shaded regions (incident windows, a warm-up) and event markers.
No dependency; the chart inherits `currentColor`, so it reads in both themes.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field

from .vmrange import Point

W, H = 900, 260
PAD_L, PAD_R, PAD_T, PAD_B = 70, 14, 30, 34
COLORS = ["#4c9be8", "#e0653f", "#57a773", "#c77dff", "#e8c547", "#5ad2d2"]
REGION_FILL = {"window": ("#e0653f", ".12"), "warmup": ("currentColor", ".06")}


@dataclass(frozen=True)
class Fit:
    """A straight line through a series: value at `t0_s`, slope per hour."""

    t0_s: float
    intercept: float
    slope_per_h: float

    def at(self, t_s: float) -> float:
        return self.intercept + self.slope_per_h * (t_s - self.t0_s) / 3600


@dataclass
class Chart:
    title: str
    start_s: float
    end_s: float
    unit: str = ""
    dots: bool = False
    series: list[tuple[str, list[Point]]] = field(default_factory=lambda: [])
    fits: dict[str, Fit] = field(default_factory=lambda: {})
    regions: list[tuple[float, float, str]] = field(default_factory=lambda: [])
    markers: list[tuple[float, str]] = field(default_factory=lambda: [])


def esc(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def _range(values: Sequence[float]) -> tuple[float, float]:
    lo, hi = min(min(values), 0.0), max(values)
    if hi == lo:
        hi = lo + 1
    return lo, hi + (hi - lo) * 0.08


def render(c: Chart) -> str:
    o = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="100%" role="img" '
        f'aria-label="{esc(c.title)}" style="font:12px system-ui,sans-serif">',
        f'<text x="{PAD_L}" y="18" fill="currentColor" font-weight="600">{esc(c.title)}</text>',
    ]
    values = [v for _, pts in c.series for _, v in pts]
    if not values:
        o.append(f'<text x="{W / 2}" y="{H / 2}" text-anchor="middle" fill="#888">no data for this range</text></svg>')
        return "\n".join(o)
    t0, t1 = c.start_s, max(c.end_s, c.start_s + 1)
    ymin, ymax = _range(values)

    def px(t: float) -> float:
        return min(max(PAD_L + (t - t0) / (t1 - t0) * (W - PAD_L - PAD_R), PAD_L), W - PAD_R)

    def py(v: float) -> float:
        return H - PAD_B - (v - ymin) / (ymax - ymin) * (H - PAD_T - PAD_B)

    for k in range(5):
        v = ymin + (ymax - ymin) * k / 4
        o.append(f'<line x1="{PAD_L}" y1="{py(v):.1f}" x2="{W - PAD_R}" y2="{py(v):.1f}" stroke="currentColor" stroke-opacity=".12"/>')
        o.append(
            f'<text x="{PAD_L - 6}" y="{py(v) + 4:.1f}" text-anchor="end" fill="currentColor" fill-opacity=".65">'
            f"{v:.4g}{esc(c.unit)}</text>"
        )
    for a, b, kind in c.regions:
        fill, opacity = REGION_FILL.get(kind, REGION_FILL["window"])
        o.append(
            f'<rect x="{px(a):.1f}" y="{PAD_T}" width="{max(px(b) - px(a), 1):.1f}" height="{H - PAD_T - PAD_B}" '
            f'fill="{fill}" fill-opacity="{opacity}"/>'
        )
    for t, label in c.markers:
        if not t0 <= t <= t1:
            continue
        x = px(t)
        o.append(f'<line x1="{x:.1f}" y1="{PAD_T}" x2="{x:.1f}" y2="{H - PAD_B}" stroke="#e0653f" stroke-opacity=".55" stroke-dasharray="3 3"/>')
        o.append(f'<text x="{x + 3:.1f}" y="{PAD_T + 10}" fill="#e0653f" font-size="10">{esc(label)}</text>')
    for k in range(5):
        t = t0 + (t1 - t0) * k / 4
        o.append(
            f'<text x="{px(t):.1f}" y="{H - 12}" text-anchor="middle" fill="currentColor" fill-opacity=".65">+{int((t - t0) / 60)}m</text>'
        )
    for i, (label, pts) in enumerate(c.series):
        color = COLORS[i % len(COLORS)]
        if c.dots:
            o.extend(f'<circle cx="{px(t):.1f}" cy="{py(v):.1f}" r="1.6" fill="{color}"/>' for t, v in pts)
        else:
            d = " ".join(("M" if k == 0 else "L") + f"{px(t):.1f},{py(v):.1f}" for k, (t, v) in enumerate(pts))
            o.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="1.6"/>')
        fit = c.fits.get(label)
        if fit is not None and pts:
            ta, tb = pts[0][0], pts[-1][0]
            o.append(
                f'<line x1="{px(ta):.1f}" y1="{py(fit.at(ta)):.1f}" x2="{px(tb):.1f}" y2="{py(fit.at(tb)):.1f}" '
                f'stroke="{color}" stroke-width="1.5" stroke-dasharray="5 3"/>'
            )
        o.append(f'<rect x="{PAD_L + i * 150}" y="{H - PAD_B + 14}" width="9" height="9" fill="{color}"/>')
        o.append(f'<text x="{PAD_L + i * 150 + 13}" y="{H - PAD_B + 22}" fill="currentColor">{esc(label[:22])}</text>')
    o.append("</svg>")
    return "\n".join(o)
