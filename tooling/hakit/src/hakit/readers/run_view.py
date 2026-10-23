"""What the readers take from a run directory: the load span, the verdict's windows
and phases, the records and the generator stalls.

The windows are the verdict's (`verdict.json`), closed on recovery; without a
verdict yet they are rebuilt from `events.jsonl` as the runner closed them.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from collections import defaultdict

from .. import jsonval as jv
from ..generator_health import run_stalls
from ..jsonval import Json, as_obj
from ..profile import parse_profile
from ..jsonl import read_jsonl
from ..stalls import Stall
from ..windows import build_windows


@dataclass(frozen=True)
class WindowSpan:
    event: str
    opened_s: float
    closed_s: float


@dataclass(frozen=True)
class RunView:
    run: str
    load_start_s: float
    load_end_s: float
    ended_s: float
    windows: list[WindowSpan]
    markers: list[tuple[float, str]]
    records: list[Json]
    stalls: list[Stall]

    def in_window(self, from_s: float, to_s: float) -> bool:
        """[from_s, to_s] touches a window."""
        return any(from_s <= w.closed_s and to_s >= w.opened_s for w in self.windows)


def _windows(run_dir: Path) -> tuple[list[WindowSpan], list[tuple[float, str]]]:
    vp = run_dir / "verdict.json"
    if vp.is_file():
        v = as_obj(json.loads(vp.read_text()))
        ws = jv.objs(v, "windows")
        spans = [WindowSpan(jv.string(w, "event"), jv.number(w, "opened_at_ms") / 1000, jv.number(w, "closed_at_ms") / 1000) for w in ws]
        marks = [
            (jv.number(p, "at_ms") / 1000, f"{jv.string(p, 'step')} {jv.string(p, 'phase')}")
            for w in ws
            for p in jv.objs(w, "phases")
            if jv.string(p, "phase") in ("inject", "healed")
        ]
        return spans, marks
    built = build_windows(read_jsonl(run_dir / "events.jsonl"))
    spans = [WindowSpan(w.event, w.opened_at_ms / 1000, w.closed_at_ms / 1000) for w in built]
    marks = [(p.at_ms / 1000, f"{p.step} {p.phase}") for w in built for p in w.phases if p.phase in ("inject", "healed")]
    return spans, marks


def read_run(run_dir: Path) -> RunView:
    meta = as_obj(json.loads((run_dir / "run-meta.json").read_text()))
    profile = parse_profile(as_obj(json.loads((run_dir / "profile.json").read_text())))
    start_ms = jv.opt_number(meta, "load_started_at_ms") or jv.number(meta, "started_at_ms")
    windows, markers = _windows(run_dir)
    records = read_jsonl(run_dir / "records.jsonl")
    starts: dict[str, list[float]] = defaultdict(list)
    for r in records:
        starts[jv.string(r, "stream")].append(jv.number(r, "started_at_ms"))
    return RunView(
        run=jv.string(meta, "run"),
        load_start_s=start_ms / 1000,
        load_end_s=start_ms / 1000 + profile.duration_s,
        ended_s=jv.number(meta, "ended_at_ms") / 1000,
        windows=windows,
        markers=sorted(markers),
        records=records,
        stalls=run_stalls(run_dir, starts, profile, start_ms),
    )
