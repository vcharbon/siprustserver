"""`hakit ramp-table <run-dir>`: the per-step table of a capacity ramp, a run
whose profile has rate `steps`, with the ramp's limits and the worker CPU fit
(`ramp.py` judges).

A step runs from one rate change of the stepped streams to the next (the last
to the end of the load); it is measured over its span after `settle_s`. The
callers' side comes from the run's records, the calls started in the span:
each stepped stream's calls completed (a success by the classifier), refused
503, or failed otherwise, the other streams' alike as the background, and the
stepped streams' setup p99 (nearest rank, as `setup_slow` measures a setup).
The worker's side comes from VictoriaMetrics over the same span: its new
calls accepted, cancelled and rejected, its CPU, its throttling and its CPU
limit, summed over the worker pods the spec names. Writes `ramp-steps.json`
(the rows) and `ramp.json` (limits and fit) into the run directory and
prints a markdown table and the limits.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass, replace
from pathlib import Path

import yaml

from .. import jsonval as jv
from ..classify import classify
from ..jsonl import read_jsonl
from ..jsonval import Json, as_obj
from ..model import Shape
from ..profile import Profile
from .probe_live import read_files
from .ramp import cpu_fit, limits
from .vmrange import Vm


@dataclass(frozen=True)
class RampSpec:
    namespace: str = "sip-test"
    worker_pods: str = "b2bua-worker-.*"
    worker_container: str = "b2bua-worker"
    settle_s: float = 60
    p99_ms: float = 200


def load_spec(path: Path | None) -> RampSpec:
    spec = RampSpec()
    if path is None:
        return spec
    d = as_obj(yaml.safe_load(path.read_text()) or {}, str(path))
    return replace(
        spec,
        namespace=jv.string(d, "namespace", spec.namespace),
        worker_pods=jv.string(d, "worker_pods", spec.worker_pods),
        worker_container=jv.string(d, "worker_container", spec.worker_container),
        settle_s=jv.number(d, "settle_s", spec.settle_s),
        p99_ms=jv.number(d, "p99_ms", spec.p99_ms),
    )


@dataclass(frozen=True)
class Span:
    label: str
    start_ms: int
    end_ms: int

    @property
    def seconds(self) -> float:
        return max(0.001, (self.end_ms - self.start_ms) / 1000)


def spans(profile: Profile, load_start_ms: int, settle_s: float) -> list[Span]:
    """The steps of the stepped streams, each measured after `settle_s`."""
    stepped = [s for s in profile.streams if s.steps]
    cuts = sorted({0.0, *(at for s in stepped for at, _ in s.steps if at < profile.duration_s)})
    out: list[Span] = []
    for i, at in enumerate(cuts):
        end = cuts[i + 1] if i + 1 < len(cuts) else profile.duration_s
        label = ", ".join(f"{s.name} {s.cps_at(at):g}" for s in stepped)
        start = load_start_ms + int((at + settle_s) * 1000)
        stop = load_start_ms + int(end * 1000)
        if stop > start:
            out.append(Span(f"+{at:g}s {label}", start, stop))
    return out


def _p99(values: list[float]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(0.99 * len(ordered)) - 1)]


def caller_side(records: list[Json], shapes: dict[str, Shape], stepped: set[str], span: Span) -> Json:
    counts = {True: {"completed": 0, "shed_503": 0, "other_fail": 0}, False: {"completed": 0, "shed_503": 0, "other_fail": 0}}
    setups: list[float] = []
    for r in records:
        started = jv.number(r, "started_at_ms")
        shape = shapes.get(str(r.get("shape")))
        if not span.start_ms <= started < span.end_ms or shape is None:
            continue
        mine = str(r.get("stream")) in stepped
        c = classify(r, shape)
        refused = any(i.code == 503 and i.budgetable for i in c.impacts)
        kind = "completed" if c.success else "shed_503" if refused else "other_fail"
        counts[mine][kind] += 1
        if mine and c.latency is not None:
            setups.append(c.latency.ms)
    n, bg = counts[True], counts[False]
    return {
        "stepped": {"cps": {k: round(v / span.seconds, 3) for k, v in n.items()}, "n": dict(n)},
        "background": {
            "completed_cps": round(bg["completed"] / span.seconds, 3),
            "failed": bg["shed_503"] + bg["other_fail"],
            "shed_503": bg["shed_503"],
            "shed_503_cps": round(bg["shed_503"] / span.seconds, 3),
        },
        "setup_p99_ms": _p99(setups),
    }


def worker_side(vm: Vm, spec: RampSpec, span: Span) -> Json:
    sel = f'namespace="{spec.namespace}",pod=~"{spec.worker_pods}"'
    length = max(1, int(span.seconds))
    at = span.end_ms / 1000

    def one(expr: str) -> float | None:
        got = vm.at(expr, at)
        return sum(v for _, v in got) if got else None

    def calls(outcome: str) -> float | None:
        v = one(f'sum(increase(b2bua_new_calls_total{{{sel},outcome="{outcome}"}}[{length}s]))')
        return None if v is None else round(v / length, 3)

    cont = f'{sel},container="{spec.worker_container}"'
    ksm = f'exported_namespace="{spec.namespace}",exported_pod=~"{spec.worker_pods}",container="{spec.worker_container}"'
    return {
        "worker_accepted_cps": calls("accepted"),
        "worker_cancelled_cps": calls("cancelled") or 0.0,
        "worker_rejected_cps": calls("rejected") or 0.0,
        "worker_cpu_cores": one(f"sum(rate(container_cpu_usage_seconds_total{{{cont}}}[{length}s]))"),
        "worker_throttled_s": one(f"sum(increase(container_cpu_cfs_throttled_seconds_total{{{cont}}}[{length}s]))"),
        "worker_cpu_limit_cores": one(f'sum(kube_pod_container_resource_limits{{{ksm},resource="cpu"}})'),
    }


def build(run_dir: Path, spec: RampSpec, vm: Vm) -> tuple[list[Json], Json]:
    files = read_files(run_dir)
    load_start = int(jv.opt_number(files.meta, "load_started_at_ms") or jv.number(files.meta, "started_at_ms"))
    records = read_jsonl(run_dir / "records.jsonl")
    stepped = {s.name for s in files.profile.streams if s.steps}
    rows: list[Json] = []
    for span in spans(files.profile, load_start, spec.settle_s):
        row: Json = {"label": span.label, "start_ms": span.start_ms, "end_ms": span.end_ms}
        row |= caller_side(records, files.shapes, stepped, span)
        row |= worker_side(vm, spec, span)
        rows.append(row)
    lim = limits(rows, spec.p99_ms)
    clean = {label: as_obj(v).get("clean") for label, v in jv.obj(lim, "steps").items()}
    doc: Json = {"limits": lim, "cpu_fit": cpu_fit(rows, {k: v if isinstance(v, bool) else None for k, v in clean.items()})}
    (run_dir / "ramp-steps.json").write_text(json.dumps(rows, indent=1) + "\n")
    (run_dir / "ramp.json").write_text(json.dumps(doc, indent=1) + "\n")
    return rows, doc


def table(rows: list[Json], doc: Json) -> str:
    out = [
        "| step | completed cps | 503 cps | failed | p99 ms | worker accepted | rejected | cores | clean |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    steps = jv.obj(jv.obj(doc, "limits"), "steps")
    for r in rows:
        st = jv.obj(r, "stepped")
        cps, n = jv.obj(st, "cps"), jv.obj(st, "n")
        verdict = as_obj(steps.get(str(r["label"]), {}))
        ok = verdict.get("clean")
        clean = "yes" if ok is True else f"{'no' if ok is False else 'na'}: {verdict.get('why')}"
        out.append(
            f"| {r['label']} | {cps['completed']} | {cps['shed_503']} | {n['other_fail']} | {r.get('setup_p99_ms')} "
            f"| {r.get('worker_accepted_cps')} | {r.get('worker_rejected_cps')} | {r.get('worker_cpu_cores')} | {clean} |"
        )
    lim = jv.obj(doc, "limits")
    out.append("")
    out.append(f"accepted-call capacity: {lim.get('accepted_call_capacity')}")
    out.append(f"reject throughput: {lim.get('reject_throughput')}")
    return "\n".join(out)
