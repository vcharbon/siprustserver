"""`host_under_stress`: a measurement fault that coincides with host pressure
(ADR-0049 §3.12, §6.5).

The stressed intervals of `host.jsonl` (host.py) are merged into spans. A
generator stall, a spoiled measurement or a finding with an instant that
overlaps a span, and a record gap of a stream with such a stall, make one
`host_under_stress` finding for the span: its instants, the worst io and
memory full share and swap-in rate in it, and what it overlapped. Consecutive
spans over the same generator stalls make one finding: from the first span's
start to the last one's end, their count, and the worst values among them. It stands
beside the plain `generator_stall`, so a stall on a stressed host is never
investigated as a fault of the system under test or of the tooling.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

from . import jsonval as jv
from .host import HOST_FILE, Interval, Thresholds, intervals
from .jsonl import read_jsonl
from .jsonval import Json
from .profile import Preflight
from .stalls import Stall

# Findings an instant places on the timeline.
TIMED = ("generator_failure", "stream_died", "series_stale", "generator_stall")


def thresholds(p: Preflight) -> Thresholds:
    d = Thresholds()
    return Thresholds(
        p.max_io_full_pct if p.max_io_full_pct is not None else d.io_full_pct,
        p.max_memory_full_pct if p.max_memory_full_pct is not None else d.memory_full_pct,
        p.max_swapin_pages_s if p.max_swapin_pages_s is not None else d.swapin_pages_s,
    )


@dataclass
class Span:
    start_ms: float
    end_ms: float
    io_full_pct: float
    memory_full_pct: float
    swapin_pages_s: float

    def overlaps(self, a: float, b: float) -> bool:
        return a < self.end_ms and b > self.start_ms


def spans(ivs: Sequence[Interval], t: Thresholds) -> list[Span]:
    out: list[Span] = []
    for iv in ivs:
        if not iv.stressed(t):
            continue
        io, mem = iv.pct[("io", "full")], iv.pct[("memory", "full")]
        last = out[-1] if out else None
        if last is not None and iv.start_ms <= last.end_ms:
            last.end_ms = iv.end_ms
            last.io_full_pct = max(last.io_full_pct, io)
            last.memory_full_pct = max(last.memory_full_pct, mem)
            last.swapin_pages_s = max(last.swapin_pages_s, iv.swapin_pages_s)
        else:
            out.append(Span(iv.start_ms, iv.end_ms, io, mem, iv.swapin_pages_s))
    return out


@dataclass
class _Group:
    """Consecutive stressed spans and what they overlapped."""

    spans: list[Span]
    stalls: list[Stall]
    timed: list[Json]
    gaps: list[Json]

    def take(self, sp: Span, timed: list[Json]) -> None:
        self.spans.append(sp)
        self.timed += [f for f in timed if not any(f is g for g in self.timed)]


def _finding(g: _Group, spoiled: Mapping[Stall, int], load_start_ms: float) -> Json:
    first, last = g.spans[0], g.spans[-1]
    io = max(sp.io_full_pct for sp in g.spans)
    mem = max(sp.memory_full_pct for sp in g.spans)
    swapin = max(sp.swapin_pages_s for sp in g.spans)
    what: list[str] = []
    if g.stalls:
        what.append(f"{len(g.stalls)} generator stalls ({sum(spoiled.get(st, 0) for st in g.stalls)} measurements spoiled)")
    if g.timed:
        what.append(f"{len(g.timed)} findings ({', '.join(sorted({str(f['kind']) for f in g.timed}))})")
    if g.gaps:
        what.append(f"record gaps of {', '.join(sorted(str(f['stream']) for f in g.gaps))}")
    rel = (first.start_ms - load_start_ms) / 1000
    stressed_s = sum(sp.end_ms - sp.start_ms for sp in g.spans) / 1000
    when = (
        f"at +{rel:.0f} s for {stressed_s:.1f} s"
        if len(g.spans) == 1
        else f"{len(g.spans)} times from +{rel:.0f} s to +{(last.end_ms - load_start_ms) / 1000:.0f} s, {stressed_s:.1f} s in all"
    )
    peak = "peak " if len(g.spans) > 1 else ""
    detail = (
        f"host under stress {when} ({peak}io full {io:.1f} %, memory full {mem:.1f} %, "
        f"swap-in {swapin:.0f} pages/s): {'; '.join(what)}"
    )
    return {
        "kind": "host_under_stress",
        "detail": detail,
        "at_ms": first.start_ms,
        "until_ms": last.end_ms,
        "host": {"io_full_pct": round(io, 2), "memory_full_pct": round(mem, 2), "swapin_pages_s": round(swapin, 1)},
    }


def stress_findings(
    run_dir: Path,
    t: Thresholds,
    stalls: Sequence[Stall],
    spoiled: Mapping[Stall, int],
    found: Sequence[Json],
    load_start_ms: float,
) -> list[Json]:
    groups: list[_Group] = []
    for sp in spans(intervals(read_jsonl(run_dir / HOST_FILE)), t):
        hit = [st for st in stalls if sp.overlaps(st.at_ms, st.until_ms)]
        streams = {st.stream for st in hit}
        timed = [
            f
            for f in found
            if f.get("kind") in TIMED and (at := jv.opt_number(f, "at_ms")) is not None and sp.overlaps(at, at + 1)
        ]
        gaps = [f for f in found if f.get("kind") == "record_gap" and f.get("stream") in streams]
        if not (hit or timed or gaps):
            continue
        last = groups[-1] if groups else None
        if hit and last is not None and last.stalls == hit:
            last.take(sp, timed)
        else:
            groups.append(_Group([sp], hit, timed, gaps))
    return [_finding(g, spoiled, load_start_ms) for g in groups]
