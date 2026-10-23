"""`hakit drift <run-dir>`: drift and leak verdicts of a soak, from VictoriaMetrics
and the run's records.

Writes `<run-dir>/drift/drift.json` and one SVG chart per signal family. A
sample counts only when it is clean: after the warm-up, before the load ends,
outside every window of the verdict, and (for rates) at an offered load within
10 % of the run's median, the offered load read off the records. A signal is
judged on its fit (`drift_fit`). The checks:

- worker memory series: relative late slope and growth;
- restartable containers' RSS, per instance, judged only on a long late span;
- worker CPU per completed call: relative;
- setup latency per shape, from the records (window mean, p50, p95, p99), a
  setup measured across a generator stall left out: absolute;
- after the drain: the gauges that must read 0, and worker memory against its
  last loaded span;
- one CDR outcome per completed call, per worker, over the run.

A signal with no series is `na`; the run's verdict is the worst counted check.
`hakit drift` exits 0 on pass or warn, 1 on fail, 3 on na.
"""

from __future__ import annotations

import bisect
import json
import statistics
from collections import defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path

from .. import jsonval as jv
from ..clock import Clock, RealClock
from ..classify import setup_latency
from ..jsonval import Json
from ..stalls import stall_of
from .drift_fit import DriftFit, fit, judge, judge_abs, judge_instances, run_verdict
from .drift_spec import DriftSpec
from .run_view import RunView
from .svg import Chart, Fit, render
from .vmrange import Point, Vm

MIB = 2**20


def with_labels(selector: str, extra: str) -> str:
    """`metric{a}` + `b` -> `metric{b,a}`; `metric` + `b` -> `metric{b}`."""
    if "{" in selector:
        name, _, rest = selector.partition("{")
        return f"{name}{{{extra},{rest}" if rest.strip() != "}" else f"{name}{{{extra}}}"
    return f"{selector}{{{extra}}}"


def quantile(values: list[float], q: float) -> float:
    """Nearest-rank quantile of a non-empty list."""
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, int(-(-q * len(ordered) // 1)) - 1))]


# The span before the load a baseline gauge is read over.
BASELINE_SPAN = "2m"


@dataclass
class Check:
    name: str
    verdict: str
    detail: str
    counts: bool = True
    extra: Json = field(default_factory=lambda: {})

    def doc(self) -> Json:
        return {"name": self.name, "verdict": self.verdict, "detail": self.detail, "counts": self.counts, **self.extra}


def _fit_doc(f: DriftFit | None) -> Json | None:
    return None if f is None else asdict(f)


def _chart_fit(f: DriftFit | None) -> Fit | None:
    return None if f is None else Fit(f.t0, f.intercept, f.slope_per_h)


def latency_points(view: RunView) -> dict[str, list[tuple[float, float]]]:
    """Per shape, (start in s, setup time in ms) of every call with a setup time
    not measured across a generator stall."""
    out: dict[str, list[tuple[float, float]]] = defaultdict(list)
    for r in view.records:
        lat = setup_latency(r)
        if lat is not None and stall_of(view.stalls, lat.stream, lat.at_ms - lat.ms, lat.at_ms) is None:
            out[jv.string(r, "shape")].append((jv.number(r, "started_at_ms") / 1000, lat.ms))
    return out


class Drift:
    def __init__(self, view: RunView, vm: Vm, spec: DriftSpec, out_dir: Path, clock: Clock | None = None) -> None:
        self.view = view
        # The drained reads wait on it for the settle.
        self.clock = clock or RealClock()
        self.vm = vm
        self.spec = spec
        self.out = out_dir
        self.ns = f'namespace="{spec.namespace}"'
        self.checks: list[Check] = []
        self.charts: list[str] = []
        self.warm_s = view.load_start_s + spec.warmup_s
        self.grid0 = view.load_start_s - view.load_start_s % spec.step_s
        self.starts = sorted(jv.number(r, "started_at_ms") / 1000 for r in view.records)
        rates = [self._offered(t) for t in self._grid()]
        self.median_offered = statistics.median(rates) if rates else None

    def _grid(self) -> list[float]:
        t, out = self.grid0, list[float]()
        while t <= self.view.load_end_s:
            out.append(t)
            t += self.spec.step_s
        return out

    def _offered(self, t: float) -> float:
        w = self.spec.window_s
        return (bisect.bisect_right(self.starts, t) - bisect.bisect_right(self.starts, t - w)) / w

    def clean(self, t: float, span: float = 0) -> bool:
        lo = t - span
        return lo >= self.warm_s and t <= self.view.load_end_s and not self.view.in_window(lo, t)

    def at_load(self, t: float) -> bool:
        m = self.median_offered
        return m is None or m == 0 or abs(self._offered(t) - m) <= 0.1 * m

    def range(self, expr: str) -> list[tuple[dict[str, str], list[Point]]]:
        return [(s.labels, s.points) for s in self.vm.range(expr, self.grid0, self.view.load_end_s, self.spec.step_s)]

    def check(self, name: str, verdict: str, detail: str, counts: bool = True, **extra: object) -> None:
        self.checks.append(Check(name, verdict, detail, counts, dict(extra)))

    def chart(self, name: str, title: str, series: list[tuple[str, list[Point]]], fits: dict[str, DriftFit | None], unit: str) -> None:
        regions = [(self.view.load_start_s, self.warm_s, "warmup")] + [(w.opened_s, w.closed_s, "window") for w in self.view.windows]
        c = Chart(title, self.view.load_start_s, self.view.load_end_s, unit=unit, dots=True, series=series, regions=regions)
        c.fits = {k: f for k, v in fits.items() if (f := _chart_fit(v)) is not None}
        (self.out / name).write_text(render(c))
        self.charts.append(name)

    def memory(self) -> None:
        sp = self.spec
        for metric, title in sp.memory_metrics:
            res = self.range(f'max by (pod)({metric}{{{self.ns},pod=~"{sp.worker_pods}"}})')
            seen = {labels.get("pod", "?") for labels, _ in res}
            for pod in sp.expected_workers:
                if pod not in seen:
                    self.check(f"{title} {pod}", "na", "no series in VictoriaMetrics")
            series: list[tuple[str, list[Point]]] = []
            fits: dict[str, DriftFit | None] = {}
            for labels, pts in sorted(res, key=lambda lp: lp[0].get("pod", "")):
                pod = labels.get("pod", "?")
                cp = [(t, v / MIB) for t, v in pts if self.clean(t)]
                f = fit(cp)
                v, d = judge(f, *sp.bounds.memory)
                self.check(f"{title} {pod}", v, d, fit=_fit_doc(f), unit="MiB")
                series.append((pod, cp))
                fits[pod] = f
            self.chart(f"{metric}.svg", f"{title} (MiB, clean samples, dashed = fit)", series, fits, " MiB")

    def instances(self) -> None:
        b = self.spec.bounds
        for container in self.spec.instance_containers:
            res = self.range(f'max by (pod, id)(container_memory_rss{{{self.ns},container="{container}"}})')
            series: list[tuple[str, list[Point]]] = []
            fits: dict[str, DriftFit | None] = {}
            for labels, pts in sorted(res, key=lambda lp: lp[1][0][0]):
                start = pts[0][0]
                cp = [(t, v / MIB) for t, v in pts if self.clean(t) and t >= start + b.instance_warmup_s]
                label = f"{labels.get('pod', '?')[-5:]}@+{int((start - self.view.load_start_s) / 60)}m"
                series.append((label, cp))
                fits[label] = fit(cp)
            if not res:
                self.check(f"{container} RSS", "na", "no series in VictoriaMetrics")
            else:
                iv = judge_instances(
                    [(label, fits[label]) for label, _ in series],
                    b.instance_slope_pct_per_h,
                    b.instance_mib_per_h,
                    b.instance_late_span_s,
                    "MiB",
                )
                per = [{"instance": i, "verdict": v, "detail": d, "fit": _fit_doc(fits[i])} for i, v, d in iv.per_instance]
                self.check(f"{container} RSS", iv.verdict, iv.detail, iv.counts, unit="MiB", instances=per)
            self.chart(f"rss-{container}.svg", f"{container} RSS per instance (MiB)", series[:6], fits, " MiB")

    def cpu_per_call(self) -> None:
        sp, w = self.spec, f"{self.spec.window_s}s"
        expr = (
            f'sum(increase(container_cpu_usage_seconds_total{{{self.ns},container="{sp.worker_container}"}}[{w}]))'
            f" / sum(increase({with_labels(sp.completed_calls, self.ns)}[{w}]))"
        )
        cp = [(t, v * 1000) for _, pts in self.range(expr) for t, v in pts if self.clean(t, sp.window_s) and self.at_load(t)]
        f = fit(cp)
        v, d = judge(f, *sp.bounds.cpu_per_call)
        self.check("worker CPU per call", v, d, fit=_fit_doc(f), unit="ms")
        self.chart("cpu-per-call.svg", "worker CPU per completed call (ms)", [("cpu/call", cp)], {"cpu/call": f}, " ms")

    def latency(self) -> None:
        """Setup latency per shape from the records, one sample per grid step: the
        calls started in the window ending there."""
        sp = self.spec
        by_shape = latency_points(self.view)
        if not by_shape:
            self.check("setup latency", "na", "no record with a setup time")
        series: list[tuple[str, list[Point]]] = []
        fits: dict[str, DriftFit | None] = {}
        for shape, pts in sorted(by_shape.items()):
            pts.sort()
            starts = [t for t, _ in pts]
            stats: dict[str, list[Point]] = defaultdict(list)
            for t in self._grid():
                if not (self.clean(t, sp.window_s) and self.at_load(t)):
                    continue
                win = [ms for _, ms in pts[bisect.bisect_right(starts, t - sp.window_s) : bisect.bisect_right(starts, t)]]
                if not win:
                    continue
                stats["mean"].append((t, statistics.fmean(win)))
                for q in (0.5, 0.95, 0.99):
                    stats[f"p{q * 100:g}"].append((t, quantile(win, q)))
            for stat, sps in stats.items():
                f = fit(sps)
                v, d = judge_abs(f, *sp.bounds.latency_ms)
                self.check(f"setup latency {stat} {shape}", v, d, fit=_fit_doc(f), unit="ms")
            series.append((shape, stats.get("mean", [])))
            fits[shape] = fit(stats.get("mean", []))
        self.chart("setup-latency.svg", "setup latency per shape, window means (ms)", series[:6], fits, " ms")

    def drained_baseline(self) -> None:
        """Each baseline gauge after the drain against its value just before the load."""
        sp = self.spec
        if not sp.drained_baseline:
            return
        before_t, after_t = self.view.load_start_s - 1, self.view.ended_s + sp.drained_settle_s
        rows: list[str] = []
        verdict = "pass"
        for m in sp.drained_baseline:
            # The median of the two minutes before the load: one scrape the
            # run's own start disturbed is no baseline.
            before = self.vm.at(f"quantile_over_time(0.5, sum({m}{{{self.ns}}})[{BASELINE_SPAN}:15s])", before_t)
            after = self.vm.at(f"sum({m}{{{self.ns}}})", after_t)
            if not before or not after:
                verdict = "na" if verdict == "pass" else verdict
                rows.append(f"{m} unread")
                continue
            b, a = before[0][1], after[0][1]
            ok = a <= b + sp.drained_baseline_tolerance
            verdict = verdict if ok else "fail"
            rows.append(f"{m} {a:g} {sp.drained_settle_s:g} s after the run vs {b:g} before the load (tolerance {sp.drained_baseline_tolerance:g})")
        self.check("gauges back to their baseline", verdict, "; ".join(rows))

    def quiet_zero(self) -> None:
        """Each `quiet_zero_increase` counter grows by nothing between the
        padded windows, from the load's start to the drained read."""
        sp = self.spec
        spans: list[tuple[float, float]] = []
        t = self.view.load_start_s + sp.quiet_pad_s
        for w in sorted(self.view.windows, key=lambda w: w.opened_s):
            if w.opened_s > t:
                spans.append((t, w.opened_s))
            t = max(t, w.closed_s + sp.quiet_pad_s)
        end = self.view.ended_s + sp.drained_settle_s
        if end > t:
            spans.append((t, end))
        for m in sp.quiet_zero_increase:
            grew: list[str] = []
            unread = 0
            for a, b in spans:
                r = self.vm.at(f"sum(increase({m}{{{self.ns}}}[{int(b - a)}s]))", b)
                if not r:
                    unread += 1
                elif round(r[0][1]) > 0:
                    grew.append(f"{round(r[0][1])} in {a:.0f}..{b:.0f}")
            name = f"{m} quiet outside windows"
            if grew:
                self.check(name, "fail", "; ".join(grew))
            elif unread:
                self.check(name, "na", f"{unread} of {len(spans)} spans unread")
            else:
                self.check(name, "pass", f"0 over {len(spans)} spans")

    def drained(self) -> None:
        """After the drain: the zero gauges, read once the last teardowns'
        timers ran (`drained_settle_s`), then worker memory against its last
        loaded span."""
        sp, t = self.spec, self.view.ended_s + self.spec.drained_settle_s
        # A run is read after it ended, so the wait is at most the settle; a
        # read instant further ahead is no run of this clock's.
        wait = t - self.clock.now_ms() / 1000
        if 0 < wait <= sp.drained_settle_s:
            self.clock.sleep(wait)
        gauges = {m: self.vm.at(f"sum({m}{{{self.ns}}})", t) for m in sp.drained_zero + sp.drained_report}
        values = {m: (r[0][1] if r else None) for m, r in gauges.items()}
        unread = [m for m in sp.drained_zero if values[m] is None]
        held = {m: v for m in sp.drained_zero if (v := values[m])}
        if unread:
            self.check("gauges after drain", "na", f"unread: {', '.join(unread)}", gauges=values)
        elif held:
            notes = sp.drained_zero_notes
            rows = ", ".join(f"{m}={v:g}" + (f" ({notes[m]})" if m in notes else "") for m, v in held.items())
            self.check("gauges after drain", "fail", "held: " + rows, gauges=values)
        else:
            report = ", ".join(f"{m}={values[m]}" for m in sp.drained_report)
            self.check("gauges after drain", "pass", f"{', '.join(sp.drained_zero)} at 0; {report}", gauges=values)
        self.drained_baseline()
        self.quiet_zero()
        metric, title = sp.memory_metrics[0]
        for c in [c for c in self.checks if c.name.startswith(f"{title} ") and isinstance(c.extra.get("fit"), dict)]:
            pod = c.name.split()[-1]
            r = self.vm.at(f'max({metric}{{{self.ns},pod="{pod}"}})', t)
            fit_doc = jv.as_obj(c.extra["fit"])
            if not r:
                continue
            after, late = r[0][1] / MIB, jv.number(fit_doc, "last")
            g = 100 * (after - late) / late
            bound = sp.bounds.drain_memory_pct
            self.check(
                f"{title} after drain {pod}",
                "fail" if g > bound else "pass",
                f"{after:.1f} MiB after the drain vs {late:.1f} MiB last loaded span ({g:+.1f} %, bound {bound})",
            )

    def cdr(self) -> None:
        sp, t = self.spec, self.view.ended_s
        span = f"{int(t - self.view.load_start_s) + 60}s"
        wsel = f'{self.ns},pod=~"{sp.worker_pods}"'
        sums = [
            self.vm.by(f"sum by (pod)(increase({with_labels(m, wsel)}[{span}]))", t, "pod")
            for m in (sp.completed_calls, sp.cdr_written, sp.cdr_dropped)
        ]
        pods = sorted({pod for s in sums for pod in s})
        if not pods:
            self.check("CDR per call", "na", "no worker series in VictoriaMetrics")
        bound = sp.bounds.cdr_unaccounted
        for pod in pods:
            removed, written, dropped = (s.get(pod) for s in sums)
            if removed is None or written is None or dropped is None:
                self.check(f"CDR per call {pod}", "na", "a counter is missing")
                continue
            gap = round(removed - written - dropped)
            self.check(
                f"CDR per call {pod}",
                "fail" if abs(gap) > bound else "pass",
                f"{gap:+d} completed calls with no CDR outcome: {removed:.0f} completed - {written:.0f} written"
                f" - {dropped:.0f} dropped (bound {bound})",
            )

    def run(self) -> Json:
        self.out.mkdir(parents=True, exist_ok=True)
        self.memory()
        self.instances()
        self.cpu_per_call()
        self.latency()
        self.drained()
        self.cdr()
        doc: Json = {
            "verdict": run_verdict((c.verdict, c.counts) for c in self.checks),
            "run": self.view.run,
            "spec": json.loads(json.dumps(asdict(self.spec))),
            "not_counted": [c.name for c in self.checks if not c.counts],
            "load_start_s": self.view.load_start_s,
            "warm_s": self.warm_s,
            "load_end_s": self.view.load_end_s,
            "offered_cps_median": self.median_offered,
            "excluded_windows": [[w.opened_s, w.closed_s] for w in self.view.windows],
            "checks": [c.doc() for c in self.checks],
            "charts": self.charts,
            "query_errors": self.vm.errors[:20],
        }
        (self.out / "drift.json").write_text(json.dumps(doc, indent=1) + "\n")
        return doc
