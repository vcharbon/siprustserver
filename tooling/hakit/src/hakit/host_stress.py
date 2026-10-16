"""`host_under_stress`: a measurement fault that coincides with host pressure
(ADR-0049 §3.12, §6.5).

The stressed intervals of `host.jsonl` (host.py) are merged into spans. A
generator stall, a spoiled measurement or a finding with an instant that
overlaps a span, and a record gap of a stream with such a stall, make one
`host_under_stress` finding for the span: its instants, the worst io and
memory full share and swap-in rate in it, and what it overlapped. It stands
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


def stress_findings(
    run_dir: Path,
    t: Thresholds,
    stalls: Sequence[Stall],
    spoiled: Mapping[Stall, int],
    found: Sequence[Json],
    load_start_ms: float,
) -> list[Json]:
    out: list[Json] = []
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
        what: list[str] = []
        if hit:
            what.append(f"{len(hit)} generator stalls ({sum(spoiled.get(st, 0) for st in hit)} measurements spoiled)")
        if timed:
            what.append(f"{len(timed)} findings ({', '.join(sorted({str(f['kind']) for f in timed}))})")
        if gaps:
            what.append(f"record gaps of {', '.join(sorted(str(f['stream']) for f in gaps))}")
        rel = (sp.start_ms - load_start_ms) / 1000
        detail = (
            f"host under stress at +{rel:.0f} s for {(sp.end_ms - sp.start_ms) / 1000:.1f} s "
            f"(io full {sp.io_full_pct:.1f} %, memory full {sp.memory_full_pct:.1f} %, "
            f"swap-in {sp.swapin_pages_s:.0f} pages/s): {'; '.join(what)}"
        )
        out.append(
            {
                "kind": "host_under_stress",
                "detail": detail,
                "at_ms": sp.start_ms,
                "until_ms": sp.end_ms,
                "host": {
                    "io_full_pct": round(sp.io_full_pct, 2),
                    "memory_full_pct": round(sp.memory_full_pct, 2),
                    "swapin_pages_s": round(sp.swapin_pages_s, 1),
                },
            }
        )
    return out
