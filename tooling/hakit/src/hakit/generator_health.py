"""The generator stalls of a run, from its records and its generators' statistics
(ADR-0049 §3.12). Read by the verdict and by the readers alike.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from pathlib import Path

from . import jsonval as jv
from .generator_stats import StatRow, read_stat_csv
from .profile import Profile, Stream
from .stalls import AtLimit, RateBetween, Stall, detect, watchdog_stalls

# A row this close to a gap tells the calls in flight during it (rows are 1 s apart).
LIMIT_ROW_MARGIN_MS = 1000


def call_limits(run_dir: Path) -> dict[str, int]:
    """Each stream's call limit as the runner set it (`streams.json` `call_limit`)."""
    p = run_dir / "streams.json"
    if not p.is_file():
        return {}
    out: dict[str, int] = {}
    for s in jv.as_list(json.loads(p.read_text())):
        d = jv.as_obj(s)
        limit = jv.opt_int(d, "call_limit")
        if limit is not None:
            out[jv.string(d, "stream")] = limit
    return out


def _at_limit(rows: Sequence[StatRow], limit: int | None) -> AtLimit:
    def held(a: float, b: float) -> bool:
        return limit is not None and any(
            r.current_calls is not None
            and r.current_calls >= limit
            and a - LIMIT_ROW_MARGIN_MS <= r.at_ms <= b + LIMIT_ROW_MARGIN_MS
            for r in rows
        )

    return held


def _rate(stream: Stream, load_start_ms: float) -> RateBetween:
    def between(a: float, b: float) -> float:
        return stream.rate_between((a - load_start_ms) / 1000, (b - load_start_ms) / 1000)

    return between


def run_stalls(run_dir: Path, starts: Mapping[str, Sequence[float]], profile: Profile, load_start_ms: float) -> list[Stall]:
    limits = call_limits(run_dir)
    found: list[Stall] = []
    for name, st in sorted(starts.items()):
        s = profile.stream(name)
        if s is None:
            continue
        rows = read_stat_csv(run_dir / "streams" / name / "stat.csv")
        found += detect(name, st, _rate(s, load_start_ms), profile.generator_stall_ms, _at_limit(rows, limits.get(name)))
        found += watchdog_stalls(name, rows)
    return found
