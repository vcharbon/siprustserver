"""What `hakit drift` judges, with the base's defaults: the system under test the
base charts deploy. A product passes `--spec FILE` (YAML, every key optional)
for its own pods, containers and bounds.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from pathlib import Path

import yaml

from .. import jsonval as jv
from ..jsonval import as_obj


@dataclass(frozen=True)
class DriftBounds:
    # (late slope %/h, growth % of the first span) of the worker memory series
    memory: tuple[float, float] = (5.0, 10.0)
    cpu_per_call: tuple[float, float] = (5.0, 10.0)
    # (late slope ms/h, growth ms) of the setup latency
    latency_ms: tuple[float, float] = (10.0, 20.0)
    instance_slope_pct_per_h: float = 10.0
    instance_mib_per_h: float = 2.0
    instance_warmup_s: int = 900
    instance_late_span_s: int = 2700
    drain_memory_pct: float = 10.0
    cdr_unaccounted: int = 10


@dataclass(frozen=True)
class DriftSpec:
    namespace: str = "sip-test"
    # The workers: their pod regex, their container, the pods expected.
    worker_pods: str = "b2bua-worker-.*"
    worker_container: str = "b2bua-worker"
    expected_workers: list[str] = field(default_factory=lambda: ["b2bua-worker-0", "b2bua-worker-1"])
    # Worker memory series, each with its title.
    memory_metrics: list[tuple[str, str]] = field(
        default_factory=lambda: [
            ("process_resident_memory_bytes", "worker RSS"),
            ("jemalloc_allocated_bytes", "worker jemalloc allocated"),
        ]
    )
    # Containers restarted by incidents, judged per instance.
    instance_containers: list[str] = field(default_factory=lambda: ["call-limiter", "rabbitmq", "cdr-consumer"])
    # A completed call: the denominator of the worker's CPU per call.
    completed_calls: str = 'b2bua_call_removals_by_class_total{class="terminated"}'
    # Gauges that must read 0 after the drain, then gauges only reported.
    drained_zero: list[str] = field(
        default_factory=lambda: ["b2bua_active_calls", "b2bua_census_dialogs", "b2bua_txn_active_transactions"]
    )
    drained_report: list[str] = field(default_factory=lambda: ["b2bua_census_timers", "b2bua_timer_live"])
    # What a drained-zero gauge still held means, said with its count.
    drained_zero_notes: dict[str, str] = field(default_factory=lambda: {})
    # Gauges that traffic outside the load keeps above 0 (a front proxy's
    # health probes hold transactions): after the drain each returns to its
    # median over the two minutes before the load, within
    # `drained_baseline_tolerance`. Both kinds are read `drained_settle_s`
    # after the run ended: the last teardowns' non-INVITE transactions live
    # Timer J (32 s over UDP, RFC 3261 §17.2.2), and a gauge is scraped every
    # 15 s.
    drained_baseline: list[str] = field(default_factory=lambda: [])
    drained_baseline_tolerance: float = 0
    drained_settle_s: float = 47
    # One CDR outcome (written or dropped) per completed call, per worker.
    cdr_written: str = "b2bua_cdr_written_total"
    cdr_dropped: str = "b2bua_cdr_dropped_total"
    warmup_s: int = 1800
    window_s: int = 300
    step_s: int = 60
    bounds: DriftBounds = DriftBounds()


def _pair(v: object, what: str) -> tuple[float, float]:
    items = jv.as_list(v, what)
    if len(items) != 2 or not all(isinstance(x, (int, float)) for x in items):
        raise ValueError(f"{what}: expected [slope, growth]")
    return float(str(items[0])), float(str(items[1]))


def load_spec(path: Path | None) -> DriftSpec:
    """The base spec, overridden by the keys of the YAML file at `path`."""
    spec = DriftSpec()
    if path is None:
        return spec
    d = as_obj(yaml.safe_load(path.read_text()) or {}, str(path))
    b = jv.obj(d, "bounds")
    base = spec.bounds
    bounds = DriftBounds(
        memory=_pair(b["memory"], "bounds.memory") if "memory" in b else base.memory,
        cpu_per_call=_pair(b["cpu_per_call"], "bounds.cpu_per_call") if "cpu_per_call" in b else base.cpu_per_call,
        latency_ms=_pair(b["latency_ms"], "bounds.latency_ms") if "latency_ms" in b else base.latency_ms,
        instance_slope_pct_per_h=jv.number(b, "instance_slope_pct_per_h", base.instance_slope_pct_per_h),
        instance_mib_per_h=jv.number(b, "instance_mib_per_h", base.instance_mib_per_h),
        instance_warmup_s=int(jv.number(b, "instance_warmup_s", base.instance_warmup_s)),
        instance_late_span_s=int(jv.number(b, "instance_late_span_s", base.instance_late_span_s)),
        drain_memory_pct=jv.number(b, "drain_memory_pct", base.drain_memory_pct),
        cdr_unaccounted=int(jv.number(b, "cdr_unaccounted", base.cdr_unaccounted)),
    )
    memory = spec.memory_metrics
    if "memory_metrics" in d:
        memory = [(jv.string(m, "metric"), jv.string(m, "title")) for m in jv.objs(d, "memory_metrics")]
    return replace(
        spec,
        namespace=jv.string(d, "namespace", spec.namespace),
        worker_pods=jv.string(d, "worker_pods", spec.worker_pods),
        worker_container=jv.string(d, "worker_container", spec.worker_container),
        expected_workers=jv.strings(d, "expected_workers") if "expected_workers" in d else spec.expected_workers,
        memory_metrics=memory,
        instance_containers=jv.strings(d, "instance_containers") if "instance_containers" in d else spec.instance_containers,
        completed_calls=jv.string(d, "completed_calls", spec.completed_calls),
        drained_zero=jv.strings(d, "drained_zero") if "drained_zero" in d else spec.drained_zero,
        drained_report=jv.strings(d, "drained_report") if "drained_report" in d else spec.drained_report,
        drained_zero_notes={k: str(v) for k, v in jv.obj(d, "drained_zero_notes").items()}
        or spec.drained_zero_notes,
        drained_baseline=jv.strings(d, "drained_baseline") if "drained_baseline" in d else spec.drained_baseline,
        drained_baseline_tolerance=jv.number(d, "drained_baseline_tolerance", spec.drained_baseline_tolerance),
        drained_settle_s=jv.number(d, "drained_settle_s", spec.drained_settle_s),
        cdr_written=jv.string(d, "cdr_written", spec.cdr_written),
        cdr_dropped=jv.string(d, "cdr_dropped", spec.cdr_dropped),
        warmup_s=int(jv.number(d, "warmup_s", spec.warmup_s)),
        window_s=int(jv.number(d, "window_s", spec.window_s)),
        step_s=int(jv.number(d, "step_s", spec.step_s)),
        bounds=bounds,
    )
