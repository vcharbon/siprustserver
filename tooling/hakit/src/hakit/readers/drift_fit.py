"""The judgement of a soak's drift signals: the least-squares fit of a series of
clean samples and the verdicts on it.

A fit carries the slope over the whole series, the late slope (the second half
of it by time: a leak keeps growing, a buffer that reached its high-water mark
once does not), the span that late slope covers, and the first and last span
means (a span is an hour, or a third of the series on a shorter one).

Verdicts are `pass`, `warn`, `fail` or `na`; `na` is never a pass. The run's
verdict is the worst verdict of the checks that count: an infra check that
judged no instance, because the incidents restarted each one before it lived a
late span, is `na` and does not count; every other check counts, so a signal
that could not be judged makes the run `na`.
"""

from __future__ import annotations

import statistics
from collections.abc import Iterable, Sequence
from dataclasses import dataclass

from .vmrange import Point

RANK = {"fail": 3, "na": 2, "warn": 1, "pass": 0}


@dataclass(frozen=True)
class DriftFit:
    t0: float
    samples: int
    clean_s: int
    span_s: int
    first: float
    last: float
    slope_per_h: float
    slope_pct_per_h: float | None
    last_vs_first_pct: float | None
    late_slope_per_h: float | None
    late_slope_pct_per_h: float | None
    late_span_s: int
    intercept: float


def worst(verdicts: Iterable[str], default: str = "na") -> str:
    """The worst of `verdicts` (fail > na > warn > pass); `default` when empty."""
    return max(verdicts, key=lambda v: RANK[v], default=default)


def _slope(xs: Sequence[float], ys: Sequence[float]) -> float:
    mx, my = statistics.fmean(xs), statistics.fmean(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx if sxx else 0.0


def fit(pts: Sequence[Point]) -> DriftFit | None:
    """The least-squares fit (per hour) of [(t, v)] with its late slope and the first
    and last span means; None under 4 samples."""
    if len(pts) < 4:
        return None
    t0 = pts[0][0]
    xs = [(t - t0) / 3600 for t, _ in pts]
    ys = [v for _, v in pts]
    slope = _slope(xs, ys)
    dur = pts[-1][0] - t0
    span = min(3600, dur / 3)
    f = statistics.fmean(v for t, v in pts if t <= t0 + span)
    last = statistics.fmean(v for t, v in pts if t >= pts[-1][0] - span)
    half = [(x, y) for x, y in zip(xs, ys) if x >= xs[-1] / 2]
    late: float | None = None
    late_span = 0
    if len(half) >= 3:
        late = _slope([x for x, _ in half], [y for _, y in half])
        late_span = int((half[-1][0] - half[0][0]) * 3600)
    return DriftFit(
        t0=t0,
        samples=len(pts),
        clean_s=int(dur),
        span_s=int(span),
        first=f,
        last=last,
        slope_per_h=slope,
        slope_pct_per_h=100 * slope / f if f else None,
        last_vs_first_pct=100 * (last - f) / f if f else None,
        late_slope_per_h=late,
        late_slope_pct_per_h=100 * late / f if f and late is not None else None,
        late_span_s=late_span,
        intercept=statistics.fmean(ys) - slope * statistics.fmean(xs),
    )


def judge(f: DriftFit | None, slope_bound: float, growth_bound: float) -> tuple[str, str]:
    """Relative bounds. fail: still growing late in the run AND grown over it; warn:
    grown but flat late (a high-water mark), or a late slope over half its bound."""
    if f is None or f.late_slope_pct_per_h is None or f.slope_pct_per_h is None or f.last_vs_first_pct is None:
        return "na", "too few clean samples"
    s, ls, g = f.slope_pct_per_h, f.late_slope_pct_per_h, f.last_vs_first_pct
    d = (
        f"slope {s:+.2f} %/h, late slope {ls:+.2f} %/h (bound {slope_bound}), "
        f"last vs first {g:+.1f} % (bound {growth_bound})"
    )
    if ls > slope_bound and g > growth_bound:
        return "fail", d
    if g > growth_bound or ls > slope_bound / 2:
        return "warn", d
    return "pass", d


def judge_abs(f: DriftFit | None, slope_bound: float, growth_bound: float, unit: str = "ms") -> tuple[str, str]:
    """`judge` on absolute units: late slope in unit/h, growth in unit."""
    if f is None or f.late_slope_per_h is None:
        return "na", "too few clean samples"
    ls, g = f.late_slope_per_h, f.last - f.first
    d = (
        f"first {f.first:.1f} {unit}, last {f.last:.1f} {unit} ({g:+.1f}, bound {growth_bound}), "
        f"late slope {ls:+.1f} {unit}/h (bound {slope_bound})"
    )
    if ls > slope_bound and g > growth_bound:
        return "fail", d
    if g > growth_bound or ls > slope_bound / 2:
        return "warn", d
    return "pass", d


def judge_instance(
    f: DriftFit | None, slope_pct_bound: float, abs_per_h_bound: float, min_late_span_s: int, unit: str = ""
) -> tuple[str, str]:
    """One restartable instance's fit: `na` unless its late span reaches
    `min_late_span_s`; then a late slope within the absolute bound passes whatever
    its share of a small process; past it, a relative late slope over its bound
    fails and over half of it warns."""
    if f is None or f.late_slope_per_h is None or f.late_slope_pct_per_h is None:
        return "na", "too few clean samples"
    span = f.late_span_s
    s, per_h = f.late_slope_pct_per_h, f.late_slope_per_h
    d = f"late slope {s:+.2f} %/h, {per_h:+.2f} {unit}/h over {span // 60} min"
    if span < min_late_span_s:
        return "na", f"{d}: late span under {min_late_span_s // 60} min"
    d += f" (bound {abs_per_h_bound} {unit}/h, then {slope_pct_bound} %/h)"
    if per_h <= abs_per_h_bound:
        return "pass", d
    if s > slope_pct_bound:
        return "fail", d
    if s > slope_pct_bound / 2:
        return "warn", d
    return "pass", d


@dataclass(frozen=True)
class InstancesVerdict:
    verdict: str
    detail: str
    counts: bool
    per_instance: list[tuple[str, str, str]]


def judge_instances(
    instances: Sequence[tuple[str, DriftFit | None]],
    slope_pct_bound: float,
    abs_per_h_bound: float,
    min_late_span_s: int,
    unit: str = "",
) -> InstancesVerdict:
    """A check over a container's instances: the worst verdict of the judged ones,
    counting toward the run. When none was judged it is `na` with each instance's
    reason and does not count."""
    per = [(label, *judge_instance(f, slope_pct_bound, abs_per_h_bound, min_late_span_s, unit)) for label, f in instances]
    judged = [(p, f) for p, (_, f) in zip(per, instances) if p[1] != "na"]
    if not judged:
        reasons = "; ".join(f"{label} {d}" for label, _, d in per)
        detail = f"no instance lived a clean late span of {min_late_span_s // 60} min, not counted in the run verdict: {reasons}"
        return InstancesVerdict("na", detail, False, per)
    v = worst(p[1] for p, _ in judged)
    (label, _, d), _ = max(
        ((p, f) for p, f in judged if p[1] == v), key=lambda pf: (pf[1].late_slope_per_h or 0.0) if pf[1] else 0.0
    )
    return InstancesVerdict(v, f"worst judged instance {label}: {d}; {len(judged)} of {len(per)} instances judged", True, per)


def run_verdict(checks: Iterable[tuple[str, bool]]) -> str:
    """The worst verdict of the (verdict, counts) checks that count; `na` when none counts."""
    return worst(v for v, counts in checks if counts)
