"""The host under the run: pressure and swap, sampled for the whole run
(ADR-0049 §6.5, §6.6).

A sample reads `/proc/pressure/{cpu,memory,io}` (the `some` and `full` lines:
avg10 and the stall total in µs) and `/proc/vmstat` (`pswpin`, `pswpout`)
through the executor, while the runner waits between its steps: a step that
blocks (a heal, an export) delays the next sample. Two consecutive samples make
an interval whose share of time stalled comes from the totals, so a delayed
sample's interval still counts every stall in it, averaged over its longer
span: a short burst inside a long gap reads lower than it peaked. An interval is under stress when its
`io` or `memory` full share or its swap-in rate reaches a threshold of the
profile's `preflight` (stated defaults below). The samples are kept in
`host.jsonl` and pushed as `hakit_host_*` series.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .clock import Clock
from .executor import Executor
from . import jsonval as jv
from .jsonl import JsonlLog
from .jsonval import Json
from .vm import VictoriaMetrics, sample

RESOURCES = ("cpu", "memory", "io")
KINDS = ("some", "full")
SAMPLE_MS = 1000
HOST_FILE = "host.jsonl"

# Stated, not fitted. Swap-in is the trigger: a generator's own pages may be on
# disk, and its one thread waits on each fault. Memory full pressure is reclaim
# run by every allocating task, the generator's included. A tmpfs-backed
# generator does not wait on the host's disk, so io full pressure counts only
# as a sustained stall risk, a fifth of the time with every task stalled.
DEFAULT_IO_FULL_PCT = 20.0
DEFAULT_MEMORY_FULL_PCT = 5.0
DEFAULT_SWAPIN_PAGES_S = 50.0


@dataclass(frozen=True)
class Thresholds:
    io_full_pct: float = DEFAULT_IO_FULL_PCT
    memory_full_pct: float = DEFAULT_MEMORY_FULL_PCT
    swapin_pages_s: float = DEFAULT_SWAPIN_PAGES_S


def _psi(text: str) -> Json:
    out: Json = {}
    for line in text.splitlines():
        parts = line.split()
        if not parts or parts[0] not in KINDS:
            continue
        fields = dict(p.split("=", 1) for p in parts[1:] if "=" in p)
        out[parts[0]] = {"avg10": float(fields.get("avg10", 0)), "total": int(fields.get("total", 0))}
    return out


def read_sample(executor: Executor, at_ms: int) -> Json | None:
    """One sample, or None when the host exposes no pressure (no PSI)."""
    doc: Json = {"at_ms": at_ms}
    for r in RESOURCES:
        text = executor.read_text(Path(f"/proc/pressure/{r}"))
        if text is None:
            return None
        doc[r] = _psi(text)
    vmstat = executor.read_text(Path("/proc/vmstat")) or ""
    for line in vmstat.splitlines():
        name, _, value = line.partition(" ")
        if name in ("pswpin", "pswpout") and value.strip().isdigit():
            doc[name] = int(value)
    return doc


def _psi_field(s: Json, resource: str, kind: str, name: str) -> float | None:
    r = jv.opt_obj(s, resource)
    k = None if r is None else jv.opt_obj(r, kind)
    return None if k is None else jv.opt_number(k, name)


def _total(s: Json, resource: str, kind: str) -> int:
    return int(_psi_field(s, resource, kind, "total") or 0)


def _count(s: Json, name: str) -> int:
    v = s.get(name)
    return int(v) if isinstance(v, (int, float)) else 0


@dataclass(frozen=True)
class Interval:
    start_ms: float
    end_ms: float
    # Share of the interval stalled, in %, by (resource, kind).
    pct: dict[tuple[str, str], float]
    swapin_pages_s: float
    swapout_pages_s: float

    def stressed(self, t: Thresholds) -> bool:
        return (
            self.pct[("io", "full")] >= t.io_full_pct
            or self.pct[("memory", "full")] >= t.memory_full_pct
            or self.swapin_pages_s >= t.swapin_pages_s
        )


def intervals(samples: list[Json]) -> list[Interval]:
    out: list[Interval] = []
    for a, b in zip(samples, samples[1:]):
        t0, t1 = jv.number(a, "at_ms"), jv.number(b, "at_ms")
        span_us = (t1 - t0) * 1000
        if span_us <= 0:
            continue
        pct = {
            (r, k): max(0.0, (_total(b, r, k) - _total(a, r, k)) / span_us * 100) for r in RESOURCES for k in KINDS
        }
        s = (t1 - t0) / 1000
        out.append(
            Interval(
                t0, t1, pct,
                max(0, _count(b, "pswpin") - _count(a, "pswpin")) / s,
                max(0, _count(b, "pswpout") - _count(a, "pswpout")) / s,
            )
        )
    return out


def series(run: str, iv: Interval) -> list[str]:
    at = int(iv.end_ms)
    out = [
        sample("hakit_host_pressure_ratio", {"run": run, "resource": r, "kind": k}, round(iv.pct[(r, k)] / 100, 6), at)
        for r in RESOURCES
        for k in KINDS
    ]
    out.append(sample("hakit_host_swap_pages_per_second", {"run": run, "direction": "in"}, round(iv.swapin_pages_s, 3), at))
    out.append(sample("hakit_host_swap_pages_per_second", {"run": run, "direction": "out"}, round(iv.swapout_pages_s, 3), at))
    return out


def preflight_errors(executor: Executor, clock: Clock, t: Thresholds) -> list[str]:
    """Refusals while the host is under stress now: the avg10 of io and memory
    full, and the swap-in rate over one sample interval. None on a host
    without PSI."""
    first = read_sample(executor, clock.now_ms())
    if first is None:
        return []
    clock.sleep(SAMPLE_MS / 1000)
    second = read_sample(executor, clock.now_ms())
    errs: list[str] = []
    for resource, limit in (("io", t.io_full_pct), ("memory", t.memory_full_pct)):
        avg = _psi_field(first, resource, "full", "avg10")
        if avg is not None and avg >= limit:
            errs.append(f"{resource} full pressure avg10 {avg:g} % is at or above {limit:g} %")
    if second is not None:
        (iv,) = intervals([first, second]) or (None,)
        if iv is not None and iv.swapin_pages_s >= t.swapin_pages_s:
            errs.append(f"swap-in {iv.swapin_pages_s:.0f} pages/s is at or above {t.swapin_pages_s:g}")
    return errs


class HostSampler:
    """Samples the host every `SAMPLE_MS` while the runner waits; appends each
    sample to `host.jsonl` and pushes the interval it closes. A host without
    PSI is sampled once and then left alone."""

    def __init__(self, run: str, run_dir: Path, executor: Executor, clock: Clock, vm: VictoriaMetrics) -> None:
        self.run = run
        self.executor = executor
        self.clock = clock
        self.vm = vm
        self.log = JsonlLog(run_dir / HOST_FILE)
        self.last: Json | None = None
        self.available = True
        self.due_ms = clock.now_ms()

    def tick(self) -> None:
        now = self.clock.now_ms()
        if not self.available or now < self.due_ms:
            return
        s = read_sample(self.executor, now)
        if s is None:
            self.available = False
            return
        self.log.add([s])
        if self.last is not None:
            for iv in intervals([self.last, s]):
                self.vm.push(series(self.run, iv))
        self.last = s
        self.due_ms = now + SAMPLE_MS

    def next_ms(self) -> int | None:
        return self.due_ms if self.available else None
